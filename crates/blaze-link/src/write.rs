//! Relocation, in parallel: every piece to write gets a disjoint slice of its region's
//! bytes, and each core copies its pieces' contents and applies their fixups. Then the
//! image as the writer sees it ([`Placed`]), the writer's synthetic regions, and the file.

use std::sync::Arc;

use blaze_codegen::Arch;
use rayon::prelude::*;
use rustc_hash::FxHashMap;

use crate::model::{
    Apply, Bind, Class, Export, Fixup, Function, GotEntry, Holds, Placed, Region, Special, Sym,
    SymbolKind, Tls, WindowsUnwind,
};
use crate::reloc::{self, Ctx, GotKind};
use crate::stage::{Stage, describe, error, function_of};
use crate::state::{Def, Piece, PieceId, What};
use crate::symbols::SymKey;
use crate::{Debug, Format, LinkError, OutputKind};

/// What a symbol is worth to a fixup.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Value {
    /// An address in the image: slid by the loader.
    Image(u64),
    /// A number: not slid.
    Absolute(u64),
    Import(u32),
    /// An offset in the image's TLS block.
    Tls(u64),
}

/// A piece's relocated bytes' loader work: rebased offsets and binds.
type Loader = (Vec<u64>, Vec<(u64, u32, i64)>);

/// What the writer reads, owned; [`Parts::placed`] borrows it.
pub(crate) struct Parts {
    pub regions: Vec<Region>,
    pub got: Vec<GotEntry>,
    pub rebase: Vec<u64>,
    pub binds: Vec<Bind>,
    pub exports: Vec<Export>,
    pub functions: Vec<Function>,
    pub entry: Option<u64>,
    pub init: Option<u64>,
    pub fini: Option<u64>,
    pub id: [u8; 16],
    pub debug: Vec<(&'static str, Vec<u8>)>,
}

impl Stage<'_> {
    pub fn value(&self, regions: &[Region], mut sym: Sym) -> Result<Value, String> {
        for _ in 0..64 {
            return Ok(match self.new.def(sym) {
                Def::Piece { piece, offset } => {
                    let piece = self.new.piece(piece).ok_or("a symbol in a removed piece")?;
                    Value::Image(piece.addr + offset)
                }
                Def::Import(at) => Value::Import(at),
                Def::Absolute(value) => Value::Absolute(value),
                Def::TlsOffset(value) => Value::Tls(value),
                Def::Alias(to) => {
                    sym = to;
                    continue;
                }
                Def::Special(at) => self.special(regions, &self.new.specials[at as usize])?,
                Def::None => return Err(format!("{} is not defined", describe(self.symbols.key(sym)))),
            });
        }
        Err(format!("{} is an alias of itself", describe(self.symbols.key(sym))))
    }

    fn special(&self, regions: &[Region], special: &Special) -> Result<Value, String> {
        let bounds = |matches: &dyn Fn(&Class) -> bool, what: &str| -> Result<(u64, u64), String> {
            let mut found = regions.iter().filter(|it| matches(&it.class));
            let first = found.next().ok_or_else(|| format!("the image has no {what}"))?;
            let last = found.last().unwrap_or(first);
            Ok((first.addr, last.addr + last.size))
        };
        Ok(match special {
            Special::ImageBase => Value::Image(self.plan.base),
            Special::Start(class) => Value::Image(bounds(&|it| it == class, &format!("{class:?}"))?.0),
            Special::Stop(class) => Value::Image(bounds(&|it| it == class, &format!("{class:?}"))?.1),
            Special::Section { name, end } => {
                let (start, stop) = bounds(
                    &|it| matches!(it, Class::Named { name: n, .. } if n == name),
                    &format!("section `{name}`"),
                )?;
                Value::Image(if *end { stop } else { start })
            }
            Special::Placed(at) => Value::Image(at(regions)),
            Special::Absolute(value) => Value::Absolute(*value),
            Special::LiveDescriptor => {
                let sym = self.symbols.find(&SymKey::Descriptor).ok_or("an image with no live descriptor")?;
                self.value(regions, sym)?
            }
            Special::Got(name) => {
                let sym = self.symbols.find(&SymKey::Name(name.as_str().into())).ok_or_else(|| format!("`{name}` is not defined"))?;
                Value::Image(self.got_cell(GotKind::Address, sym)?)
            }
        })
    }

