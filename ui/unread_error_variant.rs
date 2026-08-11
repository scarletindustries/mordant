// A private enum variant that is constructed but never named by a pattern
// outside the enum's own impls carries structure nobody reads.

use std::fmt;

#[derive(Debug)]
enum LoadError {
    NotFound,
    Corrupt(String),
}

impl fmt::Display for LoadError {
    // The enum's own impls must match every variant; this does not count as
    // the crate reading the structure.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LoadError::NotFound => write!(f, "not found"),
            LoadError::Corrupt(why) => write!(f, "corrupt: {why}"),
        }
    }
}

fn load(x: u32) -> Result<u32, LoadError> {
    match x {
        0 => Err(LoadError::NotFound),
        1 => Err(LoadError::Corrupt("bad header".to_owned())),
        n => Ok(n),
    }
}

fn handle(x: u32) -> u32 {
    match load(x) {
        Ok(n) => n,
        // NotFound is distinguished; Corrupt only ever reaches the catch-all,
        // so its payload is never read and the variant is flagged.
        Err(LoadError::NotFound) => 0,
        Err(other) => {
            eprintln!("{other}");
            0
        }
    }
}

// Every variant is distinguished somewhere; nothing to flag.
enum Poll {
    Ready(u32),
    Pending,
}

fn drain(p: Poll) -> u32 {
    match p {
        Poll::Ready(n) => n,
        Poll::Pending => 0,
    }
}

// Fine: an inherent accessor is the crate genuinely reading the structure,
// unlike a trait impl, so both variants count as named.
enum Ceiling {
    Budget,
    Tripwire(u64),
}

impl Ceiling {
    fn tenths(&self, budget: u64) -> u64 {
        match self {
            Ceiling::Budget => budget,
            Ceiling::Tripwire(t) => *t,
        }
    }
}

fn resolve(budget: u64) -> u64 {
    Ceiling::Budget.tenths(budget) + Ceiling::Tripwire(3).tenths(budget)
}

// No pattern anywhere names any variant, so matching is not how this enum is
// consumed and the lint stays silent about all of it.
enum Trace {
    Enter,
    Exit,
}

impl fmt::Display for Trace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Trace::Enter => write!(f, "enter"),
            Trace::Exit => write!(f, "exit"),
        }
    }
}

fn trace() {
    println!("{} {}", Trace::Enter, Trace::Exit);
}

fn main() {
    let _ = handle(0);
    let _ = resolve(10);
    let _ = drain(Poll::Ready(1));
    let _ = drain(Poll::Pending);
    trace();
}
