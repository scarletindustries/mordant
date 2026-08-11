//! Shared analysis for the panic-elimination lints: which variant a
//! constructor literal builds, which variant a pattern head names, and
//! whether a match arm is a panic rather than any other divergence.

use rustc_hir::def::{CtorOf, DefKind, Res};
use rustc_hir::def_id::DefId;
use rustc_hir::{Expr, ExprKind, Pat, PatExpr, PatExprKind, PatKind};
use rustc_lint::LateContext;

/// The variant a constructor-literal argument passes, or None for anything
/// short of a literal constructor.
pub(crate) fn ctor_literal_variant(cx: &LateContext<'_>, e: &Expr<'_>) -> Option<DefId> {
    let res = match &e.kind {
        ExprKind::Call(callee, _) => {
            let ExprKind::Path(qpath) = &callee.kind else {
                return None;
            };
            cx.qpath_res(qpath, callee.hir_id)
        }
        ExprKind::Path(qpath) => cx.qpath_res(qpath, e.hir_id),
        ExprKind::Struct(qpath, ..) => cx.qpath_res(qpath, e.hir_id),
        _ => return None,
    };
    match res {
        Res::Def(DefKind::Variant, id) => Some(id),
        Res::Def(DefKind::Ctor(CtorOf::Variant, _), id) => Some(cx.tcx.parent(id)),
        _ => None,
    }
}

/// The variant a match-arm pattern names at its head.
pub(crate) fn arm_variant(cx: &LateContext<'_>, pat: &Pat<'_>) -> Option<DefId> {
    let qpath = match &pat.kind {
        PatKind::TupleStruct(qpath, ..) | PatKind::Struct(qpath, ..) => qpath,
        PatKind::Expr(PatExpr {
            kind: PatExprKind::Path(qpath),
            ..
        }) => qpath,
        _ => return None,
    };
    match cx.qpath_res(qpath, pat.hir_id) {
        Res::Def(DefKind::Variant, id) => Some(id),
        Res::Def(DefKind::Ctor(CtorOf::Variant, _), id) => Some(cx.tcx.parent(id)),
        _ => None,
    }
}

/// A diverging arm that is a panic, not a `return`/`continue`: never-typed
/// AND rooted in a panic-family macro.
pub(crate) fn is_panic_arm(cx: &LateContext<'_>, body: &Expr<'_>) -> bool {
    if !cx.typeck_results().expr_ty(body).is_never() {
        return false;
    }
    let mut inner = body;
    while let ExprKind::Block(b, _) = inner.kind {
        match (b.stmts.len(), b.expr) {
            (0, Some(tail)) => inner = tail,
            _ => break,
        }
    }
    clippy_utils::macros::macro_backtrace(inner.span).any(|mac| {
        matches!(
            cx.tcx.item_name(mac.def_id).as_str(),
            "panic" | "unreachable" | "todo" | "unimplemented"
        )
    })
}
