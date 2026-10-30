//! Type tree for term search

use base_db::Db;
use hir_def::FindPathConfig;
use hir_expand::mod_path::ModPath;
use hir_ide::{
    db::HirDatabase,
    display::{DisplaySourceCodeError, DisplayTarget, HirDisplay},
};
use itertools::Itertools;
use span::Edition;

use crate::{
    Adt, AsAssocItem, AssocItemContainer, Const, ConstParam, EnumVariant, Field, Function, Local,
    ModuleDef, SemanticsScope, Static, Struct, StructKind, Trait, Type,
};

/// Helper function to get path to `ModuleDef`
fn mod_item_path(
    sema_scope: &SemanticsScope<'_>,
    def: &ModuleDef,
    cfg: FindPathConfig,
) -> Option<ModPath> {
    let db = sema_scope.db;
    let m = sema_scope.module();
    m.find_path(db, *def, cfg)
}

/// Helper function to get path to `ModuleDef` as string
fn mod_item_path_str(
    sema_scope: &SemanticsScope<'_>,
    def: &ModuleDef,
    cfg: FindPathConfig,
    edition: Edition,
) -> Result<String, DisplaySourceCodeError> {
    let path = mod_item_path(sema_scope, def, cfg);
    // alloc: source text for the editor, built per suggestion after the search
    path.map(|it| it.display(sema_scope.db, edition).to_string())
        .ok_or(DisplaySourceCodeError::PathNotFound)
}

