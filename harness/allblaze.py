#!/usr/bin/env python3
"""allblaze.py: programs with nothing rustc built in them (docs/no-rustc.md, steps F and G).

Eight programs, a hello, one that uses collections, `catch_unwind`, threads, channels,
formatting, a file and an uncaught panic, one with a `#[global_allocator]` of its own, one
built with `panic = "abort"`, one with a static of proc-macro clients as J2 writes it, one
of 128-bit atomics raced by threads, one of the paths of `split_paths_ref` and the XDG
directories, and one of the FMA wrappers of `core::arch::x86_64` on NaNs, are built by blaze with every crate its own, the standard library included,
and linked by blaze (`--exe`): the native libraries are the ones the crates' `#[link]`s ask
for, and nothing it could not compile may be left in the executable. A shared library is
linked too (`--exe` with `--shared`), loaded with `dlopen` and called as cargo's `cdylib` of
it is, and a proc-macro crate's, whose list of macros blaze writes into it is read off the
library.
Each is built three ways:

  - whole: the program and the standard library in one build, one object;
  - over the unit: the standard library built once, alone, into an object of its own
    (`std.o`), and each program built over it with `--prebuilt`;
  - stored: each program built alone on a store that starts empty and a remote (a directory)
    that does too. The first build compiles the standard library as a unit, keeps it in the
    store and pushes it; the others take it from the store. Last, a build on a second store
    that starts empty takes it from the remote.

What each prints, and how it exits, has to be the same however it was built: every way, and
every build of a program a way makes, is held to the first build of it, thread ids aside. What
the shared library answers is what its source says (`ANSWERS`).

    python3 allblaze.py            # every way
    python3 allblaze.py unit stored
"""
import json, os, re, shutil, subprocess, sys, time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from mixed import BLAZE
from storelib import scratch

# The crates of the standard library a program on Linux links, by the names cargo gives them,
# but the panic runtime.
STD = ["std", "core", "alloc", "compiler_builtins", "unwind", "libc", "cfg_if", "hashbrown",
       "rustc_demangle", "std_detect", "addr2line", "gimli", "object", "miniz_oxide", "adler2",
       "memchr"]


def std_of(panic):
    return STD + ["panic_abort" if panic == "abort" else "panic_unwind"]


