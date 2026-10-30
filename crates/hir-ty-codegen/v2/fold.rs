//! Folding functions that differ only in constants (#1292) over Cranelift IR: the masked form
//! of a prepared function ([`masked`]) and a group's one body with a thunk per member
//! ([`merge`]). Which functions fold is [`blaze_codegen::fold::groups`]'s.
//!
//! A masked function is its IR printed as Cranelift prints it, in layout order, with every
//! value, block, signature, stack slot, pool constant, function and global value numbered by
//! its first use, so two functions built alike are equal whatever their entities' numbers.
//! `iconst`, `f32const` and `f64const` print their immediate as its index among the
//! constants, and a function or global value named by a place prints the place's index there.
//!
//! The merged body is the first member's IR with each varying constant read from a trailing
//! parameter: an immediate's uses read the parameter, a varying function is called through
//! it (`call_indirect`) and its address is the parameter, a varying static's address is the
//! parameter. Each thunk has its member's signature and calls the body with its arguments and
//! its constants: a plain call and return, as Cranelift tail-calls only under `tail`, which
//! no member is.

use std::collections::HashMap;

use blaze_codegen::{
    CodegenError, Unit,
    backend::Emitted,
    fold::{Constant, Group, Masked, Merged},
    settings::Spec,
    template::Refs,
};
use blaze_ids::ContentId;
use cranelift_codegen::{
    cursor::{Cursor as _, FuncCursor},
    ir::{self, InstBuilder as _, InstructionData, Opcode},
};

/// The namespace of the name a thunk calls the merged body by: the last of the references it
/// is compiled against, [`Group::thunk_refs`]'s [`blaze_codegen::Target::Direct`] of the body.
pub(crate) const BODY: u32 = u32::MAX - 1;

/// What a name of a prepared function's IR is in the build.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Name {
    /// The `n`th of the instance's references: a function or a static.
    Place(u32),
    /// Anything else, by what it is: a data object by its bytes with what it points at by
    /// target (never by place), a position by its span. Equal for two functions only when it
    /// is the same thing in both.
    Fixed(ContentId),
}

/// A prepared Cranelift function, as the adapter holds it.
pub(crate) trait Source {
    fn function(&self) -> &ir::Function;
    /// What `name`, a name of [`Source::function`]'s, is; `None` for one that is neither a
    /// place nor something [`Name::Fixed`] can say (a callee still to be inlined).
    fn name(&self, name: ir::UserExternalName) -> Option<Name>;
}

/// The constants of `func` masked, for [`blaze_codegen::fold::groups`]. `None` for a function
/// that can't take parameters: without a body, with a dynamic stack slot, a static named by
/// a place that is the base of another global value, a patchable callee named by a place, or
/// a name `names` doesn't know. A C-variadic or naked function, and one whose address is
/// significant, are the caller's to leave out.
pub(crate) fn masked(
    func: &ir::Function,
    names: &dyn Fn(ir::UserExternalName) -> Option<Name>,
) -> Option<Masked> {
    walk(func, names).map(|it| it.masked)
}

/// Where a masked constant is in the function.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Site {
    /// An `iconst`, `f32const` or `f64const`.
    Inst(ir::Inst),
    Func(ir::FuncRef),
    Global(ir::GlobalValue),
}

struct Walk {
    masked: Masked,
    /// Of each of `masked.constants`.
    sites: Vec<Site>,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Kind {
    Value,
    Block,
    Func,
    Sig,
    Global,
    Slot,
    Pool,
}

/// What an entity is printed as, and which of them are numbered by first use. `None`: an
/// entity Cranelift prints inline, which a walk refuses if it ever names it by number.
const PREFIXES: [(&str, Option<Kind>); 11] = [
    ("block", Some(Kind::Block)),
    ("const", Some(Kind::Pool)),
    ("dss", None),
    ("sig", Some(Kind::Sig)),
    ("fn", Some(Kind::Func)),
    ("gv", Some(Kind::Global)),
    ("jt", None),
    ("et", None),
    ("dt", None),
    ("ss", Some(Kind::Slot)),
    ("v", Some(Kind::Value)),
];

struct Canon<'a> {
    func: &'a ir::Function,
    names: &'a dyn Fn(ir::UserExternalName) -> Option<Name>,
    text: String,
    numbers: HashMap<(Kind, u32), u32>,
    counts: HashMap<Kind, u32>,
    constants: Vec<Constant>,
    sites: Vec<Site>,
    refused: bool,
}

