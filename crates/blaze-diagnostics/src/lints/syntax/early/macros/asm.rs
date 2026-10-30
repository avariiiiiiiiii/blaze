//! The lints of what `asm!`, `global_asm!` and `naked_asm!` are given: `bad_asm_style`, which
//! their expansion buffers, and `named_asm_labels` and `binary_asm_labels`, of the late lint
//! pass `AsmLabels`, found on each template as rustc's `InlineAsm::template_strs` keeps it (its
//! string, the source text of its span, its span).
//!
//! Follows rustc's `compiler/rustc_parse/src/parser/asm.rs` (`parse_asm_operand`: the operands
//! `global_asm!` and `naked_asm!` refuse), `compiler/rustc_builtin_macros/src/asm.rs`
//! (`validate_asm_args`: what stops before the expansion; `expand_preparsed_asm`:
//! `template_strs`, `find_span` and the `BAD_ASM_STYLE` lints, buffered at
//! `current_expansion.lint_node_id`), `compiler/rustc_builtin_macros/src/util.rs`
//! (`expr_to_spanned_string`), `compiler/rustc_builtin_macros/src/diagnostics.rs`
//! (`AvoidIntelSyntax`, `AvoidAttSyntax`), `compiler/rustc_expand/src/expand.rs`
//! (`InvocationCollector`, `assign_id!`: the nodes that are a call's `lint_node_id`),
//! `compiler/rustc_lint/src/builtin.rs` (`AsmLabels`, `is_hexagon_register_span_context`),
//! `compiler/rustc_lint/src/lints.rs` (`InvalidAsmLabel`),
//! `compiler/rustc_middle/src/ty/generics.rs` (`requires_monomorphization`) and
//! `compiler/rustc_target/src/asm/mod.rs` (`InlineAsmArch::from_arch`) (MIT or Apache-2.0).

use blaze_items::ItemId;
use hir::{InFile, MacroCallId};
use hir_def::{GenericDefId, hir::generics::GenericParamDataRef};
use hir_expand::MacroDefKind;
use ra_ap_rustc_parse_format as parse_format;
use syntax::{
    AstNode, AstToken as _, SyntaxNode, SyntaxNodePtr, T,
    ast::{self, HasLoopBody as _, IsString as _},
};

use crate::{
    cx::Cx,
    lints::syntax::{Emitted, Pass, Tree, Walk, source_fact},
    lower::{Phase, break_outside_of_loop::ancestors},
    model::{Diag, Span},
};

/// rustc's `sess.asm_arch` (`InlineAsmArch::from_arch` of the target's `arch`), as far as these
/// lints tell arches apart; `None` for an arch rustc has no inline assembly for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Arch {
    /// `x86` or `x86_64`.
    X86,
    Hexagon,
    Other,
}

/// The asm arch of the crate's target, from its `target_arch`.
fn asm_arch(walk: &Walk<'_>) -> Option<Arch> {
    let arch = walk.cfg.get_cfg_values("target_arch").next()?;
    match arch.as_str() {
        "x86" | "x86_64" => Some(Arch::X86),
        "hexagon" => Some(Arch::Hexagon),
        "arm" | "arm64ec" | "aarch64" | "amdgpu" | "riscv32" | "riscv64" | "nvptx64"
        | "loongarch32" | "loongarch64" | "mips" | "mips32r6" | "mips64" | "mips64r6"
        | "powerpc" | "powerpc64" | "s390x" | "sparc" | "sparc64" | "spirv" | "wasm32"
        | "wasm64" | "bpf" | "avr" | "msp430" | "m68k" | "csky" | "xtensa" => Some(Arch::Other),
        _ => None,
    }
}

/// rustc's `AsmMacro`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AsmMacro {
    Asm,
    GlobalAsm,
    NakedAsm,
}