    fn at_of(&self, key: SymKey) -> Option<u64> {
        let sym = self.symbols.find(&key)?;
        let (piece, offset) = self.new.home(sym)?;
        Some(self.new.piece(piece)?.addr + offset)
    }

    fn got_cell(&self, kind: GotKind, target: Sym) -> Result<u64, String> {
        self.at_of(SymKey::Got(kind, Box::new(self.symbols.key(target).clone())))
            .ok_or_else(|| format!("no GOT cell of {}", describe(self.symbols.key(target))))
    }

    /// The function indexes of wasm code by code address: the imports first (every wasm
    /// import is a function), then the pieces of the code space and the functions the
    /// writer synthesizes (one at the start of each of its code regions), by address.
    fn wasm_indexes(&self, regions: &[Region]) -> FxHashMap<u64, u64> {
        let imports = self.new.resolved.imports.len();
        let mut code: Vec<u64> = self
            .new
            .live_pieces()
            .filter(|(_, it)| regions.get(it.region as usize).is_some_and(|r| r.prot.exec))
            .map(|(_, it)| it.addr)
            .chain(regions.iter().filter(|it| it.prot.exec && matches!(it.class, Class::Synthetic(_))).map(|it| it.addr))
            .collect();
        code.sort_unstable();
        code.dedup();
        code.into_iter().enumerate().map(|(at, addr)| (addr, (imports + at) as u64)).collect()
    }

