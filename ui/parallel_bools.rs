// Bool fields only ever assigned together encode one state.

// Flagged: running/done are co-assigned in two methods and nowhere else.
struct Task {
    running: bool,
    done: bool,
    retries: u32,
}

impl Task {
    fn start(&mut self) {
        self.running = true;
        self.done = false;
    }

    fn finish(&mut self) {
        self.running = false;
        self.done = true;
    }
}

// Fine: `a` has a lone write, so the fields are independent.
struct Independent {
    a: bool,
    b: bool,
}

impl Independent {
    fn set_a(&mut self) {
        self.a = true;
    }

    fn set_both(&mut self) {
        self.a = true;
        self.b = true;
    }
}

fn main() {
    let mut t = Task {
        running: false,
        done: false,
        retries: 0,
    };
    t.start();
    t.finish();
    t.retries += 1;

    let mut i = Independent { a: false, b: false };
    i.set_a();
    i.set_both();
}
