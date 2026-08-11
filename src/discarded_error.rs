use crate::baseline::emit;
use rustc_hir::{ExprKind, Stmt, StmtKind};
use rustc_lint::{LateContext, LateLintPass};
use rustc_middle::ty;
use rustc_span::sym;

rustc_session::declare_lint! {
    /// Flags `.ok();` in statement position: the `Result` becomes an `Option`
    /// that is immediately dropped, so the error value is unobservable. A
    /// deliberate discard is written `let _ = ...`, which states the intent
    /// and survives review; `.ok();` reads like handling and handles nothing.
    pub DISCARDED_ERROR,
    Warn,
    "statement-position .ok() makes the error unobservable"
}

rustc_session::declare_lint_pass!(DiscardedError => [DISCARDED_ERROR]);

impl<'tcx> LateLintPass<'tcx> for DiscardedError {
    fn check_stmt(&mut self, cx: &LateContext<'tcx>, stmt: &'tcx Stmt<'tcx>) {
        let StmtKind::Semi(expr) = stmt.kind else {
            return;
        };
        let ExprKind::MethodCall(seg, recv, [], _) = expr.kind else {
            return;
        };
        if seg.ident.as_str() != "ok" {
            return;
        }
        let ty::Adt(adt, _) = cx
            .typeck_results()
            .expr_ty_adjusted(recv)
            .peel_refs()
            .kind()
        else {
            return;
        };
        if !cx.tcx.is_diagnostic_item(sym::Result, adt.did()) {
            return;
        }
        emit(
            cx,
            DISCARDED_ERROR,
            expr.span,
            "`.ok()` in statement position discards the error unobserved",
            "handle the error, or state the discard with `let _ = ...`",
        );
    }
}
