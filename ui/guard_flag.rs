// A bool field guarded at the entry of 2+ methods is a runtime ordering
// invariant; one guard alone is not a pattern.

struct Conn {
    ready: bool,
    sent: u32,
}

impl Conn {
    fn send(&mut self) {
        if !self.ready {
            return;
        }
        self.sent += 1;
    }

    fn flush(&mut self) {
        if !self.ready {
            return;
        }
        self.sent = 0;
    }

    fn reset(&mut self) {
        self.sent = 0;
    }
}

struct Once {
    armed: bool,
    fired: u32,
}

impl Once {
    fn fire(&mut self) {
        if self.armed {
            return;
        }
        self.fired += 1;
    }
}

fn main() {
    let mut c = Conn {
        ready: true,
        sent: 0,
    };
    c.send();
    c.flush();
    c.reset();

    let mut o = Once {
        armed: false,
        fired: 0,
    };
    o.fire();
}
