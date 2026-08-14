use std::collections::{BTreeMap, BTreeSet, HashMap};

use rustc_hir::def::{CtorKind, CtorOf, DefKind, Res};
use rustc_hir::def_id::DefId;
use rustc_hir::{Expr, ExprKind};
use rustc_lint::{LateContext, LateLintPass};
use rustc_span::Symbol;

use crate::MordantConfig;
use crate::baseline::emit;

rustc_session::declare_lint! {
    /// Flags two fields of one type whose constant values agree one-for-one
    /// across every place the type is built: field `a` is a stored projection
    /// of field `b`, so the type admits pairings the constructors never make
    /// and nothing rejects one written by hand.
    ///
    /// Fires only when one of the two is a variant of an enum *this crate
    /// defined* at every site — a closed set of states, which is what makes
    /// the repair a method on that enum. Literal and named-constant columns
    /// biject whenever a small table's rows differ, which is a property of
    /// the table and not of the type; see `Val::decides`.
    ///
    /// A field that is also assigned somewhere (`x.f = …`) is skipped: the
    /// constructors are then not the only thing deciding it, and a value the
    /// literals always pair one way may be re-paired later.
    ///
    /// Silent on any type with an explicit `repr`, on foreign types, and
    /// below `stored-projection-min-sites` construction sites.
    pub STORED_PROJECTION,
    Warn,
    "a field whose value is decided by a sibling field"
}

pub struct StoredProjection {
    min_sites: usize,
    seen: HashMap<DefId, Vec<Site>>,
    /// (variant, field) pairs written by an assignment rather than a literal.
    assigned: HashMap<DefId, BTreeSet<Symbol>>,
}

rustc_session::impl_lint_pass!(StoredProjection => [STORED_PROJECTION]);

/// One site cannot exhibit a correspondence and two is the least that can.
const FLOOR: usize = 2;

impl StoredProjection {
    pub fn new(config: &MordantConfig) -> Self {
        Self {
            min_sites: config.stored_projection_min_sites.max(FLOOR),
            seen: HashMap::new(),
            assigned: HashMap::new(),
        }
    }
}

struct Site {
    fields: BTreeMap<Symbol, Val>,
}

/// What a field initialiser evaluates to, as far as a name can be put on it.
///
/// A `Variant` records *which* variant and never its payload: `Some(n)` and
/// `Some(m)` are the same fact here, which is what makes an `Option` beside an
/// enum legible as one correspondence rather than as many.
///
/// A definition is held as its `(krate, index)` pair because `DefId` is not
/// `Ord` and the comparisons below need a total order. `local` travels beside
/// it rather than being read back off `krate == 0`, which is an encoding
/// detail.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Val {
    Variant { at: (u32, u32), local: bool },
    NamedConst { at: (u32, u32), local: bool },
    Lit(String),
}

fn as_variant(d: DefId) -> Val {
    Val::Variant {
        at: (d.krate.as_u32(), d.index.as_u32()),
        local: d.is_local(),
    }
}

fn as_named_const(d: DefId) -> Val {
    Val::NamedConst {
        at: (d.krate.as_u32(), d.index.as_u32()),
        local: d.is_local(),
    }
}

impl Val {
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
    ///   `exclusive_options`' ground: two `Option` fields set to opposite
    ///   presences across two constructors biject, but `Some` and `None` are
    ///   `core`'s names and that shape already has a lint.
    ///
    /// What is left is the case the fix is actually written for: an enum
    /// names a closed set of states, a sibling restates one of them, and the
    /// repair is a method on the enum — `Ceiling::limit()`, or folding the
    /// field into the variant as `Wide { remaining }`.
    const fn decides(&self) -> bool {
        matches!(self, Val::Variant { local: true, .. })
    }
}

