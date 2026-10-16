// A lock taken again by the thread that holds it waits for itself forever.

use std::sync::{Mutex, RwLock};

struct S {
    a: Mutex<u32>,
    b: Mutex<u32>,
    r: RwLock<u32>,
}

impl S {
    // Flagged: the same field, locked twice in one body.
    fn twice(&self) -> u32 {
        let g1 = self.a.lock().unwrap();
        let g2 = self.a.lock().unwrap();
        *g1 + *g2
    }

    // Flagged: the helper locks the same `self.a` this function holds.
    fn through_helper(&self) -> u32 {
        let g = self.a.lock().unwrap();
        *g + self.read_a()
    }

    fn read_a(&self) -> u32 {
        *self.a.lock().unwrap()
    }

    // Fine: the first guard is dropped first.
    fn dropped_first(&self) -> u32 {
        let g1 = self.a.lock().unwrap();
        let v = *g1;
        drop(g1);
        let g2 = self.a.lock().unwrap();
        v + *g2
    }

    // Fine: two read locks do not wait on each other.
    fn two_reads(&self) -> u32 {
        let r1 = self.r.read().unwrap();
        let r2 = self.r.read().unwrap();
        *r1 + *r2
    }

    // Fine: `try_lock` fails instead of waiting.
    fn try_again(&self) -> u32 {
        let g = self.a.lock().unwrap();
        let again = self.a.try_lock().map(|t| *t).unwrap_or(0);
        *g + again
    }

    // Fine: a fresh guard each time round the loop.
    fn per_iteration(&self) -> u32 {
        let mut sum = 0;
        for _ in 0..3 {
            let g = self.a.lock().unwrap();
            sum += *g;
        }
        sum
    }

    // Fine: two different locks.
    fn two_fields(&self) -> u32 {
        let ga = self.a.lock().unwrap();
        let gb = self.b.lock().unwrap();
        *ga + *gb
    }
}

// Fine: `x` and `y` may be two different values.
fn two_params(x: &S, y: &S) -> u32 {
    let gx = x.a.lock().unwrap();
    let gy = y.a.lock().unwrap();
    *gx + *gy
}

static COUNT: Mutex<u32> = Mutex::new(0);

fn bump() {
    *COUNT.lock().unwrap() += 1;
}

// Flagged: `bump` locks `COUNT`, which this function holds.
fn bump_while_held() {
    let _g = COUNT.lock().unwrap();
    bump();
}

// A lock the test config lists in `reentrant-lock-types`.
struct Shared;
struct Held;

impl Shared {
    fn hold(&self) -> Held {
        Held
    }
}

struct T {
    s: Shared,
}

// Fine: the same thread may take a reentrant lock again.
fn reentrant(t: &T) {
    let _g1 = t.s.hold();
    let _g2 = t.s.hold();
}

static REG: Mutex<u32> = Mutex::new(0);

fn registry() -> &'static Mutex<u32> {
    &REG
}

fn bump_reg() {
    *registry().lock().unwrap() += 1;
}

// Flagged: both locks go through `registry`, which always returns `REG`.
fn through_accessor() {
    let _g = registry().lock().unwrap();
    bump_reg();
}

struct Ticket;

impl Drop for Ticket {
    fn drop(&mut self) {
        bump();
    }
}

// Flagged: dropping the `Ticket` at the end of the block runs its `Drop`,
// which locks `COUNT`.
fn drop_while_held() {
    let _g = COUNT.lock().unwrap();
    {
        let _t = Ticket;
    }
}

// Fine: the `Option` may be `None`, so its drop may run no `Drop` at all.
fn maybe_drop_while_held(t: Option<Ticket>) {
    let _g = COUNT.lock().unwrap();
    drop_later(t);
}

fn drop_later(t: Option<Ticket>) {
    let _ = t;
}

fn main() {
    let s = S {
        a: Mutex::new(1),
        b: Mutex::new(2),
        r: RwLock::new(3),
    };
    let _ = s.twice() + s.through_helper() + s.dropped_first() + s.two_reads();
    let _ = s.try_again() + s.per_iteration() + s.two_fields() + two_params(&s, &s);
    bump_while_held();
    reentrant(&T { s: Shared });
    through_accessor();
    drop_while_held();
    maybe_drop_while_held(None);
}
