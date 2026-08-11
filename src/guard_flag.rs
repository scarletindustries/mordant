use std::collections::HashMap;

use crate::baseline::emit;
use rustc_hir::def_id::DefId;
use rustc_hir::intravisit::FnKind;
use rustc_hir::{Body, Expr, ExprKind, FnDecl, Stmt, StmtKind};
use rustc_lint::{LateContext, LateLintPass};
use rustc_middle::ty;
use rustc_span::def_id::LocalDefId;
use rustc_span::symbol::kw;
use rustc_span::{Span, Symbol};

rustc_session::declare_lint! {
    /// Flags a bool field that two or more methods test and bail on at entry:
    /// `if self.flag { return ... }` as the first statement. The ordering
    /// invariant ("call X before Y") is enforced at runtime, per method, and
    /// only where someone remembered. A type per state enforces it at compile
    /// time everywhere.
    pub GUARD_FLAG,
    Warn,
    "bool field enforcing an ordering invariant at runtime"
}

#[derive(Default)]
pub struct GuardFlag {
    guards: HashMap<(DefId, Symbol), usize>,
}

rustc_session::impl_lint_pass!(GuardFlag => [GUARD_FLAG]);

impl GuardFlag {
    pub fn new() -> Self {
        Self::default()
    }
}

/// Strip `!`, parens, and HIR condition wrappers.
fn peel_cond<'tcx>(mut e: &'tcx Expr<'tcx>) -> &'tcx Expr<'tcx> {
    loop {
        match e.kind {
            ExprKind::Unary(rustc_hir::UnOp::Not, inner) | ExprKind::DropTemps(inner) => e = inner,
            _ => return e,
        }
    }
}

/// `self.field` where `self` is the literal receiver.
fn self_bool_field<'tcx>(
    cx: &LateContext<'tcx>,
    e: &'tcx Expr<'tcx>,
) -> Option<(ty::AdtDef<'tcx>, Symbol)> {
    let ExprKind::Field(base, ident) = e.kind else {
        return None;
    };
    let ExprKind::Path(rustc_hir::QPath::Resolved(None, path)) = base.kind else {
        return None;
    };
    if path.segments.len() != 1 || path.segments[0].ident.name != kw::SelfLower {
        return None;
    }
    let ty::Adt(adt, _) = cx.typeck_results().expr_ty(base).peel_refs().kind() else {
        return None;
    };
    if !adt.did().is_local() || !adt.is_struct() {
        return None;
    }
    let is_bool = adt.non_enum_variant().fields.iter().any(|f| {
        f.name == ident.name
            && cx
                .tcx
                .type_of(f.did)
                .instantiate_identity()
                .skip_normalization()
                .is_bool()
    });
    is_bool.then_some((*adt, ident.name))
}

/// The block's final action is a `return`.
fn ends_in_return(e: &Expr<'_>) -> bool {
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

impl<'tcx> LateLintPass<'tcx> for GuardFlag {
    fn check_fn(
        &mut self,
        cx: &LateContext<'tcx>,
        kind: FnKind<'tcx>,
        _decl: &'tcx FnDecl<'tcx>,
        body: &'tcx Body<'tcx>,
        _span: Span,
        _def_id: LocalDefId,
    ) {
        if matches!(kind, FnKind::Closure) {
            return;
        }
        let ExprKind::Block(block, _) = body.value.kind else {
            return;
        };
        let Some(Stmt {
            kind: StmtKind::Expr(first) | StmtKind::Semi(first),
            ..
        }) = block.stmts.first()
        else {
            return;
        };
        let ExprKind::If(cond, then, _) = first.kind else {
            return;
        };
        let Some((adt, field)) = self_bool_field(cx, peel_cond(cond)) else {
            return;
        };
        if ends_in_return(then) {
            *self.guards.entry((adt.did(), field)).or_default() += 1;
        }
    }

    fn check_crate_post(&mut self, cx: &LateContext<'tcx>) {
        for ((did, field), count) in &self.guards {
            if *count < 2 {
                continue;
            }
            let Some(fdef) = cx
                .tcx
                .adt_def(*did)
                .non_enum_variant()
                .fields
                .iter()
                .find(|f| f.name == *field)
            else {
                continue;
            };
            emit(
                cx,
                GUARD_FLAG,
                cx.tcx.def_span(fdef.did),
                format!(
                    "`{field}` is tested and bailed on at the start of {count} methods of `{}`",
                    cx.tcx.def_path_str(*did),
                ),
                "the ordering invariant lives at runtime; a separate type for the guarded state enforces it at compile time",
            );
        }
    }
}
