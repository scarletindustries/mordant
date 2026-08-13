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
}