    /// Applies a piece's fixups to `field`, its bytes placed at `piece.addr`.
    fn relocate(
        &self,
        piece: &Piece,
        field: &mut [u8],
        regions: &[Region],
        tls: Option<Tls>,
        wasm: &FxHashMap<u64, u64>,
    ) -> Result<Loader, LinkError> {
        let mut rebase = Vec::new();
        let mut binds = Vec::new();
        let fail = |fixup: &Fixup, what: String| {
            error(
                function_of(piece),
                format!(
                    "{:?} at {:#x} in {}: {what}",
                    fixup.apply,
                    fixup.at,
                    describe(self.symbols.key(piece.sym))
                ),
            )
        };
        if let What::Got { target, .. } = piece.what {
            if matches!(self.value(regions, target), Ok(Value::Import(_))) {
                field.fill(0);
                return Ok((rebase, binds));
            }
        }
        let unwind = matches!(piece.what, What::Native { unwind: true, .. });
        for fixup in piece.fixups.iter() {
            let p = piece.addr + fixup.at;
            let place = field.get_mut(fixup.at as usize..).ok_or_else(|| fail(fixup, "past the piece's end".into()))?;
            let value = match self.value(regions, fixup.target) {
                Ok(value) => value,
                Err(_) if unwind => {
                    let width = reloc::width(fixup.apply).min(place.len());
                    place[..width].fill(0);
                    continue;
                }
                Err(what) => return Err(fail(fixup, what)),
            };
            let tls_base = tls.map_or(0, |it| it.addr);
            let s = match value {
                Value::Image(s) | Value::Absolute(s) => s,
                Value::Tls(offset) => tls_base + offset,
                Value::Import(_) => 0,
            };
            let m = match fixup.minus {
                Some(minus) => match self.value(regions, minus).map_err(|it| fail(fixup, it))? {
                    Value::Image(m) | Value::Absolute(m) => m,
                    Value::Tls(offset) => tls_base + offset,
                    Value::Import(_) => return Err(fail(fixup, "the difference of an import".into())),
                },
                None => 0,
            };
            if let Apply::Native(r_type) = fixup.apply {
                let s = match (self.output.format, r_type, value) {
                    (
                        Format::Wasm,
                        reloc::WASM_FUNCTION_INDEX_LEB
                        | reloc::WASM_FUNCTION_INDEX_I32
                        | reloc::WASM_TABLE_INDEX_SLEB
                        | reloc::WASM_TABLE_INDEX_I32,
                        _,
                    ) => {
                        let slot = u64::from(matches!(r_type, reloc::WASM_TABLE_INDEX_SLEB | reloc::WASM_TABLE_INDEX_I32));
                        match value {
                            Value::Import(at) => u64::from(at) + slot,
                            Value::Image(s) => {
                                *wasm.get(&s).ok_or_else(|| fail(fixup, "not a function's start".into()))? + slot
                            }
                            Value::Absolute(0) if slot == 1 => 0,
                            _ => {
                                return Err(fail(
                                    fixup,
                                    format!(
                                        "{} is not defined, and a call by index has no null function to go to",
                                        describe(self.symbols.key(fixup.target))
                                    ),
                                ));
                            }
                        }
                    }
                    (_, _, Value::Import(_)) => {
                        return Err(fail(fixup, "a format relocation to an import".into()));
                    }
                    _ => s,
                };
                self.writer.apply(r_type, place, p, s, fixup.addend).map_err(|it| fail(fixup, it))?;
                continue;
            }
            let g = match reloc::got_kind(fixup.apply) {
                Some(kind) => {
                    self.got_cell(kind, self.got_target(fixup)).map_err(|it| fail(fixup, it))?
                }
                None => 0,
            };
            let mut slid = matches!(value, Value::Image(_));
            let s = match value {
                Value::Import(_) if self.through_plt(fixup) => {
                    slid = true;
                    self.at_of(SymKey::Plt(Box::new(self.symbols.key(fixup.target).clone())))
                        .ok_or_else(|| fail(fixup, "no PLT stub".into()))?
                }
                Value::Import(_) if self.plan.got_by_library && reloc::got_kind(fixup.apply).is_none() => {
                    return Err(fail(
                        fixup,
                        format!(
                            "{} is imported data, which is reached only through its import address table \
                             cell (`__imp_`)",
                            describe(self.symbols.key(fixup.target))
                        ),
                    ));
                }
                Value::Import(at) if fixup.apply == Apply::Abs64 => {
                    binds.push((fixup.at, at, fixup.addend));
                    place[..8].fill(0);
                    continue;
                }
                Value::Import(_) if reloc::got_kind(fixup.apply).is_some() => 0,
                Value::Import(_) => {
                    return Err(fail(
                        fixup,
                        format!(
                            "{} is imported: reaching it this way needs a copy relocation, which blaze \
                             does not make; reach it through the GOT",
                            describe(self.symbols.key(fixup.target))
                        ),
                    ));
                }
                _ => s,
            };
            if fixup.apply == Apply::Abs64 && slid {
                rebase.push(fixup.at);
            }
            let ctx = Ctx {
                arch: self.output.arch,
                format: self.output.format,
                s,
                a: fixup.addend,
                p,
                g,
                m,
                base: self.plan.base,
                tls,
            };
            reloc::apply(fixup.apply, &ctx, place).map_err(|it| fail(fixup, it))?;
        }
        Ok((rebase, binds))
    }

