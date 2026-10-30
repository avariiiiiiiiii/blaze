//! The keys of a dependency image.
//!
//! - [`GraphKey`]: everything below the workspace members that is not the set of instances:
//!   the dependency units (package, target, features, cfg, rustflags), the toolchain, the
//!   codegen settings, the linker and the output it links for, the base address, the run
//!   loader's thread-local bias, the C objects and libraries of the link line and the span
//!   tables positions are filled from.
//!   Images of one graph differ only in which instances they hold.
//! - [`ImageKey`]: the graph key and the instances: every function by its instance key and
//!   code hash, every static by its item, instance and definition, every name the image
//!   defines. It is the store's key of the image and the name of its one memory object on
//!   the machine.
//!
//! Every input is hashed by destructuring its type, so a field added to one is a compile
//! error here until it is hashed. Lists are hashed sorted and length-prefixed, so the order
//! the caller found them in never changes a key.

use blaze_codegen::settings::Toolchain;
use blaze_codegen::{Arch, CompiledFunction, Init, InstanceKey, Reloc, Settings, Static, Target};
use blaze_ids::{ContentId, Hasher, radix};
use blaze_items::{CodeHash, ItemKey};
use blaze_link::{Debug, Format, NativeKind, Output, OutputKind, Platform};

pub const GRAPH_DOMAIN: &str = "blaze v2 premap graph 1";
pub const IMAGE_DOMAIN: &str = "blaze v2 premap image 1";
pub const STATIC_DOMAIN: &str = "blaze v2 premap static 1";

/// The dependency graph an image is of: what every image of it shares.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct GraphKey(pub ContentId);

/// One image: its graph and the instances in it.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct ImageKey(pub ContentId);

/// A unit below the workspace members, as its bytes depend on it.
#[derive(Clone, Copy, Debug)]
pub struct DepUnit<'a> {
    /// cargo's package id: name, version and source.
    pub package: &'a str,
    pub target: &'a str,
    /// The features on, any order.
    pub features: &'a [String],
    /// The `--cfg`s it is compiled with (its build script's `rustc-cfg`s and the profile's),
    /// any order.
    pub cfg: &'a [String],
    /// The rustflags its profile gives it, in order (their order is rustc's meaning).
    pub rustflags: &'a [String],
}

/// A native input of the link line as the image's bytes depend on it: its contents, how it
/// is linked and where on the line it is (resolution takes the first definition in order).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct NativeInput {
    pub id: ContentId,
    pub kind: NativeKind,
    pub order: u32,
}

/// Everything but the instances.
#[derive(Clone, Copy, Debug)]
pub struct GraphInputs<'a> {
    pub units: &'a [DepUnit<'a>],
    pub toolchain: &'a Toolchain,
    /// Every codegen settings ID the image's functions were compiled with, any order.
    pub settings: &'a [Settings],
    /// blaze-link's code identity: another linker places other bytes.
    pub linker: ContentId,
    pub output: &'a Output,
    pub base: u64,
    /// Where the run's loader keeps the program's thread-locals: the image's code reaches
    /// them at this bias.
    pub tls_bias: i64,
    pub natives: &'a [NativeInput],
    /// The span side tables positions are filled from: each anchor and its table's content ID.
    pub spans: &'a [(ContentId, ContentId)],
}

/// A function of the image: its key and its generic's code hash.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct KeyedFunction {
    pub key: InstanceKey,
    pub code: CodeHash,
}

/// A static of the image: which one, and the content ID of its definition
/// ([`static_definition`]): a static's value is not in its key.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct KeyedStatic {
    pub item: ItemKey,
    pub instance: ContentId,
    pub definition: ContentId,
}

/// The instances of an image.
#[derive(Clone, Copy, Debug)]
pub struct ImageInputs<'a> {
    pub functions: &'a [KeyedFunction],
    pub statics: &'a [KeyedStatic],
    /// The names its Rust code defines (`#[no_mangle]`, `#[export_name]`) and what each is.
    pub names: &'a [(String, Target)],
    /// The names for debuggers and backtraces, when the output carries debug info.
    pub debug_names: &'a [(InstanceKey, String)],
}

