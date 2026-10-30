#!/usr/bin/env python3
"""store.py: generated programs and changed asm! bodies against an empty and a warm store.

usage: store.py [--backends cranelift,llvm]

Issue #418: smoke-1/c16 differed only with BLAZE_CACHE set. Two asm! wrappers can have
identical Cranelift IR but different assembly, so LLVM must key its code by the assembly too
(2a3a0a35). Compile one body, change only its assembly, then read each body back from the
store. The smoke programs (`runs.smoke_programs`) are built three times: with no store, into a
fresh store and from the store that filled. Every build with a store has to run as the one
without does, and leave no function out.
BLAZE_BIN names the compiler under test. A failure keeps its work and logs.
"""
import argparse, os, shutil, tempfile

from runs import BACKENDS, Project, differences, smoke_programs
import gen

ASM = """    #[cfg(target_arch = "x86_64")]
    #[inline(never)]
    fn transform(mut x: u64, mut y: u64) -> u64 {
        unsafe { std::arch::asm!(X86, x = inout(reg) x, y = inout(reg) y => _, options(pure, nomem, nostack)); }
        x
    }
    #[cfg(target_arch = "aarch64")]
    #[inline(never)]
    fn transform(mut x: u64, mut y: u64) -> u64 {
        unsafe { std::arch::asm!(ARM, x = inout(reg) x, y = inout(reg) y => _, options(pure, nomem, nostack)); }
        x
    }
    pub fn run() {
        for (x, y) in [(16u64, 55u64), (u64::MAX, 1), (0, 0), (0x8000_0000_0000_0000, 16)] {
            feed(transform(black_box(x), black_box(y)));
        }
    }
"""
ASSEMBLY = {
    "add": ('"add {x}, {y}"', '"add {x}, {x}, {y}"'),
    "negate": ('"neg {x}", "not {y}", "add {x}, {y}"',
               '"neg {x}, {x}", "mvn {y}, {y}", "add {x}, {x}, {y}"'),
}


def held(project, what, text, edition, backend, plain, store):
    """Builds `text` with the store and holds its run to `plain`, the run of the build without one."""
    got = project.build(text, edition, backend, store)
    findings = differences(plain, got)
    left = project.left()
    if findings or left:
        raise SystemExit(f"{what} ({backend}):\n" + "\n".join(f.detail for f in findings)
                         + "".join(f"\nleft out: {it}" for it in left[:10]))


def unstored(project, what, text, edition, backend):
    """The run of the build of `text` without a store, which has to exit 0."""
    plain = project.build(text, edition, backend)
    if plain is None:
        raise SystemExit(f"{what}: cargo turned the source down when asked for its link line")
    if plain[0] != 0:
        raise SystemExit(f"{what} ({backend}) without a store: status {plain[0]}\n{plain[2][-2000:]}")
    return plain


def asm_changes(work, backend):
    """One IR with two different asm! bodies, each compiled and restored in a new process."""
    store = os.path.join(work, "asm-store")
    project = Project(os.path.join(work, "asm"))
    for step, body in enumerate(("add", "negate", "add", "negate")):
        x86, arm = ASSEMBLY[body]
        text = gen.render([("c16", ASM.replace("X86", x86).replace("ARM", arm).splitlines())])
        what = f"asm {step} {body}"
        plain = unstored(project, what, text, "2021", backend)
        held(project, what, text, "2021", backend, plain, store)
        shutil.copyfile(os.path.join(project.fx.work, "fz.stderr"),
                        os.path.join(work, f"asm-{step}-{body}.stderr"))
        print(f"MATCH asm {step} {body} ({backend})", flush=True)
    shutil.rmtree(project.fx.work)


def smoke(work, backend):
    """The smoke programs with no store, into an empty one, and from the one that filled."""
    store = os.path.join(work, "smoke-store")
    project = Project(os.path.join(work, "smoke"))
    for seed, text, edition in smoke_programs():
        plain = unstored(project, seed, text, edition, backend)
        for phase in ("cold", "warm"):
            held(project, f"{seed} {phase}", text, edition, backend, plain, store)
            print(f"MATCH {seed} {phase} ({backend})", flush=True)
    shutil.rmtree(project.fx.work)


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--backends", default="cranelift,llvm")
    args = parser.parse_args()
    backends = args.backends.split(",")
    for backend in backends:
        if backend not in BACKENDS:
            parser.error(f"unknown backend: {backend}")
    work = tempfile.mkdtemp(prefix="blaze-fuzz-store-")
    print(f"work: {work}", flush=True)
    for backend in backends:
        own = os.path.join(work, backend)
        os.environ["BLAZE_HOME"] = os.path.join(own, "home")
        asm_changes(own, backend)
        smoke(own, backend)
    shutil.rmtree(work)


if __name__ == "__main__":
    main()
