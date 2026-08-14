//! Shapes several lints read off HIR the same way: the literal `self`
//! receiver, a field of it, a condition with its negations peeled, a branch
//! that ends in `return`, and the definition a call expression invokes. Each
//! lint applies its own filters on top; what lives here is only the part they
//! spelled identically.

use rustc_hir::def_id::DefId;
use rustc_hir::{Expr, ExprKind, QPath, Stmt, StmtKind, UnOp};
use rustc_lint::LateContext;
use rustc_span::Ident;
use rustc_span::symbol::kw;

/// The bare `self` path.
pub(crate) fn is_self_path(e: &Expr<'_>) -> bool {
    matches!(&e.kind, ExprKind::Path(QPath::Resolved(None, p))
        if p.segments.len() == 1 && p.segments[0].ident.name == kw::SelfLower)
}

/// `self.field`, as the `self` expression it is read off and the field name.
pub(crate) fn self_field<'h>(e: &Expr<'h>) -> Option<(&'h Expr<'h>, Ident)> {
    match e.kind {
        ExprKind::Field(base, ident) if is_self_path(base) => Some((base, ident)),
        _ => None,
    }
}

/// `e` with every `!` and HIR condition wrapper stripped, in any
/// interleaving, and whether at least one `!` was among them. `!!x` reports
/// negated: the callers ask whether the condition is spelled as a bail-out,
/// not what it evaluates to.
pub(crate) fn peel_not<'h>(mut e: &'h Expr<'h>) -> (&'h Expr<'h>, bool) {
    let mut negated = false;
    loop {
        match e.kind {
            ExprKind::Unary(UnOp::Not, inner) => {
                negated = true;
                e = inner;
            }
            ExprKind::DropTemps(inner) => e = inner,
            _ => return (e, negated),
        }
    }
}

/// The expression's final action is a `return`.
pub(crate) fn ends_in_return(e: &Expr<'_>) -> bool {
    match e.kind {
        ExprKind::Ret(_) => true,
        ExprKind::Block(b, _) => match (b.stmts.last(), b.expr) {
            (_, Some(tail)) => ends_in_return(tail),
            (
                Some(Stmt {
                    kind: StmtKind::Expr(s) | StmtKind::Semi(s),
                    ..
                }),
                None,
            ) => ends_in_return(s),
            _ => false,
        },
        _ => false,
    }
}

/// What a call expression invokes.
pub(crate) enum Callee<'h> {
    /// `f(..)`, `T::f(..)`, `E::V(..)`: whatever the callee path resolves to.
    Path { def: DefId, args: &'h [Expr<'h>] },
    /// `recv.m(..)`: the method type-dependent resolution picked.
    Method {
        def: DefId,
        recv: &'h Expr<'h>,
        args: &'h [Expr<'h>],
    },
}

/// Resolves a call or method call. `def` is unfiltered — constructors and
/// foreign definitions come back too — because each caller wants a different
/// subset. A call through anything but a path (a closure value, a field
/// holding a fn pointer) resolves to nothing.
pub(crate) fn callee_of<'tcx>(cx: &LateContext<'tcx>, e: &Expr<'tcx>) -> Option<Callee<'tcx>> {
    match e.kind {
        ExprKind::Call(callee, args) => {
            let ExprKind::Path(qpath) = &callee.kind else {
                return None;
            };
            let def = cx.qpath_res(qpath, callee.hir_id).opt_def_id()?;
            Some(Callee::Path { def, args })
        }
        ExprKind::MethodCall(_, recv, args, _) => {
            let def = cx.typeck_results().type_dependent_def_id(e.hir_id)?;
            Some(Callee::Method { def, recv, args })
        }
        _ => None,
    }
}
