// A function whose signature and body repeat another function's is one
// helper written twice; anything that differs in type or shape is not.

pub fn clamp_add(base: u32, delta: u32, limit: u32) -> u32 {
    let sum = base.saturating_add(delta);
    if sum > limit { limit } else { sum }
}

// Flagged: `clamp_add` again with the parameters renamed.
pub fn bounded_sum(a: u32, b: u32, max: u32) -> u32 {
    let total = a.saturating_add(b);
    if total > max { max } else { total }
}

// Fine: same shape, but the parameters are `u64`, so the signature differs.
pub fn clamp_add_wide(base: u64, delta: u64, limit: u64) -> u64 {
    let sum = base.saturating_add(delta);
    if sum > limit { limit } else { sum }
}

// Fine: one operator differs.
pub fn clamp_add_inclusive(base: u32, delta: u32, limit: u32) -> u32 {
    let sum = base.saturating_add(delta);
    if sum >= limit { limit } else { sum }
}

pub struct Row {
    cells: Vec<u32>,
    pad: u32,
}

pub struct Column {
    cells: Vec<u32>,
    pad: u32,
}

impl Row {
    pub fn extent(&self) -> u32 {
        self.cells.iter().sum::<u32>() + self.pad * 2 + 1
    }

    // Flagged: the same computation as `extent` on the same type.
    pub fn footprint(&self) -> u32 {
        self.cells.iter().sum::<u32>() + self.pad * 2 + 1
    }

    // Fine: too small to compare (under `reimplemented-helper-min-nodes`).
    pub fn pad(&self) -> u32 {
        self.pad
    }

    // Fine: an accessor as small as `pad`.
    pub fn margin(&self) -> u32 {
        self.pad
    }
}

impl Column {
    // Fine: spelled like `Row::extent`, but `self` is a `Column`, so the
    // signatures differ and the field reads are of another type.
    pub fn extent(&self) -> u32 {
        self.cells.iter().sum::<u32>() + self.pad * 2 + 1
    }
}

pub trait Resize {
    fn grow(&self, from: u32, to: u32) -> u32;
    fn shrink(&self, from: u32, to: u32) -> u32;
}

impl Row {
    fn remap(&self, from: u32, to: u32) -> u32 {
        if to > from {
            self.pad + (to - from)
        } else {
            self.pad.saturating_sub(from - to)
        }
    }
}

// Fine: the trait obliges the impl to define both, and each already forwards
// to the one shared helper, so there is no copy to delete.
impl Resize for Row {
    fn grow(&self, from: u32, to: u32) -> u32 {
        self.remap(from.min(to), to.max(from)) + self.cells.len() as u32
    }
    fn shrink(&self, from: u32, to: u32) -> u32 {
        self.remap(from.min(to), to.max(from)) + self.cells.len() as u32
    }
}

// Quiet: closures are never compared structurally, so two bodies built
// around one never pair up even when they are copies.
pub fn doubled_evens(xs: &[u32]) -> Vec<u32> {
    xs.iter().filter(|x| **x % 2 == 0).map(|x| x * 2).collect()
}

// Quiet: the copy of `doubled_evens`.
pub fn twice_the_evens(values: &[u32]) -> Vec<u32> {
    values
        .iter()
        .filter(|v| **v % 2 == 0)
        .map(|v| v * 2)
        .collect()
}

fn main() {}
