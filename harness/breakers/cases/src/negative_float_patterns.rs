//! A negative float literal in a pattern, as wasmtime's `wasm_nearest` matches `round % 2.0`
//! against `1.0` and `-1.0`: the pattern was lowered as missing, and the function refused.

use std::hint::black_box;

fn nearest(value: f64) -> f64 {
    let round = value.round();
    if (value - round).abs() != 0.5 {
        return round;
    }
    match round % 2.0 {
        1.0 => value.floor(),
        -1.0 => value.ceil(),
        _ => round,
    }
}

fn sign(value: f32) -> &'static str {
    match value {
        -0.5 => "minus a half",
        -1.0..=-0.75 => "about minus one",
        0.5 => "a half",
        _ => "other",
    }
}

pub fn run() -> (Vec<f64>, Vec<&'static str>) {
    let halves = [2.5, -2.5, 3.5, -3.5, 0.5, -0.5, 7.25].map(|it| nearest(black_box(it)));
    let signs = [-0.5, -0.8, 0.5, -0.0, 1.0].map(|it| sign(black_box(it)));
    (halves.to_vec(), signs.to_vec())
}