HELLO = """fn main() {
    let v: Vec<u32> = (1..=10).collect();
    println!("hello {} {:?}", v.iter().sum::<u32>(), std::env::args().count());
}
"""
RICH = r"""use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::io::Write as _;
use std::sync::{Arc, Mutex, mpsc};

#[derive(Debug, Clone, PartialEq)]
enum Shape { Circle(f64), Rect { w: f64, h: f64 } }

trait Area { fn area(&self) -> f64; }
impl Area for Shape {
    fn area(&self) -> f64 {
        match self { Shape::Circle(r) => std::f64::consts::PI * r * r, Shape::Rect { w, h } => w * h }
    }
}

fn main() {
    let mut words = HashMap::new();
    for w in "the quick brown fox jumps over the lazy dog the end".split(' ') {
        *words.entry(w).or_insert(0usize) += 1;
    }
    let sorted: BTreeMap<_, _> = words.iter().collect();
    println!("words {:?}", sorted);
    let shapes: Vec<Box<dyn Area>> = vec![Box::new(Shape::Circle(1.5)), Box::new(Shape::Rect { w: 2.0, h: 3.25 })];
    let total: f64 = shapes.iter().map(|s| s.area()).sum();
    println!("area {total:.6} {:e} {}", total, 1.0f64 / 3.0);
    let caught = std::panic::catch_unwind(|| {
        let v: Vec<i32> = Vec::new();
        v[3]
    });
    println!("caught {}", caught.is_err());
    let r = std::panic::catch_unwind(|| -> i32 { panic!("boom {}", 7) });
    let payload = r.unwrap_err();
    let msg = payload.downcast_ref::<String>().cloned()
        .or_else(|| payload.downcast_ref::<&str>().map(|s| s.to_string())).unwrap_or_default();
    println!("msg {msg}");
    let counter = Arc::new(Mutex::new(0u64));
    let (tx, rx) = mpsc::channel();
    let handles: Vec<_> = (0..4u64).map(|i| {
        let counter = Arc::clone(&counter);
        let tx = tx.clone();
        std::thread::spawn(move || {
            for j in 0..1000 { *counter.lock().unwrap() += i * j; }
            tx.send(i).unwrap();
        })
    }).collect();
    drop(tx);
    for h in handles { h.join().unwrap(); }
    let mut got: Vec<u64> = rx.iter().collect();
    got.sort();
    println!("threads {} {:?}", *counter.lock().unwrap(), got);
    let mut s = String::new();
    write!(s, "{:>8}|{:<6}|{:^7}|{:#x}|{:08.3}", "right", "left", "mid", 255, -3.14159).unwrap();
    println!("{s}");
    let text = std::fs::read_to_string("Cargo.toml").unwrap();
    println!("lines {}", text.lines().count());
    println!("parsed {:?} {:?}", "42".parse::<i64>(), "x".parse::<u8>().is_err());
    std::io::stdout().flush().unwrap();
    if std::env::args().count() > 1 { let none: Option<u8> = None; none.expect("asked to fail"); }
}
"""
ALLOC = r"""use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

struct Counting;
static ALLOCS: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) { unsafe { System.dealloc(ptr, layout) } }
}

#[global_allocator]
static A: Counting = Counting;

fn main() {
    let before = ALLOCS.load(Ordering::Relaxed);
    let v: Vec<Box<u64>> = (0..100).map(Box::new).collect();
    let s: String = v.iter().map(|b| b.to_string()).collect();
    println!("{} {} counted {}", s.len(), v.len(), ALLOCS.load(Ordering::Relaxed) - before >= 101);
}
"""
ABORT = r"""fn main() {
    let caught = std::panic::catch_unwind(|| 1);
    println!("caught {:?}", caught.ok());
    if std::env::args().count() > 1 { panic!("aborting {}", 3); }
}
"""
# What docs/no-rustc.md's J2 writes into a proc-macro crate: a static of each macro's client,
# which reaches `proc_macro`'s own statics (its symbol interner's thread-local) through the
# bridge it runs. The standard library's unit leaves `proc_macro` out, so the program has it.
MACROS = r"""#![feature(proc_macro_internals)]
#![allow(internal_features)]
extern crate proc_macro;

use proc_macro::TokenStream;
use proc_macro::bridge::client::Client;

fn derive_it(input: TokenStream) -> TokenStream { input }

fn attr_it(_attr: TokenStream, item: TokenStream) -> TokenStream { item }

#[unsafe(no_mangle)]
pub static ALLBLAZE_MACROS: [Client; 2] = [Client::expand1(derive_it), Client::expand2(attr_it)];

fn main() {
    let words = std::hint::black_box(&ALLBLAZE_MACROS) as *const _ as *const usize;
    let (first, second) = unsafe { (*words, *words.add(1)) };
    println!("clients {} {}", first != 0 && second != 0, first != second);
}
"""
# 128-bit atomics, raced by threads across the carry out of the low half: on AArch64 Linux,
# compare-and-swaps of compiler_builtins.
ATOMICS = r"""#![cfg_attr(target_has_atomic = "128", feature(integer_atomics))]
#[cfg(target_has_atomic = "128")]
fn atomics() {
    use std::sync::atomic::{AtomicI128, AtomicU128, Ordering::*};
    use std::sync::Arc;
    let sum = Arc::new(AtomicU128::new(u64::MAX as u128 - 3));
    let bits = Arc::new(AtomicU128::new(0));
    let max = Arc::new(AtomicI128::new(i128::MIN));
    let min = Arc::new(AtomicI128::new(i128::MAX));
    let umax = Arc::new(AtomicU128::new(0));
    let counted = Arc::new(AtomicU128::new(u64::MAX as u128));
    let threads: Vec<_> = (0..8u32).map(|t| {
        let (sum, bits, max, min) = (sum.clone(), bits.clone(), max.clone(), min.clone());
        let (umax, counted) = (umax.clone(), counted.clone());
        std::thread::spawn(move || {
            for i in 0..2000u128 {
                sum.fetch_add((1 << 64) | 3, SeqCst);
                sum.fetch_sub(1 << 63, AcqRel);
                bits.fetch_or(1 << (t * 15 + (i % 15) as u32), Relaxed);
                bits.fetch_xor(1 << 127, Release);
                let signed = ((i as i128) << 70) - ((t as i128) << 100);
                max.fetch_max(signed, Acquire);
                min.fetch_min(signed, SeqCst);
                umax.fetch_max(u128::MAX - i - (t as u128) * 7, SeqCst);
                let mut seen = counted.load(Relaxed);
                loop {
                    match counted.compare_exchange_weak(seen, seen + 1, SeqCst, Relaxed) {
                        Ok(_) => break,
                        Err(now) => seen = now,
                    }
                }
            }
        })
    }).collect();
    for t in threads { t.join().unwrap(); }
    println!("sum {:#x} bits {:#x}", sum.load(SeqCst), bits.load(SeqCst));
    println!("max {:#x} min {:#x} umax {:#x}", max.load(SeqCst), min.load(SeqCst), umax.load(SeqCst));
    println!("counted {:#x}", counted.load(SeqCst));
    let one = AtomicU128::new(u128::MAX - 1);
    println!("swap {:#x} {:#x}", one.swap(1 << 100, SeqCst), one.load(Acquire));
    one.store(0x1234_5678_9abc_def0_0fed_cba9_8765_4321, Release);
    println!("and {:#x} nand {:#x}", one.fetch_and(!(u64::MAX as u128), SeqCst), one.fetch_nand(u128::MAX >> 4, SeqCst));
    println!("now {:#x}", one.load(SeqCst));
    println!("cas {:?} {:?}", one.compare_exchange(5, 6, SeqCst, SeqCst), one.compare_exchange(one.load(SeqCst), 7, SeqCst, Acquire));
    let signed = AtomicI128::new(-1);
    println!("signed {} {} {}", signed.fetch_add(i128::MIN + 1, SeqCst), signed.fetch_min(0, SeqCst), signed.load(SeqCst));
}

#[cfg(not(target_has_atomic = "128"))]
fn atomics() {}

fn main() {
    atomics();
}
"""
# The paths of `split_paths_ref` and the XDG directories, whose iterator is a type alias of
# `impl Trait` that a function inside another defines.
PATHS = r"""#![feature(env_split_paths_ref, fs_home_dirs)]
use std::ffi::OsString;
use std::fs::HomeDirs;
use std::os::unix::fs::HomeDirsExt;
use std::path::Path;

fn main() {
    let unparsed = "/usr/bin::relative/x:/opt/a b/:/";
    let paths = std::env::split_paths_ref(unparsed).unwrap();
    println!("hint {:?}", paths.size_hint());
    println!("paths {:?} {:?}", paths.collect::<Vec<&Path>>(), std::env::split_paths_ref("").map(|it| it.count()));
    unsafe {
        std::env::set_var("HOME", "/home/someone");
        std::env::set_var("XDG_CONFIG_HOME", "relative");
        std::env::set_var("XDG_CONFIG_DIRS", "/etc/one:rel:/etc/two/");
        std::env::set_var("XDG_DATA_DIRS", "nothing:absolute");
        std::env::remove_var("XDG_CACHE_HOME");
    }
    let mut dirs = HomeDirs::xdg().unwrap();
    println!("config {:?} cache {:?}", dirs.config_home(), dirs.cache_home());
    println!("config dirs {:?}", dirs.config_dirs().map(|it| it.collect::<Vec<_>>()));
    println!("data dirs {:?}", dirs.data_dirs().map(|it| it.collect::<Vec<_>>()));
    dirs.set_config_dirs(OsString::from("/x:/y/z/"));
    dirs.set_data_dirs(OsString::from("/only"));
    println!("set {:?} {:?}", dirs.config_dirs().map(|it| it.collect::<Vec<_>>()), dirs.data_dirs().map(|it| it.count()));
    println!("debug {:?} {:?}", dirs.config_dirs(), std::env::split_paths_ref(unparsed));
}
"""
# The FMA wrappers of `core::arch::x86_64`, whose NaNs are what LLVM folds into each by the
# code around it: those of blaze's standard library, held alike each way the program is built.
FMA = r"""//! `_mm_fmadd_ps` and `_mm_fmadd_sd`, in a function with FMA, are LLVM's `vfmadd213ps` and
//! `vfmadd213sd` with the factors the other way round from libm's `fmaf` and `fma`, which
//! blaze called lane by lane: of two NaN factors rustc's kept the first and blaze's the
//! second. At `-O0` LLVM also folds the negations around such a call into it, or not, by the
//! code after it in its block (`_mm_fnmadd_ss` is one `vfnmadd213ss`, `_mm_fmsub_ps` a
//! `vpxor` and a `vfmadd213ps`), and a negation it folds keeps a NaN's sign where one it does
//! not flips it. Each FMA wrapper of `f32` and `f64` here, on lanes of NaNs of either sign,
//! quiet and signaling, and of numbers, under masks of every kind, hashed. Built as a program
//! of its own, the standard library blaze's, the wrappers are blaze's.
#![cfg_attr(not(target_arch = "x86_64"), allow(dead_code))]

#[cfg(target_arch = "x86_64")]
pub fn run() -> Vec<(&'static str, u64)> {
    if !(is_x86_feature_detected!("fma")
        && is_x86_feature_detected!("avx512f")
        && is_x86_feature_detected!("avx512vl"))
    {
        return Vec::new();
    }
    unsafe { fused() }
}

const ROUNDS: u64 = 64;

/// Which of the ten lanes of the pool, by a hash of the round, the operand and the lane.
fn pick(round: u64, operand: u64, lane: u64) -> usize {
    let mut z = (round << 16 | operand << 8 | lane).wrapping_add(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    ((z ^ (z >> 31)) % 10) as usize
}

fn lanes_f32<const N: usize>(round: u64, operand: u64) -> [u32; N] {
    let pool = [
        0x7fc0_0001 + operand as u32,
        0xffc0_0011 + operand as u32,
        0x7f80_0021 + operand as u32,
        0xff80_0031 + operand as u32,
        0x3fc0_0000,
        0xc000_0000,
        0x3e80_0000,
        0x7f80_0000,
        0x8000_0000,
        0x4040_0000,
    ];
    std::array::from_fn(|i| pool[pick(round, operand, i as u64)])
}

fn lanes_f64<const N: usize>(round: u64, operand: u64) -> [u64; N] {
    let pool = [
        0x7ff8_0000_0000_0001 + operand,
        0xfff8_0000_0000_0011 + operand,
        0x7ff0_0000_0000_0021 + operand,
        0xfff0_0000_0000_0031 + operand,
        0x3ff8_0000_0000_0000,
        0xc000_0000_0000_0000,
        0x3fd0_0000_0000_0000,
        0x7ff0_0000_0000_0000,
        0x8000_0000_0000_0000,
        0x4008_0000_0000_0000,
    ];
    std::array::from_fn(|i| pool[pick(round, operand, i as u64)])
}

fn mask(round: u64) -> u64 {
    match round % 4 {
        0 => !0,
        1 => 0,
        2 => 0x5555_5555_5555_5555 >> (round % 3),
        _ => (round + 1).wrapping_mul(0x2545_f491_4f6c_dd1d) >> 7,
    }
}

/// An operand of a wrapper, made for a round, and a result's bits.
trait Arg: Sized {
    fn make(round: u64, operand: u64) -> Self;
    fn bits(self) -> Vec<u64>;
}

macro_rules! vectors {
    ($($ty:ident: $lanes:ident, $bits:ty, $n:literal;)*) => {$(
        #[cfg(target_arch = "x86_64")]
        impl Arg for std::arch::x86_64::$ty {
            fn make(round: u64, operand: u64) -> Self {
                unsafe { std::mem::transmute($lanes::<$n>(round, operand)) }
            }
            fn bits(self) -> Vec<u64> {
                unsafe { std::mem::transmute::<Self, [$bits; $n]>(self) }.map(u64::from).to_vec()
            }
        }
    )*};
}

vectors! {
    __m128: lanes_f32, u32, 4;
    __m256: lanes_f32, u32, 8;
    __m512: lanes_f32, u32, 16;
    __m128d: lanes_f64, u64, 2;
    __m256d: lanes_f64, u64, 4;
    __m512d: lanes_f64, u64, 8;
}

macro_rules! masks {
    ($($ty:ty),*) => {$(
        impl Arg for $ty {
            fn make(round: u64, _: u64) -> Self {
                mask(round) as $ty
            }
            fn bits(self) -> Vec<u64> {
                vec![self.into()]
            }
        }
    )*};
}

masks!(u8, u16);

fn hash(h: u64, lanes: &[u64]) -> u64 {
    lanes.iter().fold(h, |h, &it| (h ^ it).wrapping_mul(0x0100_0000_01b3).rotate_left(17))
}

/// Each wrapper, by the positions of its operands, on each round.
macro_rules! rows {
    ($($name:ident($($at:literal),*),)*) => {
        vec![$((
            stringify!($name),
            (0..ROUNDS).fold(0, |h, round| hash(h, &$name($(Arg::make(round, $at)),*).bits())),
        )),*]
    };
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "fma,avx512f,avx512vl")]
unsafe fn fused() -> Vec<(&'static str, u64)> {
    use std::arch::x86_64::*;
    rows! {
        _mm_fmadd_pd(0, 1, 2),
        _mm256_fmadd_pd(0, 1, 2),
        _mm_fmadd_ps(0, 1, 2),
        _mm256_fmadd_ps(0, 1, 2),
        _mm_fmadd_sd(0, 1, 2),
        _mm_fmadd_ss(0, 1, 2),
        _mm_fmaddsub_pd(0, 1, 2),
        _mm256_fmaddsub_pd(0, 1, 2),
        _mm_fmaddsub_ps(0, 1, 2),
        _mm256_fmaddsub_ps(0, 1, 2),
        _mm_fmsub_pd(0, 1, 2),
        _mm256_fmsub_pd(0, 1, 2),
        _mm_fmsub_ps(0, 1, 2),
        _mm256_fmsub_ps(0, 1, 2),
        _mm_fmsub_sd(0, 1, 2),
        _mm_fmsub_ss(0, 1, 2),
        _mm_fmsubadd_pd(0, 1, 2),
        _mm256_fmsubadd_pd(0, 1, 2),
        _mm_fmsubadd_ps(0, 1, 2),
        _mm256_fmsubadd_ps(0, 1, 2),
        _mm_fnmadd_pd(0, 1, 2),
        _mm256_fnmadd_pd(0, 1, 2),
        _mm_fnmadd_ps(0, 1, 2),
        _mm256_fnmadd_ps(0, 1, 2),
        _mm_fnmadd_sd(0, 1, 2),
        _mm_fnmadd_ss(0, 1, 2),
        _mm_fnmsub_pd(0, 1, 2),
        _mm256_fnmsub_pd(0, 1, 2),
        _mm_fnmsub_ps(0, 1, 2),
        _mm256_fnmsub_ps(0, 1, 2),
        _mm_fnmsub_sd(0, 1, 2),
        _mm_fnmsub_ss(0, 1, 2),
        _mm512_fmadd_ps(0, 1, 2),
        _mm512_mask_fmadd_ps(0, 1, 2, 3),
        _mm512_maskz_fmadd_ps(0, 1, 2, 3),
        _mm512_mask3_fmadd_ps(0, 1, 2, 3),
        _mm256_mask_fmadd_ps(0, 1, 2, 3),
        _mm256_maskz_fmadd_ps(0, 1, 2, 3),
        _mm256_mask3_fmadd_ps(0, 1, 2, 3),
        _mm_mask_fmadd_ps(0, 1, 2, 3),
        _mm_maskz_fmadd_ps(0, 1, 2, 3),
        _mm_mask3_fmadd_ps(0, 1, 2, 3),
        _mm512_fmadd_pd(0, 1, 2),
        _mm512_mask_fmadd_pd(0, 1, 2, 3),
        _mm512_maskz_fmadd_pd(0, 1, 2, 3),
        _mm512_mask3_fmadd_pd(0, 1, 2, 3),
        _mm256_mask_fmadd_pd(0, 1, 2, 3),
        _mm256_maskz_fmadd_pd(0, 1, 2, 3),
        _mm256_mask3_fmadd_pd(0, 1, 2, 3),
        _mm_mask_fmadd_pd(0, 1, 2, 3),
        _mm_maskz_fmadd_pd(0, 1, 2, 3),
        _mm_mask3_fmadd_pd(0, 1, 2, 3),
        _mm512_fmsub_ps(0, 1, 2),
        _mm512_mask_fmsub_ps(0, 1, 2, 3),
        _mm512_maskz_fmsub_ps(0, 1, 2, 3),
        _mm512_mask3_fmsub_ps(0, 1, 2, 3),
        _mm256_mask_fmsub_ps(0, 1, 2, 3),
        _mm256_maskz_fmsub_ps(0, 1, 2, 3),
        _mm256_mask3_fmsub_ps(0, 1, 2, 3),
        _mm_mask_fmsub_ps(0, 1, 2, 3),
        _mm_maskz_fmsub_ps(0, 1, 2, 3),
        _mm_mask3_fmsub_ps(0, 1, 2, 3),
        _mm512_fmsub_pd(0, 1, 2),
        _mm512_mask_fmsub_pd(0, 1, 2, 3),
        _mm512_maskz_fmsub_pd(0, 1, 2, 3),
        _mm512_mask3_fmsub_pd(0, 1, 2, 3),
        _mm256_mask_fmsub_pd(0, 1, 2, 3),
        _mm256_maskz_fmsub_pd(0, 1, 2, 3),
        _mm256_mask3_fmsub_pd(0, 1, 2, 3),
        _mm_mask_fmsub_pd(0, 1, 2, 3),
        _mm_maskz_fmsub_pd(0, 1, 2, 3),
        _mm_mask3_fmsub_pd(0, 1, 2, 3),
        _mm512_fmaddsub_ps(0, 1, 2),
        _mm512_mask_fmaddsub_ps(0, 1, 2, 3),
        _mm512_maskz_fmaddsub_ps(0, 1, 2, 3),
        _mm512_mask3_fmaddsub_ps(0, 1, 2, 3),
        _mm256_mask_fmaddsub_ps(0, 1, 2, 3),
        _mm256_maskz_fmaddsub_ps(0, 1, 2, 3),
        _mm256_mask3_fmaddsub_ps(0, 1, 2, 3),
        _mm_mask_fmaddsub_ps(0, 1, 2, 3),
        _mm_maskz_fmaddsub_ps(0, 1, 2, 3),
        _mm_mask3_fmaddsub_ps(0, 1, 2, 3),
        _mm512_fmaddsub_pd(0, 1, 2),
        _mm512_mask_fmaddsub_pd(0, 1, 2, 3),
        _mm512_maskz_fmaddsub_pd(0, 1, 2, 3),
        _mm512_mask3_fmaddsub_pd(0, 1, 2, 3),
        _mm256_mask_fmaddsub_pd(0, 1, 2, 3),
        _mm256_maskz_fmaddsub_pd(0, 1, 2, 3),
        _mm256_mask3_fmaddsub_pd(0, 1, 2, 3),
        _mm_mask_fmaddsub_pd(0, 1, 2, 3),
        _mm_maskz_fmaddsub_pd(0, 1, 2, 3),
        _mm_mask3_fmaddsub_pd(0, 1, 2, 3),
        _mm512_fmsubadd_ps(0, 1, 2),
        _mm512_mask_fmsubadd_ps(0, 1, 2, 3),
        _mm512_maskz_fmsubadd_ps(0, 1, 2, 3),
        _mm512_mask3_fmsubadd_ps(0, 1, 2, 3),
        _mm256_mask_fmsubadd_ps(0, 1, 2, 3),
        _mm256_maskz_fmsubadd_ps(0, 1, 2, 3),
        _mm256_mask3_fmsubadd_ps(0, 1, 2, 3),
        _mm_mask_fmsubadd_ps(0, 1, 2, 3),
        _mm_maskz_fmsubadd_ps(0, 1, 2, 3),
        _mm_mask3_fmsubadd_ps(0, 1, 2, 3),
        _mm512_fmsubadd_pd(0, 1, 2),
        _mm512_mask_fmsubadd_pd(0, 1, 2, 3),
        _mm512_maskz_fmsubadd_pd(0, 1, 2, 3),
        _mm512_mask3_fmsubadd_pd(0, 1, 2, 3),
        _mm256_mask_fmsubadd_pd(0, 1, 2, 3),
        _mm256_maskz_fmsubadd_pd(0, 1, 2, 3),
        _mm256_mask3_fmsubadd_pd(0, 1, 2, 3),
        _mm_mask_fmsubadd_pd(0, 1, 2, 3),
        _mm_maskz_fmsubadd_pd(0, 1, 2, 3),
        _mm_mask3_fmsubadd_pd(0, 1, 2, 3),
        _mm512_fnmadd_ps(0, 1, 2),
        _mm512_mask_fnmadd_ps(0, 1, 2, 3),
        _mm512_maskz_fnmadd_ps(0, 1, 2, 3),
        _mm512_mask3_fnmadd_ps(0, 1, 2, 3),
        _mm256_mask_fnmadd_ps(0, 1, 2, 3),
        _mm256_maskz_fnmadd_ps(0, 1, 2, 3),
        _mm256_mask3_fnmadd_ps(0, 1, 2, 3),
        _mm_mask_fnmadd_ps(0, 1, 2, 3),
        _mm_maskz_fnmadd_ps(0, 1, 2, 3),
        _mm_mask3_fnmadd_ps(0, 1, 2, 3),
        _mm512_fnmadd_pd(0, 1, 2),
        _mm512_mask_fnmadd_pd(0, 1, 2, 3),
        _mm512_maskz_fnmadd_pd(0, 1, 2, 3),
        _mm512_mask3_fnmadd_pd(0, 1, 2, 3),
        _mm256_mask_fnmadd_pd(0, 1, 2, 3),
        _mm256_maskz_fnmadd_pd(0, 1, 2, 3),
        _mm256_mask3_fnmadd_pd(0, 1, 2, 3),
        _mm_mask_fnmadd_pd(0, 1, 2, 3),
        _mm_maskz_fnmadd_pd(0, 1, 2, 3),
        _mm_mask3_fnmadd_pd(0, 1, 2, 3),
        _mm512_fnmsub_ps(0, 1, 2),
        _mm512_mask_fnmsub_ps(0, 1, 2, 3),
        _mm512_maskz_fnmsub_ps(0, 1, 2, 3),
        _mm512_mask3_fnmsub_ps(0, 1, 2, 3),
        _mm256_mask_fnmsub_ps(0, 1, 2, 3),
        _mm256_maskz_fnmsub_ps(0, 1, 2, 3),
        _mm256_mask3_fnmsub_ps(0, 1, 2, 3),
        _mm_mask_fnmsub_ps(0, 1, 2, 3),
        _mm_maskz_fnmsub_ps(0, 1, 2, 3),
        _mm_mask3_fnmsub_ps(0, 1, 2, 3),
        _mm512_fnmsub_pd(0, 1, 2),
        _mm512_mask_fnmsub_pd(0, 1, 2, 3),
        _mm512_maskz_fnmsub_pd(0, 1, 2, 3),
        _mm512_mask3_fnmsub_pd(0, 1, 2, 3),
        _mm256_mask_fnmsub_pd(0, 1, 2, 3),
        _mm256_maskz_fnmsub_pd(0, 1, 2, 3),
        _mm256_mask3_fnmsub_pd(0, 1, 2, 3),
        _mm_mask_fnmsub_pd(0, 1, 2, 3),
        _mm_maskz_fnmsub_pd(0, 1, 2, 3),
        _mm_mask3_fnmsub_pd(0, 1, 2, 3),
        _mm_mask_fmadd_ss(0, 1, 2, 3),
        _mm_maskz_fmadd_ss(0, 1, 2, 3),
        _mm_mask3_fmadd_ss(0, 1, 2, 3),
        _mm_mask_fmadd_sd(0, 1, 2, 3),
        _mm_maskz_fmadd_sd(0, 1, 2, 3),
        _mm_mask3_fmadd_sd(0, 1, 2, 3),
        _mm_mask_fmsub_ss(0, 1, 2, 3),
        _mm_maskz_fmsub_ss(0, 1, 2, 3),
        _mm_mask3_fmsub_ss(0, 1, 2, 3),
        _mm_mask_fmsub_sd(0, 1, 2, 3),
        _mm_maskz_fmsub_sd(0, 1, 2, 3),
        _mm_mask3_fmsub_sd(0, 1, 2, 3),
        _mm_mask_fnmadd_ss(0, 1, 2, 3),
        _mm_maskz_fnmadd_ss(0, 1, 2, 3),
        _mm_mask3_fnmadd_ss(0, 1, 2, 3),
        _mm_mask_fnmadd_sd(0, 1, 2, 3),
        _mm_maskz_fnmadd_sd(0, 1, 2, 3),
        _mm_mask3_fnmadd_sd(0, 1, 2, 3),
        _mm_mask_fnmsub_ss(0, 1, 2, 3),
        _mm_maskz_fnmsub_ss(0, 1, 2, 3),
        _mm_mask3_fnmsub_ss(0, 1, 2, 3),
        _mm_mask_fnmsub_sd(0, 1, 2, 3),
        _mm_maskz_fnmsub_sd(0, 1, 2, 3),
        _mm_mask3_fnmsub_sd(0, 1, 2, 3),
    }
}

#[cfg(not(target_arch = "x86_64"))]
pub fn run() -> Vec<(&'static str, u64)> {
    Vec::new()
}
fn main() {
    println!("{:?}", run());
}
"""
# (program, its source, the runs to compare: the arguments of each, its panic strategy)
PROGRAMS = [("hello", HELLO, [["a", "b"]], "unwind"), ("rich", RICH, [[], ["fail"]], "unwind"),
            ("counting", ALLOC, [[]], "unwind"), ("abort", ABORT, [[], ["fail"]], "abort"),
            ("macros", MACROS, [[]], "unwind"), ("atomics", ATOMICS, [[]], "unwind"),
            ("paths", PATHS, [[]], "unwind"), ("fma", FMA, [[]], "unwind")]
