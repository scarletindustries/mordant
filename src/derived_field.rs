use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use rustc_hir::def_id::DefId;
use rustc_lint::{LateContext, LateLintPass};
use rustc_span::{Span, Symbol};

use crate::MordantConfig;
use crate::adt_facts::{has_fixed_repr, has_positional_fields};
use crate::baseline::emit_with_note;
use crate::field_writes::{self, Value};

rustc_lint::declare_lint! {
    /// Flags a field that always has the same value for a given value of a
    /// sibling field, in every place the type is built. It is a stored copy
    /// of something the sibling decides, and nothing rejects a mismatched
    /// pair written by hand. A method on the sibling's enum replaces it.
    ///
    /// Fires only when one of the two is a variant of an enum *this crate
    /// defined* at every site — a closed set of states, which is what makes
    /// the repair a method on that enum. Literal and named-constant columns
    /// biject whenever a small table's rows differ, which is a property of
    /// the table and not of the type; see `Val::decides`.
    ///
    /// A field that is also written after a build (`x.f = …`, `x.f += …`,
    /// `&mut x.f`) is skipped: the builds are then not the only thing
    /// deciding it, and a value the literals always pair one way may be
    /// re-paired later. So is a field another crate can assign.
    ///
    /// Silent on any type with an explicit `repr`, on foreign types, on a
    /// type another crate can build, and below `derived-field-min-sites`
    /// construction sites.
    pub DERIVED_FIELD,
    Warn,
    "a field whose value is decided by a sibling field"
}

pub struct DerivedField {
    min_sites: usize,
}

rustc_lint::impl_lint_pass!(DerivedField => [DERIVED_FIELD]);

/// One site cannot exhibit a correspondence and two is the least that can.
const FLOOR: usize = 2;

impl DerivedField {
    pub fn new(config: &MordantConfig) -> Self {
        Self {
            min_sites: config.derived_field_min_sites.max(FLOOR),
        }
    }
}

struct Site {
    span: Span,
    derived: bool,
    fields: BTreeMap<Symbol, Val>,
}

/// What a field initialiser evaluates to, as far as a name can be put on it.
///
/// A `Variant` records *which* variant and never its payload: `Some(n)` and
/// `Some(m)` are the same fact here, which is what makes an `Option` beside an
/// enum legible as one correspondence rather than as many.
#[derive(Clone, PartialEq, Eq, Hash)]
enum Val {
    Variant(DefId),
    NamedConst(DefId),
    Lit(String),
}

impl Val {
    fn of(v: &Value) -> Option<Val> {
        match v {
            Value::Variant(d) => Some(Val::Variant(*d)),
            Value::Const(d) => Some(Val::NamedConst(*d)),
            Value::Bool(b) => Some(Val::Lit(b.to_string())),
            Value::Int(n) => Some(Val::Lit(n.to_string())),
            Value::Str(s) => Some(Val::Lit(format!("{s:?}"))),
            Value::OtherLit(s) => Some(Val::Lit(s.clone())),
            Value::Default | Value::Unknown => None,
        }
    }

    /// Can this value *decide* a sibling — is it drawn from a closed set this
    /// crate defined?
    ///
    /// Only a local enum variant is. That is a stronger test than "has a
    /// name", and each of the three things it rules out was measured firing
    /// before it was added:
    ///
    /// * **Bare literals.** Two literal columns are in bijection whenever
    ///   their rows happen to differ, which is a property of a table with few
    ///   rows and not of the type. Every such pairing on the tree this lint
    ///   was measured against was a fixture.
    /// * **Named constants.** A two-row table pairing distinct strings with a
    ///   per-row constant satisfies "one side is named" for free, and the
    ///   advice to replace it with a method is wrong there — the constant is
    ///   the row's own datum, not a restatement of its neighbour.
    /// * **Foreign variants**, which is what keeps this off
    ///   `options_as_enum`' ground: two `Option` fields set to opposite
    ///   presences across two constructors biject, but `Some` and `None` are
    ///   `core`'s names and that shape already has a lint.
    ///
    /// What is left is the case the fix is actually written for: an enum
    /// names a closed set of states, a sibling restates one of them, and the
    /// repair is a method on the enum — `Ceiling::limit()`, or folding the
    /// field into the variant as `Wide { remaining }`.
    fn decides(&self) -> bool {
        matches!(self, Val::Variant(d) if d.is_local())
    }
}

/// The builds of `did` with the fields each one names a value for, when the
/// lint looks at the type at all.
fn sites(cx: &LateContext<'_>, did: DefId) -> Option<Vec<Site>> {
    let facts = field_writes::facts();
    if facts.access(did)?.others_build {
        return None;
    }
    let adt = field_writes::adt_of(cx.tcx, did);
    // A wire record legitimately restates what a sibling implies.
    if has_fixed_repr(adt) || has_positional_fields(adt.variant_with_id(did)) {
        return None;
    }
    let sites = facts
        .builds(did)
        .iter()
        .map(|b| Site {
            span: b.span,
            derived: b.derived,
            fields: b
                .fields
                .iter()
                .filter_map(|i| Some((i.name, Val::of(&i.value)?)))
                .collect(),
        })
        .collect();
    Some(sites)
}

