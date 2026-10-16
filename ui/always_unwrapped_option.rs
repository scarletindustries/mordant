// An Option field that every reader unwraps and no reader handles is a state
// nobody survives; one handled read anywhere silences the field.

struct Conn {
    sock: Option<u32>,
    tag: Option<u32>,
    alt: Option<u32>,
}

impl Conn {
    fn new() -> Conn {
        Conn {
            sock: None,
            tag: None,
            alt: None,
        }
    }

    fn ready(&self) -> u32 {
        self.sock.unwrap()
    }

    fn doubled(&self) -> u32 {
        self.sock.as_ref().unwrap() + 1
    }

    // `tag` has an unwrap too, but also a reader that handles None.
    fn tag_or_zero(&self) -> u32 {
        self.tag.unwrap_or(0)
    }

    fn tag_forced(&self) -> u32 {
        self.tag.unwrap()
    }

    // A single unwrap is not a pattern.
    fn alt_once(&self) -> u32 {
        self.alt.expect("set at startup")
    }
}

fn main() {
    let mut c = Conn::new();
    c.sock = Some(1);
    c.tag = Some(2);
    c.alt = Some(3);
    let _ = c.ready() + c.doubled() + c.tag_or_zero() + c.tag_forced() + c.alt_once();
}

// Flagged: a derived `Debug` or `Clone` reads the field but does not handle `None`.
#[derive(Debug, Clone)]
struct Derived {
    sock: Option<u32>,
}

impl Derived {
    fn ready(&self) -> u32 {
        self.sock.unwrap()
    }

    fn doubled(&self) -> u32 {
        self.sock.unwrap() + 1
    }
}

// Silent: an `allow` on the struct applies to its finding.
#[allow(always_unwrapped_option)]
struct Allowed {
    sock: Option<u32>,
}

impl Allowed {
    fn ready(&self) -> u32 {
        self.sock.unwrap()
    }

    fn doubled(&self) -> u32 {
        self.sock.unwrap() + 1
    }
}

fn derived_and_allowed() -> u32 {
    let d = Derived { sock: Some(1) };
    let a = Allowed { sock: Some(1) };
    d.clone().ready() + d.doubled() + a.ready() + a.doubled()
}
