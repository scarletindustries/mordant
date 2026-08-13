// A lookup-unwrap proven by an insert just above, with nothing in between
// that could touch the map or key, re-fetches a value the code already had.

use std::collections::HashMap;

fn helper() {}

fn adjacent_is_flagged() -> u32 {
    let mut m: HashMap<u32, u32> = HashMap::new();
    m.insert(1, 2);
    let v = m.get(&1).unwrap();
    *v
}

fn pure_statement_between_is_flagged() -> u32 {
    let mut m: HashMap<u32, u32> = HashMap::new();
    m.insert(7, 8);
    let offset = 1 + 2;
    let v = m.get(&7).unwrap();
    *v + offset
}

fn call_between_is_fine() -> u32 {
    let mut m: HashMap<u32, u32> = HashMap::new();
    m.insert(1, 2);
    helper();
    *m.get(&1).unwrap()
}

fn remove_between_is_fine() -> u32 {
    let mut m: HashMap<u32, u32> = HashMap::new();
    m.insert(1, 2);
    m.remove(&1);
    *m.get(&1).unwrap_or(&0)
}

fn different_key_is_fine() -> u32 {
    let mut m: HashMap<u32, u32> = HashMap::new();
    m.insert(1, 2);
    *m.get(&3).unwrap_or(&0)
}

fn main() {
    let _ = adjacent_is_flagged()
        + pure_statement_between_is_flagged()
        + call_between_is_fine()
        + remove_between_is_fine()
        + different_key_is_fine();
}
