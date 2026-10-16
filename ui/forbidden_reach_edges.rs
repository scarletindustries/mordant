// The test mordant.toml bans `Vec::push` from `trait_root`, `drop_root`,
// `closure_root` and `generic_root`. These pin the edges the walk follows
// beyond plain calls: trait calls whose impl the types pick, drops, and the
// bodies of closures a function builds.

trait Store {
    fn put(&mut self, n: u32);
}

struct Pushes(Vec<u32>);

impl Store for Pushes {
    fn put(&mut self, n: u32) {
        self.0.push(n);
    }
}

// Flagged: `put` on a `Pushes` can only be the impl above.
fn trait_root(s: &mut Pushes) {
    s.put(1);
}

struct Logs(Vec<u32>);

impl Drop for Logs {
    fn drop(&mut self) {
        self.0.push(0);
    }
}

struct Holder {
    logs: Logs,
}

// Flagged: dropping a `Holder` drops its `Logs`, whose `Drop` impl pushes.
fn drop_root(h: Holder) {
    let _ = h.logs.0.len();
}

// Flagged: the closure body is part of what this function can run.
fn closure_root(v: &mut Vec<u32>) {
    let mut add = |n| v.push(n);
    add(1);
}

// Fine: which `put` runs depends on `S`, so the call does not resolve and the
// walk does not guess.
fn generic_root<S: Store>(s: &mut S) {
    s.put(1);
}

fn main() {
    trait_root(&mut Pushes(Vec::new()));
    drop_root(Holder {
        logs: Logs(Vec::new()),
    });
    closure_root(&mut Vec::new());
    generic_root(&mut Pushes(Vec::new()));
}
