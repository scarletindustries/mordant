use rustc_hir::{BinOpKind, Expr, ExprKind, QPath};
use rustc_lint::{LateContext, LateLintPass};

use crate::baseline::emit;

rustc_session::declare_lint! {
    /// Flags addition, subtraction, and comparison between values whose names
    /// claim different units: `timeout_ms + deadline_ns` compiles and is
    /// always wrong. Multiplication and division stay silent, since they are
    /// how units legitimately convert.
    pub UNIT_MISMATCH,
    Warn,
    "arithmetic between values whose names claim different units"
}

rustc_session::declare_lint_pass!(UnitMismatch => [UNIT_MISMATCH]);

/// Unit classes by name suffix. Aliases share a class; a mismatch is two
/// operands from different classes.
fn unit_class(suffix: &str) -> Option<&'static str> {
    Some(match suffix {
        "ns" | "nanos" => "ns",
        "us" | "micros" => "us",
        "ms" | "millis" => "ms",
        "sec" | "secs" | "seconds" => "s",
        "mins" | "minutes" => "min",
        "bytes" => "bytes",
        "kb" | "kib" => "kb",
        "mb" | "mib" => "mb",
        "gb" | "gib" => "gb",
        "tenths" => "tenths",
        _ => return None,
    })
}

/// The unit an expression's name claims, from the final identifier of a path,
/// field access, or method call. Casts and references are transparent: they
/// change representation, not the unit the name asserts.
fn claimed_unit(e: &Expr<'_>) -> Option<(&'static str, String)> {
    let name = match &e.kind {
        ExprKind::Cast(inner, _) | ExprKind::AddrOf(_, _, inner) | ExprKind::Unary(_, inner) => {
            return claimed_unit(inner);
        }
        ExprKind::Field(_, ident) => ident.name.to_string(),
        ExprKind::Path(QPath::Resolved(_, path)) => path.segments.last()?.ident.name.to_string(),
        ExprKind::MethodCall(seg, ..) => seg.ident.name.to_string(),
        ExprKind::Call(callee, _) => {
            let ExprKind::Path(QPath::Resolved(_, path)) = &callee.kind else {
                return None;
            };
            path.segments.last()?.ident.name.to_string()
        }
        _ => return None,
    };
    let (_, suffix) = name.rsplit_once('_')?;
    unit_class(suffix).map(|c| (c, name))
}

impl<'tcx> LateLintPass<'tcx> for UnitMismatch {
    fn check_expr(&mut self, cx: &LateContext<'tcx>, expr: &'tcx Expr<'tcx>) {
        let ExprKind::Binary(op, lhs, rhs) = expr.kind else {
            return;
        };
        if !matches!(
            op.node,
            BinOpKind::Add
                | BinOpKind::Sub
                | BinOpKind::Lt
                | BinOpKind::Le
                | BinOpKind::Gt
                | BinOpKind::Ge
                | BinOpKind::Eq
                | BinOpKind::Ne
        ) {
            return;
        }
        if expr.span.from_expansion() {
            return;
        }
        let (Some((lc, ln)), Some((rc, rn))) = (claimed_unit(lhs), claimed_unit(rhs)) else {
            return;
        };
        if lc != rc {
            emit(
                cx,
                UNIT_MISMATCH,
                expr.span,
                format!(
                    "`{ln}` claims {lc} and `{rn}` claims {rc}; {} between them mixes units",
                    if matches!(op.node, BinOpKind::Add | BinOpKind::Sub) {
                        "arithmetic"
                    } else {
                        "comparison"
                    }
                ),
                "convert one side, or rename whichever name is lying about its unit",
            );
        }
    }
}