    /// Copies and relocates every piece to write, in parallel.
    pub fn relocate_all(&mut self) -> Result<(), LinkError> {
        let regions = self.new.regions();
        let tls = self.new.tls;
        let wasm = if self.output.format == Format::Wasm { self.wasm_indexes(&regions) } else { FxHashMap::default() };
        let mut ids: Vec<PieceId> = self.write.iter().copied().filter(|it| self.new.piece(*it).is_some()).collect();
        ids.sort_by_key(|id| {
            let piece = self.new.piece(*id).expect("live");
            (piece.region, piece.addr)
        });
        let mut spaces = std::mem::take(&mut self.new.spaces);
        for space in &mut spaces {
            if !space.zero && !matches!(space.region.class, Class::Synthetic(_)) {
                let size = space.region.size as usize;
                let bytes = Arc::make_mut(&mut space.bytes);
                if bytes.len() < size {
                    bytes.resize(size, 0);
                }
            }
        }
        let starts: Vec<u64> = spaces.iter().map(|it| it.region.addr).collect();
        let mut jobs: Vec<(PieceId, &mut [u8])> = Vec::with_capacity(ids.len());
        let mut rest: Vec<Option<&mut [u8]>> = Vec::new();
        for space in spaces.iter_mut() {
            if space.zero || matches!(space.region.class, Class::Synthetic(_)) {
                rest.push(None);
            } else {
                rest.push(Some(Arc::get_mut(&mut space.bytes).expect("made unique above").as_mut_slice()));
            }
        }
        let mut taken = vec![0u64; starts.len()];
        for id in &ids {
            let piece = self.new.piece(*id).expect("live");
            let region = piece.region as usize;
            let Some(slot) = rest.get_mut(region) else {
                return Err(error(function_of(piece), "a piece in no region"));
            };
            let Some(buffer) = slot.take() else {
                if piece.fixups.is_empty() {
                    continue;
                }
                return Err(error(
                    function_of(piece),
                    format!("{} has relocations in a zeroed region", describe(self.symbols.key(piece.sym))),
                ));
            };
            let start = starts[region];
            let offset = piece.addr - start - taken[region];
            let (_, tail) = buffer.split_at_mut(offset as usize);
            let (mine, after) = tail.split_at_mut(piece.size as usize);
            taken[region] = piece.addr - start + piece.size;
            *slot = Some(after);
            jobs.push((*id, mine));
        }
        let full = self.full;
        let results: Vec<Result<(PieceId, Loader), LinkError>> = jobs
            .into_par_iter()
            .map(|(id, field)| {
                let piece = self.new.piece(id).expect("live");
                let source = match self.bytes.get(&id) {
                    Some(bytes) => Some(&**bytes),
                    None => match self.old.piece(id) {
                        Some(old) if full || old.addr != piece.addr || old.region != piece.region => {
                            Some(self.old.bytes(old))
                        }
                        _ => None,
                    },
                };
                match source {
                    Some(bytes) if bytes.is_empty() => field.fill(0),
                    Some(bytes) if bytes.len() == field.len() => field.copy_from_slice(bytes),
                    Some(bytes) => {
                        return Err(error(
                            function_of(piece),
                            format!(
                                "{} has {} bytes for a piece of {}",
                                describe(self.symbols.key(piece.sym)),
                                bytes.len(),
                                field.len()
                            ),
                        ));
                    }
                    None => {}
                }
                let loader = self.relocate(piece, field, &regions, tls, &wasm)?;
                Ok((id, loader))
            })
            .collect();
        self.new.spaces = spaces;
        for result in results {
            let (id, (rebase, binds)) = result?;
            let piece = self.new.piece_mut(id).expect("live");
            piece.rebase = rebase.into();
            piece.binds = binds.into();
        }
        Ok(())
    }

