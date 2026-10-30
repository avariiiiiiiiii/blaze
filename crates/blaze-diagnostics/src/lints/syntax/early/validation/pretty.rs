//! rustc's pretty printing of a `where` predicate, as `deprecated_where_clause_location`'s
//! suggestion prints the predicates it moves: the types, bounds, paths and generic arguments in
//! them, with rustc's spacing. Lines are never broken: rustc's printer breaks only past 78
//! columns, which no predicate of a type alias's header reaches.
//!
//! Follows rustc's `compiler/rustc_ast_pretty/src/pprust/state.rs` (`print_type`,
//! `print_type_bounds`, `print_poly_trait_ref`, `print_path`, `print_qpath`,
//! `print_generic_args`, `print_assoc_item_constraint`, `print_generic_params`,
//! `print_fn_ret_ty`, `print_param`) and `state/item.rs` (`print_where_predicate`,
//! `print_where_bound_predicate`, `print_fn`), and the expressions of `state/expr.rs` a type
//! holds (an array's length, a const argument) (MIT or Apache-2.0).

use span::Edition;
use syntax::{
    AstNode, NodeOrToken, SyntaxKind, SyntaxNode,
    ast::{self, HasGenericArgs as _, HasName as _, HasTypeBounds as _},
    utils::is_raw_identifier,
};

/// rustc's printing of `pred`, in `edition` (which decides whether a raw identifier keeps its
/// `r#`).
pub(crate) fn where_predicate(pred: &ast::WherePred, edition: Edition) -> String {
    let mut p = Printer { out: String::new(), edition };
    p.where_predicate(pred);
    p.out
}

/// rustc's printing of an identifier written `text` (`Ident`'s `Display`): with `r#` when it is
/// a keyword of `edition`, else without, however it is written.
pub(crate) fn ident(text: &str, edition: Edition) -> String {
    let bare = text.strip_prefix("r#").unwrap_or(text);
    match is_raw_identifier(bare, edition) {
        true => format!("r#{bare}"),
        false => bare.to_owned(),
    }
}

struct Printer {
    out: String,
    edition: Edition,
}

impl Printer {
    fn word(&mut self, text: &str) {
        self.out.push_str(text);
    }

    fn where_predicate(&mut self, pred: &ast::WherePred) {
        self.outer_attrs(pred.syntax());
        if let Some(binder) = pred.for_binder() {
            self.formal_generic_params(&binder);
        }
        let bounds: Vec<ast::TypeBound> =
            pred.type_bound_list().map(|it| it.bounds().collect()).unwrap_or_default();
        match (pred.lifetime(), pred.ty()) {
            (Some(lifetime), _) => {
                self.lifetime(&lifetime);
                self.word(":");
                if !bounds.is_empty() {
                    self.word(" ");
                    self.lifetime_bounds(&bounds);
                }
            }
            (None, Some(ty)) => {
                self.ty(&ty);
                self.word(":");
                if !bounds.is_empty() {
                    self.word(" ");
                    self.type_bounds(&bounds);
                }
            }
            (None, None) => self.source(pred.syntax()),
        }
    }

    /// `print_outer_attributes`: each attribute as written, then a space.
    fn outer_attrs(&mut self, node: &SyntaxNode) {
        for attr in node.children().filter(|it| it.kind() == SyntaxKind::ATTR) {
            self.source(&attr);
            self.word(" ");
        }
    }

    fn ident(&mut self, text: &str) {
        let printed = ident(text, self.edition);
        self.word(&printed);
    }

    fn lifetime(&mut self, lifetime: &ast::Lifetime) {
        self.word(lifetime.text());
    }

    fn lifetime_bounds(&mut self, bounds: &[ast::TypeBound]) {
        for (i, bound) in bounds.iter().enumerate() {
            if i != 0 {
                self.word(" + ");
            }
            match bound.lifetime() {
                Some(lifetime) => self.lifetime(&lifetime),
                None => self.source(bound.syntax()),
            }
        }
    }

    fn type_bounds(&mut self, bounds: &[ast::TypeBound]) {
        for (i, bound) in bounds.iter().enumerate() {
            if i != 0 {
                self.word(" + ");
            }
            self.type_bound(bound);
        }
    }

