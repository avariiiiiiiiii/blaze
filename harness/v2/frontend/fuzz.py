"""fuzz.py: small programs generated from a seed, aimed at the front end's redesigned paths, each
compared under rustc and blaze as cases.py compares a case; a mismatch is shrunk and written as a
new case (docs/v2/plan/lanes/frontend/fast-diff.md; section 9 of docs/v2/plan/lanes/frontend/fast.md).

    uv run --frozen python harness/v2/frontend/fuzz.py '{"engine": {"kind": "v2", "blaze": "target/release/blaze"}, "out": "/tmp/fuzz.json"}'
    uv run --frozen python harness/v2/frontend/fuzz.py '{"engine": {...}, "out": "...", "seed": 7, "count": 500, "areas": ["consts"]}'
    uv run --frozen python harness/v2/frontend/fuzz.py selftest

Program `i` of seed `s` is made by `random.Random("blaze-fuzz:<s>:<i>")` alone, so a seed gives
the same programs on every machine and run. Its area is `areas[i % len(areas)]`:

    traits     trait goals: generic and blanket impls, associated types, supertraits, coherence
               with a local type, auto traits (`Send`, `Sync`, `Unpin`, negative and unsafe
               impls), `Sized` and `?Sized` bounds
    methods    method calls: autoderef through `&`, `Box`, `Rc` and user `Deref` chains, autoref,
               by-value against by-reference impls, inherent against trait, traits in scope,
               ambiguity
    coercions  reborrows, unsizing (slices, `dyn`, struct tails, upcasting), `!`, closures and fn
               items to fn pointers, deref coercion of arguments
    consts     every integer type and operator, in `const`, `static`, `const fn`, at run time and
               as literals: overflow and division by zero, casts, array lengths, a generic
               constant that fails only when instantiated
    borrows    two-phase borrows, loops, moves in match arms, NLL, closures, disjoint fields

A program is a few snippets of its area, each printing what it computed; a snippet is broken on
purpose about one time in five (a missing impl, a conflicting borrow), so rustc rejects some of
them. rustc is the truth: there is no header to hold it to. A program blaze treats otherwise
(cases.py's categories) is shrunk as C-Reduce does, by passes to a fixpoint: delete an item or a
statement (or empty a block), then delete an element of a list (an argument, a field, a match
arm), then delete a line; each pass's candidates are compared in one batch, every deletion that
keeps the same mismatch (`signature`) is tried together, and the shrinking starts again from the
first pass after each success. The result is written as a case of `into` (`<area>-s<seed>-p<i>.rs`)
whose header is what rustc does with it.

    pass   blaze treats the program as rustc does
    fail   it does not; the detail names the mismatch and the case it was written as
    error  the harness could not say (rustc crashed on the program or timed out, ...)

The input is one JSON object (inline, a file's path, or `-` for stdin):

    engine    the blaze under test (engine.EngineSpec)
    out       the path the result JSON is written to (report.py)
    work      the directory everything is built in; empty: a new one under /tmp
    seed      the seed (0)
    count     how many programs (200)
    areas     which areas, in order (all of them)
    shrink    shrink each mismatch and write it as a case (true)
    rounds    at most this many batches of candidates a mismatch is shrunk with (40)
    into      where shrunk mismatches are written ("": cases/fuzz beside this file)
    batch, whole, jobs, keep, cache
              as cases.py takes them
"""

import hashlib
import platform
import random
import re
import sys
import tempfile
from collections import Counter
from collections.abc import Callable, Sequence
from dataclasses import dataclass
from pathlib import Path
from typing import Literal, get_args

HERE = Path(__file__).resolve().parent
sys.path[:0] = [str(HERE), str(HERE.parent), str(HERE.parent.parent)]
from typed_input import fail, parse  # noqa: E402
import engine, report, rejections  # noqa: E402
import cases  # noqa: E402

SCRIPT = "fuzz.py"
Area = Literal["traits", "methods", "coercions", "consts", "borrows"]
AREAS: tuple[Area, ...] = get_args(Area)
INTO = HERE / "cases" / "fuzz"
EDITION = "2021"
BAD = 0.2


@dataclass(frozen=True)
class Input:
    engine: engine.EngineSpec
    out: str
    work: str = ""
    seed: int = 0
    count: int = 200
    areas: tuple[Area, ...] = AREAS
    shrink: bool = True
    rounds: int = 40
    into: str = ""
    batch: int = 100
    whole: bool = False
    jobs: int = 4
    keep: bool = False
    cache: str = "~/.cache/blaze-v2-proof"


def fill(template: str, names: dict[str, str]) -> str:
    """`template` with each `@name` replaced, longest names first (so `@ab` is not `@a` then b)."""
    for key in sorted(names, key=lambda k: -len(k)):
        template = template.replace("@" + key, names[key])
    return template


class Gen:
    """A program being written: crate attributes, items and the statements of `main`. Only
    `random()` and `randrange` are drawn, whose sequences Python keeps across versions."""

    def __init__(self, rng: random.Random):
        self.rng = rng
        self.attrs: set[str] = set()
        self.items: list[str] = []
        self.body: list[str] = []
        self.count = 0

    def name(self, stem: str) -> str:
        self.count += 1
        return f"{stem}{self.count}"

    def chance(self, p: float) -> bool:
        return self.rng.random() < p

    def bad(self) -> bool:
        return self.chance(BAD)

    def pick[T](self, options: Sequence[T]) -> T:
        return options[self.rng.randrange(len(options))]

    def between(self, lo: int, hi: int) -> int:
        return lo + self.rng.randrange(hi - lo + 1)

    def item(self, template: str, **names: str) -> None:
        self.items.append(fill(template, names).strip("\n"))

    def stmt(self, template: str, **names: str) -> None:
        self.body += fill(template, names).strip("\n").splitlines()

    def say(self, expr: str, fmt: str = "{}") -> None:
        self.body.append('println!("' + fmt + '", ' + expr + ");")

    def text(self) -> str:
        main = ["fn main() {", *(("    " + line) if line else "" for line in self.body), "}"]
        return "\n".join([f"//@ edition: {EDITION}", *sorted(self.attrs), "", *"\n\n".join(self.items).splitlines(),
                          "", *main]) + "\n"


# --- traits

SCALARS = (("u8", "7u8"), ("i64", "(-3i64)"), ("bool", "true"), ("char", "'x'"), ("u32", "40u32"))
BLANKETS = (
    (0.5, "impl<X: @T> @T for Vec<X> {\n    fn @m(&self) -> String {\n"
          "        self.iter().map(|x| x.@m()).collect::<Vec<_>>().join(\",\")\n    }\n}"),
    (0.5, "impl<X: @T> @T for Option<X> {\n    fn @m(&self) -> String {\n        match self {\n"
          "            Some(x) => format!(\"some {}\", x.@m()),\n            None => String::from(\"none\"),\n"
          "        }\n    }\n}"),
    (0.4, "impl<X: @T, Y: @T> @T for (X, Y) {\n    fn @m(&self) -> String {\n"
          "        format!(\"({} {})\", self.0.@m(), self.1.@m())\n    }\n}"),
    (0.4, "impl<X: @T, const N: usize> @T for [X; N] {\n    fn @m(&self) -> String {\n"
          "        format!(\"[{}] {}\", N, self.iter().map(|x| x.@m()).collect::<Vec<_>>().join(\" \"))\n    }\n}"),
    (0.3, "impl<X: @T + ?Sized> @T for &X {\n    fn @m(&self) -> String {\n"
          "        format!(\"&{}\", (**self).@m())\n    }\n}"),
    (0.1, "impl<X: std::fmt::Debug> @T for X {\n    fn @m(&self) -> String {\n        format!(\"{:?}\", self)\n    }\n}"),
)


def composite(g: Gen, depth: int) -> str:
    kind = g.pick(("scalar", "vec", "option", "tuple", "array", "ref") if depth > 0 else ("scalar",))
    if kind == "scalar":
        return g.pick(SCALARS)[1]
    inner = composite(g, depth - 1)
    if kind == "vec":
        return f"vec![{inner}, {inner}]"
    if kind == "option":
        return f"Some({inner})"
    if kind == "tuple":
        return f"({inner}, {composite(g, depth - 1)})"
    if kind == "array":
        return f"[{inner}, {inner}, {inner}]"
    return f"&{inner}"


