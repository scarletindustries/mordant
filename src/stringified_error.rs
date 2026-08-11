use crate::baseline::emit;
use rustc_hir::{Expr, ExprKind};
use rustc_lint::{LateContext, LateLintPass};
use rustc_middle::ty;
use rustc_span::sym;

rustc_session::declare_lint! {
    /// Flags the site where a typed error is collapsed into a string:
    /// `.map_err(|e| e.to_string())` on a `Result` whose error type is not
    /// already a string. `stringly_error` flags the signature that demands
    /// this; this lint flags the expression that performs the destruction.
    pub STRINGIFIED_ERROR,
    Warn,
    "typed error collapsed into a string"
}

rustc_session::declare_lint_pass!(StringifiedError => [STRINGIFIED_ERROR]);

impl<'tcx> LateLintPass<'tcx> for StringifiedError {
    fn check_expr(&mut self, cx: &LateContext<'tcx>, expr: &'tcx Expr<'tcx>) {
        let ExprKind::MethodCall(seg, recv, [arg], _) = expr.kind else {
            return;
        };
        if seg.ident.as_str() != "map_err" {
            return;
        }
        let recv_ty = cx.typeck_results().expr_ty_adjusted(recv);
        let ty::Adt(adt, args) = recv_ty.peel_refs().kind() else {
            return;
        };
        if !cx.tcx.is_diagnostic_item(sym::Result, adt.did()) {
            return;
        }
        let err_ty = args.type_at(1);
        // Already a string, or opaque to us: nothing is being destroyed.
        let ty::Adt(err_adt, _) = err_ty.peel_refs().kind() else {
            return;
        };
        if cx
            .tcx
            .is_lang_item(err_adt.did(), rustc_hir::LangItem::String)
        {
            return;
        }
        // The call must actually produce a string error, or nothing was
        // collapsed.
        let out_ty = cx.typeck_results().expr_ty(expr);
        let ty::Adt(out_adt, out_args) = out_ty.kind() else {
            return;
        };
        if !cx.tcx.is_diagnostic_item(sym::Result, out_adt.did()) {
            return;
        }
        let out_err = out_args.type_at(1);
        let is_string_out = match out_err.peel_refs().kind() {
            ty::Adt(a, _) => cx.tcx.is_lang_item(a.did(), rustc_hir::LangItem::String),
            _ => out_err.peel_refs().is_str(),
        };
        if !is_string_out {
            return;
        }
        let ExprKind::Closure(closure) = arg.kind else {
            return;
        };
        let body = cx.tcx.hir_body(closure.body);
        if closure_body_stringifies(cx, body.value) {
            emit(
                cx,
                STRINGIFIED_ERROR,
                expr.span,
                format!("typed error `{err_ty}` collapsed into a string"),
                "return the error type, or an error enum with a variant that wraps it",
            );
        }
    }
}

/// The closure body is exactly `param.to_string()`, `String::from(param)`, or a
/// `format!` invocation — a stringification and nothing else.
fn closure_body_stringifies(cx: &LateContext<'_>, body: &Expr<'_>) -> bool {
    // A `format!` body expands to a call, so the macro check runs first.
    let from_format = clippy_utils::macros::macro_backtrace(body.span)
        .next()
        .is_some_and(|mac| clippy_utils::macros::is_format_macro(cx, mac.def_id));
    if from_format {
        return true;
    }
    match body.kind {
        ExprKind::MethodCall(seg, recv, [], _) => {
            seg.ident.as_str() == "to_string" && is_param(cx, recv)
        }
        ExprKind::Call(callee, [inner]) => {
            let ExprKind::Path(qpath) = &callee.kind else {
                return false;
            };
            let res = cx.typeck_results().qpath_res(qpath, callee.hir_id);
            let Some(def_id) = res.opt_def_id() else {
                return false;
            };
            cx.tcx.def_path_str(def_id) == "std::string::String::from" && is_param(cx, inner)
        }
        _ => false,
    }
}

fn is_param(cx: &LateContext<'_>, expr: &Expr<'_>) -> bool {
    let _ = cx;
    matches!(expr.kind, ExprKind::Path(_))
}