    fn type_bound(&mut self, bound: &ast::TypeBound) {
        if let Some(lifetime) = bound.lifetime() {
            self.lifetime(&lifetime);
            return;
        }
        if let Some(args) = bound.use_bound_generic_args() {
            self.word("use<");
            for (i, arg) in args.use_bound_generic_args().enumerate() {
                if i != 0 {
                    self.word(", ");
                }
                match arg {
                    ast::UseBoundGenericArg::Lifetime(it) => self.lifetime(&it),
                    ast::UseBoundGenericArg::NameRef(it) => self.ident(it.text()),
                }
            }
            self.word(">");
            return;
        }
        if let Some(binder) = bound.for_binder() {
            self.formal_generic_params(&binder);
        }
        if bound.l_brack_token().is_some() || bound.tilde_token().is_some() {
            self.word("[const] ");
        } else if bound.const_token().is_some() {
            self.word("const ");
        }
        if bound.async_token().is_some() {
            self.word("async ");
        }
        if bound.question_mark_token().is_some() {
            self.word("?");
        }
        match bound.ty() {
            Some(ty) => self.ty(&ty),
            None => self.source(bound.syntax()),
        }
    }

    /// `print_formal_generic_params`: `for<..> `.
    fn formal_generic_params(&mut self, binder: &ast::ForBinder) {
        let Some(list) = binder.generic_param_list() else { return };
        let params: Vec<ast::GenericParam> = list.generic_params().collect();
        if params.is_empty() {
            return;
        }
        self.word("for");
        self.generic_params(&params);
        self.word(" ");
    }

    fn generic_params(&mut self, params: &[ast::GenericParam]) {
        self.word("<");
        for (i, param) in params.iter().enumerate() {
            if i != 0 {
                self.word(", ");
            }
            self.outer_attrs(param.syntax());
            match param {
                ast::GenericParam::LifetimeParam(it) => {
                    if let Some(lifetime) = it.lifetime() {
                        self.lifetime(&lifetime);
                    }
                    let bounds: Vec<_> = it.type_bound_list().map(|it| it.bounds().collect()).unwrap_or_default();
                    if !bounds.is_empty() {
                        self.word(": ");
                        self.lifetime_bounds(&bounds);
                    }
                }
                ast::GenericParam::TypeParam(it) => {
                    if let Some(name) = it.name() {
                        self.ident(name.text());
                    }
                    let bounds: Vec<_> = it.type_bound_list().map(|it| it.bounds().collect()).unwrap_or_default();
                    if !bounds.is_empty() {
                        self.word(": ");
                        self.type_bounds(&bounds);
                    }
                    if let Some(default) = it.default_type() {
                        self.word(" = ");
                        self.ty(&default);
                    }
                }
                ast::GenericParam::ConstParam(it) => {
                    self.word("const ");
                    if let Some(name) = it.name() {
                        self.ident(name.text());
                    }
                    self.word(" : ");
                    if let Some(ty) = it.ty() {
                        self.ty(&ty);
                    }
                    if let Some(default) = it.default_val() {
                        self.word(" = ");
                        self.const_arg(&default);
                    }
                }
            }
        }
        self.word(">");
    }

    fn ty(&mut self, ty: &ast::Type) {
        match ty {
            ast::Type::SliceType(it) => {
                self.word("[");
                self.opt_ty(it.ty());
                self.word("]");
            }
            ast::Type::PtrType(it) => {
                self.word("*");
                self.word(if it.mut_token().is_some() { "mut " } else { "const " });
                self.opt_ty(it.ty());
            }
            ast::Type::RefType(it) => {
                self.word("&");
                if let Some(lifetime) = it.lifetime() {
                    self.lifetime(&lifetime);
                    self.word(" ");
                }
                if it.mut_token().is_some() {
                    self.word("mut ");
                }
                self.opt_ty(it.ty());
            }
            ast::Type::NeverType(_) => self.word("!"),
            ast::Type::TupleType(it) => {
                let fields: Vec<ast::Type> = it.fields().collect();
                self.word("(");
                for (i, field) in fields.iter().enumerate() {
                    if i != 0 {
                        self.word(", ");
                    }
                    self.ty(field);
                }
                if fields.len() == 1 {
                    self.word(",");
                }
                self.word(")");
            }
            ast::Type::ParenType(it) => {
                self.word("(");
                self.opt_ty(it.ty());
                self.word(")");
            }
            ast::Type::FnPtrType(it) => self.fn_ptr(it),
            ast::Type::ForType(it) => {
                if let Some(binder) = it.for_binder() {
                    self.formal_generic_params(&binder);
                }
                self.opt_ty(it.ty());
            }
            ast::Type::PathType(it) => match it.path() {
                Some(path) => self.path(&path, false),
                None => self.source(it.syntax()),
            },
            ast::Type::DynTraitType(it) => {
                if it.dyn_token().is_some() {
                    self.word("dyn ");
                }
                self.bound_list(it.type_bound_list());
            }
            ast::Type::ImplTraitType(it) => {
                self.word("impl ");
                self.bound_list(it.type_bound_list());
            }
            ast::Type::ArrayType(it) => {
                self.word("[");
                self.opt_ty(it.ty());
                self.word("; ");
                if let Some(len) = it.const_arg() {
                    self.const_arg(&len);
                }
                self.word("]");
            }
            ast::Type::InferType(_) => self.word("_"),
            ast::Type::PatternType(it) => {
                self.opt_ty(it.ty());
                self.word(" is ");
                if let Some(pat) = it.pat() {
                    self.source(pat.syntax());
                }
            }
            ast::Type::MacroType(it) => self.source(it.syntax()),
        }
    }