def trait_impls(g: Gen) -> None:
    t, m = g.name("Describe"), g.name("describe")
    g.item("trait @T {\n    fn @m(&self) -> String;\n}", T=t, m=m)
    for ty, _ in SCALARS:
        if g.chance(0.6):
            g.item("impl @T for @ty {\n    fn @m(&self) -> String {\n        format!(\"@ty {}\", self)\n    }\n}",
                   T=t, m=m, ty=ty)
    for p, template in BLANKETS:
        if g.chance(p):
            g.item(template, T=t, m=m)
    for _ in range(g.between(1, 3)):
        value = composite(g, g.between(0, 2))
        g.say(f"{t}::{m}(&{value})" if g.chance(0.5) else f"({value}).{m}()")


AUTO = (("u8", "7u8"), ("String", "String::from(\"s\")"), ("Vec<u8>", "vec![1u8]"),
        ("std::rc::Rc<u8>", "std::rc::Rc::new(1u8)"), ("std::cell::Cell<u8>", "std::cell::Cell::new(1u8)"),
        ("std::sync::Arc<std::sync::Mutex<u8>>", "std::sync::Arc::new(std::sync::Mutex::new(1u8))"),
        ("*const u8", "&1u8 as *const u8"), ("Box<u8>", "Box::new(1u8)"),
        ("std::cell::RefCell<u8>", "std::cell::RefCell::new(1u8)"),
        ("std::marker::PhantomData<*const u8>", "std::marker::PhantomData"),
        ("(u8, String)", "(1u8, String::new())"), ("[std::rc::Rc<u8>; 1]", "[std::rc::Rc::new(1u8)]"),
        ("std::sync::Arc<std::cell::Cell<u8>>", "std::sync::Arc::new(std::cell::Cell::new(1u8))"),
        ("&'static str", "\"st\""), ("std::marker::PhantomPinned", "std::marker::PhantomPinned"))


def trait_auto(g: Gen) -> None:
    auto, f = g.pick(("Send", "Sync", "Unpin")), g.name("needs")
    g.item("fn @f<X: @A + ?Sized>(_: &X) -> &'static str {\n    \"@A\"\n}", f=f, A=auto)
    for _ in range(g.between(1, 3)):
        ty, value = g.pick(AUTO)
        v = g.name("v")
        g.stmt("let @v: @ty = @e;", v=v, ty=ty, e=value)
        g.say(f"{f}(&{v})")
    if g.chance(0.6):
        ty, value = g.pick(AUTO)
        s, v = g.name("Holder"), g.name("held")
        g.item("struct @S {\n    f: @ty,\n}", S=s, ty=ty)
        opt = g.pick(("none", "negative", "positive"))
        if opt == "negative" and auto != "Unpin":
            g.attrs.add("#![feature(negative_impls)]")
            g.item("impl !@A for @S {}", A=auto, S=s)
        elif opt == "positive":
            g.item("unsafe impl @A for @S {}" if auto != "Unpin" else "impl @A for @S {}", A=auto, S=s)
        g.stmt("let @v = @S { f: @e };", v=v, S=s, e=value)
        g.say(f"{f}(&{v})")


def trait_sized(g: Gen) -> None:
    f, k = g.name("size_of_val_"), g.name("sized_")
    g.item("fn @f<X: ?Sized>(x: &X) -> usize {\n    std::mem::size_of_val(x)\n}", f=f)
    g.item("fn @k<X>(_: X) -> usize {\n    std::mem::size_of::<X>()\n}", k=k)
    for _ in range(g.between(1, 3)):
        e = g.pick(("\"abc\"", "&[1u16, 2, 3][..]", "&5u32", "&[0u8; 7]", "&String::from(\"xy\")", "&(1u8, 2u32)",
                    "&()", "&[1u64; 0][..]", "&[[1u8; 2]; 3][..]"))
        g.say(f"{f}({e})")
    g.say(f"{k}({g.pick(('(1u8, 2u16)', '[0u32; 3]', '()', 'Some(1u8)', '1i128', 'String::new()'))})")
    t = g.name("Tail")
    g.item("struct @S {\n    len: u8,\n    data: [u16],\n}", S=t)
    g.say(f"std::mem::size_of::<&{t}>()")
    if g.bad():
        b, u = g.name("unsized_"), g.name("Unsized")
        broken = g.pick((("item", "fn @b<X: ?Sized>(x: X) -> usize {\n    0\n}"),
                         ("item", "struct @U {\n    data: [u8],\n    len: u8,\n}"),
                         ("say", f"{k}(*\"abc\")"),
                         ("say", f"{f}::<str>(\"abc\")"),
                         ("stmt", "let @b: [u8] = [1u8, 2][..];")))
        if broken[0] == "item":
            g.item(broken[1], b=b, U=u)
        elif broken[0] == "say":
            g.say(broken[1])
        else:
            g.stmt(broken[1], b=b)


PRODUCERS = (("u8", "u16", "(*self as u16) * 2", "5u8"), ("bool", "u8", "*self as u8", "true"),
             ("i32", "i64", "(*self as i64) - 1", "(-4i32)"), ("char", "u32", "*self as u32", "'c'"))


def trait_assoc(g: Gen) -> None:
    it, p, tw = g.name("Produce"), g.name("produce"), g.name("twice")
    g.item("trait @I {\n    type Out;\n    fn @p(&self) -> Self::Out;\n}", I=it, p=p)
    for ty, out, body, _ in PRODUCERS:
        if g.chance(0.7):
            g.item("impl @I for @ty {\n    type Out = @out;\n    fn @p(&self) -> @out {\n        @body\n    }\n}",
                   I=it, p=p, ty=ty, out=out, body=body)
    g.item("fn @t<X: @I>(x: &X) -> X::Out\nwhere\n    X::Out: std::ops::Add<Output = X::Out> + Copy,\n{\n"
           "    let a = x.@p();\n    a + a\n}", t=tw, I=it, p=p)
    for _ in range(g.between(1, 3)):
        g.say(f"{tw}(&{g.pick(PRODUCERS)[3]})")


def trait_super(g: Gen) -> None:
    b, d, s, base, derived = g.name("Base"), g.name("Derived"), g.name("S"), g.name("base"), g.name("derived")
    names = dict(B=b, D=d, S=s, base=base, derived=derived)
    g.item("trait @B {\n    fn @base(&self) -> u32 {\n        1\n    }\n}", **names)
    g.item("trait @D: @B {\n    fn @derived(&self) -> u32 {\n        self.@base() + 10\n    }\n}", **names)
    g.item("struct @S;", **names)
    if not g.bad():
        g.item("impl @B for @S {}", **names)
    if g.chance(0.5):
        g.item("impl @D for @S {\n    fn @derived(&self) -> u32 {\n        self.@base() * 3\n    }\n}", **names)
    else:
        g.item("impl @D for @S {}", **names)
    g.say(f"{s}.{derived}()")
    g.say(f"(&{s} as &dyn {d}).{base}()")
    if g.chance(0.4):
        g.say(f"{{ let up: &dyn {b} = &{s} as &dyn {d}; up.{base}() }}")


def trait_overlap(g: Gen) -> None:
    k, m, l = g.name("Mark"), g.name("mark"), g.name("Local")
    g.item("trait @K {\n    fn @m(&self) -> u8;\n}", K=k, m=m)
    g.item("impl<X: Clone> @K for X {\n    fn @m(&self) -> u8 {\n        1\n    }\n}", K=k, m=m)
    g.item(("#[derive(Clone)]\n" if g.bad() else "") + "struct @L;", L=l)
    g.item("impl @K for @L {\n    fn @m(&self) -> u8 {\n        2\n    }\n}", K=k, m=m, L=l)
    g.say(f"5u8.{m}()")
    g.say(f"{l}.{m}()")
    g.say(f"String::new().{m}()")


# --- methods

