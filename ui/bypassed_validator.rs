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

// Shapes that fail without checking anything they store: none of these make
// their type validated, so the pub fields and outside literals are fine.
pub mod not_validators {
    use std::collections::TryReserveError;

    // Parser: fails on the *input*, stores the payload it read.
    pub struct Percentage {
        pub v: f32,
    }
    impl Percentage {
        pub fn parse(input: &mut std::str::Chars<'_>) -> Option<Percentage> {
            let c = input.next()?;
            let d = c.to_digit(10)?;
            if input.next() != Some('%') {
                return None;
            }
            Some(Percentage { v: d as f32 / 100.0 })
        }
    }

    // `?` on a fallible conversion: the narrower field type carries the
    // guarantee, not this function.
    pub struct Narrow {
        pub n: u16,
    }
    impl Narrow {
        pub fn new(n: u32) -> Result<Narrow, std::num::TryFromIntError> {
            Ok(Narrow { n: <u16 as core::convert::TryFrom<u32>>::try_from(n)? })
        }
    }

    // Resource failure: allocation says nothing about `cap`.
    pub struct Buf {
        pub bytes: Vec<u8>,
        pub cap: usize,
    }
    impl Buf {
        pub fn with_capacity(cap: usize) -> Result<Buf, TryReserveError> {
            let mut bytes = Vec::new();
            bytes.try_reserve(cap)?;
            Ok(Buf { bytes, cap })
        }
    }

    // Checks the input object, then stores things read out of it.
    pub struct Config {
        pub port: u32,
        pub host: String,
    }
    impl Config {
        pub fn from_pairs(pairs: &[(&str, &str)]) -> Result<Config, ()> {
            if pairs.is_empty() {
                return Err(());
            }
            let port = pairs.iter().find(|p| p.0 == "port").ok_or(())?.1.parse().map_err(|_| ())?;
            let host = pairs.iter().find(|p| p.0 == "host").ok_or(())?.1.to_owned();
            Ok(Config { port, host })
        }
    }
}

pub mod not_validators_2 {
    // A `debug_assert!` panics; it is not a failure exit and decides none.
    // A length check on the input slice is not a check on the byte stored.
    pub struct Header {
        pub kq: i32,
        pub tag: u8,
        pub port: u32,
    }
    impl Header {
        pub fn decode(kq: i32, bytes: &[u8], port: u32) -> Result<Header, ()> {
            debug_assert!(kq > -1);
            if bytes.len() < 2 {
                return Err(());
            }
            if port == 0 {
                return Err(());
            }
            Ok(Header { kq, tag: bytes[1], port })
        }
    }

    // `?` on a method of a value only part of which is stored says nothing
    // about that part.
    pub struct Parser {
        pub log: u32,
        pub pos: usize,
    }
    pub struct Lexer {
        pub log: u32,
        pub pos: usize,
    }
    impl Lexer {
        fn next(&mut self) -> Result<(), ()> {
            self.pos += 1;
            if self.pos > 10 { Err(()) } else { Ok(()) }
        }
    }
    impl Parser {
        pub fn init(mut lexer: Lexer) -> Result<Parser, ()> {
            lexer.next()?;
            Ok(Parser { log: lexer.log, pos: 0 })
        }
    }
}

// Shapes that DO check a value they store, in ways a signature can't see.
pub mod validators {
    // Check routed through a helper predicate.
    pub struct Even {
        pub n: u32,
    }
    fn is_even(n: u32) -> bool {
        n % 2 == 0
    }
    impl Even {
        pub fn new(n: u32) -> Option<Even> {
            if !is_even(n) {
                return None;
            }
            Some(Even { n })
        }
    }

    // Read the input first, then reject on the value about to be stored:
    // only `port` is checked, `host` is not.
    pub struct Checked {
        pub port: u32,
        pub host: String,
    }
    impl Checked {
        pub fn from_pairs(pairs: &[(&str, &str)]) -> Result<Checked, ()> {
            let port: u32 = pairs.iter().find(|p| p.0 == "port").ok_or(())?.1.parse().map_err(|_| ())?;
            let host = pairs.iter().find(|p| p.0 == "host").ok_or(())?.1.to_owned();
            if port > 65535 {
                return Err(());
            }
            Ok(Checked { port, host })
        }
    }

    // Check and construction inside a closure.
    pub struct Small {
        pub n: u8,
    }
    impl Small {
        pub fn new(n: Option<u8>) -> Option<Small> {
            n.and_then(|n| if n < 10 { Some(Small { n }) } else { None })
        }
    }

    // `match` with a rejecting arm on the stored value.
    pub struct NonZero {
        pub n: i32,
    }
    impl NonZero {
        pub fn new(n: i32) -> Result<NonZero, ()> {
            match n {
                0 => Err(()),
                n => Ok(NonZero { n }),
            }
        }
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
    let _ = not_validators::Percentage::parse(&mut "5%".chars()).map(|p| p.v);
    let _ = not_validators::Percentage { v: 0.5 };
    let _ = not_validators::Narrow::new(7).map(|n| n.n);
    let _ = not_validators::Buf::with_capacity(4).map(|b| (b.cap, b.bytes.len()));
    let _ = not_validators::Config::from_pairs(&[]).map(|c| (c.port, c.host));
    let _ = not_validators_2::Header::decode(0, &[1, 2], 1).map(|h| (h.kq, h.tag, h.port));
    let _ = not_validators_2::Parser::init(not_validators_2::Lexer { log: 0, pos: 0 }).map(|p| (p.log, p.pos));
    let _ = validators::Even::new(4).map(|e| e.n);
    let _ = validators::Checked::from_pairs(&[]).map(|c| (c.port, c.host));
    let _ = validators::Small::new(Some(3)).map(|s| s.n);
    let _ = validators::NonZero::new(1).map(|z| z.n);
    let _ = validators::NonZero { n: 0 };
}