fn walk(func: &ir::Function, names: &dyn Fn(ir::UserExternalName) -> Option<Name>) -> Option<Walk> {
    func.layout.entry_block()?;
    if !func.dynamic_stack_slots.is_empty() {
        return None;
    }
    let mut canon = Canon {
        func,
        names,
        text: format!("{}\n", func.signature),
        numbers: HashMap::new(),
        counts: HashMap::new(),
        constants: Vec::new(),
        sites: Vec::new(),
        refused: false,
    };
    let mut size = 0u32;
    for block in func.layout.blocks() {
        canon.entity(Kind::Block, block.as_u32());
        canon.text.push('(');
        for &param in func.dfg.block_params(block) {
            canon.entity(Kind::Value, param.as_u32());
            canon.text.push_str(&format!(": {}, ", func.dfg.value_type(param)));
        }
        canon.text.push_str(if func.layout.is_cold(block) { ") cold:\n" } else { "):\n" });
        for inst in func.layout.block_insts(block) {
            size += 1;
            canon.inst(inst);
            canon.text.push('\n');
        }
    }
    if canon.refused {
        return None;
    }
    let Canon { text, constants, sites, .. } = canon;
    Some(Walk { masked: Masked { shape: text.into_bytes(), constants, size }, sites })
}

impl Canon<'_> {
    fn inst(&mut self, inst: ir::Inst) {
        let func = self.func;
        let dfg = &func.dfg;
        let constant = match dfg.insts[inst] {
            InstructionData::UnaryImm { opcode: Opcode::Iconst, imm } => {
                let bits = dfg.ctrl_typevar(inst).bits() as u8;
                Some(Constant::Int { bits, value: u128::from(imm.bits() as u64) })
            }
            InstructionData::UnaryIeee32 { opcode: Opcode::F32const, imm } => {
                Some(Constant::F32(imm.bits()))
            }
            InstructionData::UnaryIeee64 { opcode: Opcode::F64const, imm } => {
                Some(Constant::F64(imm.bits()))
            }
            _ => None,
        };
        let Some(constant) = constant else {
            // What Cranelift prints after `  ; ` are the values of the constants it reads.
            let shown = dfg.display_inst(inst).to_string();
            return self.tokens(shown.split("  ; ").next().unwrap_or_default());
        };
        self.entity(Kind::Value, dfg.first_result(inst).as_u32());
        let opcode = dfg.insts[inst].opcode();
        let ty = dfg.ctrl_typevar(inst);
        self.text.push_str(&format!(" = {opcode}.{ty} #{}", self.constants.len()));
        self.constants.push(constant);
        self.sites.push(Site::Inst(inst));
    }

    fn tokens(&mut self, text: &str) {
        let word = |c: char| c.is_ascii_alphanumeric() || c == '_';
        let mut rest = text;
        while let Some(start) = rest.find(word) {
            self.text.push_str(&rest[..start]);
            let len = rest[start..].find(|c: char| !word(c)).unwrap_or(rest.len() - start);
            self.token(&rest[start..start + len]);
            rest = &rest[start + len..];
        }
        self.text.push_str(rest);
    }

    fn token(&mut self, word: &str) {
        for (prefix, kind) in PREFIXES {
            let Some(digits) = word.strip_prefix(prefix) else { continue };
            if digits.is_empty() || !digits.bytes().all(|it| it.is_ascii_digit()) {
                continue;
            }
            match (kind, digits.parse::<u32>()) {
                (Some(kind), Ok(raw)) => self.entity(kind, raw),
                _ => self.refused = true,
            }
            return;
        }
        self.text.push_str(word);
    }

    /// Prints the entity by its number of first use, and what it is where it is first used.
    fn entity(&mut self, kind: Kind, raw: u32) {
        let raw = match kind {
            Kind::Value => self.func.dfg.resolve_aliases(ir::Value::from_u32(raw)).as_u32(),
            _ => raw,
        };
        let prefix = PREFIXES.iter().find(|it| it.1 == Some(kind)).map_or("?", |it| it.0);
        if let Some(number) = self.numbers.get(&(kind, raw)) {
            return self.text.push_str(&format!("{prefix}{number}"));
        }
        let count = self.counts.entry(kind).or_default();
        let number = *count;
        *count += 1;
        self.numbers.insert((kind, raw), number);
        self.text.push_str(&format!("{prefix}{number}"));
        let func = self.func;
        match kind {
            Kind::Value | Kind::Block => {}
            Kind::Func => {
                let callee = ir::FuncRef::from_u32(raw);
                let data = &func.dfg.ext_funcs[callee];
                self.text.push('{');
                let place = self.name(&data.name, Site::Func(callee));
                self.text.push(' ');
                self.entity(Kind::Sig, data.signature.as_u32());
                self.text.push_str(&format!(" {} {}}}", data.colocated, data.patchable));
                if place && data.patchable {
                    self.refused = true;
                }
            }
            Kind::Global => {
                let global = ir::GlobalValue::from_u32(raw);
                self.text.push('{');
                match func.global_values[global] {
                    ir::GlobalValueData::Symbol { ref name, offset, colocated, tls } => {
                        self.name(name, Site::Global(global));
                        let offset: i64 = offset.into();
                        self.text.push_str(&format!(" {offset} {colocated} {tls}"));
                    }
                    ir::GlobalValueData::Load { base, offset, global_type, flags } => {
                        self.refused |= self.is_place(base);
                        self.text.push_str(&format!("load.{global_type} {flags} "));
                        self.entity(Kind::Global, base.as_u32());
                        self.text.push_str(&format!("{offset}"));
                    }
                    ir::GlobalValueData::IAddImm { base, offset, global_type } => {
                        self.refused |= self.is_place(base);
                        self.text.push_str(&format!("iadd_imm.{global_type} "));
                        self.entity(Kind::Global, base.as_u32());
                        self.text.push_str(&format!(", {offset}"));
                    }
                    ref other => self.text.push_str(&other.to_string()),
                }
                self.text.push('}');
            }
            Kind::Sig => {
                let sig = &func.dfg.signatures[ir::SigRef::from_u32(raw)];
                self.text.push_str(&format!("{{{sig}}}"));
            }
            Kind::Slot => {
                let slot = &func.sized_stack_slots[ir::StackSlot::from_u32(raw)];
                self.text.push_str(&format!("{{{slot}}}"));
            }
            Kind::Pool => {
                let data = func.dfg.constants.get(ir::Constant::from_u32(raw));
                self.text.push_str(&format!("{{{data}}}"));
            }
        }
    }

    /// Prints a function's or global value's name; whether it is a place, which is then a
    /// constant.
    fn name(&mut self, name: &ir::ExternalName, site: Site) -> bool {
        let ir::ExternalName::User(user) = name else {
            self.text.push_str(&name.display(None).to_string());
            return false;
        };
        match (self.names)(self.func.params.user_named_funcs()[*user].clone()) {
            Some(Name::Place(place)) => {
                self.text.push_str(&format!("#{}", self.constants.len()));
                self.constants.push(Constant::Place(place));
                self.sites.push(site);
                true
            }
            Some(Name::Fixed(id)) => {
                self.text.push_str(&format!("={id}"));
                false
            }
            None => {
                self.refused = true;
                false
            }
        }
    }

    fn is_place(&self, global: ir::GlobalValue) -> bool {
        let ir::GlobalValueData::Symbol { name: ir::ExternalName::User(user), .. } =
            &self.func.global_values[global]
        else {
            return false;
        };
        let name = self.func.params.user_named_funcs()[*user].clone();
        matches!((self.names)(name), Some(Name::Place(_)))
    }
}