    /// The image as the writer reads it.
    pub fn parts(&self) -> Result<Parts, LinkError> {
        let regions = self.new.regions();
        let tls = self.new.tls;
        let mut got = Vec::new();
        let mut rebase = Vec::new();
        let mut binds = Vec::new();
        let mut functions = Vec::new();
        for (_, piece) in self.new.live_pieces() {
            rebase.extend(piece.rebase.iter().map(|at| piece.addr + at));
            binds.extend(piece.binds.iter().map(|(at, import, addend)| Bind {
                addr: piece.addr + at,
                import: *import,
                addend: *addend,
            }));
            match &piece.what {
                What::Got { kind, target } => {
                    let value = self.value(&regions, *target).map_err(|it| error(None, it))?;
                    let offset = |s: u64| s.checked_sub(tls.map_or(0, |it| it.addr));
                    let holds = match (kind, value) {
                        (GotKind::Address, Value::Import(at)) => Holds::Import(at),
                        (GotKind::Address, Value::Image(s) | Value::Absolute(s)) => Holds::Local(s),
                        (GotKind::TpOff, Value::Import(at)) => Holds::ImportTpOff(at),
                        (GotKind::TlsGd, Value::Import(at)) => Holds::TlsGd { import: Some(at), offset: 0 },
                        (GotKind::TlsDesc, Value::Import(at)) => Holds::TlsDesc { import: Some(at), offset: 0 },
                        (kind, value) => {
                            let tls = tls.ok_or_else(|| error(None, "a thread-local GOT cell in an image with no TLS"))?;
                            let at = match value {
                                Value::Tls(offset) => offset,
                                Value::Image(s) => offset(s).ok_or_else(|| error(None, "a TLS cell of what isn't thread-local"))?,
                                _ => return Err(error(None, "a TLS cell of an absolute symbol")),
                            };
                            match kind {
                                GotKind::TpOff => Holds::TpOff(
                                    reloc::tp_offset(self.output.arch, tls, tls.addr + at).map_err(|it| error(None, it))?,
                                ),
                                GotKind::TlsGd => Holds::TlsGd { import: None, offset: at },
                                GotKind::TlsDesc => Holds::TlsDesc { import: None, offset: at },
                                GotKind::Address => unreachable!("matched above"),
                            }
                        }
                    };
                    got.push(GotEntry { addr: piece.addr, holds });
                }
                What::Function(info) => {
                    let fde = info.fde.and_then(|it| self.new.home(it)).and_then(|(p, _)| self.new.piece(p)).map(|p| p.addr);
                    let windows = match info.windows {
                        Some((unwind, handler, lsda)) => {
                            let at = |sym: Sym| -> Result<u64, LinkError> {
                                match self.value(&regions, sym).map_err(|it| error(Some(info.key), it))? {
                                    Value::Image(s) => Ok(s),
                                    _ => Err(error(Some(info.key), "an unwind handler not in the image")),
                                }
                            };
                            Some(WindowsUnwind {
                                info: at(unwind)?,
                                handler: handler.map(at).transpose()?,
                                lsda: lsda.map(at).transpose()?,
                            })
                        }
                        None => None,
                    };
                    functions.push(Function {
                        addr: piece.addr,
                        size: piece.size,
                        fde,
                        windows,
                        name: self.new.debug_names.get(&info.key).map(|it| it.to_string()),
                    });
                }
                What::Native { unwind: false, .. }
                    if self.plan.indexed_code && regions.get(piece.region as usize).is_some_and(|it| it.prot.exec) =>
                {
                    let name = piece.defines.iter().find_map(|sym| match self.symbols.key(*sym) {
                        SymKey::Name(name) => Some(name.to_string()),
                        _ => None,
                    });
                    functions.push(Function { addr: piece.addr, size: piece.size, fde: None, windows: None, name });
                }
                _ => {}
            }
        }
        got.sort_by_key(|it| it.addr);
        rebase.sort_unstable();
        binds.sort_by_key(|it| it.addr);
        functions.sort_by_key(|it| it.addr);
        let mut exports = Vec::new();
        for (name, symbol, data) in self.exports() {
            let Some(sym) = self.symbols.find(&SymKey::Name(symbol.as_str().into())) else {
                return Err(error(None, format!("the export `{name}` is not defined")));
            };
            let value = self.value(&regions, sym).map_err(|it| error(None, format!("the export `{name}`: {it}")))?;
            let Value::Image(addr) = value else {
                return Err(error(None, format!("the export `{name}` is not in the image")));
            };
            let (kind, size) = match self.new.home(sym).and_then(|(p, o)| Some((self.new.piece(p)?, o))) {
                Some((piece, offset)) => {
                    let class = &regions[piece.region as usize].class;
                    let kind = match (&piece.what, class) {
                        (_, Class::Tdata | Class::Tbss) => SymbolKind::Tls,
                        (What::Function(_), _) | (_, Class::Text | Class::Patch | Class::Stubs) => SymbolKind::Function,
                        _ => SymbolKind::Data,
                    };
                    (kind, if offset == 0 { piece.size } else { 0 })
                }
                None => (SymbolKind::Other, 0),
            };
            let kind = if data { SymbolKind::Data } else { kind };
            exports.push(Export { name, addr, size, kind });
        }
        let image = |name: &str| -> Option<u64> {
            let sym = self.symbols.find(&SymKey::Name(name.into()))?;
            match self.value(&regions, sym) {
                Ok(Value::Image(addr)) => Some(addr),
                _ => None,
            }
        };
        let entry = match &self.output.kind {
            OutputKind::Executable { entry } => Some(entry.as_str()),
            OutputKind::Shared { .. } => self.writer.shared_entry(),
        };
        let entry = match entry {
            Some(entry) => Some(
                image(entry).ok_or_else(|| error(None, format!("the entry point `{entry}` is not in the image")))?,
            ),
            None => None,
        };
        let generation = self.new.generation.to_le_bytes();
        let full = blaze_ids::ContentId::of("blaze v2 link image 1", &[&self.new.lineage[..], &generation].concat());
        let mut id = [0; 16];
        id.copy_from_slice(&full.0[..16]);
        let debug = match self.output.debug {
            Debug::None => Vec::new(),
            Debug::Lines => self.debug()?,
        };
        Ok(Parts {
            regions,
            got,
            rebase,
            binds,
            exports,
            functions,
            entry,
            init: image("_init"),
            fini: image("_fini"),
            id,
            debug,
        })
    }