# A shared library, as J2 links a proc macro's (`--exe` with `--shared`): its function
# allocates, formats, keeps a thread-local and catches a panic, which the allocator shim, the
# library's TLS and the unwinder have to be right for, loaded into a process blaze did not link.
LIBRARY = r"""use std::cell::RefCell;
use std::collections::BTreeMap;

thread_local! {
    static SEEN: RefCell<BTreeMap<u64, String>> = RefCell::new(BTreeMap::new());
}

#[unsafe(no_mangle)]
pub extern "C" fn allblaze_count(n: u64) -> u64 {
    let text: String = (0..n).map(|i| format!("{i},")).collect();
    SEEN.with(|seen| seen.borrow_mut().insert(n, text.clone()));
    std::panic::set_hook(Box::new(|_| {}));
    let caught = std::panic::catch_unwind(|| if n == 3 { panic!("three") } else { n });
    let seen = SEEN.with(|seen| seen.borrow().len() as u64);
    seen * 1000 + text.len() as u64 * 10 + caught.is_err() as u64
}
"""
CALLS = [1, 3, 10, 3]
# What `allblaze_count` gives for each of CALLS, all made on one thread: the distinct `n` it
# has seen, a thousand each, the length of the text times ten, and whether it caught a panic.
ANSWERS = [1020, 2061, 3200, 3061]
# A proc-macro crate linked as J2 links one: the library has the list of its macros blaze
# writes into the crate (crates/blaze-engine/src/macro_list.rs) where rustc's metadata has it.
PROC_MACROS = r"""extern crate proc_macro;
use proc_macro::TokenStream;

#[proc_macro_derive(Hello, attributes(hello, greet))]
pub fn hello(_input: TokenStream) -> TokenStream {
    "impl Hello for () {}".parse().unwrap()
}

#[proc_macro_attribute]
pub fn same(_attr: TokenStream, item: TokenStream) -> TokenStream {
    item
}

#[proc_macro]
pub fn answer(_input: TokenStream) -> TokenStream {
    "42".parse().unwrap()
}
"""
# Kind (derive, attribute, function-like), name and helper attributes, by name.
MACRO_LIST = [[0, "Hello", ["hello", "greet"]], [2, "answer", []], [1, "same", []]]
# Crates of the standard library a program links that the library's unit leaves out. What
# they depend on (`rustc-literal-escaper` of `proc_macro`) makes the unit another one.
LINKS = {"macros": ["proc_macro"]}