def method_pick(g: Gen) -> None:
    w, t, m, md = g.name("W"), g.name("Pick"), g.name("m"), g.name("traits")
    g.item("struct @W(i32);", W=w)
    inherent = g.pick((None, "self", "&self", "&mut self"))
    if inherent is not None:
        g.item("impl @W {\n    fn @m(@r) -> &'static str {\n        \"inherent @r\"\n    }\n}", W=w, m=m, r=inherent)
    receiver = g.pick(("self", "&self", "&mut self"))
    nested = g.chance(0.4)
    prefix = "super::" if nested else ""
    targets = [x for x in (f"{prefix}{w}", f"&{prefix}{w}", f"&mut {prefix}{w}", f"Box<{prefix}{w}>") if g.chance(0.45)]
    impls = [fill("impl @T for @X {\n    fn @m(@r) -> &'static str {\n        \"@T for @shown @r\"\n    }\n}",
                  dict(T=t, X=x, m=m, r=receiver, shown=x.replace(prefix, ""))) for x in targets or [f"{prefix}{w}"]]
    declaration = fill("trait @T {\n    fn @m(@r) -> &'static str;\n}", dict(T=t, m=m, r=receiver))
    if nested:
        body = "\n".join(["pub " + declaration, *impls])
        g.item("mod @md {\n" + "\n".join("    " + line if line else "" for line in body.splitlines()) + "\n}", md=md)
        if not g.bad():
            g.item("use @md::@T;", md=md, T=t)
    else:
        g.item(declaration)
        for text in impls:
            g.item(text)
    expr = f"{w}(1)"
    for _ in range(g.rng.randrange(4)):
        expr = g.pick(("&$", "&mut $", "Box::new($)", "std::rc::Rc::new($)")).replace("$", expr)
    v = g.name("recv")
    g.stmt("let mut @v = @e;", v=v, e=expr)
    g.say(f"{v}.{m}()")
    if g.chance(0.3):
        g.say(f"(&{v}).{m}()")


def method_deref(g: Gen) -> None:
    base, get, bump = g.name("Base"), g.name("get"), g.name("bump")
    g.item("struct @B(i32);", B=base)
    g.item("impl @B {\n    fn @get(&self) -> i32 {\n        self.0\n    }\n    fn @bump(&mut self) {\n        self.0 += 1;\n"
           "    }\n}", B=base, get=get, bump=bump)
    prev, expr = base, f"{base}(3)"
    for k in range(g.between(1, 4)):
        layer = g.name("Layer")
        names = dict(L=layer, P=prev, get=get, value=str((k + 1) * 100))
        g.item("struct @L(@P);", **names)
        g.item("impl std::ops::Deref for @L {\n    type Target = @P;\n    fn deref(&self) -> &@P {\n        &self.0\n"
               "    }\n}", **names)
        if not g.bad():
            g.item("impl std::ops::DerefMut for @L {\n    fn deref_mut(&mut self) -> &mut @P {\n        &mut self.0\n"
                   "    }\n}", **names)
        if g.chance(0.2):
            g.item("impl @L {\n    fn @get(&self) -> i32 {\n        @value\n    }\n}", **names)
        prev, expr = layer, f"{layer}({expr})"
    v = g.name("chain")
    g.stmt("let mut @v = @e;", v=v, e=expr)
    g.stmt("@v.@bump();", v=v, bump=bump)
    g.say(f"{v}.{get}()")
    g.say(f"(*{v}).{get}()" if g.chance(0.5) else f"(&&{v}).{get}()")


def method_ambiguous(g: Gen) -> None:
    a, b, s, m = g.name("A"), g.name("B"), g.name("S"), g.name("which")
    names = dict(A=a, B=b, S=s, m=m)
    g.item("trait @A {\n    fn @m(&self) -> u8 {\n        1\n    }\n}", **names)
    g.item("trait @B {\n    fn @m(&self) -> u8 {\n        2\n    }\n}", **names)
    g.item("struct @S;", **names)
    g.item("impl @A for @S {}", **names)
    both = g.chance(0.4)
    if both:
        g.item("impl @B for @S {}", **names)
    if g.chance(0.4 if both else 0.2):
        g.item("impl @S {\n    fn @m(&self) -> u8 {\n        3\n    }\n}", **names)
    g.say(f"{s}.{m}()")
    g.say(f"<{s} as {a}>::{m}(&{s})")
    if both:
        g.say(f"{b}::{m}(&{s})")


def method_value_ref(g: Gen) -> None:
    v, m, x = g.name("V"), g.name("v"), g.name("x")
    g.item("trait @V {\n    fn @m(self) -> &'static str;\n}", V=v, m=m)
    for ty in ("u8", "&u8", "&&u8", "&mut u8"):
        if g.chance(0.5):
            g.item("impl @V for @ty {\n    fn @m(self) -> &'static str {\n        \"@ty\"\n    }\n}", V=v, m=m, ty=ty)
    g.stmt("let mut @x = 3u8;", x=x)
    for _ in range(g.between(1, 3)):
        g.say(g.pick(("$.@m()", "(&$).@m()", "(&&$).@m()", "(&mut $).@m()")).replace("$", x).replace("@m", m))


def method_shadowed_len(g: Gen) -> None:
    t, extra, v = g.name("Len"), g.name("extra"), g.name("items")
    g.item("trait @T {\n    fn len(&self) -> usize {\n        99\n    }\n    fn @e(&self) -> usize {\n"
           "        self.len() + 1\n    }\n}", T=t, e=extra)
    g.item(g.pick(("impl<X> @T for Vec<X> {}", "impl<X> @T for [X] {}", "impl @T for str {}")), T=t)
    g.stmt(g.pick(("let @v = vec![1u8, 2];", "let @v: &[u8] = &[1, 2, 3];", "let @v = \"four\";")), v=v)
    g.say(f"{v}.len()")
    g.say(f"{v}.{extra}()")
    g.say(f"{t}::len(&{v})" if g.chance(0.5) else f"{t}::len({v})")


# --- coercions

def coerce_reborrow(g: Gen) -> None:
    a, v, r = g.name("add"), g.name("v"), g.name("r")
    names = dict(a=a, v=v, r=r, r2=g.name("r"))
    g.item("fn @a(x: &mut i32, by: i32) {\n    *x += by;\n}", **names)
    g.stmt("let mut @v = 1;\nlet @r = &mut @v;\n@a(@r, 2);\n@a(@r, 3);\n@a(&mut *@r, 4);", **names)
    if g.chance(0.6):
        g.stmt("let @r2 = @r;" if g.bad() else "let @r2: &mut i32 = @r;", **names)
        g.stmt("@a(@r2, 5);\n@a(@r, 6);", **names)
    if g.chance(0.4):
        g.stmt("let shared: &i32 = @r;\nprintln!(\"{}\", shared);", **names)
    g.say(v)


UNSIZE = (
    ("let @x: &[u8] = &[1, 2, 3];", "@x.len()"),
    ("let @x: Box<[u16]> = Box::new([4, 5]);", "@x.len()"),
    ("let @x: &dyn std::fmt::Display = &5u8;", "@x"),
    ("let @x: Box<dyn Fn(i32) -> i32> = Box::new(|y| y * 2);", "@x(3)"),
    ("let @x: std::rc::Rc<[i32]> = std::rc::Rc::new([1, 2]);", "@x.len()"),
    ("let @x: &dyn std::fmt::Debug = &[1u8, 2];", "format!(\"{:?}\", @x)"),
    ("let @x: Vec<Box<dyn std::fmt::Display>> = vec![Box::new(1u8), Box::new(\"two\"), Box::new('3')];",
     "@x.iter().map(|d| d.to_string()).collect::<Vec<_>>().join(\",\")"),
    ("let @x: &mut [i32] = &mut [3, 1, 2];\n@x.sort();", "format!(\"{:?}\", @x)"),
    ("let @x: Box<dyn std::any::Any> = Box::new(7u16);", "@x.downcast_ref::<u16>().copied().unwrap_or(0)"),
)
BAD_UNSIZE = (
    ("let @x: &[u8; 2] = &[1u8, 2, 3][..];", "@x.len()"),
    ("let @x: Box<dyn Fn()> = Box::new(5);", "1"),
    ("let @x: &dyn std::fmt::Display = &vec![1u8];", "@x"),
    ("let @x: &[u32] = &[1u8, 2];", "@x.len()"),
)


