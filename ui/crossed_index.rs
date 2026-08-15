// A place indexed by names of one kind must not also be indexed by a name of another kind.

struct Lockfile {
    dependencies: Vec<&'static str>,
    // One slot per dependency, holding the package it resolved to.
    resolutions: Vec<u32>,
    packages: Vec<&'static str>,
}

fn crossed_table_is_flagged(l: &Lockfile, dep_id: u32, pkg_id: u32) -> bool {
    let dep = l.dependencies[dep_id as usize];
    let resolved = l.resolutions[dep_id as usize];
    let name = l.packages[pkg_id as usize];
    // Flagged: `resolutions` is the per-dependency table and `pkg_id` is what indexes `packages`.
    let stale = l.resolutions[pkg_id as usize];
    dep == name && resolved == stale
}

fn crossed_get_is_flagged(
    sources: &[&str],
    parts: &[u32],
    source_index: usize,
    part_index: usize,
) -> bool {
    let path = sources[source_index];
    let part = parts.get(part_index);
    let sibling = parts[part_index.saturating_sub(1)];
    // Flagged: `parts` is indexed by `part_index`; `source_index` is what indexes `sources`.
    let crossed = parts.get(source_index);
    path.is_empty() && part == crossed && sibling == 0
}

struct Graph {
    files: Vec<&'static str>,
    parts: Vec<Vec<u32>>,
    flags: Vec<bool>,
}

impl Graph {
    fn crossed_field_is_flagged(&mut self, source_index: u32, part_index: u32) {
        let _ = self.files[source_index as usize];
        let _ = self.parts[source_index as usize][part_index as usize];
        // Flagged: `self.files` is the per-source table; `part_index` indexes `self.parts[..]`.
        let _ = self.files.get_mut(part_index as usize);
    }

    // Fine: a two-level table is a different place at each level.
    fn nested_table_is_fine(&self, source_index: u32, part_index: u32) -> u32 {
        let _ = self.files[source_index as usize];
        self.parts[source_index as usize][part_index as usize]
            + self.parts[source_index as usize].as_slice()[part_index as usize]
            + u32::from(self.flags[part_index as usize])
    }

    // Fine: parallel columns share one index kind.
    fn parallel_columns_are_fine(&self, source_index: u32) -> bool {
        self.files[source_index as usize].is_empty()
            && self.parts[source_index as usize].is_empty()
            && self.flags[source_index as usize]
    }
}

// Fine: a role name for the same kind has no table of its own name.
fn role_name_is_fine(nodes: &[u32], depths: &[u32], node_idx: usize, parent_idx: usize) -> u32 {
    nodes[node_idx] + depths[node_idx] + nodes[parent_idx] + depths[parent_idx]
}

// Fine: the two prefixes abbreviate one word.
fn abbreviation_is_fine(
    l: &Lockfile,
    dep_id: u32,
    dependency_id: u32,
    pkg_id: u32,
    package_id: u32,
) {
    let _ = l.dependencies[dep_id as usize];
    let _ = l.resolutions[dep_id as usize];
    let _ = l.resolutions[dependency_id as usize];
    let _ = l.packages[pkg_id as usize];
    let _ = l.packages[package_id as usize];
}

// Fine: two locals of one name are two places.
fn shadowed_local_is_fine(l: &Lockfile, per_package: &[u32], dep_id: u32, pkg_id: u32) -> u32 {
    let resolutions = &l.resolutions;
    let a = resolutions[dep_id as usize] + l.dependencies[dep_id as usize].len() as u32;
    let resolutions = per_package;
    a + resolutions[pkg_id as usize] + l.packages[pkg_id as usize].len() as u32
}

// Fine: names without a kind suffix claim nothing.
fn unsuffixed_names_are_fine(v: &[u8], w: &[u8], i: usize, at: usize, n: usize) -> u8 {
    v[i] + v[at] + w[at] + v[n] + w[0]
}

fn main() {
    let l = Lockfile {
        dependencies: vec!["a"],
        resolutions: vec![0],
        packages: vec!["a"],
    };
    let _ = crossed_table_is_flagged(&l, 0, 0);
    let _ = crossed_get_is_flagged(&["a"], &[0], 0, 0);
    let mut g = Graph {
        files: vec!["a"],
        parts: vec![vec![0]],
        flags: vec![false],
    };
    g.crossed_field_is_flagged(0, 0);
    let _ = g.nested_table_is_fine(0, 0);
    let _ = g.parallel_columns_are_fine(0);
    let _ = role_name_is_fine(&[0], &[0], 0, 0);
    abbreviation_is_fine(&l, 0, 0, 0, 0);
    let _ = shadowed_local_is_fine(&l, &[0], 0, 0);
    let _ = unsuffixed_names_are_fine(&[0], &[0], 0, 0, 0);
}