    fn opt_ty(&mut self, ty: Option<ast::Type>) {
        if let Some(ty) = ty {
            self.ty(&ty);
        }
    }

    fn bound_list(&mut self, list: Option<ast::TypeBoundList>) {
        let bounds: Vec<_> = list.map(|it| it.bounds().collect()).unwrap_or_default();
        self.type_bounds(&bounds);
    }

    /// `print_ty_fn` of a function pointer: `unsafe extern "C" fn(u8) -> u8`.
    fn fn_ptr(&mut self, it: &ast::FnPtrType) {
        if it.unsafe_token().is_some() {
            self.word("unsafe ");
        }
        if let Some(abi) = it.abi() {
            self.word("extern ");
            if let Some(string) = abi.string_token() {
                self.word(string.text());
                self.word(" ");
            }
        }
        self.word("fn(");
        if let Some(params) = it.param_list() {
            for (i, param) in params.params().enumerate() {
                if i != 0 {
                    self.word(", ");
                }
                self.param(&param);
            }
        }
        self.word(")");
        self.ret_type(it.ret_type());
    }

    fn param(&mut self, param: &ast::Param) {
        self.outer_attrs(param.syntax());
        if let Some(pat) = param.pat() {
            self.source(pat.syntax());
            self.word(": ");
        }
        match (param.ty(), param.dotdotdot_token()) {
            (Some(ty), _) => self.ty(&ty),
            (None, Some(_)) => self.word("..."),
            (None, None) => {}
        }
    }

    fn ret_type(&mut self, ret: Option<ast::RetType>) {
        if let Some(ty) = ret.and_then(|it| it.ty()) {
            self.word(" -> ");
            self.ty(&ty);
        }
    }

    /// `print_path`, or `print_qpath` when its first segment is `<T as Trait>`;
    /// `colons_before_params` prints `::<..>` (in an expression) rather than `<..>`.
    fn path(&mut self, path: &ast::Path, colons_before_params: bool) {
        for (i, segment) in path.segments().enumerate() {
            if let Some(anchor) = segment.type_anchor() {
                self.word("<");
                self.opt_ty(anchor.ty());
                if let Some(trait_) = anchor.path_type().and_then(|it| it.path()) {
                    self.word(" as ");
                    self.path(&trait_, false);
                }
                self.word(">");
                continue;
            }
            if i > 0 || segment.coloncolon_token().is_some() {
                self.word("::");
            }
            match segment.name_ref() {
                Some(name) => self.ident(name.text()),
                None => self.source(segment.syntax()),
            }
            if let Some(args) = segment.generic_arg_list() {
                self.generic_arg_list(&args, colons_before_params);
            } else if let Some(args) = segment.parenthesized_arg_list() {
                if colons_before_params {
                    self.word("::");
                }
                self.word("(");
                for (i, arg) in args.type_args().enumerate() {
                    if i != 0 {
                        self.word(", ");
                    }
                    self.opt_ty(arg.ty());
                }
                self.word(")");
                self.ret_type(segment.ret_type());
            } else if segment.return_type_syntax().is_some() {
                if colons_before_params {
                    self.word("::");
                }
                self.word("(..)");
            }
        }
    }

    fn generic_arg_list(&mut self, args: &ast::GenericArgList, colons_before_params: bool) {
        if colons_before_params {
            self.word("::");
        }
        self.word("<");
        for (i, arg) in args.generic_args().enumerate() {
            if i != 0 {
                self.word(", ");
            }
            match arg {
                ast::GenericArg::TypeArg(it) => self.opt_ty(it.ty()),
                ast::GenericArg::LifetimeArg(it) => {
                    if let Some(lifetime) = it.lifetime() {
                        self.lifetime(&lifetime);
                    }
                }
                ast::GenericArg::ConstArg(it) => self.const_arg(&it),
                ast::GenericArg::AssocTypeArg(it) => self.assoc_item_constraint(&it),
            }
        }
        self.word(">");
    }

