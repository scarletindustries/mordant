// A safe fn returns `&mut` to memory a second call can reach again: a
// static, a thread-local, or what a copied argument points to.

#![allow(static_mut_refs, dead_code)]

use std::cell::{Cell, UnsafeCell};
use std::mem::MaybeUninit;
use std::ptr::NonNull;

static mut COUNTER: u64 = 0;

struct Racy<T>(UnsafeCell<T>);
unsafe impl<T> Sync for Racy<T> {}
impl<T> Racy<T> {
    const fn new(v: T) -> Self {
        Self(UnsafeCell::new(v))
    }
    fn get(&self) -> *mut T {
        self.0.get()
    }
}

static BUF: Racy<[u8; 64]> = Racy::new([0; 64]);
static STATE: Racy<MaybeUninit<Vec<u8>>> = Racy::new(MaybeUninit::uninit());

thread_local! {
    static SCRATCH: UnsafeCell<[u8; 32]> = const { UnsafeCell::new([0; 32]) };
    static LAZY: Cell<*mut [u8; 16]> = const { Cell::new(std::ptr::null_mut()) };
}

// Flagged: a `static mut` handed out as `&'static mut`.
fn counter() -> &'static mut u64 {
    unsafe { &mut COUNTER }
}

// Flagged: an `UnsafeCell` static, reached through a method of the cell.
fn buf() -> &'static mut [u8; 64] {
    unsafe { &mut *BUF.get() }
}

// Flagged: `assume_init_mut` on a static.
fn state() -> &'static mut Vec<u8> {
    unsafe { (*STATE.get()).assume_init_mut() }
}

// Flagged: a fn that forwards another such fn.
fn state_again() -> &'static mut Vec<u8> {
    state()
}

// Flagged: a thread-local, through `with`.
fn scratch() -> &'static mut [u8; 32] {
    SCRATCH.with(|c| unsafe { &mut *c.get() })
}

// Flagged: a thread-local pointer set up on first use.
fn lazy() -> &'static mut [u8; 16] {
    let p = LAZY.with(|c| {
        let mut p = c.get();
        if p.is_null() {
            p = Box::into_raw(Box::new([0; 16]));
            c.set(p);
        }
        p
    });
    unsafe { &mut *p }
}

// Flagged: a shared reference argument, with a lifetime the result does not use.
fn cell_mut<const N: usize>(c: &UnsafeCell<[u8; N]>) -> &'static mut [u8; N] {
    unsafe { &mut *c.get() }
}

struct Owner {
    log: *mut Vec<String>,
}

impl Owner {
    // Flagged: a raw pointer read through `&self`, with an unbound lifetime.
    fn log_mut<'a>(&self) -> &'a mut Vec<String> {
        unsafe { &mut *self.log }
    }
}

// Flagged: a raw pointer argument.
fn node<'a>(p: *mut u32) -> &'a mut u32 {
    unsafe { &mut *p }
}

// Flagged: a `NonNull` argument.
fn node_nn<'a>(mut p: NonNull<u32>) -> &'a mut u32 {
    unsafe { p.as_mut() }
}

// Fine: an `unsafe fn` states the contract in its signature.
unsafe fn counter_unchecked() -> &'static mut u64 {
    unsafe { &mut COUNTER }
}

// Fine: the result is tied to a `&mut` argument.
fn first(v: &mut [u8]) -> &mut u8 {
    &mut v[0]
}

// Fine: the result is tied to `&self`. Clippy's `mut_from_ref` covers this shape.
impl Owner {
    fn log_tied(&self) -> &mut Vec<String> {
        unsafe { &mut *self.log }
    }
}

// Fine: a fresh allocation each call.
fn leaked() -> &'static mut Vec<u8> {
    Box::leak(Box::new(Vec::new()))
}

// Fine: the `Box` argument is moved in, so no caller can pass it twice.
fn leak_box(b: Box<u32>) -> &'static mut u32 {
    Box::leak(b)
}

// Fine: the lifetime is carried by the `&'a mut` argument.
fn reborrow<'a>(x: &'a mut u32) -> &'a mut u32 {
    x
}

// Fine: a shared reference return, which two calls may share.
fn counter_ref() -> &'static u64 {
    unsafe { &*(&raw const COUNTER) }
}

trait Slot {
    fn slot() -> &'static mut u64;
}

// Fine: the trait sets the signature, so the fix belongs on the trait.
impl Slot for Owner {
    fn slot() -> &'static mut u64 {
        unsafe { &mut COUNTER }
    }
}

// Fine: the `#[allow]` on the fn is read.
#[allow(safe_fn_returns_shared_mut)]
fn allowed() -> &'static mut u64 {
    unsafe { &mut COUNTER }
}

fn main() {}
