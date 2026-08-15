use std::collections::{HashMap, HashSet};
use std::ops::ControlFlow;

use clippy_utils::visitors::for_each_expr_without_closures;
use rustc_hir::def::Res;
use rustc_hir::{Body, Expr, ExprKind, QPath, UnOp};
use rustc_lint::{LateContext, LateLintPass};
use rustc_span::{Span, Symbol};

use crate::baseline::emit_with_note;
use crate::hir_shapes::value_name;

rustc_session::declare_lint! {
    /// Flags an index whose name claims one kind (`source_index`, `pkg_id`:
    /// the non-empty prefix before `_index`, `_idx`, `_id` or `_i`) landing
    /// on a place the same function otherwise indexes by names of another
    /// kind, when the function also shows the crossing name indexing a table
    /// named after it: `sources[source_index]`, `parts[part_index]` twice,
    /// then `parts[source_index]`. Both indices are plain integers, so
    /// `parts` accepts a source index and returns whichever element sits at
    /// that offset. `[]`, `.get`, `.get_mut` and `get_unchecked*` all count
    /// as indexing; a place is the binding or item at the root plus the
    /// fields, zero-argument accessors and earlier indices on the way
    /// (`self.graph.parts`, `lockfile.packages.names()`, `parts[..]`), so a
    /// two-level table is a different place at each level and two locals
    /// both called `resolutions` are two places.
    ///
    /// The claim is read off names only, never types: silent when either
    /// name carries no kind suffix (`i`, `n`, `at`), when the two prefixes
    /// share or abbreviate a word (`dep_id`/`dependency_id`,
    /// `pkg_id`/`package_id`, `other_chunk_index`/`chunk_index`), when the
    /// place is itself named after the crossing kind, and when the crossing
    /// name has no table of its own name in the function that the other name
    /// leaves alone, which is how a role name for the same kind reads
    /// (`nodes[parent_idx]` beside `nodes[node_idx]`, `symbols[existing_id]`
    /// beside `symbols[symbol_id]`).
    pub CROSSED_INDEX,
    Warn,
    "a place indexed by names of two different index kinds within one function"
}

rustc_session::declare_lint_pass!(CrossedIndex => [CROSSED_INDEX]);

/// The index kind a name claims: the non-empty prefix before `_index`,
/// `_idx`, `_id` or `_i`.
fn claimed_kind(name: &str) -> Option<&str> {
    ["_index", "_idx", "_id", "_i"]
        .iter()
        .find_map(|suffix| name.strip_suffix(suffix))
        .filter(|prefix| !prefix.is_empty())
}

/// `short` spells `long`: equal, a prefix of it, or its letters in order
/// from the same initial (`pkg`/`package`, `func`/`function`), three letters
/// at least so `id`-sized fragments do not unify everything.
fn abbreviates(short: &str, long: &str) -> bool {
    if short == long {
        return true;
    }
    if short.len() < 3 || short.len() > long.len() || short.as_bytes()[0] != long.as_bytes()[0] {
        return false;
    }
    let mut rest = long.chars();
    short.chars().all(|c| rest.any(|l| l == c))
}

/// `entries` -> `entry`, `hashes` -> `hash`, `parts` -> `part`; anything
/// else unchanged.
fn singular(word: &str) -> &str {
    if let Some(stem) = word.strip_suffix("ies") {
        // `y` is not in the stem, but `abbreviates` only needs the prefix.
        return stem;
    }
    for tail in ["shes", "ches", "xes", "sses"] {
        if word.ends_with(tail) {
            return &word[..word.len() - 2];
        }
    }
    match word.strip_suffix('s') {
        Some(stem) if !stem.ends_with('s') => stem,
        _ => word,
    }
}

/// Two `_`-separated names name one kind when any word of one is, or
/// abbreviates, a word of the other, plurals aside: `unresolved_dep` and
/// `dependencies`, `other_chunk` and `chunk`, `pkg` and `package_names`.
fn same_kind(a: &str, b: &str) -> bool {
    a.split('_').map(singular).any(|x| {
        b.split('_')
            .map(singular)
            .any(|y| abbreviates(x, y) || abbreviates(y, x))
    })
}