/// What of a call's arguments decides whether rustc expands it and how it reads its templates.
#[derive(Default)]
struct Args {
    raw: bool,
    att_syntax: bool,
    noreturn: bool,
    /// An `out`, `lateout`, `inout` or `inlateout` operand.
    outputs: bool,
    labels: bool,
    clobber_abi: bool,
    /// What `parse_asm_operand` refuses whatever `cfg` says of it: an `in`, `out`, `lateout`,
    /// `inout`, `inlateout` or `label` operand of `global_asm!` or `naked_asm!`, or `_` going
    /// in.
    unparsed: bool,
}

impl Args {
    fn of(tree: &Tree, asm: &ast::AsmExpr, kind: AsmMacro) -> Args {
        let mut args = Args::default();
        for piece in asm.asm_pieces() {
            let on = !tree.skipped(piece.syntax().text_range());
            match piece {
                ast::AsmPiece::AsmOptions(options) if on => {
                    for option in options.asm_options() {
                        match option.syntax().first_token().map(|it| it.kind()) {
                            Some(T![raw]) => args.raw = true,
                            Some(T![att_syntax]) => args.att_syntax = true,
                            Some(T![noreturn]) => args.noreturn = true,
                            _ => {}
                        }
                    }
                }
                ast::AsmPiece::AsmClobberAbi(_) if on => args.clobber_abi = true,
                ast::AsmPiece::AsmOperandNamed(named) => match named.asm_operand() {
                    Some(ast::AsmOperand::AsmRegOperand(op)) => {
                        let goes_in = op.asm_dir_spec().is_some_and(|dir| {
                            dir.in_token().is_some()
                                || dir.inout_token().is_some()
                                || dir.inlateout_token().is_some()
                        });
                        let underscore = matches!(
                            op.asm_operand_expr().and_then(|it| it.in_expr()),
                            Some(ast::Expr::UnderscoreExpr(_))
                        );
                        args.unparsed |= kind != AsmMacro::Asm || (goes_in && underscore);
                        let output = op.asm_dir_spec().is_some_and(|dir| dir.in_token().is_none());
                        args.outputs |= on && output;
                    }
                    Some(ast::AsmOperand::AsmLabel(_)) => {
                        args.unparsed |= kind != AsmMacro::Asm;
                        args.labels |= on;
                    }
                    _ => {}
                },
                _ => {}
            }
        }
        args
    }

    /// Whether rustc's parser or `validate_asm_args` refuses the call, so that it is never
    /// expanded.
    fn refused(&self, kind: AsmMacro) -> bool {
        self.unparsed
            || (kind == AsmMacro::Asm && self.noreturn && self.outputs && !self.labels)
            || (kind != AsmMacro::Asm && self.clobber_abi)
    }
}

/// A template as rustc's `template_strs` keeps it.
struct Template<'db> {
    value: String,
    span: Span,
    /// The source text of `span`, quotes included (`span_to_snippet`).
    snippet: Option<&'db str>,
}

/// The lints of the crate's `asm!`, `global_asm!` and `naked_asm!` calls.
pub(super) fn lints(cx: &Cx<'_, '_>, walk: &Walk<'_>) -> Vec<Emitted> {
    let arch = asm_arch(walk);
    let mut emitted = Vec::new();
    for tree in &walk.trees {
        let Some(call) = tree.call else { continue };
        if !matches!(call.loc(cx.db).def.kind, MacroDefKind::BuiltIn(_, it) if it.is_asm()) {
            continue;
        }
        let Some(asm) = tree.nodes.iter().find_map(|it| ast::AsmExpr::cast(it.clone())) else {
            continue;
        };
        asm_lints(cx, tree, call, &asm, arch, &mut emitted);
    }
    emitted
}

