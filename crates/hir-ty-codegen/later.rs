//! What is made of a function once its code is in the object: what a debugger is told of
//! it. That does not change the object, so a build that puts the functions in the object one
//! at a time makes it for all of them at once, on every core ([`ObjectCodegen::make_later`]):
//! made as each function was put in, it took the thread that does that as long as the
//! putting in itself.

#![forbid(unsafe_code)]

use base_db::Db;
use super::*;

/// What a function put in the object is.
pub(super) enum Of {
    /// A function without generic arguments, under its rustc symbol.
    Root {
        func: FunctionId,
        symbol: String,
    },
    Instance {
        instance: Instance,
        env: StoredParamEnvAndCrate,
    },
}

/// A function whose code is in the object, with what is still to be made of it.
pub(super) struct Later<'c> {
    pub(super) id: FuncId,
    pub(super) of: Of,
    pub(super) compiled: &'c CompiledFunction,
    /// The rows of a unit's code, which are not those of the query.
    pub(super) unit_rows: Option<Vec<Row>>,
    /// Code of a level above 1, which is of a profile, and told of by the profile.
    pub(super) tiered: bool,
}

impl<'c> Later<'c> {
    pub(super) fn new(id: FuncId, of: Of, compiled: &'c CompiledFunction) -> Self {
        Later { id, of, compiled, unit_rows: None, tiered: false }
    }
}

/// What was made of a [`Later`].
pub(super) struct Made {
    /// What a debugger is told of the function.
    pub(super) debug: Option<FunctionDebug>,
}

/// What a [`Later`] is made with: what of the build it reads, and its own paths.
pub(super) struct Keeper<'k, 'db> {
    pub(super) db: &'db Db,
    // dyn: the build's one resolver, made by blaze-engine or blaze-wasm (which depend on this crate)
    pub(super) symbols: &'k dyn SymbolResolver,
    pub(super) source_paths: &'k mut FxHashMap<base_db::FileId, Option<String>>,
    pub(super) told: &'k ToldTypes,
    /// Whether a debugger is told of the functions, and of their variables.
    pub(super) debug: bool,
    pub(super) variables: bool,
}