def coerce_unsize(g: Gen) -> None:
    for _ in range(g.between(1, 3)):
        let, shown = g.pick(BAD_UNSIZE if g.bad() else UNSIZE)
        x = g.name("u")
        g.stmt(let, x=x)
        g.say(fill(shown, dict(x=x)))
    if g.chance(0.4):
        w, x = g.name("Wrap"), g.name("tail")
        g.item("struct @W<T: ?Sized> {\n    tag: u8,\n    data: T,\n}", W=w)
        g.stmt("let @x: &@W<[u8]> = &@W { tag: 1, data: [1, 2, 3] };", W=w, x=x)
        g.say(f"{x}.tag as usize + {x}.data.len()")
    if g.chance(0.4):
        sup, sub, s, x, y = g.name("Sup"), g.name("Sub"), g.name("s"), g.name("down"), g.name("up")
        names = dict(Sup=sup, Sub=sub, s=s, x=x, y=y)
        g.item("trait @Sup {\n    fn @s(&self) -> u8 {\n        1\n    }\n}", **names)
        g.item("trait @Sub: @Sup {}", **names)
        g.item("impl @Sup for u8 {}\nimpl @Sub for u8 {}", **names)
        g.stmt("let @x: &dyn @Sub = &1u8;\nlet @y: &dyn @Sup = @x;", **names)
        g.say(f"{y}.{s}()")


def coerce_never(g: Gen) -> None:
    f, d, v, w, q = g.name("pick"), g.name("diverge"), g.name("v"), g.name("w"), g.name("q")
    names = dict(f=f, d=d, v=v, w=w, q=q, b=g.name("b"))
    g.item("fn @f(c: bool) -> u32 {\n    let x: u32 = if c { 5 } else { return 9 };\n    x + 1\n}", **names)
    g.item("fn @d() -> ! {\n    panic!(\"diverged\")\n}", **names)
    g.say(f"{f}(true) + {f}(false)")
    g.stmt("let @v: i32 = match Some(3) {\n    Some(v) => v,\n    None => panic!(\"none\"),\n};", **names)
    g.stmt("let @w: u8 = loop {\n    break 4;\n};", **names)
    g.stmt("let @q: u64 = if @v > 0 { 7 } else { @d() };", **names)
    g.say(f"{v} as u64 + {w} as u64 + {q}")
    if g.bad():
        g.stmt("let @b: u32 = if @v > 0 { 5 } else { \"five\" };", **names)
    if g.chance(0.15):
        g.stmt("if @w == 4 {\n    @d();\n}", **names)


def coerce_fnptr(g: Gen) -> None:
    i, d, f, c, h, t, u = (g.name(s) for s in ("inc", "dbl", "f", "c", "h", "t", "u"))
    names = dict(i=i, d=d, f=f, c=c, h=h, t=t, u=u, k=g.name("k"), bad=g.name("bad"))
    g.item("fn @i(x: i32) -> i32 {\n    x + 1\n}", **names)
    g.item("fn @d(x: i32) -> i32 {\n    x * 2\n}", **names)
    g.stmt("let @f: fn(i32) -> i32 = |x| x - 1;", **names)
    g.say(f"{f}(5)")
    g.stmt("let @c = @f(1) > 0;\nlet @h = if @c { @i } else { @d };", **names)
    g.say(f"{h}(5)")
    g.stmt("let @t: [fn(i32) -> i32; 3] = [@i, @d, |x| x];", **names)
    g.say(f"{t}.iter().map(|g| g(2)).sum::<i32>()")
    if g.chance(0.5):
        g.stmt("let @u: unsafe fn(i32) -> i32 = @i;", **names)
        g.say(f"unsafe {{ {u}(1) }}")
    if g.chance(0.4):
        g.stmt("let @u = match @f(3) {\n    2 => @i,\n    _ => @d,\n};", **names)
        g.say(f"{u}(10)")
    if g.bad():
        g.stmt("let @k = 3;\nlet @bad: fn(i32) -> i32 = |x| x + @k;", **names)


def coerce_deref(g: Gen) -> None:
    s, l, a, b, v = g.name("takes_str"), g.name("takes_slice"), g.name("a"), g.name("b"), g.name("v")
    names = dict(s=s, l=l, a=a, b=b, v=v)
    g.item("fn @s(s: &str) -> usize {\n    s.len()\n}", **names)
    g.item("fn @l(s: &[i32]) -> i32 {\n    s.iter().sum()\n}", **names)
    g.stmt("let @a = String::from(\"hello\");\nlet @b = Box::new(String::from(\"box\"));\nlet @v = vec![1, 2, 3];",
           **names)
    calls = ["@s(&@a)", "@s(&@b)", "@s(&&&@a)", "@l(&@v)", "@l(&[4, 5])", "@s(\"lit\")", "@l(&Box::new([6, 7]))",
             "@s(&std::rc::Rc::new(String::from(\"rc\")))", "@l(&mut vec![8])"]
    for _ in range(g.between(2, 4)):
        g.say(fill(g.pick(calls), names))
    if g.bad():
        g.say(fill(g.pick(("@s(@a)", "@l(@v)", "@s(&@v)")), names))


# --- constants

INTS = (("i8", 8, True), ("i16", 16, True), ("i32", 32, True), ("i64", 64, True), ("i128", 128, True),
        ("isize", 64, True), ("u8", 8, False), ("u16", 16, False), ("u32", 32, False), ("u64", 64, False),
        ("u128", 128, False), ("usize", 64, False))
OPS = ("+", "-", "*", "/", "%", "<<", ">>", "&", "|", "^")
OP_NAMES = {"+": "add", "-": "sub", "*": "mul", "/": "div", "%": "rem", "<<": "shl", ">>": "shr"}
SATURATING = ("+", "-", "*", "/")
FLOATS = ("1e10_f64", "(-1.5_f64)", "255.9_f64", "f64::NAN", "f64::INFINITY", "(-0.0_f64)", "300.0_f32", "0.5_f32")


def bounds(bits: int, signed: bool) -> tuple[int, int]:
    return (-(1 << (bits - 1)), (1 << (bits - 1)) - 1) if signed else (0, (1 << bits) - 1)