fn asm_lints<'db>(
    cx: &Cx<'_, 'db>,
    tree: &Tree,
    call: MacroCallId,
    asm: &ast::AsmExpr,
    arch: Option<Arch>,
    emitted: &mut Vec<Emitted>,
) {
    let kind = if asm.global_asm_token().is_some() {
        AsmMacro::GlobalAsm
    } else if asm.naked_asm_token().is_some() {
        AsmMacro::NakedAsm
    } else {
        AsmMacro::Asm
    };
    let args = Args::of(tree, asm, kind);
    if args.refused(kind) {
        return;
    }
    let mut lint_node = None;
    let mut templates = Vec::new();
    for expr in asm.template() {
        if tree.skipped(expr.syntax().text_range()) {
            continue;
        }
        let Some((value, style)) = string_of(cx, expr.clone()) else { return };
        let span = cx.span(InFile::new(tree.file, expr.syntax().text_range()));
        let snippet = cx.snippet(&span);
        if arch == Some(Arch::X86) {
            for (needle, message) in [
                (".intel_syntax", "avoid using `.intel_syntax`, Intel syntax is the default"),
                (
                    ".att_syntax",
                    "avoid using `.att_syntax`, prefer using `options(att_syntax)` instead",
                ),
            ] {
                if !value.contains(needle) {
                    continue;
                }
                let at = *lint_node
                    .get_or_insert_with(|| lint_node_of(cx, &call.loc(cx.db).to_node(cx.db)));
                let end: fn(char) -> bool = |c| matches!(c, '\n' | ';' | '\\' | '"');
                let found = snippet.and_then(|it| inner(it, needle, end));
                let mut diag = Diag::lint("bad_asm_style", message, within(&span, found));
                source_fact(cx, &mut diag, at);
                emitted.push(Emitted::new(diag, Phase::Resolve, Pass::Buffered, at));
            }
        }
        let parsed = args.raw || parses(&value, style);
        templates.push(Template { value, span, snippet });
        if !parsed {
            return;
        }
    }
    if kind == AsmMacro::GlobalAsm {
        return;
    }
    let at = tree.ptr(asm.syntax());
    if kind == AsmMacro::NakedAsm && !needs_monomorphization(cx, at) {
        return;
    }
    let binary = !args.att_syntax && matches!(arch, Some(Arch::X86) | None);
    for template in &templates {
        for (label, label_kind) in labels(&template.value, args.raw, arch == Some(Arch::Hexagon)) {
            if label_kind == Label::Binary && !binary {
                continue;
            }
            let found = template.snippet.and_then(|it| inner(it, label, |c| c == ':'));
            let mut diag = label_lint(label_kind, within(&template.span, found), found.is_none());
            source_fact(cx, &mut diag, at);
            emitted.push(Emitted::new(diag, Phase::Late, Pass::Early, at));
        }
    }
}

/// `InvalidAsmLabel` of a label of `kind` at `span`.
fn label_lint(kind: Label, span: Span, missing_precise_span: bool) -> Diag {
    const SEE: &str = "see the asm section of Rust By Example \
        <https://doc.rust-lang.org/nightly/rust-by-example/unsafe/asm.html#labels> for more \
        information";
    const NAMED: &str = "avoid using named labels in inline assembly";
    const LOCAL: &str = "only local labels of the form `<number>:` should be used in inline asm";
    let mut diag = match kind {
        Label::Named => {
            let mut diag = Diag::lint("named_asm_labels", NAMED, span);
            diag.help(LOCAL).note(SEE);
            diag
        }
        Label::FormatArg => {
            let mut diag = Diag::lint("named_asm_labels", NAMED, span);
            diag.help(LOCAL).note("format arguments may expand to a non-numeric value").note(SEE);
            diag
        }
        Label::Binary => {
            let message =
                "avoid using labels containing only the digits `0` and `1` in inline assembly";
            let mut diag = Diag::lint("binary_asm_labels", message, span.clone());
            diag.span_label(span, "use a different label that doesn't start with `0` or `1`")
                .help("start numbering with `2` instead")
                .note("an LLVM bug makes these labels ambiguous with a binary literal number on x86")
                .note(
                    "see <https://github.com/llvm/llvm-project/issues/99547> for more information",
                );
            diag
        }
    };
    if missing_precise_span {
        diag.note("the label may be declared in the expansion of a macro");
    }
    diag
}