impl<'db> Keeper<'_, 'db> {
    pub(super) fn make(&mut self, later: &Later<'_>) -> Made {
        let db = self.db;
        let (debug, declared, rows) = match &later.of {
            Of::Root { func, symbol } => {
                let name = hir_ty_mir::mir::type_name::rustc_debug_function(db, *func, &[])
                    // alloc: the name `FunctionDebug` owns
                    .unwrap_or_else(|| FunctionSignature::of(db, *func).name.as_str().to_owned());
                let declared = self.debug_declaration_at(*func);
                let rows = match &later.unit_rows {
                    Some(rows) => Some(rows),
                    None => (self.debug && !later.tiered).then(|| root_rows(db, *func)),
                };
                // clone: the linkage name `FunctionDebug` owns; the `Later` is only borrowed
                (FunctionDebug::new(name, symbol.clone()), declared, rows)
            }
            Of::Instance { instance, env } => {
                let name = backtrace_name(db, instance);
                let declared = self.written_at(instance, env.krate);
                let known = match instance.def {
                    InstanceDef::Fn(func) => hir_ty_mir::mir::type_name::rustc_debug_function(
                        db,
                        func,
                        instance.args.as_ref().as_slice(),
                    ),
                    InstanceDef::Closure(id) => {
                        let at = InternedClosureId::from_id(id).loc(db);
                        debug_closure(db, instance, at.owner, at.expr)
                    }
                    InstanceDef::Coroutine(id) | InstanceDef::CoroutineByMove(id) => {
                        let at = InternedCoroutineId::from_id(id).loc(db);
                        debug_closure(db, instance, at.owner, at.expr)
                    }
                    _ => None,
                };
                // clone: `FunctionDebug` owns both its name and its linkage name
                let known = known.unwrap_or_else(|| name.clone());
                let rows = match &later.unit_rows {
                    Some(rows) => Some(rows),
                    None => (self.debug && !later.tiered)
                        // clone: an interned handle, the tracked query's key
                        .then(|| instance_rows(db, *instance, env.clone())),
                };
                (FunctionDebug::new(known, name), declared, rows)
            }
        };
        Made { debug: self.describe(later.compiled, rows, debug, declared) }
    }

    /// The path rustc reports `file` by, spelled once per file.
    fn spelled(&mut self, file: base_db::FileId) -> Option<&str> {
        let symbols = self.symbols;
        let spelled = || symbols.source_path(file).map(|it| as_rustc_spells(it.as_str()));
        self.source_paths.entry(file).or_insert_with(spelled).as_deref()
    }

    fn source_path(&mut self, file: base_db::FileId) -> Option<String> {
        // alloc: the path a location owns
        self.spelled(file).map(str::to_owned)
    }

    fn location_at(&mut self, at: Option<(DefWithBodyId, MirSpan)>) -> Option<(String, u32, u32)> {
        at.and_then(|(owner, span)| source_location(self.db, owner, span))
            .and_then(|(file, line, col)| Some((self.source_path(file)?, line, col)))
    }

    fn debug_declaration_at(&mut self, func: FunctionId) -> Option<(String, u32, u32)> {
        debug_declaration_location(self.db, func)
            .and_then(|(file, line, col)| Some((self.source_path(file)?, line, col)))
    }

    /// Where what `instance` is the code of is written, which is the line of whatever of its
    /// code is of no statement: a function where it is declared, a closure or an `async`
    /// block where it stands, and a shim where the function it stands in for is declared, as
    /// rustc has it.
    fn written_at(&mut self, instance: &Instance, krate: Crate) -> Option<(String, u32, u32)> {
        if !self.debug {
            return None;
        }
        let db = self.db;
        let lang_items = DbInterner::new_with(db, krate).lang_items();
        let ty = || instance.args.as_ref().as_slice().first().and_then(|arg| arg.ty());
        let of_expr = |owner: InferBodyId<'db>, expr| match owner {
            InferBodyId::DefWithBodyId(owner) => Some((owner, MirSpan::ExprId(expr))),
            _ => None,
        };
        let func = match instance.def {
            InstanceDef::Fn(func) => Some(func),
            InstanceDef::Closure(id) => {
                let at = InternedClosureId::from_id(id).loc(db);
                return self.location_at(of_expr(at.owner, at.expr));
            }
            InstanceDef::Coroutine(id) | InstanceDef::CoroutineByMove(id) => {
                let at = InternedCoroutineId::from_id(id).loc(db);
                return self.location_at(of_expr(at.owner, at.expr));
            }
            InstanceDef::DropGlue => lang_items.DropInPlace.or(lang_items.DropGlue),
            InstanceDef::CallOnceShim => lang_items.FnOnce_call_once,
            InstanceDef::VtableShim(func) => Some(func),
            InstanceDef::FnPointerShim => match ty()?.kind() {
                TyKind::Closure(id, _) => {
                    let at = id.0.loc(db);
                    return self.location_at(of_expr(at.owner, at.expr));
                }
                _ => None,
            },
            InstanceDef::ReifyShim { .. } => match ty()?.kind() {
                TyKind::FnDef(def, _) => match def.0 {
                    CallableDefId::FunctionId(func) => Some(func),
                    constructor => {
                        return constructor_location(db, constructor).and_then(
                            |(file, line, col)| Some((self.source_path(file)?, line, col)),
                        );
                    }
                },
                _ => lang_items.FnOnce_call_once,
            },
        };
        self.debug_declaration_at(func?)
    }

    /// What a debugger is told of the compiled function: what it is called, as `debug` has
    /// it, and which line each of its instructions is of, as `rows` have it. `None` when no
    /// debugger is told anything, which is when there are no `rows`.
    fn describe(
        &mut self,
        compiled: &CompiledFunction,
        rows: Option<&Vec<Row>>,
        mut debug: FunctionDebug,
        declared: Option<(String, u32, u32)>,
    ) -> Option<FunctionDebug> {
        let rows = rows?;
        let start = stdx::time::Instant::now();
        if let Some((file, line, column)) = declared {
            debug.declared = Some(debug.place(file, line, column));
        }
        for &(offset, file, line, column) in rows {
            let Some(path) = self.spelled(file) else { continue };
            // As `FunctionDebug::place`, with the path copied only the first time it is seen.
            let place = match debug.files.iter().position(|it| it == path) {
                Some(file) => debuginfo::Place { file: file as u32, line, column },
                // alloc: once per file of the function, which `FunctionDebug` owns
                None => debug.place(path.to_owned(), line, column),
            };
            debug.row(offset, place);
        }
        // Compiled code of no statement with a place is of where the function is written
        // throughout. A function that is its assembly has no lines, as rustc gives it none.
        if let Some(declared) = debug.declared
            && debug.rows.is_empty()
            && compiled.code.unwind.is_some()
        {
            debug.row(0, declared);
        }
        if let Some(source) = &compiled.source
            && let Some((env, variables)) = &compiled.variables
            && self.debug
            && self.variables
        {
            let owner = &source.owner;
            let places = &compiled.code.slots;
            let db = self.db;
            let len = compiled.code.bytes.len() as u32;
            let mut rows: Vec<(u32, MirSpan)> = Vec::with_capacity(compiled.code.lines.len());
            rows.extend(
                (compiled.code.lines.iter())
                    .filter_map(|row| Some((row.offset, *source.spans.get(row.span as usize)?))),
            );
            blaze_ids::radix::sort_copy_by_key(&mut rows, |row| row.0);
            let scopes = describe_scopes(db, *owner, variables, &rows, len, &mut debug);
            describe_variables(
                db,
                *owner,
                (self.told, env),
                variables,
                places,
                &scopes,
                &mut debug,
            );
        }
        backend::Tally::global().add(Step::Describe, start.elapsed());
        Some(debug)
    }
}