    /// `print_assoc_item_constraint`: `Item = u8`, and `Item : Copy` with the space rustc
    /// puts before the colon.
    fn assoc_item_constraint(&mut self, it: &ast::AssocTypeArg) {
        if let Some(name) = it.name_ref() {
            self.ident(name.text());
        }
        if let Some(args) = it.generic_arg_list() {
            self.generic_arg_list(&args, false);
        } else if it.return_type_syntax().is_some() {
            self.word("(..)");
        } else if let Some(params) = it.param_list() {
            self.word("(");
            for (i, param) in params.params().enumerate() {
                if i != 0 {
                    self.word(", ");
                }
                self.param(&param);
            }
            self.word(")");
            self.ret_type(it.ret_type());
        }
        self.word(" ");
        if it.eq_token().is_some() {
            self.word("= ");
            match (it.ty(), it.const_arg()) {
                (Some(ty), _) => self.ty(&ty),
                (None, Some(ct)) => self.const_arg(&ct),
                (None, None) => {}
            }
        } else {
            let bounds: Vec<_> = it.type_bound_list().map(|it| it.bounds().collect()).unwrap_or_default();
            if !bounds.is_empty() {
                self.word(": ");
                self.type_bounds(&bounds);
            }
        }
    }

    fn const_arg(&mut self, arg: &ast::ConstArg) {
        match arg.expr() {
            Some(expr) => self.expr(&expr),
            None => self.source(arg.syntax()),
        }
    }

    /// `print_expr` of the expressions a type holds: literals, paths, blocks of one
    /// expression, parentheses, unary and binary operators; any other expression is printed
    /// as written, its white space collapsed.
    fn expr(&mut self, expr: &ast::Expr) {
        match expr {
            ast::Expr::Literal(it) => self.source(it.syntax()),
            ast::Expr::PathExpr(it) => match it.path() {
                Some(path) => self.path(&path, true),
                None => self.source(it.syntax()),
            },
            ast::Expr::UnderscoreExpr(_) => self.word("_"),
            ast::Expr::ParenExpr(it) => {
                self.word("(");
                if let Some(inner) = it.expr() {
                    self.expr(&inner);
                }
                self.word(")");
            }
            ast::Expr::PrefixExpr(it) => match (it.op_token(), it.expr()) {
                (Some(op), Some(inner)) => {
                    self.word(op.text());
                    self.expr(&inner);
                }
                _ => self.source(it.syntax()),
            },
            ast::Expr::BinExpr(it) => match (it.lhs(), it.op_token(), it.rhs()) {
                (Some(lhs), Some(op), Some(rhs)) => {
                    self.expr(&lhs);
                    self.word(" ");
                    self.word(op.text());
                    self.word(" ");
                    self.expr(&rhs);
                }
                _ => self.source(it.syntax()),
            },
            ast::Expr::BlockExpr(it)
                if it.modifier().is_none() && it.statements().next().is_none() =>
            {
                match it.tail_expr() {
                    Some(tail) => {
                        self.word("{ ");
                        self.expr(&tail);
                        self.word(" }");
                    }
                    None => self.word("{}"),
                }
            }
            _ => self.source(expr.syntax()),
        }
    }

    /// `node` as written, without its comments, each run of white space one space.
    fn source(&mut self, node: &SyntaxNode) {
        let mut space = false;
        for token in node.descendants_with_tokens().filter_map(NodeOrToken::into_token) {
            match token.kind() {
                SyntaxKind::WHITESPACE => space = true,
                SyntaxKind::COMMENT => {}
                _ => {
                    if space && !self.out.is_empty() && !self.out.ends_with(' ') {
                        self.word(" ");
                    }
                    space = false;
                    self.word(token.text());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use syntax::{AstNode, SourceFile};

    use super::*;

    fn printed(predicate: &str) -> String {
        let text = format!("fn f() where {predicate} {{}}");
        let file = SourceFile::parse(&text, Edition::Edition2021).tree();
        let pred = file.syntax().descendants().find_map(ast::WherePred::cast).unwrap();
        where_predicate(&pred, Edition::Edition2021)
    }

    #[test]
    fn a_predicate_is_printed_with_rustcs_spacing() {
        assert_eq!(printed("u32:Copy"), "u32: Copy");
        assert_eq!(printed("T :  Clone+'static"), "T: Clone + 'static");
        assert_eq!(printed("'a:'b+'c"), "'a: 'b + 'c");
        assert_eq!(printed("for<'a> &'a T: ?Sized"), "for<'a> &'a T: ?Sized");
        assert_eq!(printed("Vec::<T>: Iterator<Item=u8>"), "Vec<T>: Iterator<Item = u8>");
        assert_eq!(printed("T: Iterator<Item: Copy>"), "T: Iterator<Item : Copy>");
        assert_eq!(printed("<T as Tr>::A: Fn(u8)->u8"), "<T as Tr>::A: Fn(u8) -> u8");
        assert_eq!(printed("(u8,): Sized"), "(u8,): Sized");
        assert_eq!(printed("[u8; N+1]: Sized"), "[u8; N + 1]: Sized");
        assert_eq!(printed("*mut dyn Tr: Sized"), "*mut dyn Tr: Sized");
        assert_eq!(printed("T:"), "T:");
    }
}
