// Sequence fields that only change length together and are read at one index are one Vec of a record.
#![allow(dead_code)]

use std::collections::VecDeque;

// Flagged: `names` and `ages` are pushed together, truncated together, and read at one index.
struct People {
    names: Vec<String>,
    ages: Vec<u32>,
    seen: usize,
}

impl People {
    fn add(&mut self, name: String, age: u32) {
        self.names.push(name);
        self.ages.push(age);
        self.seen += 1;
    }

    fn forget(&mut self, keep: usize) {
        self.names.truncate(keep);
        self.ages.truncate(keep);
    }

    fn describe(&self, i: usize) -> String {
        format!("{} is {}", self.names[i], self.ages[i])
    }

    // A `for` over `&mut self.ages` changes no length.
    fn birthday(&mut self) {
        for age in &mut self.ages {
            *age += 1;
        }
    }
}

// Flagged: three columns grown through a local binding and read by `zip`.
struct Table {
    keys: Vec<u32>,
    values: Vec<String>,
    phases: VecDeque<u8>,
}

fn insert(t: &mut Table, key: u32, value: String, phase: u8) {
    t.keys.push(key);
    t.values.push(value);
    t.phases.push_back(phase);
}

fn dump(t: &Table) {
    for (k, v) in t.keys.iter().zip(&t.values) {
        println!("{k} {v}");
    }
}

// Flagged: slices from one mark, through a field of `self`.
struct Tape {
    items: Vec<u8>,
    locs: Vec<u32>,
}

struct Builder {
    tape: Tape,
}

impl Builder {
    fn item(&mut self, item: u8, loc: u32) {
        self.tape.items.push(item);
        self.tape.locs.push(loc);
    }

    fn close(&mut self, mark: usize) -> usize {
        let n = self.tape.items[mark..].len() + self.tape.locs[mark..].len();
        self.tape.items.truncate(mark);
        self.tape.locs.truncate(mark);
        n
    }
}

// Fine: `errors` is also pushed alone, so it is not in step with `lines`.
struct Report {
    lines: Vec<String>,
    errors: Vec<String>,
}

impl Report {
    fn line(&mut self, l: String) {
        self.lines.push(l.clone());
        self.errors.push(l);
    }

    fn fail(&mut self, e: String) {
        self.errors.push(e);
    }

    fn pair(&self, i: usize) -> (&str, &str) {
        (&self.lines[i], &self.errors[i])
    }
}

// Fine: grown together but never read in step; each is consumed whole.
struct Rules {
    ltr: Vec<u8>,
    rtl: Vec<u8>,
}

impl Rules {
    fn add(&mut self, l: u8, r: u8) {
        self.ltr.push(l);
        self.rtl.push(r);
    }

    fn total(&self) -> usize {
        self.ltr.iter().map(|b| *b as usize).sum::<usize>() + self.rtl.len()
    }
}

// Fine: pushes to two different values are not a pair.
struct Lanes {
    xs: Vec<u8>,
    ys: Vec<u8>,
}

fn cross(a: &mut Lanes, b: &mut Lanes) {
    a.xs.push(1);
    b.ys.push(2);
}

fn lane(l: &Lanes, i: usize) -> u8 {
    l.xs[i] + l.ys[i]
}

// Fine: a `&mut` borrow handed to a function is a lone length change.
struct Swap {
    a: Vec<u8>,
    b: Vec<u8>,
}

impl Swap {
    fn both(&mut self, x: u8) {
        self.a.push(x);
        self.b.push(x);
    }

    fn reset(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.a)
    }

    fn sum(&self, i: usize) -> u8 {
        self.a[i] + self.b[i]
    }
}

// Fine: the two pushes sit in different match arms.
enum Side {
    Left,
    Right,
}

struct Split {
    left: Vec<u8>,
    right: Vec<u8>,
}

impl Split {
    fn put(&mut self, side: Side, x: u8) {
        match side {
            Side::Left => self.left.push(x),
            Side::Right => self.right.push(x),
        }
    }

    fn at(&self, i: usize) -> u8 {
        self.left[i] + self.right[i]
    }
}

// Fine: public fields of an exported struct can be grown by any crate.
pub struct Open {
    pub firsts: Vec<u8>,
    pub seconds: Vec<u8>,
}

pub fn open(o: &mut Open, i: usize) -> u8 {
    o.firsts.push(1);
    o.seconds.push(2);
    o.firsts[i] + o.seconds[i]
}

fn main() {}
