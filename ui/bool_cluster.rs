// Bool fields that the crate only ever builds in some of their combinations.
#![allow(dead_code)]

// Flagged: three bools, built in two of their eight combinations.
struct Refusals {
    too_big: bool,
    exhausted: bool,
    fragmented: bool,
    slots_leaked: u64,
}

fn refuse_big(slots_leaked: u64) -> Refusals {
    Refusals {
        too_big: true,
        exhausted: false,
        fragmented: false,
        slots_leaked,
    }
}

fn refuse_fragmented() -> Refusals {
    Refusals {
        too_big: false,
        exhausted: false,
        fragmented: true,
        slots_leaked: 0,
    }
}

// Flagged: the same fn builds two combinations, so the variants are named
// after the bools they set. A second build of a combination adds nothing.
struct Mode {
    read: bool,
    write: bool,
    append: bool,
}

fn mode(append: Option<()>) -> Mode {
    match append {
        Some(()) => Mode {
            read: false,
            write: true,
            append: true,
        },
        None => Mode {
            read: true,
            write: false,
            append: false,
        },
    }
}

fn reader() -> Mode {
    Mode {
        read: true,
        write: false,
        append: false,
    }
}

// Flagged: `Refusals` again with the common derives. The derived `Default`
// builds one more combination, all `false`.
#[derive(Clone, Debug, Default)]
struct RefusalsTwin {
    too_big: bool,
    exhausted: bool,
    fragmented: bool,
}

fn twin_big() -> RefusalsTwin {
    RefusalsTwin {
        too_big: true,
        exhausted: false,
        fragmented: false,
    }
}

fn twin_fragmented() -> RefusalsTwin {
    RefusalsTwin {
        too_big: false,
        exhausted: false,
        fragmented: true,
    }
}

// Fine: built only once. The bools never change, so there is no state.
struct Once {
    a: bool,
    b: bool,
    c: bool,
}

fn once() -> Once {
    Once {
        a: true,
        b: false,
        c: true,
    }
}

// Fine: a later assignment can make any combination.
struct Assigned {
    a: bool,
    b: bool,
    c: bool,
}

fn assigned_a() -> Assigned {
    Assigned {
        a: true,
        b: false,
        c: false,
    }
}

fn assigned_b() -> Assigned {
    let mut s = Assigned {
        a: false,
        b: true,
        c: false,
    };
    s.c = true;
    s
}

// Fine: a computed value can be either.
struct Computed {
    a: bool,
    b: bool,
    c: bool,
}

fn computed(x: bool) -> Computed {
    Computed {
        a: x,
        b: false,
        c: false,
    }
}

fn computed_b() -> Computed {
    Computed {
        a: false,
        b: true,
        c: false,
    }
}

// Fine: `..base` copies values not known here.
struct Copied {
    a: bool,
    b: bool,
    c: bool,
}

fn copied_a() -> Copied {
    Copied {
        a: true,
        b: false,
        c: false,
    }
}

fn copied_b(base: Copied) -> Copied {
    Copied { b: true, ..base }
}

// Fine: a `&mut` to one bool can change it alone.
struct Lent {
    a: bool,
    b: bool,
    c: bool,
}

fn lent_a() -> Lent {
    Lent {
        a: true,
        b: false,
        c: false,
    }
}

fn lent_b() -> Lent {
    Lent {
        a: false,
        b: true,
        c: false,
    }
}

fn lend(l: &mut Lent) -> &mut bool {
    &mut l.c
}

// Fine: another crate can build it with any combination.
pub struct Open {
    pub a: bool,
    pub b: bool,
    pub c: bool,
}

pub fn open_a() -> Open {
    Open {
        a: true,
        b: false,
        c: false,
    }
}

pub fn open_b() -> Open {
    Open {
        a: false,
        b: true,
        c: false,
    }
}

// Fine: two bools is under the threshold.
struct Pair {
    a: bool,
    b: bool,
}

fn pair_a() -> Pair {
    Pair { a: true, b: false }
}

fn pair_b() -> Pair {
    Pair { a: false, b: true }
}

// Fine: every one of the eight combinations is built.
struct Full {
    a: bool,
    b: bool,
    c: bool,
}

fn full() -> [Full; 8] {
    [
        Full { a: false, b: false, c: false },
        Full { a: false, b: false, c: true },
        Full { a: false, b: true, c: false },
        Full { a: false, b: true, c: true },
        Full { a: true, b: false, c: false },
        Full { a: true, b: false, c: true },
        Full { a: true, b: true, c: false },
        Full { a: true, b: true, c: true },
    ]
}

// Fine: an explicit repr means the layout is fixed outside Rust, so all eight
// combinations may be real.
#[repr(C)]
struct HwFlags {
    enabled: bool,
    pending: bool,
    masked: bool,
}

fn hw_a() -> HwFlags {
    HwFlags {
        enabled: true,
        pending: false,
        masked: false,
    }
}

fn hw_b() -> HwFlags {
    HwFlags {
        enabled: false,
        pending: true,
        masked: false,
    }
}

// Fine: a tuple struct has no field names to report.
struct Positional(bool, bool, bool);

fn main() {
    let _ = Positional(true, false, true);
}