/// Zero-argument methods that expose the receiver's own elements rather
/// than select a different table, so `v.as_slice()[i]` indexes `v`.
const VIEWS: &[&str] = &[
    "as_slice",
    "as_mut_slice",
    "slice",
    "slice_mut",
    "as_ref",
    "as_mut",
    "borrow",
    "borrow_mut",
    "deref",
    "deref_mut",
    "iter",
    "iter_mut",
    "unwrap",
];

/// Where an index lands, as an identity (`key`, roots told apart by
/// resolution), as it reads in the source (`shown`), and the names along
/// it (`root`, each field and accessor) that may say what kind indexes it.
struct Place {
    key: String,
    shown: String,
    names: Vec<Symbol>,
}

impl Place {
    /// Some name along the place is the table of `kind`: `sources` or
    /// `self.graph.input_files` for `source`, `lockfile.packages.names()`
    /// for `pkg`.
    fn named_after(&self, kind: &str) -> bool {
        self.names.iter().any(|n| same_kind(n.as_str(), kind))
    }
}

/// The place `e` denotes: a local, parameter or item at the root, then
/// fields, non-view zero-argument accessors and earlier indices. `&`, `*`
/// and HIR temporaries are transparent. Anything else at the root (a call
/// with arguments, a literal) is no place.
fn place_of(cx: &LateContext<'_>, mut e: &Expr<'_>) -> Option<Place> {
    let mut path = Vec::new();
    let mut names = Vec::new();
    loop {
        match e.kind {
            ExprKind::AddrOf(_, _, inner)
            | ExprKind::Unary(UnOp::Deref, inner)
            | ExprKind::DropTemps(inner) => e = inner,
            ExprKind::Field(inner, ident) => {
                path.push(format!(".{}", ident.name));
                names.push(ident.name);
                e = inner;
            }
            ExprKind::Index(inner, _, _) => {
                path.push("[..]".to_string());
                e = inner;
            }
            ExprKind::MethodCall(seg, recv, [], _) => {
                if !VIEWS.contains(&seg.ident.name.as_str()) {
                    path.push(format!(".{}()", seg.ident.name));
                    names.push(seg.ident.name);
                }
                e = recv;
            }
            ExprKind::Path(ref qpath) => {
                let root = match cx.qpath_res(qpath, e.hir_id) {
                    Res::Local(id) => format!("{id:?}"),
                    Res::Def(_, did) => format!("{did:?}"),
                    _ => return None,
                };
                let name = match qpath {
                    QPath::Resolved(_, p) => p.segments.last()?.ident.name,
                    QPath::TypeRelative(_, seg) => seg.ident.name,
                };
                names.push(name);
                path.reverse();
                let tail = path.concat();
                return Some(Place {
                    key: format!("{root}{tail}"),
                    shown: format!("{name}{tail}"),
                    names,
                });
            }
            _ => return None,
        }
    }
}

/// `base[idx]`, `base.get(idx)`, `base.get_mut(idx)`,
/// `base.get_unchecked(idx)`, `base.get_unchecked_mut(idx)`.
fn index_parts<'h>(e: &'h Expr<'h>) -> Option<(&'h Expr<'h>, &'h Expr<'h>)> {
    match e.kind {
        ExprKind::Index(base, idx, _) => Some((base, idx)),
        ExprKind::MethodCall(seg, recv, [idx], _)
            if matches!(
                seg.ident.name.as_str(),
                "get" | "get_mut" | "get_unchecked" | "get_unchecked_mut"
            ) =>
        {
            Some((recv, idx))
        }
        _ => None,
    }
}

struct Site {
    place: usize,
    kind: Symbol,
    name: Symbol,
    span: Span,
}

/// One body's index sites and the places they land on.
struct Sites {
    places: Vec<Place>,
    sites: Vec<Site>,
}

