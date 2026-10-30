//! Names for what the module rustc built has: the symbols rustc gave its functions and
//! statics, found from their paths. The paths are `rustc_symbols`', as the engine's are; what
//! is here is the table they are looked up in and how it spells a crate.

use std::{borrow::Cow, path::PathBuf};

use hir::Crate;
use hir_def::{FunctionId, ItemContainerId, Lookup, ModuleDefId, StaticId};
use hir_ty_codegen::{SymbolResolver, exported_symbol, link_name};
use base_db::Db;
use intern::Symbol;
use rustc_symbols::{Paths, Places, StaticPath};
use base_db::FileId;

use crate::symbols::SymbolTable;

pub(crate) struct Resolver<'a> {
    pub(crate) db: &'a Db,
    pub(crate) table: &'a SymbolTable,
    pub(crate) places: Places,
}

impl<'a> Paths<'a> for Resolver<'a> {
    fn db(&self) -> &'a Db {
        self.db
    }

    fn crate_path(&self, krate: Crate) -> Option<Cow<'_, str>> {
        let name = krate.display_name(self.db)?;
        let path = self.table.crate_path(name.crate_name().as_str(), &self.crate_root(krate)?)?;
        Some(Cow::Owned(path))
    }

    fn sema(&self) -> hir::view::Semantics<'a> {
        hir::view::Semantics::new(self.db)
    }

    fn places(&self) -> &Places {
        &self.places
    }
}

impl Resolver<'_> {
    fn in_generic_impl(&self, func: FunctionId) -> bool {
        let ItemContainerId::ImplId(imp) = func.lookup(self.db).container else { return false };
        let imp = hir::GenericDef::from(hir::Impl::from(imp));
        !imp.type_or_const_params(self.db).is_empty()
    }

    /// rustc's `StableCrateId` of a crate it built something of.
    fn rustc_crate_id(&self, krate: base_db::Crate) -> Option<u64> {
        let krate = Crate::from(krate);
        let name = krate.display_name(self.db)?;
        self.table.stable_crate_id(name.crate_name().as_str(), &self.crate_root(krate)?)
    }

    /// The file the crate's source starts at.
    fn crate_root(&self, krate: Crate) -> Option<PathBuf> {
        crate_root_file(self.db, krate)
    }

    /// What stands for a crate rustc built nothing of, the same after every edit: its name.
    fn own_crate_id(&self, krate: base_db::Crate) -> Option<u64> {
        if !krate.data(self.db).origin.is_local() || self.rustc_crate_id(krate).is_some() {
            return None;
        }
        let name = Crate::from(krate).display_name(self.db)?;
        Some(name.crate_name().as_str().bytes().fold(0xcbf29ce484222325, |hash, byte| {
            (hash ^ byte as u64).wrapping_mul(0x100000001b3)
        }))
    }
}

impl SymbolResolver for Resolver<'_> {
    fn source_path(&self, file: FileId) -> Option<Symbol> {
        let root = self.db.file_source_root(file).source_root_id(self.db);
        let root = self.db.source_root(root).source_root(self.db);
        Some(Symbol::intern(&root.path_for_file(&file)?.to_string())) // alloc: wasm, per name
    }

    // alloc: wasm, per name: each symbol interned
    fn function(&self, func: FunctionId) -> Option<Symbol> {
        if let Some(symbol) = exported_symbol(self.db, func) {
            return Some(Symbol::intern(&symbol));
        }
        // Not a function until the `impl<T>` it is in is given its types: there is nothing to
        // name, the more so in a crate of such `impl`s alone, which rustc builds nothing of.
        if self.in_generic_impl(func) {
            return None;
        }
        let path = self.function_path(func)?;
        if let Some(symbol) = self.table.get(&path) {
            return Some(Symbol::intern(symbol));
        }
        // rustc has built nothing of the crate: the function is ours to name, unless an
        // `extern` block declares it, and it is defined elsewhere under the name it links to.
        let krate = hir_def::HasModule::krate(&func, self.db);
        let id = self.own_crate_id(krate)?;
        if let Some(symbol) = link_name(self.db, func) {
            return Some(Symbol::intern(&symbol));
        }
        Some(Symbol::intern(&format!("blaze.{id:x}.{path}"))) // alloc: wasm, per name
    }

    fn defines(&self, symbol: &str) -> bool {
        self.table.defines(symbol)
    }

    fn stable_crate_id(&self, krate: base_db::Crate) -> Option<u64> {
        self.rustc_crate_id(krate).or_else(|| self.own_crate_id(krate))
    }

    fn declared_in(&self, adt: hir_def::AdtId) -> Option<FunctionId> {
        self.declaring_function(adt)
    }

    fn impl_place(&self, id: hir_def::ImplId) -> Option<u32> {
        rustc_symbols::impl_place(self.db, id)
    }

    // alloc: wasm, per name: the symbol interned
    fn static_(&self, id: StaticId) -> Option<Symbol> {
        let symbol = match self.static_path(id)? {
            StaticPath::Item(path) => self.table.get(&path),
            StaticPath::InBody { path, closures, place, around } => self
                .table
                .get_nested(&path, &closures, place, &around)
                .or_else(|| self.table.get_at(&path, place, &around)),
        };
        symbol.map(Symbol::intern)
    }

    fn path(&self, path: &str) -> Option<&str> {
        self.table.get(path)
    }

    fn item(&self, item: ModuleDefId) -> Option<String> {
        self.item_path_of(item)
    }
}

fn crate_root_file(db: &Db, krate: Crate) -> Option<PathBuf> {
    let file = krate.root_file(db);
    let source_root = db.file_source_root(file).source_root_id(db);
    let source_root = db.source_root(source_root).source_root(db);
    Some(PathBuf::from(source_root.path_for_file(&file)?.to_string())) // alloc: wasm, per crate asked about
}
