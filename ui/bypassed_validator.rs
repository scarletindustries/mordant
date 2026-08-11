// A literal that bypasses a validating constructor from outside the type's
// module is flagged. Literals in the type's own module or impls, and types
// with no validator, are not.

mod port {
    pub struct Port {
        pub(crate) n: u16,
    }

    impl Port {
        pub fn new(n: u32) -> Result<Port, ()> {
            // Inside the type's impl: this literal IS the validator.
            if n <= u16::MAX as u32 {
                Ok(Port { n: n as u16 })
            } else {
                Err(())
            }
        }
    }

    // Same module as the type: the author wrote this, so it is not a bypass.
    pub fn well_known() -> Port {
        Port { n: 443 }
    }
}

use port::Port;

impl Default for Port {
    fn default() -> Self {
        // Trait impls of the type count as its own code even from here.
        Port { n: 80 }
    }
}

fn bypass_is_flagged() -> Port {
    Port { n: 0 }
}

struct Free {
    n: u16,
}

fn no_validator_is_fine() -> Free {
    Free { n: 0 }
}

// A method with a receiver is not a constructor: `parent` navigates and
// `try_clone` copies a value that already exists. Neither makes `Node`
// validated, so its pub field is fine.
pub struct Node {
    pub depth: u8,
    up: Option<Box<Node>>,
}

impl Node {
    pub fn parent(&self) -> Option<&Node> {
        self.up.as_deref()
    }

    pub fn try_clone(&self) -> Result<Node, ()> {
        if self.up.is_some() { Err(()) } else { Ok(Node { depth: self.depth, up: None }) }
    }
}

// A receiver-less lookup returning `Option<&Self>` hands back a row of an
// existing table; it validates nothing, so the pub fields are fine.
pub struct Alias {
    pub from: &'static str,
    pub to: &'static str,
}

static ALIASES: [Alias; 2] = [
    Alias { from: "fs", to: "node:fs" },
    Alias { from: "path", to: "node:path" },
];

impl Alias {
    pub fn get(name: &str) -> Option<&'static Alias> {
        ALIASES.iter().find(|a| a.from == name)
    }
}

mod inner {
    // Validated, but the field is pub: any holder assigns around the check.
    pub struct Level {
        pub value: u8,
    }

    impl Level {
        pub fn new(v: u8) -> Result<Level, ()> {
            if v <= 10 { Ok(Level { value: v }) } else { Err(()) }
        }
    }

    // Validated with a private field; nothing to flag.
    pub struct Sealed {
        value: u8,
    }

    impl Sealed {
        pub fn new(v: u8) -> Result<Sealed, ()> {
            if v <= 10 { Ok(Sealed { value: v }) } else { Err(()) }
        }

        pub fn value(&self) -> u8 {
            self.value
        }
    }
}

fn main() {
    let _ = Port::new(80).map(|p| p.n);
    let _ = Port::default();
    let _ = port::well_known();
    let _ = bypass_is_flagged();
    let _ = no_validator_is_fine();
    let _ = Free { n: 1 };
    let _ = Node { depth: 0, up: None }.parent().map(|n| n.try_clone());
    let _ = Alias::get("fs").map(|a| a.to);
    let _ = inner::Level::new(3);
    let _ = inner::Sealed::new(3).map(|s| s.value());
}