/// rustc's `expr_to_spanned_string` of a template once the macros it is a call of are expanded:
/// its string and, when it is raw, its number of `#`s; `None` for what is not a string.
fn string_of(cx: &Cx<'_, '_>, expr: ast::Expr) -> Option<(String, Option<usize>)> {
    match expr {
        ast::Expr::Literal(literal) => match literal.kind() {
            ast::LiteralKind::String(it) => {
                let value = it.value().ok()?.into_owned();
                let hashes =
                    it.is_raw().then(|| it.text()[1..].bytes().take_while(|&b| b == b'#').count());
                Some((value, hashes))
            }
            _ => None,
        },
        ast::Expr::MacroExpr(it) => {
            let expanded = cx.sema.expand_macro_call(&it.macro_call()?)?;
            string_of(cx, ast::Expr::cast(expanded.value)?)
        }
        _ => None,
    }
}

/// Whether `rustc_parse_format` reads `template` as an asm template without an error.
fn parses(template: &str, style: Option<usize>) -> bool {
    let mut parser =
        parse_format::Parser::new(template, style, None, false, parse_format::ParseMode::InlineAsm);
    while parser.next().is_some() {
        if !parser.errors.is_empty() {
            break;
        }
    }
    parser.errors.is_empty()
}

/// `find_span` and `find_label_span`: from where `needle` first is in a template's source text
/// to the first character after it that `end` holds for, else to the last character but one.
fn inner(snippet: &str, needle: &str, end: fn(char) -> bool) -> Option<(usize, usize)> {
    let pos = snippet.find(needle)?;
    let rest = &snippet[pos..];
    Some((pos, pos + rest.find(end).unwrap_or(rest.len() - 1)))
}

/// `Span::from_inner` of `span` for what [`inner`] found, else `span`.
fn within(span: &Span, inner: Option<(usize, usize)>) -> Span {
    match inner {
        Some((lo, hi)) => span.with_lo(span.lo + lo as u32).with_hi(span.lo + hi as u32),
        None => span.clone(),
    }
}

/// rustc's `current_expansion.lint_node_id` at the macro call `call`: the innermost node around
/// it `InvocationCollector` gives an id with `assign_id!`, through the calls of the macros it
/// is in to the crate's root.
fn lint_node_of(cx: &Cx<'_, '_>, call: &InFile<SyntaxNode>) -> InFile<SyntaxNodePtr> {
    let chain = ancestors(cx, call);
    let at =
        chain.iter().skip(1).find(|it| has_lint_id(&it.value)).unwrap_or(&chain[chain.len() - 1]);
    at.as_ref().map(SyntaxNodePtr::new)
}

/// Whether rustc's node for `node` is one `assign_id!` gives an id: the crate, an item, a
/// variant, a where predicate, a field, a field of a pattern or of a struct expression, a
/// parameter, a generic parameter, a match arm, a `let` or a statement ending in `;`, a type, a
/// pattern or an expression; never a macro call, which its expansion replaces.
fn has_lint_id(node: &SyntaxNode) -> bool {
    let kind = node.kind();
    if ast::MacroCall::can_cast(kind)
        || ast::MacroExpr::can_cast(kind)
        || ast::MacroType::can_cast(kind)
        || ast::MacroPat::can_cast(kind)
    {
        return false;
    }
    if let Some(stmt) = ast::ExprStmt::cast(node.clone()) {
        return stmt.semicolon_token().is_some()
            && !matches!(stmt.expr(), Some(ast::Expr::MacroExpr(_)));
    }
    if let Some(block) = ast::BlockExpr::cast(node.clone()) {
        return is_expr(&block);
    }
    ast::SourceFile::can_cast(kind)
        || ast::Item::can_cast(kind)
        || ast::Variant::can_cast(kind)
        || ast::WherePred::can_cast(kind)
        || ast::RecordField::can_cast(kind)
        || ast::TupleField::can_cast(kind)
        || ast::RecordPatField::can_cast(kind)
        || ast::RecordExprField::can_cast(kind)
        || ast::Param::can_cast(kind)
        || ast::SelfParam::can_cast(kind)
        || ast::GenericParam::can_cast(kind)
        || ast::MatchArm::can_cast(kind)
        || ast::LetStmt::can_cast(kind)
        || ast::Type::can_cast(kind)
        || ast::Pat::can_cast(kind)
        || ast::Expr::can_cast(kind)
}

