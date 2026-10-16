use std::cell::{Cell, RefCell, UnsafeCell};
use std::marker::PhantomData;
use std::ptr::NonNull;
use std::rc::Rc;
use std::sync::Mutex;

// Silent: the only blocker is a raw pointer.
struct OnlyPointer {
    p: *mut u8,
    len: usize,
}
unsafe impl Send for OnlyPointer {}

// Silent: `NonNull` is a pointer too.
struct OnlyNonNull {
    p: NonNull<u8>,
}
unsafe impl Send for OnlyNonNull {}
unsafe impl Sync for OnlyNonNull {}

// Flagged: `count` is a `Cell`, which no raw pointer explains.
struct CellAndPointer {
    p: *const u8,
    count: Cell<u32>,
}
unsafe impl Sync for CellAndPointer {}

// Silent: a `Cell` is `Send`, so only the pointer blocks `Send`.
struct CellSend {
    p: *const u8,
    count: Cell<u32>,
}
unsafe impl Send for CellSend {}

// Flagged: an `Rc` is never `Send`.
struct HoldsRc {
    shared: Rc<String>,
}
unsafe impl Send for HoldsRc {}

// Flagged: the `RefCell` is found inside a nested struct.
struct Inner {
    log: RefCell<Vec<u8>>,
}
struct Outer {
    inner: Inner,
    p: *mut u8,
}
unsafe impl Sync for Outer {}

// Flagged: an `UnsafeCell` beside a mutex the type does not tie it to.
struct Guarded {
    lock: Mutex<()>,
    items: UnsafeCell<Vec<u32>>,
}
unsafe impl Sync for Guarded {}

// Silent: the `Cell` is inside the `Mutex`, whose own impl covers it.
struct InMutex {
    count: Mutex<Cell<u32>>,
    p: *mut u8,
}
unsafe impl Sync for InMutex {}

// Silent: an `UnsafeCell` of a generic parameter is how a lock is built.
struct SpinLock<T> {
    locked: std::sync::atomic::AtomicBool,
    value: UnsafeCell<T>,
}
unsafe impl<T: Send> Sync for SpinLock<T> {}

// Silent: a generic parameter counts as meeting the bound.
struct Generic<T> {
    p: *mut T,
    value: T,
}
unsafe impl<T: Send> Send for Generic<T> {}

// Flagged: `Handle` opts out of `Send` with a marker.
struct Handle {
    id: u32,
    _not_send: PhantomData<*const ()>,
}
struct HoldsHandle {
    h: Handle,
}
unsafe impl Send for HoldsHandle {}

// Silent: `PhantomData<*const T>` here is about variance, not a marker.
struct Iter<T> {
    p: *const T,
    _m: PhantomData<*const T>,
}
unsafe impl<T: Sync> Send for Iter<T> {}

// Flagged: `JsHandle` is listed in `thread-affine-types`.
struct JsHandle {
    p: NonNull<u8>,
}
struct Task {
    promise: JsHandle,
}
unsafe impl Send for Task {}

// Silent: a field whose type has its own `unsafe impl Send` is covered by
// that impl, which is the fix this lint asks for.
struct SendPtr(*mut u8);
unsafe impl Send for SendPtr {}
struct Fixed {
    p: SendPtr,
    count: u32,
}
unsafe impl Send for Fixed {}

// Silent: a `&Cell` shares the cell the way a raw pointer would.
struct BorrowsCell<'a> {
    c: &'a Cell<u8>,
}
unsafe impl Send for BorrowsCell<'_> {}

// Flagged: another thread could clone the `Rc` through the reference.
struct BorrowsRc<'a> {
    r: &'a Rc<u8>,
}
unsafe impl Send for BorrowsRc<'_> {}

// Silent: a one-field wrapper around a cell is the small type itself.
struct SlotCell(UnsafeCell<Vec<u8>>);
unsafe impl Sync for SlotCell {}

// Flagged: one field, but the `RefCell` is inside `Inner`, not named here.
struct WrapsInner(Inner);
unsafe impl Sync for WrapsInner {}

// Silent: the atomic guards the cell, and the type holds nothing else.
struct AtomicGuard {
    busy: std::sync::atomic::AtomicBool,
    cache: Cell<*mut u8>,
}
unsafe impl Sync for AtomicGuard {}

// Flagged: only cells and no atomic, so nothing guards them.
struct TwoCells {
    reader: UnsafeCell<Vec<u8>>,
    state: UnsafeCell<u32>,
}
unsafe impl Sync for TwoCells {}

// Silent: an `UnsafeCell` of nothing is an opaque FFI handle.
struct Opaque {
    _p: UnsafeCell<[u8; 0]>,
    _m: PhantomData<(*mut u8, std::marker::PhantomPinned)>,
}
unsafe impl Sync for Opaque {}
unsafe impl Send for Opaque {}

// Silent: an enum with only pointer payloads.
enum Slot {
    Empty,
    Full(*mut u8),
}
unsafe impl Send for Slot {}

fn main() {}
