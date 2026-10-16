// A value bounded only by a debug assertion must not then be narrowed.

use std::convert::TryFrom;

const INNER_BITS: u64 = (1 << 28) - 1;

struct Lexer {
    start: usize,
}

struct Parser {
    lexer: Lexer,
    names: Vec<u32>,
}

// Flagged: the mask keeps 28 bits, and only debug builds check the value fits.
fn pack(inner: u32, src: u32) -> u64 {
    debug_assert!((inner as u64) <= INNER_BITS);
    (inner as u64 & INNER_BITS) | ((src as u64) << 32)
}

// Flagged: `request << 16` loses bits above 16 in a release build.
fn kind_bits(kind: u32, request: u32) -> u32 {
    debug_assert!(request <= u16::MAX as u32);
    kind | request << 16
}

// Flagged: a field path, cast after a debug check of the same path.
fn memo_key(p: &Parser) -> u32 {
    debug_assert!(p.lexer.start <= (u32::MAX >> 1) as usize);
    p.lexer.start as u32
}

// Flagged: a `.len()` checked in a debug assertion, then cast.
fn short_len(s: &[u8]) -> u32 {
    debug_assert!(s.len() <= u32::MAX as usize);
    s.len() as u32
}

// Flagged: an `if cfg!(debug_assertions)` block with `assert!` is debug-only too.
fn tag(n: usize) -> u8 {
    if cfg!(debug_assertions) {
        assert!(n < 256);
    }
    n as u8
}

// Flagged: each side of `&&` is a bound.
fn both(off: usize, len: usize) -> (u32, u32) {
    debug_assert!(off <= u32::MAX as usize && len <= u32::MAX as usize);
    (len as u32, off as u32)
}

// Silent: `assert!` stays in release builds.
fn checked_tag(n: usize) -> u8 {
    assert!(n < 256);
    n as u8
}

// Silent: `try_from` is a release check of the same value.
fn converted(n: usize) -> u32 {
    debug_assert!(n <= u32::MAX as usize);
    u32::try_from(n).expect("fits")
}

// Silent: a release comparison with a constant bounds it too.
fn early_out(n: usize) -> Option<u16> {
    debug_assert!(n <= u16::MAX as usize);
    if n > 60_000 {
        return None;
    }
    Some(n as u16)
}

// Silent: the bound does not fit the narrowed type, so the check is not what
// makes this cast safe.
fn loose(n: usize) -> u8 {
    debug_assert!(n <= u32::MAX as usize);
    n as u8
}

// Silent: the value is written between the check and the cast.
fn reassigned(mut n: usize) -> u32 {
    debug_assert!(n <= u32::MAX as usize);
    n += 1;
    n as u32
}

// Silent: the vector grows between the check and the cast.
fn pushed(p: &mut Parser) -> u32 {
    debug_assert!(p.names.len() <= u32::MAX as usize);
    p.names.push(0);
    p.names.len() as u32
}

// Silent: the cast comes before the check.
fn cast_first(n: usize) -> u32 {
    let narrow = n as u32;
    debug_assert!(n <= u32::MAX as usize);
    narrow
}

// Silent: the cast cannot lose anything, `u16` fits in `u32`.
fn widening(n: u16) -> u32 {
    debug_assert!(n <= 1000);
    n as u32
}

// Silent: a bound against a variable says nothing about a fixed width.
fn against_len(i: usize, n: usize) -> u32 {
    debug_assert!(i <= n);
    i as u32
}

// Silent: a mask that is not low bits does not keep values below a limit.
fn odd_mask(n: u32) -> u32 {
    debug_assert!(n <= 0xff);
    n & 0xf0
}

// Silent: written later in a loop around the cast.
fn looped(mut n: usize) -> u32 {
    debug_assert!(n <= u32::MAX as usize);
    let mut out = 0;
    for _ in 0..3 {
        out += n as u32;
        n *= 2;
    }
    out
}

// Silent: slicing a fixed-size array by the length panics in release mode too.
fn literal(s: &[u8]) -> ([u8; 8], u8) {
    debug_assert!(s.len() <= 8);
    let mut buf = [0u8; 8];
    buf[..s.len()].copy_from_slice(s);
    (buf, s.len() as u8)
}

// Silent: `buf[i]` panics in release mode once `i` reaches 8, and `i` counts up to the length.
fn widen(s: &[u8]) -> ([u16; 8], u8) {
    debug_assert!(s.len() <= 8);
    let mut buf = [0u16; 8];
    let mut i = 0;
    while i < s.len() {
        buf[i] = s[i] as u16;
        i += 1;
    }
    (buf, s.len() as u8)
}

fn main() {
    let mut p = Parser {
        lexer: Lexer { start: 0 },
        names: Vec::new(),
    };
    let _ = (pack(1, 2), kind_bits(1, 2), memo_key(&p), short_len(b"ab"));
    let _ = (tag(1), both(1, 2), checked_tag(1), converted(1), early_out(1));
    let _ = (loose(1), reassigned(1), pushed(&mut p), cast_first(1));
    let _ = (widening(1), against_len(1, 2), odd_mask(1), looped(1), literal(b"a"), widen(b"a"));
}