/// Whether `block` is an expression to rustc, not the `Block` of a function, a loop, the
/// branch an `if` takes, a `let .. else` or an asm `label`.
fn is_expr(block: &ast::BlockExpr) -> bool {
    let Some(parent) = block.syntax().parent() else { return true };
    let is = |it: Option<ast::BlockExpr>| it.as_ref() == Some(block);
    let kind = parent.kind();
    if ast::Fn::can_cast(kind) || ast::LetElse::can_cast(kind) || ast::AsmLabel::can_cast(kind) {
        return false;
    }
    if let Some(it) = ast::IfExpr::cast(parent.clone()) {
        return !is(it.then_branch());
    }
    if let Some(it) = ast::LoopExpr::cast(parent.clone()) {
        return !is(it.loop_body());
    }
    if let Some(it) = ast::WhileExpr::cast(parent.clone()) {
        return !is(it.loop_body());
    }
    if let Some(it) = ast::ForExpr::cast(parent) {
        return !is(it.loop_body());
    }
    true
}

/// `generics_of(owner).requires_monomorphization()` for the item whose body `at` is in: a type
/// or const parameter of it or of its parent (a trait's `Self` too); lifetimes don't count.
fn needs_monomorphization(cx: &Cx<'_, '_>, at: InFile<SyntaxNodePtr>) -> bool {
    let def = match crate::lints::body_item(cx, at) {
        Some((ItemId::Function(it), _)) => GenericDefId::FunctionId(it),
        Some((ItemId::Const(it), _)) => GenericDefId::ConstId(it),
        Some((ItemId::Static(it), _)) => GenericDefId::StaticId(it),
        Some((ItemId::Adt(it), _)) => GenericDefId::AdtId(it),
        _ => return true,
    };
    hir_ty::generics::generics(cx.db, def)
        .iter(false)
        .any(|(_, it)| !matches!(it, GenericParamDataRef::LifetimeParamData(_)))
}

/// rustc's `AsmLabelKind`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Label {
    Named,
    FormatArg,
    Binary,
}

/// The labels `AsmLabels` finds in a template's string: each statement (split at `\n` and `;`,
/// a `//` comment cut off) read from its start, a label ending at each `:` until something is
/// not one.
fn labels(template: &str, raw: bool, hexagon: bool) -> Vec<(&str, Label)> {
    let mut labels = Vec::new();
    for statement in template.split(['\n', ';']) {
        let statement = statement.find("//").map_or(statement, |at| &statement[..at]);
        let mut start = 0;
        'label: for (at, _) in statement.match_indices(':') {
            let label = statement[start..at].trim();
            let mut chars = label.chars();
            let Some(first) = chars.next() else { break };
            let mut in_bracket = false;
            let mut kind = Label::Named;
            if !raw && first == '{' {
                in_bracket = true;
                kind = Label::FormatArg;
            } else if matches!(first, '0' | '1') {
                kind = Label::Binary;
            } else if !(first.is_ascii_alphabetic() || matches!(first, '.' | '_')) {
                break;
            }
            if hexagon && is_hexagon_register_span(label, statement, at) {
                break;
            }
            for c in chars {
                if !raw && in_bracket {
                    if c == '{' {
                        break 'label;
                    }
                    if c == '}' {
                        in_bracket = false;
                    }
                } else if !raw && c == '{' {
                    in_bracket = true;
                    kind = Label::FormatArg;
                } else {
                    let continues = match kind {
                        Label::Named | Label::FormatArg => {
                            c.is_ascii_alphanumeric() || matches!(c, '_' | '$')
                        }
                        Label::Binary => matches!(c, '0' | '1'),
                    };
                    if !continues {
                        break 'label;
                    }
                }
            }
            labels.push((label, kind));
            start = at + 1;
        }
    }
    labels
}

