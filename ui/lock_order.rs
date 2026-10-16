// Two locks taken in both orders across a crate is a deadlock waiting for
// the interleaving; one consistent order is fine.

use std::sync::Mutex;

struct S {
    a: Mutex<u32>,
    b: Mutex<u32>,
}

impl S {
    fn ab(&self) -> u32 {
        let ga = self.a.lock().unwrap();
        let gb = self.b.lock().unwrap();
        *ga + *gb
    }

    // Flagged together with `ab`: the reverse order.
    fn ba(&self) -> u32 {
        let gb = self.b.lock().unwrap();
        let ga = self.a.lock().unwrap();
        *ga + *gb
    }

    // Fine: the first guard is dropped before the second lock.
    fn sequential(&self) -> u32 {
        let ga = self.a.lock().unwrap();
        let v = *ga;
        drop(ga);
        let gb = self.b.lock().unwrap();
        v + *gb
    }
}

struct T {
    c: Mutex<u32>,
    d: Mutex<u32>,
}

impl T {
    // Fine: both sites agree on the order.
    fn first(&self) -> u32 {
        let gc = self.c.lock().unwrap();
        let gd = self.d.lock().unwrap();
        *gc + *gd
    }

    fn second(&self) -> u32 {
        let gc = self.c.lock().unwrap();
        let gd = self.d.lock().unwrap();
        *gc * *gd
    }
}

struct Pool {
    x: Mutex<u32>,
    y: Mutex<u32>,
}

impl Pool {
    fn xy(&self) -> u32 {
        let gx = self.x.lock().unwrap();
        let gy = self.y.lock().unwrap();
        *gx + *gy
    }
}

struct Cache {
    x: Mutex<u32>,
    y: Mutex<u32>,
}

impl Cache {
    // Fine: `Cache::y` and `Cache::x` are not `Pool::x` and `Pool::y`; a
    // field name shared by two types is not one lock.
    fn yx(&self) -> u32 {
        let gy = self.y.lock().unwrap();
        let gx = self.x.lock().unwrap();
        *gx + *gy
    }
}

struct Deferred {
    e: Mutex<u32>,
    f: Mutex<u32>,
}

impl Deferred {
    fn ef(&self) -> u32 {
        let ge = self.e.lock().unwrap();
        let gf = self.f.lock().unwrap();
        *ge + *gf
    }

    // Fine: the reverse order is only inside a closure built while `f` is
    // held; here it runs after the drop, and in general when its holder says.
    fn fe_later(&self) -> u32 {
        let gf = self.f.lock().unwrap();
        let job = || *self.e.lock().unwrap();
        let v = *gf;
        drop(gf);
        v + job()
    }
}

struct Spin {
    g: Mutex<u32>,
    h: Mutex<u32>,
}

impl Spin {
    fn gh(&self) -> u32 {
        let gg = self.g.lock().unwrap();
        let gh = self.h.lock().unwrap();
        *gg + *gh
    }

    // Flagged together with `gh`: the reverse order sits directly in a
    // `loop {}` body, which is a block of its own.
    fn hg_in_loop(&self) -> u32 {
        loop {
            let gh = self.h.lock().unwrap();
            let gg = self.g.lock().unwrap();
            if *gg > 0 {
                return *gg + *gh;
            }
        }
    }
}

// Flagged: the second lock is taken in a callee. `Calls::b_then_a` takes the
// other order directly.
struct Calls {
    a: Mutex<u32>,
    b: Mutex<u32>,
}

impl Calls {
    fn a_then_b(&self) -> u32 {
        let ga = self.a.lock().unwrap();
        *ga + self.read_b()
    }

    fn read_b(&self) -> u32 {
        *self.b.lock().unwrap()
    }

    fn b_then_a(&self) -> u32 {
        let gb = self.b.lock().unwrap();
        let ga = self.a.lock().unwrap();
        *ga + *gb
    }
}

// Flagged: three locks in a cycle, no two of them in both orders.
struct Ring {
    x: Mutex<u32>,
    y: Mutex<u32>,
    z: Mutex<u32>,
}

impl Ring {
    fn xy(&self) -> u32 {
        let gx = self.x.lock().unwrap();
        let gy = self.y.lock().unwrap();
        *gx + *gy
    }

    fn yz(&self) -> u32 {
        let gy = self.y.lock().unwrap();
        let gz = self.z.lock().unwrap();
        *gy + *gz
    }

    fn zx(&self) -> u32 {
        let gz = self.z.lock().unwrap();
        let gx = self.x.lock().unwrap();
        *gz + *gx
    }
}

// Flagged: two statics in both orders.
static FIRST: Mutex<u32> = Mutex::new(0);
static SECOND: Mutex<u32> = Mutex::new(0);

fn first_then_second() -> u32 {
    let g1 = FIRST.lock().unwrap();
    let g2 = SECOND.lock().unwrap();
    *g1 + *g2
}

fn second_then_first() -> u32 {
    let g2 = SECOND.lock().unwrap();
    let g1 = FIRST.lock().unwrap();
    *g1 + *g2
}