impl Sites {
    fn collect<'tcx>(cx: &LateContext<'tcx>, body: &Body<'tcx>) -> Self {
        let mut places: Vec<Place> = Vec::new();
        let mut place_ids: HashMap<String, usize> = HashMap::new();
        let mut sites = Vec::new();
        for_each_expr_without_closures(body.value, |e: &'tcx Expr<'tcx>| {
            if !e.span.from_expansion()
                && let Some((base, idx)) = index_parts(e)
                && let Some(name) = value_name(idx)
                && let Some(kind) = claimed_kind(name.name.as_str())
                && let Some(place) = place_of(cx, base)
            {
                let next = places.len();
                let id = *place_ids.entry(place.key.clone()).or_insert(next);
                if id == next {
                    places.push(place);
                }
                sites.push(Site {
                    place: id,
                    kind: Symbol::intern(kind),
                    name: name.name,
                    span: e.span,
                });
            }
            ControlFlow::<()>::Continue(())
        });
        Self { places, sites }
    }

    /// For each place indexed by two kinds: every site of the less-used kind
    /// (ties: the later-introduced one) that is not at home on the place and
    /// does have a home table in this body the other kind never touches,
    /// with the other kind's first site, its count, and that home table.
    fn crossings(&self) -> Vec<(&Site, &Site, usize, usize)> {
        let mut reach: HashMap<Symbol, HashSet<usize>> = HashMap::new();
        let mut by_place: HashMap<usize, Vec<&Site>> = HashMap::new();
        for site in &self.sites {
            reach.entry(site.kind).or_default().insert(site.place);
            by_place.entry(site.place).or_default().push(site);
        }
        let mut out = Vec::new();
        for (&place, place_sites) in &by_place {
            let mut kinds: Vec<(Symbol, usize, &Site)> = Vec::new();
            for &site in place_sites {
                match kinds.iter_mut().find(|(k, ..)| *k == site.kind) {
                    Some(entry) => entry.1 += 1,
                    None => kinds.push((site.kind, 1, site)),
                }
            }
            kinds.sort_by_key(|&(_, n, first)| (std::cmp::Reverse(n), first.span.lo()));
            for (i, &(kind, ..)) in kinds.iter().enumerate() {
                if self.places[place].named_after(kind.as_str()) {
                    continue;
                }
                let home_of_kind_outside = |major: Symbol| {
                    reach[&kind]
                        .iter()
                        .copied()
                        .filter(|p| {
                            !reach[&major].contains(p) && self.places[*p].named_after(kind.as_str())
                        })
                        .min()
                };
                let Some((&(_, major_n, major_first), home)) = kinds[..i]
                    .iter()
                    .filter(|(major, ..)| !same_kind(major.as_str(), kind.as_str()))
                    .find_map(|m| Some((m, home_of_kind_outside(m.0)?)))
                else {
                    continue;
                };
                for &site in place_sites {
                    if site.kind == kind {
                        out.push((site, major_first, major_n, home));
                    }
                }
            }
        }
        out.sort_by_key(|(site, ..)| site.span.lo());
        out
    }
}

impl<'tcx> LateLintPass<'tcx> for CrossedIndex {
    fn check_body(&mut self, cx: &LateContext<'tcx>, body: &Body<'tcx>) {
        let sites = Sites::collect(cx, body);
        for (site, major_first, major_n, home) in sites.crossings() {
            emit_with_note(
                cx,
                CROSSED_INDEX,
                site.span,
                format!(
                    "`{place}` is indexed by `{name}` here but by `{major}` elsewhere in this \
                     function ({major_n} site{s}), while `{name}` is what indexes `{home}`: \
                     the names claim different index kinds and both are plain integers, so \
                     the crossing compiles",
                    place = sites.places[site.place].shown,
                    name = site.name,
                    major = major_first.name,
                    s = if major_n == 1 { "" } else { "s" },
                    home = sites.places[home].shown,
                ),
                major_first.span,
                "indexed by the other kind here",
                "an index newtype per table, with `Index` implemented only for its own, turns \
                 the crossing into a type error",
            );
        }
    }
}
