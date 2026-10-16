// A reference parameter must stay valid until its fn returns. Freeing the
// value behind it, through a raw pointer made from it, is undefined behaviour.

use std::alloc::{Layout, dealloc};
use std::ptr;

struct Task {
    id: u32,
    child: *mut Task,
}

impl Task {
    // Silent: the parameter is a raw pointer, which is the fix.
    unsafe fn destroy(this: *mut Task) {
        drop(unsafe { Box::from_raw(this) });
    }

    // Flagged: `destroy` frees `*self` while `&mut self` is live.
    fn finish(&mut self) {
        self.id = 0;
        unsafe { Task::destroy(ptr::from_mut(self)) };
    }

    // Flagged: the cast from `&self` reaches `Box::from_raw` and the drop.
    fn close(&self) {
        let p = self as *const Task as *mut Task;
        let owned = unsafe { Box::from_raw(p) };
        drop(owned);
    }

    // Flagged: `dealloc` frees the memory behind `self`.
    fn release(&mut self) {
        let p = ptr::from_mut(self).cast::<u8>();
        unsafe { dealloc(p, Layout::new::<Task>()) };
    }

    // Flagged: `finalize` takes back the `Box` and drops it at its end.
    fn done(&mut self) {
        Task::finalize(ptr::from_mut(self));
    }

    // Silent: a raw pointer parameter again.
    fn finalize(this: *mut Task) {
        let owned = unsafe { take(this) };
        let _ = owned.id;
    }

    // Flagged: a free behind an `if` in this fn is still a free on that path.
    fn finish_if(&mut self, last: bool) {
        if last {
            unsafe { Task::destroy(ptr::from_mut(self)) };
        }
    }

    // Flagged, as "can free": `deref` frees only when the count reaches zero.
    fn unref(&mut self) {
        unsafe { deref(ptr::from_mut(self)) };
    }

    // Flagged: the generic `release_all` calls the trait's default `free`.
    fn drop_via_trait(&mut self) {
        unsafe { release_all(ptr::from_mut(self)) };
    }

    // Silent: this frees the child, a pointer read out of `self`.
    fn drop_child(&mut self) {
        drop(unsafe { Box::from_raw(self.child) });
    }

    // Silent: the pointer is kept, not freed.
    fn register(&mut self) {
        keep(ptr::from_mut(self));
    }

    // Silent: the `Box` is forgotten, so nothing is freed.
    fn forget(&mut self) {
        std::mem::forget(unsafe { Box::from_raw(ptr::from_mut(self)) });
    }

    // Silent: it passes a reference on. `finish` is reported on its own.
    fn finish_twice(&mut self) {
        self.finish();
    }
}

unsafe fn take(this: *mut Task) -> Box<Task> {
    unsafe { Box::from_raw(this) }
}

static mut COUNT: u32 = 1;

unsafe fn deref(this: *mut Task) {
    unsafe {
        COUNT -= 1;
        if COUNT == 0 {
            Task::destroy(this);
        }
    }
}

trait Free: Sized {
    unsafe fn free(this: *mut Self) {
        drop(unsafe { Box::from_raw(this) });
    }
}

impl Free for Task {}

unsafe fn release_all<T: Free>(p: *mut T) {
    unsafe { T::free(p) };
}

fn keep(_: *mut Task) {}

struct Pool;

impl Pool {
    // Its body frees nothing, but `freed-while-borrowed-releases` lists it.
    fn release(_: *mut Task) {}
}

impl Task {
    // Flagged, as "can free": the configuration says `Pool::release` can free.
    fn give_back(&mut self) {
        Pool::release(ptr::from_mut(self));
    }
}

// Silent: this frees the value the stored pointer points to, not `*slot`.
fn clear(slot: &mut *mut Task) {
    drop(unsafe { Box::from_raw(*slot) });
}

// Flagged: a closure parameter is a reference parameter too.
fn with_closure() {
    let close = |t: &mut Task| unsafe { Task::destroy(ptr::from_mut(t)) };
    let _ = close;
}

fn main() {}