impl GraphKey {
    pub fn of(inputs: &GraphInputs<'_>) -> GraphKey {
        let GraphInputs { units, toolchain, settings, linker, output, base, tls_bias, natives, spans } = inputs;
        let mut hasher = Hasher::new(GRAPH_DOMAIN);

        let mut order: Vec<usize> = (0..units.len()).collect();
        order.sort_unstable_by(|a, b| {
            let (a, b) = (&units[*a], &units[*b]);
            (a.package, a.target).cmp(&(b.package, b.target))
        });
        count(&mut hasher, units.len());
        for at in order {
            let DepUnit { package, target, features, cfg, rustflags } = &units[at];
            hasher.part(package.as_bytes()).part(target.as_bytes());
            set(&mut hasher, features);
            set(&mut hasher, cfg);
            count(&mut hasher, rustflags.len());
            for flag in rustflags.iter() {
                hasher.part(flag.as_bytes());
            }
        }

        toolchain_into(&mut hasher, toolchain);

        let mut ids: Vec<[u8; 32]> = settings.iter().map(|it| it.0.0).collect();
        ids.sort_unstable();
        ids.dedup();
        count(&mut hasher, ids.len());
        hasher.update(ids.as_flattened());

        hasher.id(linker);
        output_into(&mut hasher, output);
        hasher.update(&base.to_le_bytes()).update(&tls_bias.to_le_bytes());

        let mut natives: Vec<&NativeInput> = natives.iter().collect();
        radix::sort_copy_by_key(&mut natives, |it| it.order);
        count(&mut hasher, natives.len());
        for NativeInput { id, kind, order } in natives {
            hasher.id(id).update(&order.to_le_bytes());
            match kind {
                NativeKind::Object => hasher.update(&[0]),
                NativeKind::Archive { whole } => hasher.update(&[1, *whole as u8]),
                NativeKind::Shared { as_needed } => hasher.update(&[2, *as_needed as u8]),
            };
        }

        let mut spans: Vec<&(ContentId, ContentId)> = spans.iter().collect();
        spans.sort_unstable();
        count(&mut hasher, spans.len());
        for (anchor, table) in spans {
            hasher.id(anchor).id(table);
        }
        GraphKey(hasher.finish())
    }
}

impl ImageKey {
    pub fn of(graph: GraphKey, inputs: &ImageInputs<'_>) -> ImageKey {
        let ImageInputs { functions, statics, names, debug_names } = inputs;
        let mut hasher = Hasher::new(IMAGE_DOMAIN);
        hasher.id(&graph.0);

        // One flat buffer of 64-byte records, sorted, hashed in one call: BLAKE3 hashes a
        // long input on all its SIMD lanes at once.
        let mut flat: Vec<[u8; 64]> = functions
            .iter()
            .map(|KeyedFunction { key, code }| {
                let mut it = [0; 64];
                it[..32].copy_from_slice(&key.0.0);
                it[32..].copy_from_slice(&code.0.0);
                it
            })
            .collect();
        flat.sort_unstable();
        flat.dedup();
        count(&mut hasher, flat.len());
        hasher.update(flat.as_flattened());

        let mut flat: Vec<[u8; 96]> = statics
            .iter()
            .map(|KeyedStatic { item, instance, definition }| {
                let mut it = [0; 96];
                it[..32].copy_from_slice(&item.0.0);
                it[32..64].copy_from_slice(&instance.0);
                it[64..].copy_from_slice(&definition.0);
                it
            })
            .collect();
        flat.sort_unstable();
        flat.dedup();
        count(&mut hasher, flat.len());
        hasher.update(flat.as_flattened());

        let mut sorted: Vec<&(String, Target)> = names.iter().collect();
        sorted.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        count(&mut hasher, sorted.len());
        for (name, target) in sorted {
            hasher.part(name.as_bytes());
            target_into(&mut hasher, target);
        }

        let mut sorted: Vec<&(InstanceKey, String)> = debug_names.iter().collect();
        sorted.sort_unstable();
        count(&mut hasher, sorted.len());
        for (key, name) in sorted {
            hasher.id(&key.0).part(name.as_bytes());
        }
        ImageKey(hasher.finish())
    }
}

/// The content ID of a static's definition: its value, alignment, mutability, thread-local
/// storage, section and the data its value refers to.
pub fn static_definition(value: &Static) -> ContentId {
    let Static { item, instance, align, init, data, mutable, thread_local, section } = value;
    let mut hasher = Hasher::new(STATIC_DOMAIN);
    hasher.id(&item.0).id(instance).update(&align.to_le_bytes());
    match init {
        Init::Bytes { bytes, relocs } => {
            hasher.update(&[0]).part(bytes);
            relocs_into(&mut hasher, relocs);
        }
        Init::Zeroed { size } => {
            hasher.update(&[1]).update(&size.to_le_bytes());
        }
    }
    count(&mut hasher, data.len());
    for blaze_codegen::Data { id, align, bytes, relocs } in data {
        hasher.id(id).update(&align.to_le_bytes()).part(bytes);
        relocs_into(&mut hasher, relocs);
    }
    hasher.update(&[*mutable as u8, *thread_local as u8]);
    match section {
        Some(section) => hasher.update(&[1]).part(section.as_bytes()),
        None => hasher.update(&[0]),
    };
    hasher.finish()
}

