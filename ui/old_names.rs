// A lint's old name still silences it in an `allow`; rustc notes the rename.
// `exclusive_options` is now `options_as_enum`, `wildcard_local_enum` is now
// `wildcard_over_own_enum`.
#![allow(exclusive_options)]

// Silenced by the crate-level allow: the shape ui/options_as_enum.rs flags.
struct Outcome {
    ok: Option<u32>,
    err: Option<String>,
}

fn success() -> Outcome {
    Outcome {
        ok: Some(1),
        err: None,
    }
}

fn failure() -> Outcome {
    Outcome {
        ok: None,
        err: Some("boom".to_owned()),
    }
}

enum Op {
    Add,
    Sub,
    Mul,
}

// Flagged: nothing allows it.
fn wild(o: Op) -> i32 {
    match o {
        Op::Add => 1,
        _ => 0,
    }
}

// Silenced by the old name on the item.
#[allow(wildcard_local_enum)]
fn wild_allowed(o: Op) -> i32 {
    match o {
        Op::Add => 1,
        _ => 0,
    }
}

fn main() {
    let _ = (success(), failure());
}
