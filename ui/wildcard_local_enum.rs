// Wildcard arms over small crate-local enums absorb future variants.

enum Op {
    Add,
    Sub,
    Mul,
}

fn wild_is_flagged(o: Op) -> i32 {
    match o {
        Op::Add => 1,
        _ => 0,
    }
}

fn binding_is_flagged(o: Op) -> i32 {
    match o {
        Op::Add => 1,
        other => cost(other),
    }
}

fn cost(_o: Op) -> i32 {
    2
}

fn exhaustive_is_fine(o: Op) -> i32 {
    match o {
        Op::Add => 1,
        Op::Sub => 2,
        Op::Mul => 3,
    }
}

// Extractors ask "is it this one shape?"; future variants are correctly not
// that shape, so `_ => None` and `_ => false` stay silent.
fn extractor_none_is_fine(o: &Op) -> Option<i32> {
    match o {
        Op::Add => Some(1),
        _ => None,
    }
}

fn extractor_false_is_fine(o: &Op) -> bool {
    match o {
        Op::Add => true,
        _ => false,
    }
}

fn foreign_enum_is_fine(x: Option<u32>) -> u32 {
    match x {
        Some(v) => v,
        _ => 0,
    }
}

fn non_enum_is_fine(x: u32) -> u32 {
    match x {
        1 => 2,
        _ => 0,
    }
}

fn main() {
    let _ = wild_is_flagged(Op::Add);
    let _ = binding_is_flagged(Op::Sub);
    let _ = exhaustive_is_fine(Op::Mul);
    let _ = extractor_none_is_fine(&Op::Add);
    let _ = extractor_false_is_fine(&Op::Sub);
    let _ = foreign_enum_is_fine(Some(1));
    let _ = non_enum_is_fine(1);
}