def run(cmd, cwd, what, env=None):
    start = time.monotonic()
    ran = subprocess.run(cmd, cwd=cwd, capture_output=True, text=True,
                         env={**os.environ, "RUST_BACKTRACE": "0", **(env or {})})
    took = time.monotonic() - start
    if ran.returncode != 0:
        raise SystemExit(f"FAIL: {what} exited {ran.returncode}:\n{ran.stdout}{ran.stderr}")
    return ran, took


def outcome(binary, args, cwd):
    ran = subprocess.run([binary, *args], cwd=cwd, capture_output=True, text=True,
                         env={**os.environ, "RUST_BACKTRACE": "0"})
    unthreaded = lambda text: re.sub(r"thread '([^']*)' \(\d+\)", r"thread '\1' (N)", text)
    return ran.returncode, unthreaded(ran.stdout), unthreaded(ran.stderr)


def package(work, name, source, panic):
    root = os.path.join(work, name)
    os.makedirs(os.path.join(root, "src"))
    with open(os.path.join(root, "Cargo.toml"), "w") as f:
        f.write(f'[package]\nname = "{name}"\nversion = "0.1.0"\nedition = "2021"\n[workspace]\n')
        if panic == "abort":
            f.write('[profile.dev]\npanic = "abort"\n')
    with open(os.path.join(root, "src/main.rs"), "w") as f:
        f.write(source)
    subprocess.run(["cargo", "generate-lockfile", "-q"], cwd=root, check=True)
    return root