// A lock with no guard, named by the test config's `lock-methods` and
// `unlock-methods`.
struct RawLock;

impl RawLock {
    fn lock(&self) {}
    fn unlock(&self) {}
}

struct Raw {
    p: RawLock,
    q: RawLock,
}

impl Raw {
    // Flagged together with `qp`.
    fn pq(&self) {
        self.p.lock();
        self.q.lock();
        self.q.unlock();
        self.p.unlock();
    }

    fn qp(&self) {
        self.q.lock();
        self.p.lock();
        self.p.unlock();
        self.q.unlock();
    }

    // Fine: `q` is unlocked before `p` is taken.
    fn q_then_p(&self) {
        self.q.lock();
        self.q.unlock();
        self.p.lock();
        self.p.unlock();
    }
}

// A guard type named by the test config's `lock-guard-types`.
struct Held;

struct Cell;

impl Cell {
    fn hold(&self) -> Held {
        Held
    }
}

struct Cells {
    m: Cell,
    n: Cell,
}

impl Cells {
    // Flagged together with `nm`, through a helper that returns the guard.
    fn mn(&self) {
        let _gm = self.m.hold();
        let _gn = self.hold_n();
    }

    fn hold_n(&self) -> Held {
        self.n.hold()
    }

    fn nm(&self) {
        let _gn = self.n.hold();
        let _gm = self.m.hold();
    }
}

trait Visit {
    fn visit(&self);
}

struct Generic {
    u: Mutex<u32>,
    v: Mutex<u32>,
}

impl Generic {
    fn uv(&self) {
        let _gu = self.u.lock().unwrap();
        let _gv = self.v.lock().unwrap();
    }

    // Fine: what `visit` locks depends on `V`, so the call is not followed.
    fn v_then_visit<V: Visit>(&self, other: &V) {
        let _gv = self.v.lock().unwrap();
        other.visit();
    }
}

impl Visit for Generic {
    fn visit(&self) {
        let _gu = self.u.lock().unwrap();
    }
}

struct RawPair {
    s: RawLock,
    t: RawLock,
}

struct Handle {
    pair: *const RawPair,
}

impl Handle {
    fn pair(&self) -> &RawPair {
        // SAFETY: the test never builds a dangling handle.
        unsafe { &*self.pair }
    }
}

impl RawPair {
    fn st(&self) {
        self.s.lock();
        self.t.lock();
        self.t.unlock();
        self.s.unlock();
    }
}

// Flagged together with `RawPair::st`: `pair` is read once into a local, so
// each lock and unlock acts on the same `RawPair`.
fn ts_through_handle(h: &Handle) {
    let pair = h.pair();
    pair.t.lock();
    pair.s.lock();
    pair.s.unlock();
    pair.t.unlock();
}

struct RawTrio {
    u: RawLock,
    w: RawLock,
}

impl RawTrio {
    fn uw(&self) {
        self.u.lock();
        self.w.lock();
        self.w.unlock();
        self.u.unlock();
    }
}

// Fine: the `w` unlocked may not be the `w` locked, so nothing says how
// long `a.w` is held.
fn wu_unclear(a: &RawTrio, b: &RawTrio) {
    a.w.lock();
    b.w.unlock();
    a.u.lock();
    a.u.unlock();
}

fn main() {
    let s = S {
        a: Mutex::new(1),
        b: Mutex::new(2),
    };
    let _ = s.ab() + s.ba() + s.sequential();
    let t = T {
        c: Mutex::new(3),
        d: Mutex::new(4),
    };
    let _ = t.first() + t.second();
    let p = Pool {
        x: Mutex::new(5),
        y: Mutex::new(6),
    };
    let c = Cache {
        x: Mutex::new(7),
        y: Mutex::new(8),
    };
    let _ = p.xy() + c.yx();
    let d = Deferred {
        e: Mutex::new(9),
        f: Mutex::new(10),
    };
    let _ = d.ef() + d.fe_later();
    let sp = Spin {
        g: Mutex::new(11),
        h: Mutex::new(12),
    };
    let _ = sp.gh() + sp.hg_in_loop();
    let c = Calls {
        a: Mutex::new(1),
        b: Mutex::new(2),
    };
    let _ = c.a_then_b() + c.b_then_a();
    let r = Ring {
        x: Mutex::new(1),
        y: Mutex::new(2),
        z: Mutex::new(3),
    };
    let _ = r.xy() + r.yz() + r.zx();
    let _ = first_then_second() + second_then_first();
    let raw = Raw {
        p: RawLock,
        q: RawLock,
    };
    raw.pq();
    raw.qp();
    raw.q_then_p();
    let cells = Cells { m: Cell, n: Cell };
    cells.mn();
    cells.nm();
    let g = Generic {
        u: Mutex::new(1),
        v: Mutex::new(2),
    };
    g.uv();
    g.v_then_visit(&g);
    let pair = RawPair {
        s: RawLock,
        t: RawLock,
    };
    pair.st();
    ts_through_handle(&Handle { pair: &pair });
    let trio = RawTrio {
        u: RawLock,
        w: RawLock,
    };
    trio.uw();
    wu_unclear(&trio, &trio);
}