    fn debug(&self) -> Result<Vec<(&'static str, Vec<u8>)>, LinkError> {
        let mut views = Vec::new();
        for (_, piece) in self.new.live_pieces() {
            let What::Function(info) = &piece.what else { continue };
            let mut lines = Vec::with_capacity(info.lines.len());
            for line in &info.lines {
                let table = self.new.spans.get(&line.span.anchor).ok_or_else(|| {
                    error(Some(info.key), format!("a line in {:?}, whose span table the link was not given", line.span.anchor))
                })?;
                let at = table.spans.get(line.span.index as usize).ok_or_else(|| {
                    error(Some(info.key), format!("span {} of {:?}, past its table", line.span.index, line.span.anchor))
                })?;
                lines.push((u64::from(line.at), table.files[at.file as usize].as_str(), at.line, at.column));
            }
            views.push(crate::dwarf::DebugFunction {
                addr: piece.addr,
                size: piece.size,
                name: self.new.debug_names.get(&info.key).map(|it| &**it),
                lines,
            });
        }
        let size = match self.output.arch {
            Arch::Wasm32 => 4,
            Arch::X86_64 | Arch::Aarch64 => 8,
        };
        crate::dwarf::sections(&views, size).map_err(|it| error(None, format!("debug info: {it}")))
    }
}

impl Stage<'_> {
    /// The address of a name the image defines, among `regions`: [`Placed::symbol`].
    pub fn address_of(&self, regions: &[Region], name: &str) -> Option<u64> {
        let sym = self.symbols.find(&SymKey::Name(name.into()))?;
        match self.value(regions, sym) {
            Ok(Value::Image(addr)) => Some(addr),
            _ => None,
        }
    }
}

impl Parts {
    pub fn placed<'a>(
        &'a self,
        stage: &'a Stage<'_>,
        contents: &'a [&'a [u8]],
        symbol: &'a dyn Fn(&str) -> Option<u64>,
    ) -> Placed<'a> {
        Placed {
            output: stage.output,
            name: stage.name,
            plan: stage.plan,
            regions: &self.regions,
            contents,
            libraries: &stage.new.resolved.libraries,
            imports: &stage.new.resolved.imports,
            got: &self.got,
            rebase: &self.rebase,
            binds: &self.binds,
            exports: &self.exports,
            functions: &self.functions,
            entry: self.entry,
            init: self.init,
            fini: self.fini,
            tls: stage.new.tls,
            id: self.id,
            debug: &self.debug,
            symbol,
        }
    }
}