/// Type tree shows how can we get from set of types to some type.
///
/// Consider the following code as an example
/// ```ignore
/// fn foo(x: i32, y: bool) -> Option<i32> { None }
/// fn bar() {
///    let a = 1;
///    let b = true;
///    let c: Option<i32> = _;
/// }
/// ```
/// If we generate type tree in the place of `_` we get
/// ```txt
///       Option<i32>
///           |
///     foo(i32, bool)
///      /        \
///  a: i32      b: bool
/// ```
/// So in short it pretty much gives us a way to get type `Option<i32>` using the items we have in
/// scope.
#[derive(Debug, Clone, Eq, Hash, PartialEq)]
pub enum Expr<'db> {
    /// Constant
    Const(Const),
    /// Static variable
    Static(Static),
    /// Local variable
    Local(Local<'db>),
    /// Constant generic parameter
    ConstParam(ConstParam),
    /// Well known type (such as `true` for bool)
    FamousType { ty: Type<'db>, value: &'static str },
    /// Function call (does not take self param)
    Function { func: Function, generics: Vec<Type<'db>>, params: Vec<Expr<'db>> },
    /// Method call (has self param)
    Method {
        func: Function,
        generics: Vec<Type<'db>>,
        target: Box<Expr<'db>>,
        params: Vec<Expr<'db>>,
    },
    /// Enum variant construction
    Variant { variant: EnumVariant, generics: Vec<Type<'db>>, params: Vec<Expr<'db>> },
    /// Struct construction
    Struct { strukt: Struct, generics: Vec<Type<'db>>, params: Vec<Expr<'db>> },
    /// Tuple construction
    Tuple { ty: Type<'db>, params: Vec<Expr<'db>> },
    /// Struct field access
    Field { expr: Box<Expr<'db>>, field: Field },
    /// Passing type as reference (with `&`)
    Reference(Box<Expr<'db>>),
    /// Indicates possibility of many different options that all evaluate to `ty`
    Many(Type<'db>),
}

impl<'db> Expr<'db> {
    /// Generate source code for type tree.
    ///
    /// Note that trait imports are not added to generated code.
    /// To make sure that the code is valid, callee has to also ensure that all the traits listed
    /// by `traits_used` method are also imported.
    pub fn gen_source_code(
        &self,
        sema_scope: &SemanticsScope<'db>,
        many_formatter: &mut impl FnMut(&Type<'db>) -> String,
        cfg: FindPathConfig,
        display_target: DisplayTarget,
    ) -> Result<String, DisplaySourceCodeError> {
        let db = sema_scope.db;
        let edition = display_target.edition;
        let mod_item_path_str = |s, def| mod_item_path_str(s, def, cfg, edition);
        match self {
            Expr::Const(it) => match it.as_assoc_item(db).map(|it| it.container(db)) {
                Some(container) => {
                    let container_name =
                        container_name(container, sema_scope, cfg, edition, display_target)?;
                    let const_name = it
                        .name(db)
                        .map(|c| c.display(db, edition).to_string()) // alloc: editor text
                        .unwrap_or(String::new()); // alloc: empty
                    Ok(format!("{container_name}::{const_name}")) // alloc: editor text
                }
                None => mod_item_path_str(sema_scope, &ModuleDef::Const(*it)),
            },
            Expr::Static(it) => mod_item_path_str(sema_scope, &ModuleDef::Static(*it)),
            // alloc: source text for the editor, built per suggestion after the search
            Expr::Local(it) => Ok(it.name(db).display(db, edition).to_string()),
            // alloc: as above
            Expr::ConstParam(it) => Ok(it.name(db).display(db, edition).to_string()),
            // alloc: as above
            Expr::FamousType { value, .. } => Ok(value.to_string()),
            Expr::Function { func, params, .. } => {
                let args = params
                    .iter()
                    .map(|f| f.gen_source_code(sema_scope, many_formatter, cfg, display_target))
                    .collect::<Result<Vec<String>, DisplaySourceCodeError>>()?
                    .into_iter()
                    .join(", ");

                match func.as_assoc_item(db).map(|it| it.container(db)) {
                    Some(container) => {
                        let container_name =
                            container_name(container, sema_scope, cfg, edition, display_target)?;
                        // alloc: editor text
                        let fn_name = func.name(db).display(db, edition).to_string();
                        Ok(format!("{container_name}::{fn_name}({args})")) // alloc: editor text
                    }
                    None => {
                        let fn_name = mod_item_path_str(sema_scope, &ModuleDef::Function(*func))?;
                        Ok(format!("{fn_name}({args})")) // alloc: editor text
                    }
                }
            }
            Expr::Method { func, target, params, .. } => {
                if self.contains_many_in_illegal_pos(db) {
                    return Ok(many_formatter(&target.ty(db)));
                }

                // alloc: editor text
                let func_name = func.name(db).display(db, edition).to_string();
                let self_param = func.self_param(db).unwrap();
                let target_str =
                    target.gen_source_code(sema_scope, many_formatter, cfg, display_target)?;
                let args = params
                    .iter()
                    .map(|f| f.gen_source_code(sema_scope, many_formatter, cfg, display_target))
                    .collect::<Result<Vec<String>, DisplaySourceCodeError>>()?
                    .into_iter()
                    .join(", ");

                match func.as_assoc_item(db).and_then(|it| it.container_or_implemented_trait(db)) {
                    Some(trait_) => {
                        let trait_name = mod_item_path_str(sema_scope, &ModuleDef::Trait(trait_))?;
                        let target = match self_param.access(db) {
                            // alloc: editor text
                            crate::Access::Shared if !target.is_many() => format!("&{target_str}"),
                            crate::Access::Exclusive if !target.is_many() => {
                                format!("&mut {target_str}") // alloc: editor text
                            }
                            crate::Access::Owned => target_str,
                            _ => many_formatter(&target.ty(db)),
                        };
                        let res = match args.is_empty() {
                            // alloc: editor text
                            true => format!("{trait_name}::{func_name}({target})",),
                            // alloc: editor text
                            false => format!("{trait_name}::{func_name}({target}, {args})",),
                        };
                        Ok(res)
                    }
                    None => Ok(format!("{target_str}.{func_name}({args})")), // alloc: editor text
                }
            }
            Expr::Variant { variant, params, .. } => {
                let inner = match variant.kind(db) {
                    StructKind::Tuple => {
                        let args = params
                            .iter()
                            .map(|f| {
                                f.gen_source_code(sema_scope, many_formatter, cfg, display_target)
                            })
                            .collect::<Result<Vec<String>, DisplaySourceCodeError>>()?
                            .into_iter()
                            .join(", ");
                        format!("({args})") // alloc: editor text
                    }
                    StructKind::Record => {
                        let fields = variant.fields(db);
                        let args = params
                            .iter()
                            .zip(fields.iter())
                            .map(|(a, f)| {
                                // alloc: editor text
                                let tmp = format!(
                                    "{}: {}",
                                    f.name(db).display(db, edition),
                                    a.gen_source_code(
                                        sema_scope,
                                        many_formatter,
                                        cfg,
                                        display_target
                                    )?
                                );
                                Ok(tmp)
                            })
                            .collect::<Result<Vec<String>, DisplaySourceCodeError>>()?
                            .into_iter()
                            .join(", ");
                        format!("{{ {args} }}") // alloc: editor text
                    }
                    StructKind::Unit => String::new(), // alloc: empty
                };

                let prefix = mod_item_path_str(sema_scope, &ModuleDef::EnumVariant(*variant))?;
                Ok(format!("{prefix}{inner}")) // alloc: editor text
            }
            Expr::Struct { strukt, params, .. } => {
                let inner = match strukt.kind(db) {
                    StructKind::Tuple => {
                        let args = params
                            .iter()
                            .map(|a| {
                                a.gen_source_code(sema_scope, many_formatter, cfg, display_target)
                            })
                            .collect::<Result<Vec<String>, DisplaySourceCodeError>>()?
                            .into_iter()
                            .join(", ");
                        format!("({args})") // alloc: editor text
                    }
                    StructKind::Record => {
                        let fields = strukt.fields(db);
                        let args = params
                            .iter()
                            .zip(fields.iter())
                            .map(|(a, f)| {
                                // alloc: editor text
                                let tmp = format!(
                                    "{}: {}",
                                    f.name(db).display(db, edition),
                                    a.gen_source_code(
                                        sema_scope,
                                        many_formatter,
                                        cfg,
                                        display_target
                                    )?
                                );
                                Ok(tmp)
                            })
                            .collect::<Result<Vec<String>, DisplaySourceCodeError>>()?
                            .into_iter()
                            .join(", ");
                        format!(" {{ {args} }}") // alloc: editor text
                    }
                    StructKind::Unit => String::new(), // alloc: empty
                };

                let prefix = mod_item_path_str(sema_scope, &ModuleDef::Adt(Adt::Struct(*strukt)))?;
                Ok(format!("{prefix}{inner}")) // alloc: editor text
            }
            Expr::Tuple { params, .. } => {
                let args = params
                    .iter()
                    .map(|a| a.gen_source_code(sema_scope, many_formatter, cfg, display_target))
                    .collect::<Result<Vec<String>, DisplaySourceCodeError>>()?
                    .into_iter()
                    .join(", ");
                let res = format!("({args})"); // alloc: editor text
                Ok(res)
            }
            Expr::Field { expr, field } => {
                if expr.contains_many_in_illegal_pos(db) {
                    return Ok(many_formatter(&expr.ty(db)));
                }

                let strukt =
                    expr.gen_source_code(sema_scope, many_formatter, cfg, display_target)?;
                let field = field.name(db).display(db, edition).to_string(); // alloc: editor text
                Ok(format!("{strukt}.{field}")) // alloc: editor text
            }
            Expr::Reference(expr) => {
                if expr.contains_many_in_illegal_pos(db) {
                    return Ok(many_formatter(&expr.ty(db)));
                }

                let inner =
                    expr.gen_source_code(sema_scope, many_formatter, cfg, display_target)?;
                Ok(format!("&{inner}")) // alloc: editor text
            }
            Expr::Many(ty) => Ok(many_formatter(ty)),
        }
    }

    /// Get type of the type tree.
    ///
    /// Same as getting the type of root node
    pub fn ty(&self, db: &'db Db) -> Type<'db> {
        match self {
            Expr::Const(it) => it.ty(db),
            Expr::Static(it) => it.ty(db),
            Expr::Local(it) => it.ty(db),
            Expr::ConstParam(it) => it.ty(db),
            Expr::FamousType { ty, .. } => *ty,
            Expr::Function { func, generics, .. } => func.ret_type(db).instantiate(db, generics),
            Expr::Method { func, generics, target, .. } => func
                .ret_type(db)
                .instantiate(db, target.ty(db).type_arguments().chain(generics.iter().copied())),
            Expr::Variant { variant, generics, .. } => {
                Adt::from(variant.parent_enum(db)).ty(db).instantiate(db, generics)
            }
            Expr::Struct { strukt, generics, .. } => {
                Adt::from(*strukt).ty(db).instantiate(db, generics)
            }
            Expr::Tuple { ty, .. } => *ty,
            Expr::Field { expr, field } => {
                field.ty(db).instantiate(db, expr.ty(db).type_arguments())
            }
            Expr::Reference(it) => it.ty(db),
            Expr::Many(ty) => *ty,
        }
    }

    /// List the traits used in type tree
    pub fn traits_used(&self, db: &Db) -> Vec<Trait> {
        let mut res = Vec::new(); // alloc: the returned list, once per suggestion
        self.push_traits_used(db, &mut res);
        res
    }

    /// `traits_used`, appended to `out`.
    fn push_traits_used(&self, db: &Db, out: &mut Vec<Trait>) {
        if let Expr::Method { func, params, .. } = self {
            for param in params {
                param.push_traits_used(db, out);
            }
            if let Some(it) = func.as_assoc_item(db)
                && let Some(it) = it.container_or_implemented_trait(db)
            {
                out.push(it);
            }
        }
    }

    /// Check in the tree contains `Expr::Many` variant in illegal place to insert `todo`,
    /// `unimplemented` or similar macro
    ///
    /// Some examples are following
    /// ```no_compile
    /// macro!().foo
    /// macro!().bar()
    /// &macro!()
    /// ```
    fn contains_many_in_illegal_pos(&self, db: &Db) -> bool {
        match self {
            Expr::Method { target, func, .. } => {
                match func.as_assoc_item(db).and_then(|it| it.container_or_implemented_trait(db)) {
                    Some(_) => false,
                    None => target.is_many(),
                }
            }
            Expr::Field { expr, .. } => expr.contains_many_in_illegal_pos(db),
            Expr::Reference(target) => target.is_many(),
            Expr::Many(_) => true,
            _ => false,
        }
    }

    /// Helper function to check if outermost type tree is `Expr::Many` variant
    pub fn is_many(&self) -> bool {
        matches!(self, Expr::Many(_))
    }
}

/// Helper function to find name of container
fn container_name(
    container: AssocItemContainer,
    sema_scope: &SemanticsScope<'_>,
    cfg: FindPathConfig,
    edition: Edition,
    display_target: DisplayTarget,
) -> Result<String, DisplaySourceCodeError> {
    let container_name = match container {
        crate::AssocItemContainer::Trait(trait_) => {
            mod_item_path_str(sema_scope, &ModuleDef::Trait(trait_), cfg, edition)?
        }
        crate::AssocItemContainer::Impl(imp) => {
            let self_ty = imp.self_ty(sema_scope.db);
            // Should it be guaranteed that `mod_item_path` always exists?
            match self_ty.as_adt().and_then(|adt| mod_item_path(sema_scope, &adt.into(), cfg)) {
                // alloc: editor text
                Some(path) => path.display(sema_scope.db, edition).to_string(),
                // alloc: editor text
                None => self_ty.display(sema_scope.db, display_target).to_string(),
            }
        }
    };
    Ok(container_name)
}
