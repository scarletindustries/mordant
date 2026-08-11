// A SAFETY comment's backticked names must still exist somewhere: in the
// enclosing function, or as a definition in the crate.

struct Table {
    slots: Vec<u64>,
}

fn stale_name_is_flagged(p: *const u64) -> u64 {
    // SAFETY: `frames_lock` is held for the duration of this read.
    unsafe { *p }
}

fn local_name_is_fine(p: *const u64, guard: &std::sync::Mutex<()>) -> u64 {
    let _held = guard.lock().unwrap();
    // SAFETY: `_held` keeps `guard` locked while the pointer is read.
    unsafe { *p }
}

fn crate_item_is_fine(p: *const u64) -> u64 {
    // SAFETY: every pointer handed here indexes a live `Table`'s `slots`.
    unsafe { *p }
}

fn no_backticks_is_fine(p: *const u64) -> u64 {
    // SAFETY: single-threaded test harness; nothing else writes this cell.
    unsafe { *p }
}

fn main() {
    let x = 7u64;
    let t = Table { slots: vec![x] };
    let m = std::sync::Mutex::new(());
    let _ = stale_name_is_flagged(&t.slots[0]);
    let _ = local_name_is_fine(&t.slots[0], &m);
    let _ = crate_item_is_fine(&t.slots[0]);
    let _ = no_backticks_is_fine(&t.slots[0]);
}
