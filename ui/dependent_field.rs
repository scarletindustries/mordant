// A field read only under a test of a sibling, and filled with a placeholder otherwise.

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Tag {
    Cmd,
    Pipe,
    Subproc,
}

// Flagged: `raw` is read under `tag == Subproc` (an `if`, a `match` arm and
// a diverging guard) and is null wherever another tag is built.
#[derive(Clone, Copy, Debug)]
struct Child {
    node: u32,
    tag: Tag,
    raw: *mut u8,
}

fn cmd(node: u32) -> Child {
    Child {
        node,
        tag: Tag::Cmd,
        raw: core::ptr::null_mut(),
    }
}

fn subproc(raw: *mut u8) -> Child {
    Child {
        node: 0,
        tag: Tag::Subproc,
        raw,
    }
}

fn dispatch(c: &Child) -> usize {
    if c.tag == Tag::Subproc && c.node == 0 {
        return c.raw as usize;
    }
    match c.tag {
        Tag::Subproc => c.raw as usize,
        Tag::Cmd | Tag::Pipe => c.node as usize,
    }
}

fn captured(c: &Child) -> *mut u8 {
    if c.tag != Tag::Subproc {
        return core::ptr::null_mut();
    }
    c.raw
}

// Flagged: `reject` is only consulted when `request` is set, through a bare
// bool test, a `matches!` and a `let .. else`; the unset construction
// defaults it.
struct Verify {
    request: bool,
    reject: bool,
    depth: u8,
}

fn no_request() -> Verify {
    Verify {
        request: false,
        reject: false,
        depth: 0,
    }
}

fn with_request(reject: bool) -> Verify {
    Verify {
        request: true,
        reject,
        depth: 4,
    }
}

fn apply(v: &Verify) -> u8 {
    let mut out = v.depth;
    if v.request {
        out += u8::from(v.reject);
    }
    if !matches!(v.request, true) {
        return out;
    }
    let true = v.request else { return out };
    out + u8::from(v.reject)
}

// Fine: `len` has an accessor that reads it whatever `kind` is.
#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Inline,
    Heap,
}

struct Buf {
    kind: Kind,
    len: usize,
}

impl Buf {
    fn len(&self) -> usize {
        self.len
    }

    fn heap_len(&self) -> usize {
        if self.kind == Kind::Heap { self.len } else { 0 }
    }
}

fn bufs(n: usize) -> [Buf; 2] {
    [
        Buf {
            kind: Kind::Inline,
            len: 0,
        },
        Buf {
            kind: Kind::Heap,
            len: n,
        },
    ]
}

// Fine: every read is guarded, but the other construction gives `port` a
// real value, so it means something there too.
struct Addr {
    unix: bool,
    port: u16,
}

fn addrs(p: u16) -> [Addr; 3] {
    [
        Addr {
            unix: true,
            port: 0,
        },
        Addr {
            unix: true,
            port: p + 1,
        },
        Addr {
            unix: false,
            port: p,
        },
    ]
}

fn port(a: &Addr) -> u16 {
    if !a.unix { a.port } else { 0 }
}

fn port_again(a: &Addr) -> u16 {
    if a.unix {
        return 0;
    }
    a.port
}

// Fine: the test is on a copy of the sibling, which proves nothing about the
// place the read goes through.
struct Slot {
    live: bool,
    value: u64,
}

fn slots(v: u64) -> [Slot; 2] {
    [
        Slot {
            live: false,
            value: 0,
        },
        Slot {
            live: true,
            value: v,
        },
    ]
}

fn slot_value(s: &Slot) -> u64 {
    let live = s.live;
    if live { s.value } else { 0 }
}

// Fine: a destructuring pattern reads `extra` unconditionally.
struct Opts {
    verbose: bool,
    extra: u32,
}

fn opts(e: u32) -> [Opts; 2] {
    [
        Opts {
            verbose: false,
            extra: 0,
        },
        Opts {
            verbose: true,
            extra: e,
        },
    ]
}

fn extra(o: &Opts) -> u32 {
    if o.verbose {
        return o.extra;
    }
    let Opts { verbose: _, extra } = o;
    *extra
}

// Fine: the reads sit under tests of `tag` against different variants, so no
// single case owns `count`.
struct Tally {
    tag: Tag,
    count: u32,
}

fn tallies(n: u32) -> [Tally; 2] {
    [
        Tally {
            tag: Tag::Cmd,
            count: 0,
        },
        Tally {
            tag: Tag::Pipe,
            count: n,
        },
    ]
}

fn tally(t: &Tally) -> u32 {
    match t.tag {
        Tag::Pipe => t.count,
        Tag::Subproc => t.count + 1,
        Tag::Cmd => 0,
    }
}

// Fine: no `Printer` is ever made or set non-minifying, so `out` is dead
// rather than the payload of a case the crate has.
struct Printer {
    minify: bool,
    out: String,
}

fn printer() -> Printer {
    Printer {
        minify: true,
        out: String::new(),
    }
}

fn newline(p: &mut Printer) {
    if !p.minify {
        p.out.push('\n');
    }
}

// Fine: exported, so other crates read it where they like.
pub struct Public {
    pub on: bool,
    pub data: u32,
}

pub fn public(d: u32) -> [Public; 2] {
    [Public { on: false, data: 0 }, Public { on: true, data: d }]
}

pub fn public_data(p: &Public) -> u32 {
    if p.on { p.data } else { 0 }
}

fn main() {
    let c = cmd(1);
    let s = subproc(core::ptr::null_mut());
    let _ = (dispatch(&c), dispatch(&s), captured(&c), format!("{c:?}"));
    let _ = (apply(&no_request()), apply(&with_request(true)));
    let [a, b] = bufs(9);
    let _ = (a.len(), b.heap_len());
    let [x, y, z] = addrs(3);
    let _ = (port(&x), port_again(&y), port(&z));
    let [p, q] = slots(5);
    let _ = (slot_value(&p), slot_value(&q));
    let [m, n] = opts(2);
    let _ = (extra(&m), extra(&n));
    let [t, w] = tallies(7);
    let _ = (tally(&t), tally(&w));
    newline(&mut printer());
    let [u, v] = public(1);
    let _ = (public_data(&u), public_data(&v));
}
