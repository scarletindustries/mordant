// A bool field written only beside an Option field, `true` with `Some`, is that Option's `is_some()`.

// Flagged: `connected` is `peer.is_some()`: false/None at construction,
// true/Some in `open`, false/None in `close`, and nowhere else.
struct Conn {
    peer: Option<String>,
    connected: bool,
    bytes: u64,
}

impl Conn {
    fn new() -> Self {
        Conn {
            peer: None,
            connected: false,
            bytes: 0,
        }
    }

    fn open(&mut self, peer: String) {
        self.peer = Some(peer);
        self.connected = true;
    }

    fn close(&mut self) {
        self.connected = false;
        self.peer = None;
        self.bytes = 0;
    }

    fn peer(&self) -> Option<&str> {
        self.peer.as_deref()
    }

    fn is_up(&self) -> bool {
        self.connected
    }
}

// Flagged: the reverse polarity, `stale` is `fresh.is_none()`; reading the
// Option through `as_mut` cannot change that.
struct Cache {
    fresh: Option<u32>,
    stale: bool,
}

impl Cache {
    fn filled(v: u32) -> Self {
        Cache {
            fresh: Some(v),
            stale: false,
        }
    }

    fn invalidate(&mut self) {
        self.fresh = None;
        self.stale = true;
    }

    fn bump(&mut self) {
        if let Some(v) = self.fresh.as_mut() {
            *v += 1;
        }
    }
}

// Fine: `done` is also set on its own, so it is not `error.is_some()`.
struct Scan {
    error: Option<String>,
    done: bool,
}

impl Scan {
    fn new() -> Self {
        Scan {
            error: None,
            done: false,
        }
    }

    fn fail(&mut self, e: String) {
        self.error = Some(e);
        self.done = true;
    }

    fn finish(&mut self) {
        self.done = true;
    }
}

// Fine: the Option is drained with `take()`, a write the flag does not follow.
struct Exit {
    status: Option<i32>,
    exited: bool,
}

impl Exit {
    fn new() -> Self {
        Exit {
            status: None,
            exited: false,
        }
    }

    fn on_exit(&mut self, code: i32) {
        self.exited = true;
        self.status = Some(code);
    }

    fn reap(&mut self) -> Option<i32> {
        self.status.take()
    }
}

// Fine: the flag is written from a computed value once.
struct Probe {
    addr: Option<u32>,
    reachable: bool,
}

impl Probe {
    fn new() -> Self {
        Probe {
            addr: None,
            reachable: false,
        }
    }

    fn resolve(&mut self, addr: u32, ok: bool) {
        self.addr = Some(addr);
        self.reachable = ok;
    }
}

// Fine: the flag is only ever `false`; a constant is not a copy of anything.
struct Idle {
    job: Option<u32>,
    busy: bool,
}

impl Idle {
    fn new() -> Self {
        Idle {
            job: None,
            busy: false,
        }
    }

    fn park(&mut self) {
        self.job = None;
        self.busy = false;
    }
}

// Fine: the two writes are in different blocks, so one can run without the other.
struct Lazy {
    value: Option<u32>,
    loaded: bool,
}

impl Lazy {
    fn new() -> Self {
        Lazy {
            value: None,
            loaded: false,
        }
    }

    fn load(&mut self, v: Option<u32>) {
        self.loaded = true;
        if let Some(v) = v {
            self.value = Some(v);
        }
    }
}

// Fine: exported fields can be written by other crates.
pub struct Public {
    pub handle: Option<u32>,
    pub attached: bool,
}

impl Public {
    pub fn new() -> Self {
        Public {
            handle: None,
            attached: false,
        }
    }

    pub fn attach(&mut self, h: u32) {
        self.handle = Some(h);
        self.attached = true;
    }
}

fn main() {
    let mut c = Conn::new();
    c.open("p".to_owned());
    let _ = (c.peer(), c.is_up());
    c.close();
    c.bytes += 1;

    let mut k = Cache::filled(1);
    k.bump();
    k.invalidate();
    let _ = k.stale;

    let mut s = Scan::new();
    s.fail("e".to_owned());
    s.finish();
    let _ = (s.done, s.error.is_some());

    let mut e = Exit::new();
    e.on_exit(0);
    let _ = (e.exited, e.reap());

    let mut p = Probe::new();
    p.resolve(1, true);
    let _ = (p.reachable, p.addr);

    let mut i = Idle::new();
    i.park();
    let _ = (i.busy, i.job);

    let mut l = Lazy::new();
    l.load(Some(1));
    let _ = (l.loaded, l.value);

    let mut u = Public::new();
    u.attach(1);
    let _ = (u.attached, u.handle);
}
