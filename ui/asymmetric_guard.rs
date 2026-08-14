// A permission-flavored guard must read everything its gated action touches.

struct Sched {
    queue: Vec<u32>,
    conns: Vec<u32>,
}

impl Sched {
    fn can_donate(&self) -> bool {
        self.queue.is_empty()
    }

    fn can_flush(&self) -> bool {
        self.queue.is_empty() && self.conns.is_empty()
    }

    // Flagged: `detach` touches `conns`, which `can_donate` never reads.
    fn donate(&mut self) {
        if !self.can_donate() {
            return;
        }
        self.detach();
    }

    // Fine: `can_flush` reads both fields `detach` touches.
    fn flush(&mut self) {
        if self.can_flush() {
            self.detach();
        }
    }

    fn detach(&mut self) {
        self.queue.clear();
        self.conns = Vec::new();
    }

    // Flagged: the gated call's result is let-bound, the shape the real
    // donation bug used.
    fn donate_binding(&mut self) -> usize {
        if !self.can_donate() {
            return 0;
        }
        let n = self.drain();
        n
    }

    fn drain(&mut self) -> usize {
        self.conns = Vec::new();
        self.conns.len()
    }

    // Fine: the gated call does not mutate.
    fn report(&mut self) {
        if !self.can_donate() {
            return;
        }
        let _ = self.count();
    }

    fn count(&self) -> usize {
        self.queue.len() + self.conns.len()
    }

    // Fine: `detach` sits under a condition of its own; `can_donate` opened the
    // method, it did not approve this call.
    fn donate_when_idle(&mut self) {
        if !self.can_donate() {
            return;
        }
        if self.conns.is_empty() {
            self.detach();
        }
    }

    // Fine: same, in the then-branch form.
    fn donate_if_idle(&mut self) {
        if self.can_donate() {
            if self.conns.is_empty() {
                self.detach();
            }
        }
    }
}

fn main() {
    let mut s = Sched {
        queue: vec![1],
        conns: vec![2],
    };
    let _ = s.can_donate();
    let _ = s.can_flush();
    s.donate();
    let _ = s.donate_binding();
    s.flush();
    s.report();
    s.donate_when_idle();
    s.donate_if_idle();
    s.detach();
}