/// What the key holds of a function: its key and its code hash, the code hash given by the
/// caller from the instance it compiled.
pub fn keyed(function: &CompiledFunction, code: CodeHash) -> KeyedFunction {
    KeyedFunction { key: function.key, code }
}

fn count(hasher: &mut Hasher, n: usize) {
    hasher.update(&(n as u64).to_le_bytes());
}

/// A set of strings, sorted and deduplicated. Per unit, once per key: units are hundreds.
fn set(hasher: &mut Hasher, items: &[String]) {
    let mut sorted: Vec<&str> = items.iter().map(String::as_str).collect();
    sorted.sort_unstable();
    sorted.dedup();
    count(hasher, sorted.len());
    for item in sorted {
        hasher.part(item.as_bytes());
    }
}

fn toolchain_into(hasher: &mut Hasher, toolchain: &Toolchain) {
    let Toolchain { id, target, arch, cpu, features, cranelift, llvm, frame_pointers, relocation } = toolchain;
    hasher.id(id).part(target.as_bytes()).update(&[arch_tag(*arch)]).part(cpu.as_bytes());
    let mut sorted: Vec<&str> = features.iter().map(String::as_str).collect();
    sorted.sort_unstable();
    count(hasher, sorted.len());
    for feature in sorted {
        hasher.part(feature.as_bytes());
    }
    hasher.part(cranelift.as_bytes());
    match llvm {
        Some(llvm) => hasher.update(&[1]).part(llvm.as_bytes()),
        None => hasher.update(&[0]),
    };
    hasher.update(&[*frame_pointers as u8, *relocation as u8]);
}

fn output_into(hasher: &mut Hasher, output: &Output) {
    let Output { format, arch, kind, interpreter, platform, incremental, debug, live, tags, stack, windows_subsystem } =
        output;
    hasher.update(&[format_tag(*format), arch_tag(*arch)]);
    match kind {
        OutputKind::Executable { entry } => {
            hasher.update(&[0]).part(entry.as_bytes());
        }
        OutputKind::Shared { name, exports } => {
            hasher.update(&[1]).part(name.as_bytes());
            count(hasher, exports.len());
            for export in exports {
                hasher.part(export.as_bytes());
            }
        }
    }
    match interpreter {
        Some(it) => hasher.update(&[1]).part(it.as_bytes()),
        None => hasher.update(&[0]),
    };
    match platform {
        Some(Platform { platform, minimum, sdk }) => hasher
            .update(&[1])
            .update(&platform.to_le_bytes())
            .update(&minimum.to_le_bytes())
            .update(&sdk.to_le_bytes()),
        None => hasher.update(&[0]),
    };
    hasher.update(&[*incremental as u8, matches!(debug, Debug::Lines) as u8]);
    // Where running programs take live patches: an image is never patched, so a live output
    // is refused before it is keyed ([`crate::prelink`]); hashed all the same.
    match live {
        Some(live) => hasher.update(&[1]).part(live.dir.as_str().as_bytes()).part(live.path.as_bytes()),
        None => hasher.update(&[0]),
    };
    hasher.update(&tags.to_le_bytes());
    match stack {
        Some(stack) => hasher.update(&[1]).update(&stack.to_le_bytes()),
        None => hasher.update(&[0]),
    };
    hasher.update(&[*windows_subsystem as u8]);
}

fn relocs_into(hasher: &mut Hasher, relocs: &[Reloc]) {
    count(hasher, relocs.len());
    for Reloc { at, kind, target, addend } in relocs {
        hasher.update(&at.to_le_bytes()).update(&[blaze_codegen::content::kind_tag(*kind)]).update(&addend.to_le_bytes());
        target_into(hasher, target);
    }
}

fn target_into(hasher: &mut Hasher, target: &Target) {
    match target {
        Target::Function(key) => hasher.update(&[0]).id(&key.0),
        Target::Direct(key) => hasher.update(&[1]).id(&key.0),
        Target::Static { item, instance } => hasher.update(&[2]).id(&item.0).id(instance),
        Target::Data(id) => hasher.update(&[3]).id(id),
        Target::Extern(name) => hasher.update(&[4]).part(name.as_bytes()),
        Target::Weak(name) => hasher.update(&[5]).part(name.as_bytes()),
        Target::Position(span) => hasher.update(&[6]).id(&span.anchor).update(&span.index.to_le_bytes()),
    };
}

pub(crate) fn arch_tag(arch: Arch) -> u8 {
    match arch {
        Arch::X86_64 => 0,
        Arch::Aarch64 => 1,
        Arch::Wasm32 => 2,
    }
}

pub(crate) fn format_tag(format: Format) -> u8 {
    match format {
        Format::Elf => 0,
        Format::MachO => 1,
        Format::Pe => 2,
        Format::Wasm => 3,
    }
}