def library_package(work):
    root = os.path.join(work, "shared")
    os.makedirs(os.path.join(root, "src"))
    with open(os.path.join(root, "Cargo.toml"), "w") as f:
        f.write('[package]\nname = "shared"\nversion = "0.1.0"\nedition = "2021"\n'
                '[lib]\ncrate-type = ["cdylib"]\n[workspace]\n')
    with open(os.path.join(root, "src/lib.rs"), "w") as f:
        f.write(LIBRARY)
    subprocess.run(["cargo", "generate-lockfile", "-q"], cwd=root, check=True)
    return root


def proc_macro_package(work):
    root = os.path.join(work, "macros-crate")
    os.makedirs(os.path.join(root, "src"))
    with open(os.path.join(root, "Cargo.toml"), "w") as f:
        f.write('[package]\nname = "allmacros"\nversion = "0.1.0"\nedition = "2021"\n'
                '[lib]\nproc-macro = true\n[workspace]\n')
    with open(os.path.join(root, "src/lib.rs"), "w") as f:
        f.write(PROC_MACROS)
    subprocess.run(["cargo", "generate-lockfile", "-q"], cwd=root, check=True)
    return root


def macro_list(library):
    """The list of macros the library exports, each with whether its client is there, read by a
    process of its own."""
    code = ("import ctypes, json, sys\n"
            "class Slice(ctypes.Structure):\n"
            "    _fields_ = [('ptr', ctypes.c_void_p), ('len', ctypes.c_size_t)]\n"
            "class Macro(ctypes.Structure):\n"
            "    _fields_ = [('kind', ctypes.c_uint32), ('name', Slice), ('helpers', Slice),\n"
            "                ('client', ctypes.c_void_p)]\n"
            "text = lambda it: ctypes.string_at(it.ptr, it.len).decode()\n"
            "listed = Slice.in_dll(ctypes.CDLL(sys.argv[1]), 'BLAZE_PROC_MACROS')\n"
            "macros = (Macro * listed.len).from_address(listed.ptr)\n"
            "helpers = lambda it: [text(h) for h in (Slice * it.len).from_address(it.ptr or 1)]\n"
            "print(json.dumps([[m.kind, text(m.name), helpers(m.helpers), bool(m.client)]\n"
            "                  for m in macros]))\n")
    ran = subprocess.run([sys.executable, "-c", code, library], capture_output=True, text=True)
    return ran.returncode, ran.stdout, ran.stderr


