//! Structural identity of HIR: a hash to bucket candidates and an equality
//! to confirm them, both blind to spans, `HirId`s and binding names, both
//! reading paths by what they resolve to. Thin over
//! `clippy_utils::hir_utils`; what lives here is the pairing of local
//! bindings across two bodies, which `SpanlessEq` leaves to its caller (a
//! `Res::Local` on the left equals one on the right only when the two are
//! pre-mapped), and never `deny_side_effects`, since that makes every method
//! call unequal to itself.
//!
//! Hash where typeck results exist (`check_fn`, `check_expr`); the equality
//! functions work from `check_crate_post`, where the context has no
//! enclosing body, by fetching each side's typeck results themselves.

use clippy_utils::{SpanlessEq, SpanlessHash};
use rustc_hir::def::Res;
use rustc_hir::def_id::LocalDefId;
use rustc_hir::intravisit::{Visitor, walk_expr, walk_pat};
use rustc_hir::{BodyId, Expr, ExprKind, HirId, HirIdSet, Pat, PatKind, QPath};
use rustc_lint::LateContext;
use rustc_span::SyntaxContext;

/// Structural hash of a fn body: spans, `HirId`s and binding names do not
/// contribute; resolved paths, literals, field and method names, operators
/// and shape do.
pub(crate) fn body_hash(cx: &LateContext<'_>, body: BodyId) -> u64 {
    let mut h = SpanlessHash::new(cx).paths_by_resolution();
    h.hash_body(body);
    h.finish()
}

/// The bindings the body's parameter patterns introduce, in source order.
fn param_bindings(cx: &LateContext<'_>, body: BodyId) -> Vec<HirId> {
    let mut ids = Vec::new();
    for param in cx.tcx.hir_body(body).params {
        param
            .pat
            .each_binding_or_first(&mut |_, id, _, _| ids.push(id));
    }
    ids
}

/// Two fn bodies are the same computation up to renaming of parameters and
/// locals. Callers must have checked the two signatures are equal first
/// (`fn_sigs_equal`): method calls compare by name, so `.len()` on two
/// different receiver types is the same call to this function. Two bodies
/// containing a closure are never equal (`SpanlessEq` refuses closures).
pub(crate) fn bodies_equal(cx: &LateContext<'_>, l: BodyId, r: BodyId) -> bool {
    let (lp, rp) = (param_bindings(cx, l), param_bindings(cx, r));
    if lp.len() != rp.len() {
        return false;
    }
    let mut eq = SpanlessEq::new(cx).paths_by_resolution();
    let mut ie = eq.inter_expr(SyntaxContext::root());
    ie.locals.extend(lp.into_iter().zip(rp));
    ie.eq_body(l, r)
}

/// Erased signatures equal: same arity, each input and the output the same
/// `Ty` once late-bound regions are erased. For methods input 0 is `Self`,
/// which is what keeps `Foo::is_empty` and `Bar::is_empty` apart.
pub(crate) fn fn_sigs_equal(cx: &LateContext<'_>, l: LocalDefId, r: LocalDefId) -> bool {
    let sig = |d: LocalDefId| {
        cx.tcx.instantiate_bound_regions_with_erased(
            cx.tcx
                .fn_sig(d.to_def_id())
                .instantiate_identity()
                .skip_normalization(),
        )
    };
    sig(l).inputs_and_output == sig(r).inputs_and_output
}

/// Structural hash of one expression, for bucketing across bodies. Every
/// local hashes alike, so which binding an arm reads never separates two
/// buckets; `exprs_equal` decides that.
pub(crate) fn expr_hash(cx: &LateContext<'_>, e: &Expr<'_>) -> u64 {
    let mut h = SpanlessHash::new(cx).paths_by_resolution();
    h.hash_expr(e);
    h.finish()
}

/// The locals `e` reads but does not bind, in order of first use.
fn free_locals(e: &Expr<'_>) -> Vec<HirId> {
    struct V {
        bound: HirIdSet,
        free: Vec<HirId>,
    }
    impl<'tcx> Visitor<'tcx> for V {
        fn visit_pat(&mut self, p: &'tcx Pat<'tcx>) {
            if let PatKind::Binding(_, id, ..) = p.kind {
                self.bound.insert(id);
            }
            walk_pat(self, p);
        }
        fn visit_expr(&mut self, e: &'tcx Expr<'tcx>) {
            if let ExprKind::Path(QPath::Resolved(None, path)) = e.kind
                && let Res::Local(id) = path.res
                && !self.bound.contains(&id)
                && !self.free.contains(&id)
            {
                self.free.push(id);
            }
            walk_expr(self, e);
        }
    }
    let mut v = V {
        bound: HirIdSet::default(),
        free: Vec::new(),
    };
    v.visit_expr(e);
    v.free
}

/// Two expressions from two bodies (each given with its body owner) are the
/// same computation up to renaming: the locals each reads from outside
/// itself are paired in order of first use and must have the same type in
/// their own body, and under that pairing the two are structurally equal.
pub(crate) fn exprs_equal(
    cx: &LateContext<'_>,
    (l_owner, l): (LocalDefId, &Expr<'_>),
    (r_owner, r): (LocalDefId, &Expr<'_>),
) -> bool {
    let (lf, rf) = (free_locals(l), free_locals(r));
    if lf.len() != rf.len() {
        return false;
    }
    let (lt, rt) = (cx.tcx.typeck(l_owner), cx.tcx.typeck(r_owner));
    let erased = |ty| cx.tcx.erase_and_anonymize_regions(ty);
    for (&a, &b) in lf.iter().zip(&rf) {
        match (lt.node_type_opt(a), rt.node_type_opt(b)) {
            (Some(ta), Some(tb)) if erased(ta) == erased(tb) => {}
            _ => return false,
        }
    }
    let mut eq = SpanlessEq::new(cx).paths_by_resolution();
    let mut ie = eq.inter_expr(SyntaxContext::root());
    ie.locals.extend(lf.into_iter().zip(rf));
    ie.eq_expr(l, r)
}