def interesting(g: Gen, bits: int, signed: bool) -> int:
    lo, hi = bounds(bits, signed)
    pool = [0, 1, 2, 3, hi, hi - 1, lo, lo + 1, hi // 2, lo + g.rng.randrange(hi - lo + 1)]
    return g.pick(pool + ([-1, -2] if signed else []))


def literal(ty: str, value: int, bits: int, signed: bool) -> str:
    """`value` of type `ty` as a literal that is always in range (no `overflowing_literals`)."""
    lo, hi = bounds(bits, signed)
    if value == hi:
        return f"{ty}::MAX"
    if signed and value == lo:
        return f"{ty}::MIN"
    return f"(-{-value}{ty})" if value < 0 else f"{value}{ty}"


def operands(g: Gen, op: str, ty: str, bits: int, signed: bool) -> tuple[str, str, str]:
    """The left and right operand of `op` on `ty`, and the right one's type (a shift's is `u32`)."""
    a = literal(ty, interesting(g, bits, signed), bits, signed)
    if op in ("<<", ">>"):
        return a, f"{g.pick((0, 1, 3, bits - 1, bits, bits + 1))}u32", "u32"
    return a, literal(ty, interesting(g, bits, signed), bits, signed), ty


def const_arith(g: Gen) -> None:
    ty, bits, signed = g.pick(INTS)
    op = g.pick(OPS)
    a, b, rty = operands(g, op, ty, bits, signed)
    mode = g.pick(("const", "static", "runtime", "literal", "method", "constfn"))
    if mode == "method" and (op not in OP_NAMES or (op not in SATURATING and g.chance(0.5))):
        mode = "const"
    names = dict(ty=ty, rty=rty, a=a, b=b, op=op, c=g.name("C"), x=g.name("x"), y=g.name("y"), f=g.name("op"))
    if mode == "const":
        g.item("const @c: @ty = @a @op @b;", **names)
        g.say(names["c"])
    elif mode == "static":
        g.item("static @c: @ty = @a @op @b;", **names)
        g.say(names["c"])
    elif mode == "runtime":
        g.stmt("let @x: @ty = std::hint::black_box(@a);\nlet @y: @rty = std::hint::black_box(@b);", **names)
        g.say(fill("@x @op @y", names))
    elif mode == "literal":
        g.stmt("let @x: @ty = @a @op @b;", **names)
        g.say(names["x"])
    elif mode == "constfn":
        g.item("const fn @f(x: @ty, y: @rty) -> @ty {\n    x @op y\n}", **names)
        g.item("const @c: @ty = @f(@a, @b);", **names)
        g.say(names["c"])
    else:
        kinds = ("checked", "wrapping", "overflowing") + (("saturating",) if op in SATURATING else ())
        g.say(f"{a}.{g.pick(kinds)}_{OP_NAMES[op]}({b})", "{:?}")


def const_unary_cast(g: Gen) -> None:
    ty, bits, signed = g.pick(INTS)
    lit = literal(ty, interesting(g, bits, signed), bits, signed)
    shape = g.pick(("neg", "not", "cast", "float", "compare"))
    if shape == "neg" and not signed:
        shape = "not"
    if shape == "neg":
        expr, shown = f"-({lit})", ty
    elif shape == "not":
        expr, shown = f"!{lit}", ty
    elif shape == "cast":
        shown = g.pick(INTS)[0]
        expr = f"{lit} as {shown}"
    elif shape == "float":
        expr, shown = f"{g.pick(FLOATS)} as {ty}", ty
    else:
        other = literal(ty, interesting(g, bits, signed), bits, signed)
        expr, shown = f"{lit} {g.pick(('<', '<=', '==', '!=', '>'))} {other}", "bool"
    c = g.name("K")
    if g.chance(0.5):
        g.item("const @c: @t = @e;", c=c, t=shown, e=expr)
    else:
        g.stmt("let @c: @t = std::hint::black_box(@e);", c=c, t=shown, e=expr)
    g.say(c)


def const_lengths(g: Gen) -> None:
    n, arr = g.name("N"), g.name("arr")
    expr = g.pick((f"{g.between(0, 7)}usize - {g.between(0, 8)}usize", f"{g.between(1, 4)}usize * {g.between(0, 5)}",
                   f"{g.between(0, 9)} / {g.between(0, 3)}", f"usize::MAX / {g.between(1, 4)} - usize::MAX / 5 + 1"))
    if "MAX" not in expr:
        g.item("const @n: usize = @e;", n=n, e=expr)
        g.stmt("let @a = [0u8; @n];", a=arr, n=n)
        g.say(f"{arr}.len()")
    p, s = g.name("P"), g.name("sum")
    g.item("struct @P<const K: usize>;", P=p)
    g.item("impl<const K: usize> @P<K> {\n    const X: usize = 100 / K;\n}", P=p)
    g.say(f"{p}::<{g.pick((1, 4, 7, 0) if g.bad() else (1, 4, 7))}>::X")
    g.item("fn @s<const K: usize>(a: [u8; K]) -> usize {\n    a.iter().map(|&x| x as usize).sum::<usize>() + K\n}", s=s)
    g.say(f"{s}([1, 2, 3])")


# --- borrows

def borrow_two_phase(g: Gen) -> None:
    v = g.name("v")
    g.stmt("let mut @v = vec![1usize, 2];\n@v.push(@v.len());\n@v.insert(0, @v[0] + @v.len());", v=v)
    if g.bad():
        g.stmt(g.pick(("@v.push({\n    let r = &mut @v;\n    r.len()\n});",
                       "let first = &@v[0];\n@v.push(1);\nprintln!(\"{}\", first);",
                       "@v.extend(@v.iter().copied());")), v=v)
    else:
        g.stmt(g.pick(("let last = @v[@v.len() - 1];\n@v.push(last);", "@v.truncate(@v.len() - 1);",
                       "@v.swap(0, @v.len() - 1);")), v=v)
    g.say(v, "{:?}")


def borrow_loops(g: Gen) -> None:
    names = dict(i=g.name("items"), l=g.name("longest"), a=g.name("acc"), s=g.name("stack"), r=g.name("refs"),
                 o=g.name("owned"))
    g.stmt("let @i = vec![String::from(\"a\"), String::from(\"bbb\"), String::from(\"cc\")];\nlet mut @l = &@i[0];\n"
           "for s in &@i {\n    if s.len() > @l.len() {\n        @l = s;\n    }\n}", **names)
    g.say(names["l"])
    g.stmt("let mut @a = String::new();\nfor n in 0..3 {\n    let s = n.to_string();\n    @a.push_str(&s);\n}", **names)
    g.say(names["a"])
    if g.chance(0.5):
        g.stmt("let mut @s = vec![1, 2, 3];\nwhile let Some(top) = @s.pop() {\n    if top == 2 {\n        @s.push(10);\n"
               "        @s.pop();\n    }\n    println!(\"{}\", top);\n}", **names)
    if g.bad():
        g.stmt(g.pick(("let mut @r: Vec<&String> = Vec::new();\nfor n in 0..3 {\n    let s = n.to_string();\n"
                       "    @r.push(&s);\n}\nprintln!(\"{}\", @r.len());",
                       "let @o = String::from(\"x\");\nfor _ in 0..2 {\n    drop(@o);\n}")), **names)


def borrow_match(g: Gen) -> None:
    names = dict(o=g.name("opt"), f=g.name("flag"), s=g.name("s"), t=g.name("t"), k=g.name("k"))
    g.stmt("let @o: Option<String> = Some(String::from(\"m\"));\nmatch @o {\n    Some(ref s) => println!(\"{}\", s),\n"
           "    None => {}\n}\nmatch &@o {\n    Some(s) => println!(\"{}\", s.len()),\n    None => {}\n}", **names)
    g.stmt("let @f = @o.is_some();\nlet @s = String::from(\"x\");\nlet @t = match @f {\n    true => @s,\n"
           "    false => String::new(),\n};", **names)
    g.say(names["t"])
    if g.bad():
        g.stmt(g.pick(("println!(\"{}\", @s);", "match @o {\n    Some(s) => drop(s),\n    None => {}\n}\n"
                                                "println!(\"{:?}\", @o);")), **names)
    else:
        g.say(names["o"], "{:?}")
    g.stmt("let @k = 3;\nlet label = match @k {\n    n if n > 2 => \"big\",\n    0 => \"zero\",\n    _ => \"small\",\n};",
           **names)
    g.say("label")


def borrow_nll(g: Gen) -> None:
    names = dict(x=g.name("x"), r=g.name("r"), y=g.name("y"), n=g.name("n"), c=g.name("c"))
    g.stmt("let mut @x = 5;\nlet @r = &mut @x;\n*@r += 1;", **names)
    if g.bad():
        g.stmt("let @y = &@x;\n*@r += 1;\nprintln!(\"{}\", @y);", **names)
    else:
        g.stmt("let @y = &@x;\nprintln!(\"{}\", @y);", **names)
    g.stmt("let mut @n = 0;\nlet mut @c = || @n += 1;\n@c();", **names)
    if g.bad():
        g.stmt("println!(\"{}\", @n);\n@c();", **names)
    else:
        g.stmt("@c();\nprintln!(\"{}\", @n);", **names)


def borrow_fields(g: Gen) -> None:
    names = dict(P=g.name("Pair"), m=g.name("bump"), v=g.name("pair"), c=g.name("c"))
    g.item("struct @P {\n    a: Vec<u8>,\n    b: u8,\n}", **names)
    g.item("impl @P {\n    fn @m(&mut self) {\n        self.b += 1;\n    }\n}", **names)
    g.stmt("let mut @v = @P { a: vec![1], b: 2 };\nlet ra = &mut @v.a;\nlet rb = &@v.b;\nra.push(*rb);\n"
           "let @c = || @v.b + 1;\n@v.a.push(3);\nprintln!(\"{}\", @c());", **names)
    if g.bad():
        g.stmt("let ra = &mut @v.a;\n@v.@m();\nra.push(1);", **names)
    else:
        g.stmt("@v.@m();", **names)
    g.say(f"{names['v']}.a.len() + {names['v']}.b as usize")


SNIPPETS: dict[Area, tuple[Callable[[Gen], None], ...]] = {
    "traits": (trait_impls, trait_auto, trait_sized, trait_assoc, trait_super, trait_overlap),
    "methods": (method_pick, method_deref, method_ambiguous, method_value_ref, method_shadowed_len),
    "coercions": (coerce_reborrow, coerce_unsize, coerce_never, coerce_fnptr, coerce_deref),
    "consts": (const_arith, const_arith, const_unary_cast, const_lengths),
    "borrows": (borrow_two_phase, borrow_loops, borrow_match, borrow_nll, borrow_fields),
}


def generate(seed: int, index: int, areas: Sequence[Area] = AREAS) -> tuple[Area, str]:
    """Program `index` of `seed`: its area and its text."""
    rng = random.Random(f"blaze-fuzz:{seed}:{index}")
    area = areas[index % len(areas)]
    g = Gen(rng)
    for _ in range(g.between(2, 4)):
        g.pick(SNIPPETS[area])(g)
    return area, g.text()


# --- shrinking

@dataclass(frozen=True)
class Token:
    start: int
    end: int
    kind: str
    text: str


RAW = re.compile(r'b?r(#*)"')
WORD = re.compile(r"[A-Za-z_][A-Za-z0-9_]*|[0-9][A-Za-z0-9_.]*")
CHAR = re.compile(r"'(?:\\(?:x[0-9a-fA-F]{2}|u\{[0-9a-fA-F]{1,6}\}|.)|[^\\'\n])'")
LIFETIME = re.compile(r"'[A-Za-z_][A-Za-z0-9_]*")
PAIRS = {"(": ")", "[": "]", "{": "}"}
CLOSERS = set(PAIRS.values())
# After a `{...}`, these go on with the same statement, item or element.
CONTINUES = frozenset({".", "?", ";", ",", "=", "+", "-", "*", "/", "%", "&", "|", "^", "<", ">", "as", "else", ")",
                       "]"})
type Edit = tuple[int, int, str]


def lex(text: str) -> list[Token]:
    """Rust's tokens as far as bracket structure needs them: comments are skipped; strings, raw
    strings, byte strings and characters are one token, so a bracket inside one is not one."""
    out: list[Token] = []
    i, n = 0, len(text)
    while i < n:
        c = text[i]
        if c.isspace():
            i += 1
            continue
        if text.startswith("//", i):
            j = text.find("\n", i)
            i = n if j < 0 else j
            continue
        if text.startswith("/*", i):
            depth, j = 1, i + 2
            while j < n and depth:
                if text.startswith("/*", j):
                    depth, j = depth + 1, j + 2
                elif text.startswith("*/", j):
                    depth, j = depth - 1, j + 2
                else:
                    j += 1
            if depth:
                raise ValueError(f"a block comment at {i} is not closed")
            i = j
            continue
        if raw := RAW.match(text, i):
            close = '"' + raw[1]
            j = text.find(close, raw.end())
            if j < 0:
                raise ValueError(f"a raw string at {i} is not closed")
            out.append(Token(i, j + len(close), "literal", text[i:j + len(close)]))
            i = j + len(close)
            continue
        if c == '"' or text.startswith('b"', i):
            j = i + (2 if c == "b" else 1)
            while j < n and text[j] != '"':
                j += 2 if text[j] == "\\" else 1
            if j >= n:
                raise ValueError(f"a string at {i} is not closed")
            out.append(Token(i, j + 1, "literal", text[i:j + 1]))
            i = j + 1
            continue
        if c == "'":
            m = CHAR.match(text, i) or LIFETIME.match(text, i)
            if m is None:
                raise ValueError(f"a quote at {i} is neither a character nor a lifetime")
            out.append(Token(i, m.end(), "literal", m[0]))
            i = m.end()
            continue
        if m := WORD.match(text, i):
            out.append(Token(i, m.end(), "word", m[0]))
            i = m.end()
            continue
        out.append(Token(i, i + 1, c if c in "()[]{};," else "punct", c))
        i += 1
    return out


def matching(tokens: Sequence[Token]) -> dict[int, int]:
    """Each bracket's partner, both ways; a ValueError when they do not balance."""
    stack: list[int] = []
    found: dict[int, int] = {}
    for i, t in enumerate(tokens):
        if t.kind in PAIRS:
            stack.append(i)
        elif t.kind in CLOSERS:
            if not stack or PAIRS[tokens[stack[-1]].kind] != t.kind:
                raise ValueError(f"`{t.text}` at {t.start} closes nothing it opened")
            j = stack.pop()
            found[j], found[i] = i, j
    if stack:
        raise ValueError(f"`{tokens[stack[-1]].text}` at {tokens[stack[-1]].start} is not closed")
    return found


def pieces(tokens: Sequence[Token], match: dict[int, int], lo: int, hi: int, sep: str) -> list[tuple[int, int]]:
    """The token ranges `[a, b)` that `tokens[lo:hi]` falls into: split after each `sep` at this
    depth, and after a `{...}` that the next token does not go on from (an item, a statement or
    an arm that ends in a block)."""
    found: list[tuple[int, int]] = []
    start = i = lo
    while i < hi:
        t = tokens[i]
        if t.kind in PAIRS:
            j = match[i]
            if t.kind == "{" and (j + 1 >= hi or tokens[j + 1].text not in CONTINUES):
                found.append((start, j + 1))
                start = j + 1
            i = j + 1
            continue
        if t.kind == sep:
            found.append((start, i + 1))
            start = i + 1
        i += 1
    if start < hi:
        found.append((start, hi))
    return found


def removal(text: str, start: int, end: int) -> Edit:
    """The edit deleting `text[start:end]`, with its line when nothing else is on it."""
    line_start = text.rfind("\n", 0, start) + 1
    line_end = text.find("\n", end)
    line_end = len(text) if line_end < 0 else line_end
    if not text[line_start:start].strip() and not text[end:line_end].strip():
        return line_start, min(line_end + 1, len(text)), ""
    return start, end, ""


def candidates(text: str, level: int) -> list[Edit]:
    """The edits of one pass: 0 deletes an item or a statement or empties a block; 1 deletes an
    element of a list; 2 deletes a line. A `//@` line is never deleted."""
    if level == 2:
        edits: list[Edit] = []
        at = 0
        for line in text.splitlines(keepends=True):
            if line.strip() and not line.startswith("//@"):
                edits.append((at, at + len(line), ""))
            at += len(line)
        return edits
    tokens = lex(text)
    match = matching(tokens)
    groups = [(i, match[i]) for i, t in enumerate(tokens) if t.kind in PAIRS]
    edits = []
    if level == 0:
        for lo, hi in [(0, len(tokens)), *((i + 1, j) for i, j in groups if tokens[i].kind == "{")]:
            spans = pieces(tokens, match, lo, hi, ";")
            edits += [removal(text, tokens[a].start, tokens[b - 1].end) for a, b in spans]
        edits += [(tokens[i].end, tokens[j].start, "") for i, j in groups
                  if tokens[i].kind == "{" and j > i + 1]
    else:
        for i, j in groups:
            spans = pieces(tokens, match, i + 1, j, ",")
            if len(spans) > 1:
                edits += [(tokens[a].start, tokens[b - 1].end, "") for a, b in spans]
    return edits


def applied(text: str, edits: Sequence[Edit]) -> str:
    """`text` with `edits`, which do not overlap, made."""
    for start, end, by in sorted(edits, key=lambda e: e[0], reverse=True):
        text = text[:start] + by + text[end:]
    return text


def disjoint(edits: Sequence[Edit]) -> list[Edit]:
    """The largest edits first, each that overlaps none taken before it."""
    taken: list[Edit] = []
    for edit in sorted(edits, key=lambda e: (e[0] - e[1], e[0])):
        if all(edit[1] <= s or e <= edit[0] for s, e, _ in taken):
            taken.append(edit)
    return taken


LEVELS = 3


def shrink(text: str, still: Callable[[list[str]], list[bool]], rounds: int) -> tuple[str, int]:
    """`text` made as small as the passes of `candidates` can while `still` holds of it (asked
    of a pass's candidates at once), and how many batches it took. A text the lexer cannot
    read is shrunk by lines alone."""
    level, used = 0, 0
    while used < rounds:
        try:
            edits = candidates(text, level)
        except ValueError:
            edits = candidates(text, 2) if level < 2 else []
            level = 2
        variants: dict[str, Edit] = {}
        for edit in edits:
            variant = applied(text, [edit])
            if variant != text and variant not in variants:
                variants[variant] = edit
        if not variants:
            if level + 1 >= LEVELS:
                break
            level += 1
            continue
        tried = list(variants)
        held = still(tried)
        used += 1
        good = [variants[v] for v, ok in zip(tried, held) if ok]
        if not good:
            if level + 1 >= LEVELS:
                break
            level += 1
            continue
        single = min((v for v, ok in zip(tried, held) if ok), key=lambda v: (len(v), v))
        together = disjoint(good)
        combined = applied(text, together) if len(together) > 1 else single
        if combined != single and used < rounds:
            used += 1
            text = combined if still([combined])[0] else single
        else:
            text = single
        level = 0
    return text, used


# --- comparing

def signature(c: cases.Compared) -> tuple[str, ...] | None:
    """What a mismatch is, as far as shrinking keeps it: its category and the keys it is about
    (spans and output move as text is deleted; the keys and what differs do not)."""
    if c.verdict != "fail":
        return None
    said = sorted(c.said.keys) if c.said else []
    rustc = sorted(c.truth.ref.keys)
    if c.category == "accepted what rustc rejects":
        return (c.category, *rustc)
    if c.category == "rejected what rustc accepts":
        return (c.category, *said)
    if c.category == "other codes":
        return (c.category, ",".join(rustc), ",".join(said))
    if c.category == "other spans" and c.said is not None:
        theirs = Counter(c.truth.ref.spans)
        ours = Counter(c.said.spans)
        return (c.category, *sorted({k for k, _ in (theirs - ours) + (ours - theirs)}))
    if c.category == "other run" and c.truth.run is not None and c.run is not None:
        return (c.category, run_aspect(c.truth.run, c.run))
    return (c.category,)


def run_aspect(theirs: cases.Run, ours: cases.Run) -> str:
    if ours.error is not None:
        return "did not finish"
    if ours.status != theirs.status:
        return "status"
    return "panics" if ours.panics != theirs.panics else "stdout"


def header_key(key: str) -> str:
    """A key as a case's header can hold it: a message's start up to any double quote."""
    return key.split('"', 1)[0].rstrip() if key.startswith(rejections.MESSAGE) else key


def case_text(text: str, truth: cases.Truth, note: str) -> str:
    """`text` (a generated program, its `//@` lines first) as a case: the header of what rustc
    does with it, then `note`."""
    keys = tuple(sorted({header_key(k) for k in truth.ref.keys}))
    expect = cases.Expect(not keys, keys)
    return f"//@ expect: {expect}\n// {note}\n{text}"


class Fuzzer:
    """Programs through `runner`, with each variant shrinking makes at a path named by its
    content (so rustc's answer on a variant tried before is the kept one)."""

    def __init__(self, runner: cases.Runner, work: Path):
        self.runner, self.dir = runner, work / "shrink"
        self.dir.mkdir(parents=True, exist_ok=True)

    def variant(self, text: str, like: rejections.Unit) -> cases.Case:
        sha = hashlib.sha256(text.encode()).hexdigest()[:16]
        path = self.dir / f"{sha}.rs"
        if not path.is_file():
            path.write_text(text)
        return cases.Case(rejections.Unit(f"shrink/{sha}", str(path), None, like.kind, like.edition, like.flags))

    def still(self, like: rejections.Unit, wanted: tuple[str, ...]) -> Callable[[list[str]], list[bool]]:
        def holds(texts: list[str]) -> list[bool]:
            found = self.runner.compare([self.variant(t, like) for t in texts])
            return [signature(c) == wanted for c in found]
        return holds


def unit_of(name: str, path: Path, text: str) -> rejections.Unit:
    found = rejections.classify(name, str(path), text, rejections.host_names(sys.platform, platform.machine()))
    if len(found.units) != 1:
        raise engine.Failed(f"{name}: a generated program is not one unit: {found}")
    return found.units[0]


def main(args: Input) -> None:
    for name, value, least in (("jobs", args.jobs, 1), ("batch", args.batch, 1), ("count", args.count, 1),
                               ("rounds", args.rounds, 0)):
        if value < least:
            fail(SCRIPT, f"{name}: at least {least}, not {value}")
    if not args.areas or len(set(args.areas)) != len(args.areas):
        fail(SCRIPT, f"areas: each at most once, and at least one, not {list(args.areas)}")
    try:
        eng = engine.Engine.of(args.engine)
    except engine.Failed as e:
        fail(SCRIPT, str(e))
    work = Path(args.work or tempfile.mkdtemp(prefix="bz-fuzz-", dir="/tmp")).resolve()
    into = Path(args.into).resolve() if args.into else INTO
    (work / "gen").mkdir(parents=True, exist_ok=True)
    print(f"work: {work}", flush=True)
    rep = report.Report(SCRIPT, args.engine, args.out)
    try:
        runner = cases.Runner(eng, work, args.jobs, args.batch, args.whole, args.cache, args.keep)
        programs: list[tuple[Area, cases.Case, str]] = []
        for i in range(args.count):
            area, text = generate(args.seed, i, args.areas)
            name = f"fuzz/{area}-s{args.seed}-p{i}"
            path = work / "gen" / f"{area}-s{args.seed}-p{i}.rs"
            path.write_text(text)
            programs.append((area, cases.Case(unit_of(name, path, text)), text))
        compared = runner.compare([case for _, case, _ in programs])
        fuzzer = Fuzzer(runner, work)
        tally: dict[str, Counter[str]] = {}
        for (area, case, text), c in zip(programs, compared):
            counts = tally.setdefault(area, Counter())
            counts["rustc rejects" if c.truth.ref.keys else "rustc accepts"] += 1
            counts[c.category] += 1
            if c.verdict != "fail":
                rep.add(case.name, c.verdict, c.detail)
                continue
            detail = c.detail if not c.package else f"{c.detail}\npackage: {c.package}"
            wanted = signature(c)
            if not args.shrink or wanted is None:
                rep.add(case.name, "fail", detail)
                continue
            small, used = shrink(text, fuzzer.still(case.unit, wanted), args.rounds)
            again = runner.compare([fuzzer.variant(small, case.unit)])[0]
            if signature(again) != wanted:
                small, again = text, c
            index = case.name.rsplit("-p", 1)[1]
            note = (f"fuzz.py: seed {args.seed}, program {index} ({area}), shrunk in {used} batches; "
                    f"{c.category}: {c.detail.splitlines()[0][:160]}")
            into.mkdir(parents=True, exist_ok=True)
            written = into / f"{area}-s{args.seed}-p{index}.rs"
            written.write_text(case_text(small, again.truth, note))
            rep.add(case.name, "fail", f"{detail}\nwritten as {written} ({small.count(chr(10))} lines, "
                                       f"from {text.count(chr(10))})", lines=small.count("\n"),
                    from_lines=text.count("\n"), batches=used)
    except engine.Failed as e:
        fail(SCRIPT, str(e))
    for ws, why in runner.stopped:
        rep.add(f"stopping the daemon of {ws}", "error", why)
    rep.extra.update(work=str(work), toolchain=engine.TOOLCHAIN, seed=args.seed, count=args.count,
                     rustc_runs=runner.rustc_runs, rustc_kept=runner.rustc_kept,
                     areas={area: dict(counts) for area, counts in sorted(tally.items())})
    rep.finish()


def test_deterministic() -> None:
    for i in range(30):
        assert generate(3, i) == generate(3, i), i
    assert sum(generate(3, i) != generate(4, i) for i in range(30)) >= 25, "two seeds give mostly the same programs"
    assert [generate(0, i)[0] for i in range(10)] == [*AREAS, *AREAS]
    assert {generate(0, i, ("consts",))[0] for i in range(5)} == {"consts"}


ITEM_NAME = re.compile(r"^(?:pub )?(?:const fn|fn|struct|trait|enum|const|static|mod) (\w+)", re.M)


def test_programs() -> None:
    """Every program generated of a few seeds is one Rust program as far as the shrinker reads
    it: brackets balance, `main` is there, no two items share a name."""
    seen: Counter[str] = Counter()
    for seed in range(4):
        for i in range(60):
            area, text = generate(seed, i)
            seen[area] += 1
            where = f"seed {seed} program {i}"
            assert text.startswith(f"//@ edition: {EDITION}\n"), where
            assert "\nfn main() {\n" in text and text.endswith("}\n"), where
            matching(lex(text))
            top = [m[1] for m in ITEM_NAME.finditer(text.split("\nfn main() {\n")[0])]
            assert len(top) == len(set(top)), (where, sorted(n for n, k in Counter(top).items() if k > 1))
            assert "@" not in text.replace("@ edition", ""), (where, "a placeholder was not filled")
            found = rejections.classify("t.rs", "/t.rs", text, rejections.host_names("linux", "x86_64"))
            assert len(found.units) == 1 and found.units[0].edition == EDITION, (where, found)
    assert set(seen) == set(AREAS), seen


def test_lex() -> None:
    text = ('fn f<\'a>(x: &\'a str) -> char { let s = "{(["; let r = r#"}"#; let b = b"]"; let c = \'{\';\n'
            "// } a comment\n/* { /* nested } */ ) */ let d = '\\''; let e = b'}'; [s, r] }\n")
    tokens = lex(text)
    assert [t.kind for t in tokens].count("{") == 1 and [t.kind for t in tokens].count("}") == 1, tokens
    assert {"\"{([\"", "r#\"}\"#", "b\"]\"", "'{'", "'\\''", "'a"} <= {t.text for t in tokens}
    match = matching(tokens)
    braces = [i for i, t in enumerate(tokens) if t.kind == "{"]
    assert tokens[match[braces[0]]].kind == "}"
    for bad in ("fn f() {", "fn f() }", "fn f() { ( }", 'let s = "open;', "/* open"):
        try:
            matching(lex(bad))
        except ValueError:
            continue
        raise AssertionError(f"{bad!r} reads as balanced")


PROGRAM = """//@ edition: 2021

struct Unused;

fn helper(a: u8, b: u8) -> u8 {
    a + b
}

fn main() {
    let x = helper(1, 2);
    let y = [1, 2, 3];
    if x > 2 {
        println!("big");
    } else {
        println!("small");
    }
    let bug = BUG(x, y);
    println!("{}", x);
}
"""


def test_shrink() -> None:
    def still(texts: list[str]) -> list[bool]:
        out: list[bool] = []
        for text in texts:
            try:
                matching(lex(text))
                out.append("BUG(" in text and "fn main" in text)
            except ValueError:
                out.append(False)
        return out

    small, used = shrink(PROGRAM, still, 100)
    assert "BUG(" in small and "fn main" in small and small.startswith("//@ edition: 2021\n"), small
    for gone in ("Unused", "helper", "println", "if x", "[1, 2, 3]"):
        assert gone not in small, (gone, small)
    assert len(small) < len(PROGRAM) // 3, small
    assert 0 < used <= 100
    assert shrink(PROGRAM, lambda texts: [False] * len(texts), 100)[0] == PROGRAM
    capped, used = shrink(PROGRAM, still, 1)
    assert used == 1 and len(capped) < len(PROGRAM), "one batch is all `rounds` 1 allows"
    lines = shrink("//@ edition: 2021\nfn main() { \"open\nBUG(\n", lambda ts: ["BUG(" in t for t in ts], 20)[0]
    assert lines == "//@ edition: 2021\nBUG(\n", f"a text that does not lex is shrunk by lines: {lines!r}"


def test_pieces() -> None:
    text = "struct A;\nimpl A { fn f(&self) {} }\nfn main() { let a = A { }; if true { } else { } a.f(); match 1 { 0 => {} _ => {} } }\n"
    tokens = lex(text)
    match = matching(tokens)
    top = [text[tokens[a].start:tokens[b - 1].end] for a, b in pieces(tokens, match, 0, len(tokens), ";")]
    assert top == ["struct A;", "impl A { fn f(&self) {} }", text.splitlines()[2]], top
    body_open = next(i for i, t in enumerate(tokens) if t.text == "main") + 3
    body = [text[tokens[a].start:tokens[b - 1].end] for a, b in pieces(tokens, match, body_open + 1, match[body_open], ";")]
    assert body == ["let a = A { };", "if true { } else { }", "a.f();", "match 1 { 0 => {} _ => {} }"], body
    arms_open = max(i for i, t in enumerate(tokens) if t.kind == "{" and tokens[i - 1].text == "1")
    arms = [text[tokens[a].start:tokens[b - 1].end] for a, b in pieces(tokens, match, arms_open + 1, match[arms_open], ",")]
    assert arms == ["0 => {}", "_ => {}"], arms
    assert removal("a\n    let x = 1;\nb\n", 6, 16) == (2, 17, ""), "a statement alone on its line goes with it"
    assert removal("a; b;\n", 3, 5) == (3, 5, "")
    assert disjoint([(0, 10, ""), (2, 4, ""), (10, 12, ""), (11, 20, "")]) == [(11, 20, ""), (0, 10, "")]


def fake(category: cases.Category, rustc: Sequence[str] = (), blaze: Sequence[str] = (),
         rustc_spans: rejections.Placed = (), blaze_spans: rejections.Placed = (),
         theirs: cases.Run | None = None, ours: cases.Run | None = None) -> cases.Compared:
    unit = rejections.Unit("fuzz/x", "/x.rs", None, "bin", "2021")
    ref = rejections.Reference(frozenset(rustc), spans=rustc_spans) if rustc else rejections.Reference(
        out=rejections.ACCEPTED)
    said = rejections.Said(frozenset(blaze), spans=blaze_spans)
    return cases.Compared(cases.Case(unit), cases.Truth(ref, theirs), "fail", category, "d", said, ours)


def test_signature() -> None:
    assert signature(fake("accepted what rustc rejects", ["E0277"])) == ("accepted what rustc rejects", "E0277")
    assert signature(fake("rejected what rustc accepts", [], ["E0599"])) == ("rejected what rustc accepts", "E0599")
    assert signature(fake("other codes", ["E0277"], ["E0308", "E0599"])) == ("other codes", "E0277", "E0308,E0599")
    at = rejections.Span(rejections.ROOT_FILE, 1, 1, 1, 2)
    moved = rejections.Span(rejections.ROOT_FILE, 2, 1, 2, 2)
    spans = fake("other spans", ["E0277", "E0308"], ["E0277", "E0308"], (("E0277", at), ("E0308", at)),
                 (("E0277", at), ("E0308", moved)))
    assert signature(spans) == ("other spans", "E0308"), signature(spans)
    run = fake("other run", theirs=cases.Run(0, "1\n"), ours=cases.Run(0, "2\n"))
    assert signature(run) == ("other run", "stdout")
    assert signature(fake("other run", theirs=cases.Run(0, ""), ours=cases.Run(101, ""))) == ("other run", "status")
    passed = cases.Compared(run.case, run.truth, "pass", "same", "")
    assert signature(passed) is None


def test_case_text() -> None:
    _, program = generate(0, 0)
    truth = cases.Truth(rejections.Reference(frozenset({"E0277", rejections.MESSAGE + 'a "quoted" `_` message'})))
    text = case_text(program, truth, "fuzz.py: seed 0, program 0")
    first, note, rest = text.split("\n", 2)
    expect = cases.expect_of(first)
    assert isinstance(expect, cases.Expect) and not expect.accept, (first, expect)
    assert rejections.header_holds(expect.tokens, truth.ref.keys), (expect, truth.ref.keys)
    assert note.startswith("// fuzz.py") and rest == program
    found = cases.case_of("fuzz/a.rs", Path("/c/fuzz/a.rs"), text, rejections.host_names("linux", "x86_64"))
    assert isinstance(found, cases.Case) and found.unit.edition == EDITION, found
    accepted = case_text(program, cases.Truth(rejections.Reference(out=rejections.ACCEPTED)), "n")
    assert accepted.startswith("//@ expect: accept\n")


SELFTESTS = (test_deterministic, test_programs, test_lex, test_pieces, test_shrink, test_signature, test_case_text)


def selftest() -> int:
    failed = 0
    for test in SELFTESTS:
        try:
            test()
            print(f"ok    {test.__name__}", flush=True)
        except (Exception, SystemExit) as e:
            failed += 1
            print(f"FAIL  {test.__name__}: {type(e).__name__}: {e}", flush=True)
    print(f"{SCRIPT} selftest: {len(SELFTESTS) - failed} of {len(SELFTESTS)} pass", flush=True)
    return 1 if failed else 0


if __name__ == "__main__":
    if sys.argv[1:] == ["selftest"]:
        sys.exit(selftest())
    if len(sys.argv) != 2:
        sys.exit(f"usage: {SCRIPT} '<json>' | selftest\n\n{__doc__}")
    main(parse(Input, sys.argv[1], SCRIPT))