def calls(library):
    """What the library's `allblaze_count` gives for each of CALLS, loaded by a process of its own."""
    code = ("import ctypes, sys\n"
            "f = ctypes.CDLL(sys.argv[1]).allblaze_count\n"
            "f.argtypes, f.restype = [ctypes.c_uint64], ctypes.c_uint64\n"
            f"print([f(n) for n in {CALLS!r}])\n")
    ran = subprocess.run([sys.executable, "-c", code, library], capture_output=True, text=True)
    return ran.returncode, ran.stdout, ran.stderr


def blaze(root, crates, out, *extra, env=None):
    args = sum((["--crate", c] for c in crates), [])
    return run([BLAZE, root, *args, "--out", out, *extra], root, f"blaze {' '.join(crates[:1])}", env)


def linked(ran, what):
    """How long blaze's link took, failing when it left something it could not compile."""
    said = re.search(r"^linked .* in (\S+), (\d+) functions in it blaze could not compile$",
                     ran.stderr, re.M)
    if not said:
        raise SystemExit(f"FAIL: {what}: blaze did not say it linked:\n{ran.stderr}")
    if said.group(2) != "0":
        raise SystemExit(f"FAIL: {what}: the executable can reach {said.group(2)} functions blaze "
                         f"could not compile:\n{ran.stderr}")
    return said.group(1)


