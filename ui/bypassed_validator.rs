// A literal that bypasses a validating constructor is flagged. Literals
// inside the type's own impls, and types with no validator, are not.

struct Port {
    n: u16,
}

impl Port {
    fn new(n: u32) -> Result<Port, ()> {
        // Inside the type's impl: this literal IS the validator.
        if n <= u16::MAX as u32 {
            Ok(Port { n: n as u16 })
        } else {
            Err(())
        }
    }
}

impl Default for Port {
    fn default() -> Self {
        // Trait impls of the type still count as the type's own code.
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
    let _ = Port::new(80);
    let _ = Port::default();
    let _ = bypass_is_flagged();
    let _ = no_validator_is_fine();
    let _ = Free { n: 1 };
    let _ = inner::Level::new(3);
    let _ = inner::Sealed::new(3).map(|s| s.value());
}
