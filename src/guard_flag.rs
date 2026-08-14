use std::collections::{HashMap, HashSet};

use crate::adt_facts::{field_ty, struct_field};
use crate::baseline::emit;
use crate::hir_shapes::{ends_in_return, peel_not, self_field};
use rustc_hir::def_id::DefId;
use rustc_hir::intravisit::FnKind;
use rustc_hir::{Body, Expr, ExprKind, FnDecl, Mutability, Stmt, StmtKind, UnOp};
use rustc_lint::{LateContext, LateLintPass};
use rustc_middle::ty;
use rustc_span::def_id::LocalDefId;
use rustc_span::{Span, Symbol};

rustc_session::declare_lint! {
    /// Flags a bool field that two or more methods test and bail on at entry:
    /// `if self.flag { return ... }` as the first statement. The ordering
    /// invariant ("call X before Y") is enforced at runtime, per method, and
    /// only where someone remembered. A type per state enforces it at compile
    /// time everywhere.
    ///
    /// The field must also be written somewhere after construction: a flag
    /// that is only ever set in a literal (`is_server`, `minify`) is a role
    /// or an option, and the methods bailing on it are not enforcing an
    /// order.
    pub GUARD_FLAG,
    Warn,
    "bool field enforcing an ordering invariant at runtime"
}

#[derive(Default)]
pub struct GuardFlag {
    guards: HashMap<(DefId, Symbol), usize>,
    /// Fields assigned, compound-assigned, or mutably borrowed anywhere in
    /// the crate: the ones whose value is a state rather than a setting.
    written: HashSet<(DefId, Symbol)>,
}

rustc_session::impl_lint_pass!(GuardFlag => [GUARD_FLAG]);

impl GuardFlag {
    pub fn new() -> Self {
        Self::default()
    }
}

/// `self.field` where `self` is the literal receiver.
fn self_bool_field<'tcx>(
    cx: &LateContext<'tcx>,
    e: &'tcx Expr<'tcx>,
) -> Option<(ty::AdtDef<'tcx>, Symbol)> {
    let (base, ident) = self_field(e)?;
    let ty::Adt(adt, _) = cx.typeck_results().expr_ty(base).peel_refs().kind() else {
        return None;
    };
    if !adt.did().is_local() || !adt.is_struct() {
        return None;
    }
    let is_bool = struct_field(*adt, ident.name).is_some_and(|f| field_ty(cx, f).is_bool());
    is_bool.then_some((*adt, ident.name))
}

/// The struct field a written place denotes, for any receiver: `x.flag`,
/// `(*this).flag`, `self.inner.flag`.
fn written_field<'tcx>(cx: &LateContext<'tcx>, place: &'tcx Expr<'tcx>) -> Option<(DefId, Symbol)> {
    let mut place = place;
    while let ExprKind::Unary(UnOp::Deref, inner) | ExprKind::DropTemps(inner) = place.kind {
        place = inner;
    }
    let ExprKind::Field(base, ident) = place.kind else {
        return None;
    };
    let adt = cx
        .typeck_results()
        .expr_ty_adjusted(base)
        .peel_refs()
        .ty_adt_def()?;
    (adt.is_struct() && adt.did().is_local()).then(|| (adt.did(), ident.name))
}

impl<'tcx> LateLintPass<'tcx> for GuardFlag {
    fn check_expr(&mut self, cx: &LateContext<'tcx>, expr: &'tcx Expr<'tcx>) {
        let place = match expr.kind {
            ExprKind::Assign(place, ..) | ExprKind::AssignOp(_, place, _) => place,
            ExprKind::AddrOf(_, Mutability::Mut, place) => place,
            _ => return,
        };
        if let Some(key) = written_field(cx, place) {
            self.written.insert(key);
        }
    }

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
        let Some((adt, field)) = self_bool_field(cx, peel_not(cond).0) else {
            return;
        };
        if ends_in_return(then) {
            *self.guards.entry((adt.did(), field)).or_default() += 1;
        }
    }

    fn check_crate_post(&mut self, cx: &LateContext<'tcx>) {
        for ((did, field), count) in &self.guards {
            if *count < 2 || !self.written.contains(&(*did, *field)) {
                continue;
            }
            let Some(fdef) = struct_field(cx.tcx.adt_def(*did), *field) else {
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
