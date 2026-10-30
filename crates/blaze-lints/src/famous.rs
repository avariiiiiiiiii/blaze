//! The well-known items of the standard library the lints compare with: found through the lang
//! items of the crate being linted, or by path in its `core`, `alloc` or `std`.

use base_db::{CrateOrigin, Db, LangCrateOrigin};
use hir_def::{ModuleDefId, ModuleId, lang_item::lang_items};
use hir_expand::name::Name;
use intern::Symbol;

#[derive(Clone, Copy)]
pub(crate) struct Famous<'db> {
    db: &'db Db,
    krate: base_db::Crate,
}

#[allow(non_snake_case)]
impl<'db> Famous<'db> {
    pub(crate) fn new(db: &'db Db, krate: hir::Crate) -> Famous<'db> {
        Famous { db, krate: krate.base() }
    }

    pub(crate) fn core(&self) -> Option<hir::Crate> {
        self.lang_crate(LangCrateOrigin::Core).map(hir::Crate::from)
    }

    pub(crate) fn alloc(&self) -> Option<hir::Crate> {
        self.lang_crate(LangCrateOrigin::Alloc).map(hir::Crate::from)
    }

    pub(crate) fn std(&self) -> Option<hir::Crate> {
        self.lang_crate(LangCrateOrigin::Std).map(hir::Crate::from)
    }

    pub(crate) fn core_option_Option(&self) -> Option<hir::Enum> {
        lang_items(self.db, self.krate).Option.map(hir::Enum::from)
    }

    pub(crate) fn core_result_Result(&self) -> Option<hir::Enum> {
        match self.find("core:result:Result")? {
            hir::ModuleDef::Adt(hir::Adt::Enum(it)) => Some(it),
            _ => None,
        }
    }

    pub(crate) fn core_default_Default(&self) -> Option<hir::Trait> {
        lang_items(self.db, self.krate).Default.map(hir::Trait::from)
    }

    pub(crate) fn core_iter_Iterator(&self) -> Option<hir::Trait> {
        lang_items(self.db, self.krate).Iterator.map(hir::Trait::from)
    }

    pub(crate) fn core_iter_IntoIterator(&self) -> Option<hir::Trait> {
        lang_items(self.db, self.krate).IntoIterator.map(hir::Trait::from)
    }

    /// The item at `path`, written `crate:module:...:name` with `crate` one of `core`, `alloc`,
    /// `std`, `proc_macro` and `test`.
    pub(crate) fn find(&self, path: &str) -> Option<hir::ModuleDef> {
        let mut segments = path.split(':');
        let name = segments.next_back()?;
        let origin = match LangCrateOrigin::from(segments.next()?) {
            LangCrateOrigin::Other | LangCrateOrigin::Dependency => return None,
            origin => origin,
        };
        let krate = self.lang_crate(origin)?;
        let mut module = hir_def::nameres::crate_def_map(self.db, krate).root_module_id();
        for segment in segments {
            match self.scope_get(module, segment).take_types()? {
                ModuleDefId::ModuleId(it) => module = it,
                _ => return None,
            }
        }
        let found = self.scope_get(module, name);
        let def = match found.take_types().or_else(|| found.take_values()) {
            Some(def) => def,
            None => ModuleDefId::MacroId(found.take_macros()?),
        };
        Some(hir::ModuleDef::from(def))
    }

    fn scope_get(&self, module: ModuleId, name: &str) -> hir_def::per_ns::PerNs {
        let name = Name::new_symbol_root(Symbol::intern(name));
        module.def_map(self.db).module(self.db, module).scope(self.db).get(&name)
    }

    fn lang_crate(&self, origin: LangCrateOrigin) -> Option<base_db::Crate> {
        let wanted = CrateOrigin::Lang(origin);
        let data = self.krate.data(self.db);
        if data.origin == wanted {
            return Some(self.krate);
        }
        data.dependencies
            .iter()
            .map(|dep| dep.crate_id)
            .find(|&dep| dep.data(self.db).origin == wanted)
    }
}