/// A group's body and thunks, built and not compiled.
pub(crate) struct Built {
    pub(crate) body: ir::Function,
    /// In the group's order.
    pub(crate) thunks: Vec<ir::Function>,
}

/// A member of a group: its prepared IR, what its names are, its references.
pub(crate) type Member<'a> =
    (&'a ir::Function, &'a dyn Fn(ir::UserExternalName) -> Option<Name>, &'a Refs);

/// Builds `group`'s body from its first member and a thunk for each. Every member's masked IR
/// must be the group's, with the group's values of its varying constants and equal values
/// (a place by its target) of the rest.
pub(crate) fn build(group: &Group, members: &[Member<'_>], pointer: ir::Type) -> Result<Built, String> {
    if members.is_empty()
        || members.len() != group.members.len()
        || members.len() != group.constants.len()
    {
        return Err(format!(
            "{} functions given for a group of {}",
            members.len(),
            group.members.len()
        ));
    }
    let walks = members
        .iter()
        .enumerate()
        .map(|(at, (func, names, _))| {
            walk(func, *names).ok_or_else(|| format!("member {at} has no masked form"))
        })
        .collect::<Result<Vec<Walk>, String>>()?;
    let first = &walks[0];
    for (at, (walk, (_, _, refs))) in walks.iter().zip(members).enumerate() {
        if walk.masked.shape != first.masked.shape
            || walk.masked.constants.len() != first.masked.constants.len()
        {
            return Err(format!("member {at}'s masked IR is not the first's"));
        }
        let mut varying = group.varying.iter().zip(&group.constants[at]).peekable();
        for (index, (&mine, &firsts)) in
            walk.masked.constants.iter().zip(&first.masked.constants).enumerate()
        {
            if let Some((_, &value)) = varying.next_if(|it| *it.0 as usize == index) {
                if mine != value {
                    return Err(format!("member {at}'s constant {index} is {mine:?}, not {value:?}"));
                }
                continue;
            }
            let equal = match (mine, firsts) {
                (Constant::Place(mine), Constant::Place(firsts)) => {
                    let target = |refs: &Refs, place: u32| refs.get(place as usize).cloned();
                    let mine = target(refs, mine);
                    mine.is_some() && mine == target(members[0].2, firsts)
                }
                (mine, firsts) => mine == firsts,
            };
            if !equal {
                return Err(format!("member {at}'s constant {index} varies, and is not a parameter"));
            }
        }
        if varying.next().is_some() {
            return Err(format!("the group's varying constants are not among member {at}'s"));
        }
    }
    let (reads, params) = group.parameters();
    let source = members[0].0;
    // Each parameter's type, from the first constant that reads it.
    let types = (0..params)
        .map(|param| {
            let at = reads.iter().position(|&it| it == param).expect("each parameter is read");
            match first.sites[group.varying[at] as usize] {
                Site::Inst(inst) => source.dfg.value_type(source.dfg.first_result(inst)),
                Site::Func(_) | Site::Global(_) => pointer,
            }
        })
        .collect::<Vec<ir::Type>>();
    let body = body(source, first, group, &reads, &types)?;
    let thunks = members
        .iter()
        .zip(&walks)
        .zip(&group.constants)
        .map(|(((func, _, _), walk), constants)| {
            thunk(func, walk, group, constants, &reads, params, &body.signature, pointer)
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok(Built { body, thunks })
}

fn body(
    source: &ir::Function,
    walk: &Walk,
    group: &Group,
    reads: &[u32],
    types: &[ir::Type],
) -> Result<ir::Function, String> {
    let mut body = source.clone();
    let entry = body.layout.entry_block().ok_or("the first member has no body")?;
    let params: Vec<ir::Value> = types
        .iter()
        .map(|&ty| {
            body.signature.params.push(ir::AbiParam::new(ty));
            body.dfg.append_block_param(entry, ty)
        })
        .collect();
    let insts: Vec<ir::Inst> =
        body.layout.blocks().flat_map(|block| body.layout.block_insts(block)).collect();
    for (&at, &param) in group.varying.iter().zip(reads) {
        let param = params[param as usize];
        match walk.sites[at as usize] {
            Site::Inst(inst) => alias(&mut body, inst, param)?,
            Site::Func(callee) => {
                for &inst in &insts {
                    let data = body.dfg.insts[inst];
                    match data {
                        InstructionData::FuncAddr { func_ref, .. } if func_ref == callee => {
                            alias(&mut body, inst, param)?
                        }
                        InstructionData::Call { opcode, func_ref, .. } if func_ref == callee => {
                            let sig = body.dfg.ext_funcs[func_ref].signature;
                            let args = body.dfg.inst_args(inst).to_vec();
                            match opcode {
                                Opcode::Call => body.replace(inst).call_indirect(sig, param, &args),
                                Opcode::ReturnCall => {
                                    body.replace(inst).return_call_indirect(sig, param, &args)
                                }
                                other => return Err(format!("{other} of a varying function")),
                            };
                        }
                        InstructionData::TryCall { func_ref, exception, .. }
                            if func_ref == callee =>
                        {
                            let args = body.dfg.inst_args(inst).to_vec();
                            body.replace(inst).try_call_indirect(param, &args, exception);
                        }
                        _ => {}
                    }
                }
            }
            Site::Global(global) => {
                for &inst in &insts {
                    let data = body.dfg.insts[inst];
                    if let InstructionData::UnaryGlobalValue { global_value, .. } = data
                        && global_value == global
                    {
                        alias(&mut body, inst, param)?;
                    }
                }
            }
        }
    }
    Ok(body)
}

/// Takes `inst` out, its one result standing for `param` from there on.
fn alias(func: &mut ir::Function, inst: ir::Inst, param: ir::Value) -> Result<(), String> {
    let result = func.dfg.first_result(inst);
    let (was, now) = (func.dfg.value_type(result), func.dfg.value_type(param));
    if was != now {
        return Err(format!("a constant of type {was} would be read from a parameter of {now}"));
    }
    func.layout.remove_inst(inst);
    func.dfg.detach_inst_results(inst);
    func.dfg.change_to_alias(result, param);
    Ok(())
}

/// `name`, of `from`, declared in `to`.
fn copied(from: &ir::Function, name: &ir::ExternalName, to: &mut ir::Function) -> ir::ExternalName {
    match name {
        ir::ExternalName::User(user) => {
            let user = from.params.user_named_funcs()[*user].clone();
            ir::ExternalName::user(to.declare_imported_user_function(user))
        }
        other => other.clone(),
    }
}

/// What a thunk makes a parameter of the body from.
enum Made {
    Int(ir::Type, i64),
    F32(u32),
    F64(u64),
    Func(ir::FuncRef),
    Global { global: ir::GlobalValue, tls: bool },
}

/// A function with `member`'s signature that calls the body with its arguments and
/// `constants`, the member's values of the group's varying ones.
fn thunk(
    member: &ir::Function,
    walk: &Walk,
    group: &Group,
    constants: &[Constant],
    reads: &[u32],
    params: u32,
    body: &ir::Signature,
    pointer: ir::Type,
) -> Result<ir::Function, String> {
    let mut thunk = ir::Function::with_name_signature(member.name.clone(), member.signature.clone());
    let mut made = Vec::with_capacity(params as usize);
    for param in 0..params {
        let at = reads.iter().position(|&it| it == param).expect("each parameter is read");
        let site = walk.sites[group.varying[at] as usize];
        made.push(match (site, constants[at]) {
            (Site::Inst(inst), Constant::Int { value, .. }) => {
                Made::Int(member.dfg.value_type(member.dfg.first_result(inst)), value as u64 as i64)
            }
            (Site::Inst(_), Constant::F32(bits)) => Made::F32(bits),
            (Site::Inst(_), Constant::F64(bits)) => Made::F64(bits),
            (Site::Func(callee), Constant::Place(_)) => {
                let data = &member.dfg.ext_funcs[callee];
                let signature = thunk.import_signature(member.dfg.signatures[data.signature].clone());
                let name = copied(member, &data.name, &mut thunk);
                let colocated = data.colocated;
                Made::Func(thunk.import_function(ir::ExtFuncData {
                    name,
                    signature,
                    colocated,
                    patchable: false,
                }))
            }
            (Site::Global(global), Constant::Place(_)) => {
                let ir::GlobalValueData::Symbol { ref name, offset, colocated, tls } =
                    member.global_values[global]
                else {
                    return Err(format!("varying global value {global} is not a symbol"));
                };
                let name = copied(member, name, &mut thunk);
                let data = ir::GlobalValueData::Symbol { name, offset, colocated, tls };
                Made::Global { global: thunk.create_global_value(data), tls }
            }
            (site, constant) => {
                return Err(format!("{constant:?} is not what {site:?} of the member makes"));
            }
        });
    }
    let signature = thunk.import_signature(body.clone());
    let name = thunk.declare_imported_user_function(ir::UserExternalName { namespace: BODY, index: 0 });
    let callee = thunk.import_function(ir::ExtFuncData {
        name: ir::ExternalName::user(name),
        signature,
        colocated: true,
        patchable: false,
    });
    let block = thunk.dfg.make_block();
    thunk.layout.append_block(block);
    let mut args: Vec<ir::Value> = member
        .signature
        .params
        .iter()
        .map(|it| thunk.dfg.append_block_param(block, it.value_type))
        .collect();
    let mut cursor = FuncCursor::new(&mut thunk).at_bottom(block);
    for it in made {
        args.push(match it {
            Made::Int(ty, imm) => cursor.ins().iconst(ty, imm),
            Made::F32(bits) => cursor.ins().f32const(ir::immediates::Ieee32::with_bits(bits)),
            Made::F64(bits) => cursor.ins().f64const(ir::immediates::Ieee64::with_bits(bits)),
            Made::Func(func) => cursor.ins().func_addr(pointer, func),
            Made::Global { global, tls: true } => cursor.ins().tls_value(pointer, global),
            Made::Global { global, tls: false } => cursor.ins().symbol_value(pointer, global),
        });
    }
    let call = cursor.ins().call(callee, &args);
    let results = cursor.func.dfg.inst_results(call).to_vec();
    cursor.ins().return_(&results);
    Ok(thunk)
}

/// Compiles `group`: its body from the first member, a thunk for each member, each with
/// `emit`, the adapter's compile of a prepared function's IR (here `func` in place of the
/// prepared one) against `refs`, its names in the prepared function's. The body is emitted
/// as the first member against its references and keyed [`Group::key`]; thunk `i` as member
/// `i` against [`Group::thunk_refs`] of its own, where [`BODY`] is the last.
pub(crate) fn merge<P: Source>(
    group: &Group,
    members: &[(&Unit, &Spec, &P, &Refs)],
    pointer: ir::Type,
    emit: impl Fn(&Unit, &Spec, &P, ir::Function, &Refs) -> Result<Emitted, CodegenError>,
) -> Result<Merged, CodegenError> {
    let &(first, ..) = members.first().expect("a group has members");
    let fail = |what: String| CodegenError {
        unit: group.key(),
        item: first.item,
        what: format!("folding {} functions into one body: {what}", group.members.len()),
    };
    for (at, (unit, ..)) in members.iter().enumerate() {
        if group.members.get(at) != Some(&unit.instance.key()) {
            return Err(fail(format!("member {at} is not the group's")));
        }
    }
    let names: Vec<_> = members
        .iter()
        .map(|&(_, _, prepared, _)| move |name: ir::UserExternalName| prepared.name(name))
        .collect();
    let sources: Vec<Member<'_>> = members
        .iter()
        .zip(&names)
        .map(|(&(_, _, prepared, refs), names)| {
            (prepared.function(), names as &dyn Fn(ir::UserExternalName) -> Option<Name>, refs)
        })
        .collect();
    let Built { body, thunks } = build(group, &sources, pointer).map_err(fail)?;
    let (unit, spec, prepared, refs) = members[0];
    let mut body = emit(unit, spec, prepared, body, refs)?;
    body.function.key = group.key();
    let thunks = members
        .iter()
        .zip(thunks)
        .map(|(&(unit, spec, prepared, refs), thunk)| {
            emit(unit, spec, prepared, thunk, &group.thunk_refs(refs))
        })
        .collect::<Result<Vec<Emitted>, CodegenError>>()?;
    Ok(Merged { body, thunks })
}

#[cfg(test)]
mod tests {
    use blaze_codegen::{
        InstanceKey, Settings, Target,
        fold::{Limits, groups},
    };
    use cranelift_codegen::{
        ir::{AbiParam, types},
        settings,
    };

    use super::*;

    fn key(n: u8) -> InstanceKey {
        InstanceKey(ContentId([n; 32]))
    }

    /// Namespace 0 are the places among the references, namespace 1 a data object of
    /// content `[index; 32]`.
    fn names(name: ir::UserExternalName) -> Option<Name> {
        match name.namespace {
            0 => Some(Name::Place(name.index)),
            1 => Some(Name::Fixed(ContentId([name.index as u8; 32]))),
            _ => None,
        }
    }

    fn signature() -> ir::Signature {
        let mut sig = ir::Signature::new(crate::target().unwrap().default_call_conv());
        sig.params.push(AbiParam::new(types::I64));
        sig.returns.push(AbiParam::new(types::I64));
        sig
    }

    /// `Vec<T>::grow`-like: multiplies its argument by `T`'s size, calls place `callee` with
    /// it, adds that function's address, a static's (place `stat`) and a data object's.
    /// `shuffled` declares an unused signature and name first and builds the constant last,
    /// so its entities have other numbers.
    fn grow(size: i64, callee: u32, stat: u32, shuffled: bool) -> ir::Function {
        let ptr = types::I64;
        let mut func = ir::Function::with_name_signature(ir::UserFuncName::default(), signature());
        if shuffled {
            func.import_signature(ir::Signature::new(func.signature.call_conv));
            func.declare_imported_user_function(ir::UserExternalName::new(0, 99));
        }
        let user = |func: &mut ir::Function, namespace, index| {
            ir::ExternalName::user(
                func.declare_imported_user_function(ir::UserExternalName::new(namespace, index)),
            )
        };
        let name = user(&mut func, 0, callee);
        let signature = func.import_signature(signature());
        let callee =
            func.import_function(ir::ExtFuncData { name, signature, colocated: true, patchable: false });
        let name = user(&mut func, 0, stat);
        let stat = func.create_global_value(ir::GlobalValueData::Symbol {
            name,
            offset: 0.into(),
            colocated: true,
            tls: false,
        });
        let name = user(&mut func, 1, 7);
        let data = func.create_global_value(ir::GlobalValueData::Symbol {
            name,
            offset: 0.into(),
            colocated: true,
            tls: false,
        });
        let block = func.dfg.make_block();
        func.layout.append_block(block);
        let arg = func.dfg.append_block_param(block, types::I64);
        let mut cursor = FuncCursor::new(&mut func).at_bottom(block);
        if shuffled {
            let dead = cursor.ins().iconst(types::I64, 0);
            let inst = cursor.func.dfg.value_def(dead).unwrap_inst();
            cursor.func.layout.remove_inst(inst);
        }
        let size = cursor.ins().iconst(types::I64, size);
        let scaled = cursor.ins().imul(arg, size);
        let call = cursor.ins().call(callee, &[scaled]);
        let called = cursor.func.dfg.first_result(call);
        let address = cursor.ins().func_addr(ptr, callee);
        let stat = cursor.ins().symbol_value(ptr, stat);
        let data = cursor.ins().symbol_value(ptr, data);
        let sum = cursor.ins().iadd(called, address);
        let sum = cursor.ins().iadd(sum, stat);
        let sum = cursor.ins().iadd(sum, data);
        cursor.ins().return_(&[sum]);
        func
    }

    #[test]
    fn numbering_is_not_the_shape() {
        let (a, b) = (grow(8, 0, 1, false), grow(8, 0, 1, true));
        let (a, b) = (masked(&a, &names).unwrap(), masked(&b, &names).unwrap());
        assert_eq!(a, b);
        assert_eq!(a.constants, vec![
            Constant::Int { bits: 64, value: 8 },
            Constant::Place(0),
            Constant::Place(1),
        ]);
        assert_eq!(a.size, 10);
    }

    #[test]
    fn constants_are_masked() {
        let (a, b) = (grow(8, 0, 1, false), grow(-4, 2, 3, false));
        let (a, b) = (masked(&a, &names).unwrap(), masked(&b, &names).unwrap());
        assert_eq!(a.shape, b.shape);
        assert_eq!(b.constants[0], Constant::Int { bits: 64, value: u128::from(-4i64 as u64) });
        assert_eq!(b.constants[1], Constant::Place(2));
        let text = String::from_utf8(a.shape).unwrap();
        assert!(!text.contains(" 8") && !text.contains("; "), "{text}");
    }

    #[test]
    fn other_control_flow_is_another_shape() {
        let mut other = grow(8, 0, 1, false);
        let block = other.layout.entry_block().unwrap();
        let ret = other.layout.last_inst(block).unwrap();
        let sum = other.dfg.inst_args(ret)[0];
        let next = other.dfg.make_block();
        other.layout.append_block(next);
        let mut cursor = FuncCursor::new(&mut other).at_inst(ret);
        cursor.ins().brif(sum, next, &[], next, &[]);
        cursor.func.layout.remove_inst(ret);
        let mut cursor = cursor.at_bottom(next);
        cursor.ins().return_(&[sum]);
        let a = masked(&grow(8, 0, 1, false), &names).unwrap();
        assert_ne!(masked(&other, &names).unwrap().shape, a.shape);
    }

    #[test]
    fn what_has_no_parameters_is_not_masked() {
        assert!(masked(&ir::Function::new(), &names).is_none());
        assert!(masked(&grow(8, 0, 1, false), &|_| None).is_none());
    }

    struct Prepared(ir::Function);

    impl Source for Prepared {
        fn function(&self) -> &ir::Function {
            &self.0
        }

        fn name(&self, name: ir::UserExternalName) -> Option<Name> {
            names(name)
        }
    }

    fn verified(func: &ir::Function) {
        let flags = settings::Flags::new(settings::builder());
        if let Err(errors) = cranelift_codegen::verify_function(func, &flags) {
            panic!("{}", cranelift_codegen::print_errors::pretty_verifier_error(func, None, errors));
        }
        let mut ctx = cranelift_codegen::Context::for_function(func.clone());
        assert!(!crate::emit(&mut ctx).unwrap().bytes.is_empty());
    }

    #[test]
    fn two_members_merge_into_a_body_and_thunks() {
        let pointer = crate::target().unwrap().pointer_type();
        // Two sizes, two callees, one static (place 1 of both).
        let (a, b) = (Prepared(grow(8, 0, 1, false)), Prepared(grow(24, 0, 1, true)));
        let refs_a = vec![Target::Function(key(10)), Target::Extern("STATIC".to_owned())];
        let refs_b = vec![Target::Function(key(11)), Target::Extern("STATIC".to_owned())];
        let (ma, mb) = (masked(&a.0, &names).unwrap(), masked(&b.0, &names).unwrap());
        let settings = Settings(ContentId([1; 32]));
        let limits = Limits { thunk_size: 0, ..Limits::default() };
        let found = groups(
            &[(key(1), settings, &ma, &refs_a[..]), (key(2), settings, &mb, &refs_b[..])],
            &limits,
        );
        let group = &found[0];
        assert_eq!(group.varying, vec![0, 1]);
        let names_a = |name: ir::UserExternalName| a.name(name);
        let names_b = |name: ir::UserExternalName| b.name(name);
        let members: [Member<'_>; 2] =
            [(&a.0, &names_a, &refs_a[..]), (&b.0, &names_b, &refs_b[..])];
        let Built { body, thunks } = build(group, &members, pointer).unwrap();

        let mut want = signature();
        want.params.push(AbiParam::new(types::I64));
        want.params.push(AbiParam::new(pointer));
        assert_eq!(body.signature, want);
        let opcodes: Vec<Opcode> = body
            .layout
            .blocks()
            .flat_map(|block| body.layout.block_insts(block))
            .map(|inst| body.dfg.insts[inst].opcode())
            .collect();
        assert!(opcodes.contains(&Opcode::CallIndirect));
        assert!(!opcodes.contains(&Opcode::Call) && !opcodes.contains(&Opcode::FuncAddr));
        assert!(!opcodes.contains(&Opcode::Iconst));
        // The static is equal for both: still named, not a parameter.
        assert_eq!(opcodes.iter().filter(|&&it| it == Opcode::SymbolValue).count(), 2);
        verified(&body);

        for ((thunk, member), size) in thunks.iter().zip([&a.0, &b.0]).zip([8, 24]) {
            assert_eq!(thunk.signature, member.signature);
            let block = thunk.layout.entry_block().unwrap();
            let insts: Vec<ir::Inst> = thunk.layout.block_insts(block).collect();
            let call = insts[insts.len() - 2];
            let InstructionData::Call { func_ref, .. } = thunk.dfg.insts[call] else {
                panic!("a thunk calls the body")
            };
            let ir::ExternalName::User(name) = thunk.dfg.ext_funcs[func_ref].name else {
                panic!("the body is named by a user name")
            };
            assert_eq!(thunk.params.user_named_funcs()[name].namespace, BODY);
            let args = thunk.dfg.inst_args(call);
            assert_eq!(args.len(), 3);
            assert_eq!(args[0], thunk.dfg.block_params(block)[0]);
            let InstructionData::UnaryImm { imm, .. } = thunk.dfg.insts[insts[0]] else {
                panic!("the size is made first")
            };
            assert_eq!(imm.bits(), size);
            let InstructionData::FuncAddr { func_ref, .. } = thunk.dfg.insts[insts[1]] else {
                panic!("then the callee's address")
            };
            let ir::ExternalName::User(name) = thunk.dfg.ext_funcs[func_ref].name else {
                panic!("the callee is named as the member names it")
            };
            assert_eq!(thunk.params.user_named_funcs()[name], ir::UserExternalName::new(0, 0));
            assert_eq!(thunk.dfg.insts[insts[insts.len() - 1]].opcode(), Opcode::Return);
            verified(thunk);
        }
        assert_eq!(group.thunk_refs(&refs_b).last(), Some(&Target::Direct(group.key())));
    }

    #[test]
    fn a_member_unlike_the_group_is_refused() {
        let pointer = types::I64;
        let (a, b) = (grow(8, 0, 1, false), grow(24, 0, 1, false));
        let refs = vec![Target::Function(key(10)), Target::Extern("STATIC".to_owned())];
        let other = vec![Target::Function(key(10)), Target::Extern("OTHER".to_owned())];
        let (ma, mb) = (masked(&a, &names).unwrap(), masked(&b, &names).unwrap());
        let settings = Settings(ContentId([1; 32]));
        let limits = Limits { thunk_size: 0, ..Limits::default() };
        let found = groups(&[(key(1), settings, &ma, &refs[..]), (key(2), settings, &mb, &refs[..])], &limits);
        let group = &found[0];
        assert_eq!(group.varying, vec![0]);
        // The static differs, but the group was made with it equal.
        let members: [Member<'_>; 2] = [(&a, &names, &refs[..]), (&b, &names, &other[..])];
        assert!(build(group, &members, pointer).unwrap_err().contains("varies"));
        assert!(build(group, &members[..1], pointer).is_err());
    }
}