/// `is_hexagon_register_span_context`: whether `label` and what follows the `:` at `colon` in
/// `statement` are a Hexagon register pair (`r1:0`, `V5:4.w`), not a label.
fn is_hexagon_register_span(label: &str, statement: &str, colon: usize) -> bool {
    let after = &statement[colon + 1..];
    let end = after.find(|c: char| !(c.is_ascii_alphanumeric() || c == '.'));
    let after = &after[..end.unwrap_or(after.len())];
    let mut chars = label.chars();
    let Some(first) = chars.next() else { return false };
    if after.is_empty() || !first.is_ascii_alphabetic() {
        return false;
    }
    let rest = chars.as_str();
    !rest.is_empty()
        && rest.chars().all(|c| c.is_ascii_digit())
        && after.starts_with(|c: char| c.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn named(template: &str) -> Vec<(&str, Label)> {
        labels(template, false, false)
    }

    #[test]
    fn labels_end_at_the_first_thing_that_is_not_one() {
        assert_eq!(named("foo: bar1: nop"), [("foo", Label::Named), ("bar1", Label::Named)]);
        assert_eq!(named("blah1: 2bar: nop"), [("blah1", Label::Named)]);
        for template in
            ["1bar: blah: nop", "ab cd: nop", "mov rax, qword ptr fs:[0]", ":lo12:FOO", "testÙ: nop"]
        {
            assert!(named(template).is_empty(), "{template}");
        }
    }

    #[test]
    fn statements_split_at_newlines_and_semicolons_without_comments() {
        assert_eq!(named("\n ab: nop // ab: does foo\n // cd: nop\n"), [("ab", Label::Named)]);
        assert_eq!(named("nop; foo10: nop"), [("foo10", Label::Named)]);
    }

    #[test]
    fn format_arguments_are_labels_unless_raw() {
        assert_eq!(named("test_{}: nop"), [("test_{}", Label::FormatArg)]);
        assert_eq!(named("{uwu}: nop"), [("{uwu}", Label::FormatArg)]);
        assert!(named("{{}: nop").is_empty());
        assert!(labels("test_{}: nop", true, false).is_empty());
    }

    #[test]
    fn binary_labels_are_only_zeros_and_ones() {
        assert_eq!(named("0: jmp 0b"), [("0", Label::Binary)]);
        assert_eq!(named("1001101: jmp 1001101b"), [("1001101", Label::Binary)]);
        assert!(named("12: jmp 12b").is_empty());
    }

    /// `tests/ui/asm/hexagon-register-pairs.rs`, whose `.stderr` also has a warning of
    /// `-C target-feature`, which the test harness has no flag for.
    #[test]
    fn hexagon_register_pairs_are_not_labels() {
        for template in [
            "r1:0 = memd(r29+#0)",
            "r3:2 = combine(#1, #0)",
            "r15:14 = memd(r30+#8)",
            "memd(r29+#0) = r5:4",
            "V5:4 = vaddw(v1:0, v1:0)",
            "v1:0.w = vsub(v1:0.w,v1:0.w):sat",
        ] {
            assert!(labels(template, false, true).is_empty(), "{template}");
        }
        let mixed = labels("label1: r7:6 = combine(#2, #3)", false, true);
        assert_eq!(mixed, [("label1", Label::Named)]);
        assert_eq!(labels("hexagon_label: nop", false, true), [("hexagon_label", Label::Named)]);
        assert_eq!(named("r1:0 = memd(r29+#0)"), [("r1", Label::Named)]);
    }

    #[test]
    fn a_span_runs_from_the_needle_to_its_end() {
        let colon = |c: char| c == ':';
        assert_eq!(inner("\"bar: nop\"", "bar", colon), Some((1, 4)));
        assert_eq!(inner("\"fooo\\u{003A} nop\"", "fooo", colon), Some((1, 17)));
        assert_eq!(inner("\"\\x41\\x42\"", "AB", colon), None);
        let style = |c: char| matches!(c, '\n' | ';' | '\\' | '"');
        let intel = inner("\".intel_syntax noprefix; nop\"", ".intel_syntax", style);
        assert_eq!(intel, Some((1, 23)));
    }
}