/// Strip the wrappers that do not change which value an expression names.
fn peel<'tcx>(e: &'tcx Expr<'tcx>) -> &'tcx Expr<'tcx> {
    match e.kind {
        ExprKind::AddrOf(_, _, inner) | ExprKind::DropTemps(inner) => peel(inner),
        ExprKind::Block(b, None) if b.stmts.is_empty() => b.expr.map_or(e, peel),
        _ => e,
    }
}

fn classify<'tcx>(cx: &LateContext<'tcx>, e: &'tcx Expr<'tcx>) -> Option<Val> {
    let e = peel(e);
    match e.kind {
        ExprKind::Path(ref qpath) => match cx.qpath_res(qpath, e.hir_id) {
            Res::Def(DefKind::Ctor(CtorOf::Variant, CtorKind::Const), did) => {
                Some(as_variant(cx.tcx.parent(did)))
            }
            Res::Def(DefKind::Const { .. } | DefKind::AssocConst { .. }, did) => {
                Some(as_named_const(did))
            }
            _ => None,
        },
        // `Some(x)`, `Wide(n)` — the variant is the fact, the payload is not.
        ExprKind::Call(f, _) => match f.kind {
            ExprKind::Path(ref qpath) => match cx.qpath_res(qpath, f.hir_id) {
                Res::Def(DefKind::Ctor(CtorOf::Variant, CtorKind::Fn), did) => {
                    Some(as_variant(cx.tcx.parent(did)))
                }
                _ => None,
            },
            _ => None,
        },
        ExprKind::Struct(qpath, ..) => match cx.qpath_res(qpath, e.hir_id) {
            Res::Def(DefKind::Variant, did) => Some(as_variant(did)),
            _ => None,
        },
        ExprKind::Lit(lit) => Some(Val::Lit(format!("{:?}", lit.node))),
        _ => None,
    }
}

impl StoredProjection {
    /// `s.f = …`, through any number of derefs and autoderefs: `f` of `s`'s
    /// struct is decided by more than its literals.
    fn note_assignment<'tcx>(&mut self, cx: &LateContext<'tcx>, place: &'tcx Expr<'tcx>) {
        let mut place = place;
        while let ExprKind::Unary(rustc_hir::UnOp::Deref, inner) | ExprKind::DropTemps(inner) =
            place.kind
        {
            place = inner;
        }
        let ExprKind::Field(base, field) = place.kind else {
            return;
        };
        let Some(adt) = cx.typeck_results().expr_ty_adjusted(base).peel_refs().ty_adt_def()
        else {
            return;
        };
        if !adt.is_struct() || !adt.did().is_local() {
            return;
        }
        self.assigned
            .entry(adt.non_enum_variant().def_id)
            .or_default()
            .insert(field.name);
    }
}

impl<'tcx> LateLintPass<'tcx> for StoredProjection {
    fn check_expr(&mut self, cx: &LateContext<'tcx>, expr: &'tcx Expr<'tcx>) {
        if let ExprKind::Assign(place, ..) | ExprKind::AssignOp(_, place, _) = expr.kind {
            self.note_assignment(cx, place);
            return;
        }
        let ExprKind::Struct(qpath, fields, _) = expr.kind else {
            return;
        };
        // A `..base` literal overrides part of a value that already exists, so
        // the fields written are the ones deliberately being made to differ.
        if !matches!(
            expr.kind,
            ExprKind::Struct(_, _, rustc_hir::StructTailExpr::None)
        ) {
            return;
        }
        let Some(adt) = cx.typeck_results().expr_ty(expr).ty_adt_def() else {
            return;
        };
        if !adt.did().is_local() {
            return;
        }
        // An explicit repr means something outside Rust fixes the layout, and
        // a wire record legitimately restates what a sibling implies.
        let repr = adt.repr();
        if repr.c() || repr.packed() || repr.transparent() || repr.simd() || repr.int.is_some() {
            return;
        }
        let res = cx.qpath_res(qpath, expr.hir_id);
        let variant = adt.variant_of_res(res);
        // Tuple fields are named "0", "1", …; the message wants names.
        if variant
            .fields
            .iter()
            .any(|f| f.name.as_str().starts_with(|c: char| c.is_ascii_digit()))
        {
            return;
        }
        let mut vals = BTreeMap::new();
        for f in fields {
            if let Some(v) = classify(cx, f.expr) {
                vals.insert(f.ident.name, v);
            }
        }
        self.seen
            .entry(variant.def_id)
            .or_default()
            .push(Site { fields: vals });
    }