def main():
    if not sys.platform.startswith("linux"):
        print("not run: the programs blaze links with nothing of rustc's are Linux's so far")
        return
    ways = [way for way in ["whole", "unit", "stored"] if way in sys.argv[1:] or len(sys.argv) == 1]
    work = scratch("blaze-allblaze-")
    passed = False
    try:
        roots = {name: package(work, name, source, panic) for name, source, _, panic in PROGRAMS}
        # By program: which build it was first built by, and what its runs did.
        first = {}
        library, macros = library_package(work), proc_macro_package(work)
        expected_calls = (0, f"{ANSWERS}\n", "")
        lines = []
        units = {}
        if "unit" in ways:
            for panic, program in [("unwind", "hello"), ("abort", "abort")]:
                units[panic] = os.path.join(work, f"std-{panic}.o")
                _, took = blaze(roots[program], std_of(panic), units[panic])
                lines.append(f"the standard library alone, panic = \"{panic}\": {took:.2f} s, "
                             f"{os.path.getsize(units[panic]) >> 20} MB")
        remote = {"BLAZE_REMOTE": os.path.join(work, "remote"), "BLAZE_PROJECT": "allblaze",
                  "BLAZE_REMOTE_UNSIGNED": "1"}
        os.makedirs(remote["BLAZE_REMOTE"])
        stores = [{"BLAZE_CACHE": os.path.join(work, "store"), **remote},
                  {"BLAZE_CACHE": os.path.join(work, "store2"), **remote}]
        for way in ways:
            # The first program again once the store has the library, and the second on
            # another store.
            programs = PROGRAMS + PROGRAMS[:1] + PROGRAMS[1:2] if way == "stored" else PROGRAMS
            # The units, by panic strategy and what else of the library is linked, that some
            # store or the remote has.
            compiled = set()
            for at, (name, _, runs, panic) in enumerate(programs):
                root = roots[name]
                obj = os.path.join(work, f"{name}-{way}.o")
                binary = os.path.join(work, f"{name}-{way}")
                exe = ["--exe", binary]
                how = ""
                own = [name, *LINKS.get(name, [])]
                if way == "whole":
                    ran, took = blaze(root, [*own, *std_of(panic)], obj, *exe)
                elif way == "unit":
                    ran, took = blaze(root, own, obj, "--prebuilt", units[panic], *exe)
                else:
                    store = stores[at == len(programs) - 1]
                    ran, took = blaze(root, own, obj, *exe, env=store)
                    said = re.findall(r"^standard library: (.*)$", ran.stderr, re.M)
                    stored = bool(said) and said[0].startswith("from the store")
                    unit = (panic, *LINKS.get(name, []))
                    if not said or stored != (unit in compiled):
                        raise SystemExit(f"FAIL: {name} ({way}, build {at + 1}) "
                                         f"{'should have taken' if unit in compiled else 'took'} the "
                                         f"standard library from the store: {said}\n{ran.stderr}")
                    # The unit is the crates rustc's sysroot has of the library, no other: not
                    # an optional dependency no feature turns on, as hashbrown's foldhash, nor
                    # what only the library's own tests depend on, as std's rand.
                    crates = int(re.match(r"(?:from the store, )?(\d+) crates", said[0])[1])
                    if not LINKS.get(name) and crates != len(std_of(panic)):
                        raise SystemExit(f"FAIL: {name} ({way}): the standard library's unit has "
                                         f"{crates} crates, not the {len(std_of(panic))} of "
                                         f"rustc's sysroot ({', '.join(std_of(panic))})")
                    compiled.add(unit)
                    how = (", the standard library from the remote" if store is stores[1]
                           else ", the standard library from the store" if stored
                           else ", the standard library compiled, stored and pushed")
                link_took = linked(ran, f"{name} ({way})")
                this = f"{way}, build {at + 1}"
                results = [outcome(binary, args, root) for args in runs]
                by, wanted = first.setdefault(name, (this, results))
                for args, want, got in zip(runs, wanted, results):
                    if got != want:
                        raise SystemExit(f"FAIL: {name} {' '.join(args)} ({this}) differs from its first build ({by}):\n"
                                         f"{by}: {want}\n{this}: {got}")
                lines.append(f"{name} ({way}): built and linked in {took:.2f} s{how}, the link "
                             f"{link_took}, {len(runs)} runs" + ("" if by == this else f" as its {by} build's"))
            obj, shared = (os.path.join(work, f"shared-{way}{ext}") for ext in (".o", ".so"))
            exe = ["--exe", shared, "--shared"]
            if way == "whole":
                ran, took = blaze(library, ["shared", *std_of("unwind")], obj, *exe)
            elif way == "unit":
                ran, took = blaze(library, ["shared"], obj, "--prebuilt", units["unwind"], *exe)
            else:
                ran, took = blaze(library, ["shared"], obj, *exe, env=stores[0])
                said = re.findall(r"^standard library: (.*)$", ran.stderr, re.M)
                if not said or not said[0].startswith("from the store"):
                    raise SystemExit(f"FAIL: the shared library (stored) should have taken the "
                                     f"standard library from the store: {said}\n{ran.stderr}")
            link_took = linked(ran, f"the shared library ({way})")
            got = calls(shared)
            if got != expected_calls:
                raise SystemExit(f"FAIL: the shared library ({way}) answers otherwise than its source says:\n"
                                 f"wanted: {expected_calls}\ngot:    {got}")
            lines.append(f"the shared library ({way}): built and linked in {took:.2f} s, the link "
                         f"{link_took}, {len(CALLS)} calls answered as its source says")
            obj, shared = (os.path.join(work, f"allmacros-{way}{ext}") for ext in (".o", ".so"))
            exe = ["--exe", shared, "--shared"]
            if way == "whole":
                ran, took = blaze(macros, ["allmacros", "proc_macro", *std_of("unwind")], obj, *exe)
            elif way == "unit":
                ran, took = blaze(macros, ["allmacros", "proc_macro"], obj,
                                   "--prebuilt", units["unwind"], *exe)
            else:
                ran, took = blaze(macros, ["allmacros", "proc_macro"], obj, *exe, env=stores[0])
            link_took = linked(ran, f"the proc-macro crate ({way})")
            code, out, err = macro_list(shared)
            want = [[*it, True] for it in MACRO_LIST]
            if code != 0 or json.loads(out) != want:
                raise SystemExit(f"FAIL: the proc-macro crate's library ({way}) lists other than "
                                 f"its macros:\nwanted: {want}\ngot:    {code} {out}{err}")
            lines.append(f"the proc-macro crate ({way}): built and linked in {took:.2f} s, the "
                         f"link {link_took}, its {len(MACRO_LIST)} macros listed")
        print("\n".join(lines))
        print(f"PASS: {len(PROGRAMS)} programs, a shared library and a proc-macro crate's with "
              f"nothing of rustc's, "
              f"{' and '.join(ways)}")
        passed = True
    finally:
        if passed:
            shutil.rmtree(work, ignore_errors=True)


if __name__ == "__main__":
    main()