impl<'tcx> LateLintPass<'tcx> for DerivedField {
    fn check_crate_post(&mut self, cx: &LateContext<'tcx>) {
        let facts = field_writes::facts();
        for did in facts.variants(cx.tcx) {
            let Some(sites) = sites(cx, did) else {
                continue;
            };
            if sites.len() < self.min_sites {
                continue;
            }
            // Fields every site gave a name to. A site that left one unknown
            // cannot witness a correspondence, so the whole field is dropped
            // rather than the pairing being read off a subset.
            let mut common: BTreeSet<Symbol> = sites[0].fields.keys().copied().collect();
            for s in &sites[1..] {
                common.retain(|k| s.fields.contains_key(k));
            }
            common.retain(|f| facts.only_built_here(did, *f));
            let common: Vec<Symbol> = common.into_iter().collect();
            for i in 0..common.len() {
                for j in (i + 1)..common.len() {
                    let (a, b) = (common[i], common[j]);
                    let va: Vec<&Val> = sites.iter().map(|s| &s.fields[&a]).collect();
                    let vb: Vec<&Val> = sites.iter().map(|s| &s.fields[&b]).collect();
                    if !(va.iter().all(|v| v.decides()) || vb.iter().all(|v| v.decides())) {
                        continue;
                    }
                    let da: HashSet<&&Val> = va.iter().collect();
                    let db: HashSet<&&Val> = vb.iter().collect();
                    // One distinct value on either side is a constant, not a
                    // correspondence.
                    if da.len() < 2 || db.len() < 2 {
                        continue;
                    }
                    if !bijective(&va, &vb) {
                        continue;
                    }
                    // The side drawn from a local enum decides the other; the
                    // repair is a method on that enum.
                    let (deciding, derived, by) = if va.iter().all(|v| v.decides()) {
                        (a, b, va[0])
                    } else {
                        (b, a, vb[0])
                    };
                    let Val::Variant(variant) = by else { continue };
                    // A field that several enum fields each decide is a row
                    // of a table, not a copy of one state.
                    let column =
                        |f: Symbol| -> Vec<&Val> { sites.iter().map(|s| &s.fields[&f]).collect() };
                    let of_derived = column(derived);
                    let deciders = common
                        .iter()
                        .filter(|&&c| {
                            let vc = column(c);
                            c != derived
                                && vc.iter().all(|v| v.decides())
                                && vc.iter().collect::<HashSet<_>>().len() >= 2
                                && bijective(&vc, &of_derived)
                        })
                        .count();
                    if deciders > 1 {
                        continue;
                    }
                    // Point at a build the user wrote, not at a derive.
                    let shown = sites.iter().find(|s| !s.derived).unwrap_or(&sites[0]).span;
                    let enum_name = cx.tcx.item_name(cx.tcx.parent(*variant));
                    emit_with_note(
                        cx,
                        DERIVED_FIELD,
                        cx.tcx.def_span(did),
                        format!(
                            "`{derived}` always has the same value for a given `{deciding}`, in all \
                             {} places `{}` is built. It is a stored copy of something `{deciding}` \
                             decides",
                            sites.len(),
                            cx.tcx.def_path_str(did),
                        ),
                        shown,
                        "one of the constructions",
                        format!(
                            "add `fn {derived}(&self)` to `{enum_name}` and delete the `{derived}` field"
                        ),
                    );
                }
            }
        }
    }
}

/// Do `a` and `b` agree one-for-one — each value of one always beside the same
/// value of the other, in both directions?
fn bijective(a: &[&Val], b: &[&Val]) -> bool {
    let mut fwd: HashMap<&Val, &Val> = HashMap::new();
    let mut rev: HashMap<&Val, &Val> = HashMap::new();
    for (x, y) in a.iter().zip(b.iter()) {
        if *fwd.entry(x).or_insert(y) != *y {
            return false;
        }
        if *rev.entry(y).or_insert(x) != *x {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use rustc_hir::def_id::{CrateNum, DefId, DefIndex};

    use super::{Val, bijective};

    fn lit(s: &str) -> Val {
        Val::Lit(s.to_string())
    }

    fn def(krate: u32, index: u32) -> DefId {
        DefId {
            krate: CrateNum::from_u32(krate),
            index: DefIndex::from_u32(index),
        }
    }

    #[test]
    fn one_for_one_is_bijective() {
        let (a, b) = (vec![lit("x"), lit("y")], vec![lit("1"), lit("2")]);
        let (ar, br): (Vec<&Val>, Vec<&Val>) = (a.iter().collect(), b.iter().collect());
        assert!(bijective(&ar, &br));
    }

    /// The same left value beside two different right values is the whole
    /// point: the pairing is not a function and the field is not a projection.
    #[test]
    fn a_repeated_left_with_two_rights_is_not() {
        let (a, b) = (vec![lit("x"), lit("x")], vec![lit("1"), lit("2")]);
        let (ar, br): (Vec<&Val>, Vec<&Val>) = (a.iter().collect(), b.iter().collect());
        assert!(!bijective(&ar, &br));
    }

    /// And the reverse direction, which a one-way functional check would pass.
    #[test]
    fn two_lefts_sharing_one_right_is_not() {
        let (a, b) = (vec![lit("x"), lit("y")], vec![lit("1"), lit("1")]);
        let (ar, br): (Vec<&Val>, Vec<&Val>) = (a.iter().collect(), b.iter().collect());
        assert!(!bijective(&ar, &br));
    }

    /// A bare literal decides nothing: a column of them is in bijection with
    /// its neighbour whenever a short table's rows differ.
    #[test]
    fn bare_literals_decide_nothing() {
        assert!(!lit("1").decides());
    }

    /// Nor does a constant this crate named, which is the two-row-table false
    /// positive this lint was measured emitting before `decides` narrowed.
    #[test]
    fn a_named_constant_decides_nothing() {
        assert!(!Val::NamedConst(def(0, 7)).decides());
    }

    /// A foreign variant is `Some`/`None`, which is `options_as_enum`' shape
    /// rather than this one.
    #[test]
    fn a_foreign_variant_decides_nothing() {
        assert!(!Val::Variant(def(2, 7)).decides());
    }

    #[test]
    fn a_local_enum_variant_decides() {
        assert!(Val::Variant(def(0, 7)).decides());
    }
}