    fn check_crate_post(&mut self, cx: &LateContext<'tcx>) {
        let mut keys: Vec<DefId> = self
            .seen
            .iter()
            .filter(|(_, s)| s.len() >= self.min_sites)
            .map(|(d, _)| *d)
            .collect();
        // Deterministic order: findings are compared against a baseline.
        keys.sort_by_key(|d| cx.tcx.def_span(*d));

        for did in keys {
            let sites = &self.seen[&did];
            // Fields every site gave a name to. A site that left one unknown
            // cannot witness a correspondence, so the whole field is dropped
            // rather than the pairing being read off a subset.
            let mut common: BTreeSet<Symbol> = sites[0].fields.keys().copied().collect();
            for s in &sites[1..] {
                common.retain(|k| s.fields.contains_key(k));
            }
            if let Some(assigned) = self.assigned.get(&did) {
                common.retain(|f| !assigned.contains(f));
            }
            let common: Vec<Symbol> = common.into_iter().collect();
            for i in 0..common.len() {
                for j in (i + 1)..common.len() {
                    let (a, b) = (common[i], common[j]);
                    let va: Vec<&Val> = sites.iter().map(|s| &s.fields[&a]).collect();
                    let vb: Vec<&Val> = sites.iter().map(|s| &s.fields[&b]).collect();
                    if !(va.iter().all(|v| v.decides()) || vb.iter().all(|v| v.decides())) {
                        continue;
                    }
                    let da: BTreeSet<&&Val> = va.iter().collect();
                    let db: BTreeSet<&&Val> = vb.iter().collect();
                    // One distinct value on either side is a constant, not a
                    // correspondence.
                    if da.len() < 2 || db.len() < 2 {
                        continue;
                    }
                    if !bijective(&va, &vb) {
                        continue;
                    }
                    emit(
                        cx,
                        STORED_PROJECTION,
                        cx.tcx.def_span(did),
                        format!(
                            "`{}` and `{}` of `{}` agree one-for-one across all {} places it is \
                             constructed, so one is a stored projection of the other",
                            a,
                            b,
                            cx.tcx.def_path_str(did),
                            sites.len(),
                        ),
                        "give the deciding field a method returning the other and drop the stored \
                         copy, so a pairing the constructors never make cannot be written",
                    );
                }
            }
        }
    }
}

/// Do `a` and `b` agree one-for-one — each value of one always beside the same
/// value of the other, in both directions?
fn bijective(a: &[&Val], b: &[&Val]) -> bool {
    let mut fwd: BTreeMap<&Val, &Val> = BTreeMap::new();
    let mut rev: BTreeMap<&Val, &Val> = BTreeMap::new();
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
    use super::{Val, bijective};

    fn lit(s: &str) -> Val {
        Val::Lit(s.to_string())
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
        let c = Val::NamedConst {
            at: (0, 7),
            local: true,
        };
        assert!(!c.decides());
    }

    /// A foreign variant is `Some`/`None`, which is `exclusive_options`' shape
    /// rather than this one.
    #[test]
    fn a_foreign_variant_decides_nothing() {
        let v = Val::Variant {
            at: (2, 7),
            local: false,
        };
        assert!(!v.decides());
    }

    #[test]
    fn a_local_enum_variant_decides() {
        let v = Val::Variant {
            at: (0, 7),
            local: true,
        };
        assert!(v.decides());
    }
}
