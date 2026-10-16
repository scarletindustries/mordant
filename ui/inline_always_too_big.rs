// Each comment says whether the fn is reported, and why.

pub enum Value {
    Num(i64),
    List(Vec<Value>),
}

// Reported: calls itself through `parse_list`, so the cycle cannot be inlined.
#[inline(always)]
pub fn parse_value(src: &[u8], at: &mut usize) -> Option<Value> {
    match src.get(*at)? {
        b'[' => parse_list(src, at),
        b => {
            *at += 1;
            Some(Value::Num(i64::from(*b)))
        }
    }
}

fn parse_list(src: &[u8], at: &mut usize) -> Option<Value> {
    *at += 1;
    let mut items = Vec::new();
    while src.get(*at) != Some(&b']') {
        items.push(parse_value(src, at)?);
    }
    *at += 1;
    Some(Value::List(items))
}

// Reported: calls itself directly.
#[inline(always)]
pub fn depth(v: &Value) -> usize {
    let mut most = 0;
    if let Value::List(items) = v {
        for item in items {
            most = most.max(1 + depth(item));
        }
    }
    most
}

// Silent: `count` is passed to `map` as a value, and the lint sees only direct calls.
#[inline(always)]
pub fn count(v: &Value) -> usize {
    match v {
        Value::Num(_) => 1,
        Value::List(items) => items.iter().map(count).sum(),
    }
}

// Reported: a large body called from three places.
#[inline(always)]
pub fn mix(state: &mut [u32; 8], input: &[u8]) -> u32 {
    let mut acc = 0u32;
    for (i, byte) in input.iter().enumerate() {
        let b = u32::from(*byte);
        let slot = i % 8;
        state[slot] = state[slot].wrapping_mul(31).wrapping_add(b);
        if b & 1 == 0 {
            state[(slot + 1) % 8] ^= b << 3;
        } else {
            state[(slot + 3) % 8] ^= b >> 2;
        }
        if b > 128 {
            acc = acc.wrapping_add(state[slot]);
        } else if b > 64 {
            acc = acc.rotate_left(5) ^ state[(slot + 2) % 8];
        } else {
            acc = acc.wrapping_sub(state[(slot + 5) % 8]);
        }
        if acc % 7 == 3 {
            state[0] = state[0].wrapping_add(acc);
            state[7] = state[7].rotate_right(3);
        }
        if acc % 11 == 5 {
            state[1] ^= acc;
            state[6] = state[6].wrapping_mul(17);
        }
        if acc % 13 == 7 {
            state[2] = state[2].wrapping_add(state[5]);
            state[4] ^= state[3];
        }
    }
    for s in state.iter_mut() {
        *s = s.rotate_left(acc % 32);
        acc ^= *s;
    }
    acc
}

pub fn use_mix(input: &[u8]) -> u32 {
    let mut state = [1u32; 8];
    let a = mix(&mut state, input);
    let b = mix(&mut state, &input[1..]);
    a ^ b ^ mix(&mut state, b"tail")
}

// Silent: the same large body, but only two call sites.
#[allow(reimplemented_helper)]
#[inline(always)]
pub fn mix_twice(state: &mut [u32; 8], input: &[u8]) -> u32 {
    let mut acc = 0u32;
    for (i, byte) in input.iter().enumerate() {
        let b = u32::from(*byte);
        let slot = i % 8;
        state[slot] = state[slot].wrapping_mul(31).wrapping_add(b);
        if b & 1 == 0 {
            state[(slot + 1) % 8] ^= b << 3;
        } else {
            state[(slot + 3) % 8] ^= b >> 2;
        }
        if b > 128 {
            acc = acc.wrapping_add(state[slot]);
        } else if b > 64 {
            acc = acc.rotate_left(5) ^ state[(slot + 2) % 8];
        } else {
            acc = acc.wrapping_sub(state[(slot + 5) % 8]);
        }
        if acc % 7 == 3 {
            state[0] = state[0].wrapping_add(acc);
            state[7] = state[7].rotate_right(3);
        }
        if acc % 11 == 5 {
            state[1] ^= acc;
            state[6] = state[6].wrapping_mul(17);
        }
        if acc % 13 == 7 {
            state[2] = state[2].wrapping_add(state[5]);
            state[4] ^= state[3];
        }
    }
    for s in state.iter_mut() {
        *s = s.rotate_left(acc % 32);
        acc ^= *s;
    }
    acc
}

pub fn use_mix_twice(input: &[u8]) -> u32 {
    let mut state = [1u32; 8];
    mix_twice(&mut state, input) ^ mix_twice(&mut state, b"tail")
}

// Silent: a small body, so many copies cost little.
#[inline(always)]
pub fn low_bit(x: u32) -> bool {
    x & 1 == 1
}

pub fn use_low_bit(a: u32, b: u32, c: u32, d: u32) -> bool {
    low_bit(a) && low_bit(b) && low_bit(c) && low_bit(d)
}

// Silent: plain `#[inline]` lets the compiler decide.
#[inline]
pub fn walk(v: &Value) -> usize {
    match v {
        Value::Num(_) => 1,
        Value::List(items) => items.iter().map(walk).sum(),
    }
}

pub trait Visit {
    fn visit(&self, n: u32) -> u32;
}

pub struct Counter;

impl Visit for Counter {
    // Silent: the call back into `visit` goes through `dyn`, which the lint does not follow.
    #[inline(always)]
    fn visit(&self, n: u32) -> u32 {
        let next: &dyn Visit = self;
        if n == 0 { 0 } else { next.visit(n - 1) + 1 }
    }
}

// Generic: calls from each instantiation count together. Reported: three call sites.
#[inline(always)]
pub fn mix_any<T: AsRef<[u8]>>(state: &mut [u32; 8], input: T) -> u32 {
    let input = input.as_ref();
    let mut acc = 0u32;
    for (i, byte) in input.iter().enumerate() {
        let b = u32::from(*byte);
        let slot = i % 8;
        state[slot] = state[slot].wrapping_mul(31).wrapping_add(b);
        if b & 1 == 0 {
            state[(slot + 1) % 8] ^= b << 3;
        } else {
            state[(slot + 3) % 8] ^= b >> 2;
        }
        if b > 128 {
            acc = acc.wrapping_add(state[slot]);
        } else if b > 64 {
            acc = acc.rotate_left(5) ^ state[(slot + 2) % 8];
        } else {
            acc = acc.wrapping_sub(state[(slot + 5) % 8]);
        }
        if acc % 7 == 3 {
            state[0] = state[0].wrapping_add(acc);
            state[7] = state[7].rotate_right(3);
        }
        if acc % 11 == 5 {
            state[1] ^= acc;
            state[6] = state[6].wrapping_mul(17);
        }
        if acc % 13 == 7 {
            state[2] = state[2].wrapping_add(state[5]);
            state[4] ^= state[3];
        }
    }
    acc
}

pub fn use_mix_any(input: &[u8], text: &str, owned: Vec<u8>) -> u32 {
    let mut state = [1u32; 8];
    mix_any(&mut state, input) ^ mix_any(&mut state, text) ^ mix_any(&mut state, owned)
}

fn main() {}
