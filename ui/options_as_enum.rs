// Structs whose Option fields are provably never populated together.

// Flagged: two construction sites, each sets exactly one field to Some.
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

// Fine: one site sets both, so the fields are independent.
struct Both {
    a: Option<u32>,
    b: Option<u32>,
}

fn both() -> Both {
    Both {
        a: Some(1),
        b: Some(2),
    }
}

fn neither() -> Both {
    Both { a: None, b: None }
}

// Fine: a later field assignment makes construction sites unprovable.
struct Assigned {
    x: Option<u32>,
    y: Option<u32>,
}

fn build_then_set() -> Assigned {
    let mut s = Assigned {
        x: Some(1),
        y: None,
    };
    s.y = Some(2);
    s
}

fn other_site() -> Assigned {
    Assigned {
        x: None,
        y: Some(3),
    }
}

// Fine: a field initialized from a variable is unprovable.
struct FromVar {
    x: Option<u32>,
    y: Option<u32>,
}

fn from_var(v: Option<u32>) -> FromVar {
    FromVar { x: v, y: None }
}

fn from_var_other() -> FromVar {
    FromVar {
        x: None,
        y: Some(1),
    }
}

// Fine: a single construction site is not a pattern.
struct OneSite {
    x: Option<u32>,
    y: Option<u32>,
}

fn one_site() -> OneSite {
    OneSite {
        x: Some(1),
        y: None,
    }
}

// Fine: the later assignment goes through a Box, which is still a write to
// the field.
struct Boxed {
    x: Option<u32>,
    y: Option<u32>,
}

fn boxed_x() -> Box<Boxed> {
    Box::new(Boxed {
        x: Some(1),
        y: None,
    })
}

fn boxed_y() -> Box<Boxed> {
    Box::new(Boxed {
        x: None,
        y: Some(2),
    })
}

fn reopen(b: &mut Box<Boxed>) {
    b.y = Some(3);
}

fn main() {
    let _ = success();
    let _ = failure();
    let _ = both();
    let _ = neither();
    let _ = build_then_set();
    let _ = other_site();
    let _ = from_var(Some(1));
    let _ = from_var_other();
    let _ = one_site();
    let mut b = boxed_x();
    reopen(&mut b);
    let _ = boxed_y();
}

// Flagged: the same shape, with the constructors spelled through a type alias
// and through the qualified type.
type Slot = Option<u32>;

struct Spelled {
    left: Option<u32>,
    right: Option<u32>,
}

fn spelled_left() -> Spelled {
    Spelled {
        left: Slot::Some(1),
        right: <Option<u32>>::None,
    }
}

fn spelled_right() -> Spelled {
    Spelled {
        left: Slot::None,
        right: <Option<u32>>::Some(2),
    }
}

// Flagged: a derived `Clone` or `Default` is not a construction that matters.
#[derive(Clone, Default)]
struct DerivedOutcome {
    ok: Option<u32>,
    err: Option<u32>,
}

fn derived_success() -> DerivedOutcome {
    DerivedOutcome {
        ok: Some(1),
        err: None,
    }
}

fn derived_failure() -> DerivedOutcome {
    DerivedOutcome {
        ok: None,
        err: Some(2),
    }
}

// Silent: an `allow` on the struct applies to its finding.
#[allow(options_as_enum)]
struct AllowedOutcome {
    ok: Option<u32>,
    err: Option<u32>,
}

fn allowed_success() -> AllowedOutcome {
    AllowedOutcome {
        ok: Some(1),
        err: None,
    }
}

fn allowed_failure() -> AllowedOutcome {
    AllowedOutcome {
        ok: None,
        err: Some(2),
    }
}

// Flagged: `Outcome` again with the common derives. A derived `Clone` copies
// and a derived `Default` sets both to `None`, so neither changes the answer.
#[derive(Clone, Debug, Default)]
struct OutcomeTwin {
    ok: Option<u32>,
    err: Option<String>,
}

fn twin_success() -> OutcomeTwin {
    OutcomeTwin {
        ok: Some(1),
        err: None,
    }
}

fn twin_failure() -> OutcomeTwin {
    OutcomeTwin {
        ok: None,
        err: Some("boom".to_owned()),
    }
}

// Silent: `insert` takes `&mut` to `ok` and can set it beside `err`.
struct Inserted {
    ok: Option<u32>,
    err: Option<u32>,
}

fn inserted_ok() -> Inserted {
    Inserted {
        ok: Some(1),
        err: None,
    }
}

fn inserted_err() -> Inserted {
    let mut i = Inserted {
        ok: None,
        err: Some(2),
    };
    i.ok.insert(3);
    i
}

// Silent: `..base` copies fields whose values are not known here.
struct Copied {
    ok: Option<u32>,
    err: Option<u32>,
}

fn copied_ok() -> Copied {
    Copied {
        ok: Some(1),
        err: None,
    }
}

fn copied_err(base: Copied) -> Copied {
    Copied {
        err: Some(2),
        ..base
    }
}

// Flagged: `..Default::default()` of a derived `Default` fills in `None`.
#[derive(Default)]
struct Filled {
    ok: Option<u32>,
    err: Option<u32>,
}

fn filled_ok() -> Filled {
    Filled {
        ok: Some(1),
        ..Default::default()
    }
}

fn filled_err() -> Filled {
    Filled {
        err: Some(2),
        ..Default::default()
    }
}
