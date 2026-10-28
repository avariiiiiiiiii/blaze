use core::marker::Destruct;

pub const trait StableHasher {
    fn write(&mut self, bytes: &[u8]);
    fn finish(&self) -> u64;
}

/// FNV-1a, 64 bits.
pub struct Fnv(u64);
impl Fnv {
    pub const fn new() -> Fnv {
        Fnv(0xcbf2_9ce4_8422_2325)
    }
}
const impl StableHasher for Fnv {
    fn write(&mut self, bytes: &[u8]) {
        let mut i = 0;
        while i < bytes.len() {
            self.0 = (self.0 ^ bytes[i] as u64).wrapping_mul(0x100_0000_01b3);
            i += 1;
        }
    }
    fn finish(&self) -> u64 {
        self.0
    }
}

pub const trait StableHash {
    fn stable_hash<H: [const] StableHasher>(&self, h: &mut H);
}
const impl StableHash for u32 {
    fn stable_hash<H: [const] StableHasher>(&self, h: &mut H) {
        h.write(&self.to_le_bytes());
    }
}
const impl StableHash for str {
    fn stable_hash<H: [const] StableHasher>(&self, h: &mut H) {
        h.write(self.as_bytes());
        h.write(&[0xff]);
    }
}
const impl<T: [const] StableHash> StableHash for [T] {
    fn stable_hash<H: [const] StableHasher>(&self, h: &mut H) {
        (self.len() as u32).stable_hash(h);
        let mut i = 0;
        while i < self.len() {
            self[i].stable_hash(h);
            i += 1;
        }
    }
}
const impl<A: [const] StableHash, B: [const] StableHash + ?Sized> StableHash for (A, &B) {
    fn stable_hash<H: [const] StableHasher>(&self, h: &mut H) {
        self.0.stable_hash(h);
        self.1.stable_hash(h);
    }
}

pub const fn hash_of<T: [const] StableHash + ?Sized>(t: &T) -> u64 {
    let mut h = Fnv::new();
    t.stable_hash(&mut h);
    h.finish()
}

const KEY: (u32, &str) = (7, "lib/src/main.rs");
const KEY_HASH: u64 = hash_of(&KEY);
const SLICE_HASH: u64 = hash_of::<[u32]>(&[1, 2, 3]);

#[derive_const(PartialEq, Clone, Default)]
#[derive(Debug)]
pub struct Point {
    x: i32,
    y: i32,
}
#[derive_const(PartialEq, Clone)]
#[derive(Debug)]
pub enum Shape {
    Dot,
    Circle(u32),
    Rect { w: u32, h: u32 },
}
const impl core::ops::Add for Point {
    type Output = Point;
    fn add(self, o: Point) -> Point {
        Point { x: self.x + o.x, y: self.y + o.y }
    }
}
const impl From<i32> for Point {
    fn from(v: i32) -> Point {
        Point { x: v, y: -v }
    }
}

pub const fn sum_all<T>(items: &[T]) -> T
where
    T: [const] core::ops::Add<Output = T> + [const] Default + [const] Clone + [const] Destruct,
{
    let mut acc = T::default();
    let mut i = 0;
    while i < items.len() {
        acc = acc + items[i].clone();
        i += 1;
    }
    acc
}
pub const fn largest<T: [const] Ord + [const] Destruct + Copy>(items: &[T]) -> T {
    let mut best = items[0];
    let mut i = 1;
    while i < items.len() {
        best = core::cmp::max(best, items[i]);
        i += 1;
    }
    best
}
pub const fn count_eq<T: [const] PartialEq>(items: &[T], x: &T) -> usize {
    let mut n = 0;
    let mut i = 0;
    while i < items.len() {
        if items[i] == *x {
            n += 1;
        }
        i += 1;
    }
    n
}
pub const fn convert<T: [const] From<i32>>(v: i32) -> T {
    T::from(v)
}

const POINTS: [Point; 3] =
    [Point { x: 1, y: 2 }, Point { x: 3, y: 4 }, Point { x: -5, y: 6 }];
const SUM: Point = sum_all(&POINTS);
const SUM_U64: u64 = sum_all(&[10u64, 20, 30]);
const LARGEST: (i8, char) = (largest(&[3i8, -9, 7, 2]), largest(&['q', 'z', 'a']));
const SHAPES: [Shape; 4] =
    [Shape::Dot, Shape::Circle(2), Shape::Rect { w: 1, h: 2 }, Shape::Circle(2)];
const COUNT: (usize, usize) = (
    count_eq(&SHAPES, &Shape::Circle(2)),
    count_eq(&SHAPES, &Shape::Rect { w: 2, h: 1 }),
);
const CONVERTED: Point = convert(9);
const STD_FROM: i64 = convert::<i64>(-4) * i64::from(7u8);

pub const fn apply<F: [const] Fn(u32) -> u32 + [const] Destruct>(f: F, x: u32) -> u32 {
    f(x)
}
pub const fn apply_n<F: [const] FnMut(u32) + [const] Destruct>(mut f: F, n: u32) {
    let mut i = 0;
    while i < n {
        f(i);
        i += 1;
    }
}
pub const fn hash_with<F: [const] Fn(&[u8]) -> u64 + [const] Destruct>(f: F) -> u64 {
    f(b"blaze")
}

const CLOSURES: (u32, u32, u64) = {
    let k = 5;
    let mut acc = 0;
    apply_n(const |i| acc += i * k, 4);
    let byte_hash = hash_with(const |b: &[u8]| {
        let mut h = Fnv::new();
        h.write(b);
        h.finish()
    });
    (apply(const |x| x * x + k, 6), acc, byte_hash)
};

pub struct Countdown(u32);
const impl Iterator for Countdown {
    type Item = u32;
    fn next(&mut self) -> Option<u32> {
        if self.0 == 0 {
            None
        } else {
            self.0 -= 1;
            Some(self.0)
        }
    }
}
pub const fn drain<I: [const] Iterator<Item = u32> + [const] Destruct>(mut it: I) -> u32 {
    let mut total = 0;
    while let Some(x) = it.next() {
        total = total * 10 + x;
    }
    total
}
const DRAINED: u32 = drain(Countdown(5));

pub fn run() -> String {
    let key: (u32, &str) = (7, "lib/src/main.rs");
    let runtime_closures = {
        let k = 5;
        let mut acc = 0;
        apply_n(|i| acc += i * k, 4);
        let byte_hash = hash_with(|b: &[u8]| {
            let mut h = Fnv::new();
            h.write(b);
            h.finish()
        });
        (apply(|x| x * x + k, 6), acc, byte_hash)
    };
    let points = [Point { x: 1, y: 2 }, Point { x: 3, y: 4 }, Point { x: -5, y: 6 }];
    format!(
        "hash {:x} {:x} {:x} {:x}; sum {:?} {:?} {} {}; largest {:?} {:?}; shapes {:?} {:?}; \
         from {:?} {}; closures {:?} {:?}; drain {} {}; default {:?}",
        KEY_HASH,
        hash_of(&key),
        SLICE_HASH,
        hash_of::<[u32]>(&[1, 2, 3]),
        SUM,
        sum_all(&points),
        SUM_U64,
        sum_all(&[10u64, 20, 30]),
        LARGEST,
        (largest(&[3i8, -9, 7, 2]), largest(&['q', 'z', 'a'])),
        COUNT,
        SHAPES[2].clone(),
        CONVERTED,
        STD_FROM,
        CLOSURES,
        runtime_closures,
        DRAINED,
        drain(Countdown(5)),
        Point::default(),
    )
}
