use std::collections::{HashMap, HashSet};

use clippy_utils::visitors::for_each_expr;
use rustc_hir::def::DefKind;
use rustc_hir::def_id::DefId;
use rustc_hir::intravisit::FnKind;
use rustc_hir::{Body, Expr, ExprKind, FnDecl};
use rustc_lint::{LateContext, LateLintPass};
use rustc_middle::ty;
use rustc_span::Span;
use rustc_span::def_id::LocalDefId;

use crate::baseline::emit;
use crate::enum_facts::{arm_variant, ctor_literal_variant, is_panic_arm};

rustc_session::declare_lint! {
    /// Flags a panicking match arm for a variant the matched call can never
    /// produce: the callee's return type is a crate-local enum, every value
    /// its body returns is a constructor literal, and the panicked-on variant
    /// is not among them. The return type promises more than the function
    /// delivers; narrowing it deletes the caller's panic arm at compile time.
    ///
    /// Any return position that is not a constructor literal makes the set
    /// unknowable and the function is skipped entirely.
    pub NARROWED_RETURN,
    Warn,
    "panicking arm for a variant the callee never constructs"
}

#[derive(Default)]
pub struct NarrowedReturn {
    /// fn -> provably-complete set of returned variants.
    returns: HashMap<DefId, HashSet<DefId>>,
    /// (callee, panicked variant, arm span, caller-visible name) discovered
    /// at match sites, resolved against `returns` at the end.
    suspects: Vec<(DefId, DefId, Span)>,
}

rustc_session::impl_lint_pass!(NarrowedReturn => [NARROWED_RETURN]);

impl NarrowedReturn {
    pub fn new() -> Self {
        Self::default()
    }
}

/// Collect every expression that can leave the function as its value:
/// explicit `return e`, plus the tail position, followed through blocks,
/// both `if` branches, and every match arm. Anything else in tail position
/// (a loop, a call, a variable) is not a constructor literal and poisons.
fn tail_exprs<'tcx>(e: &'tcx Expr<'tcx>, out: &mut Vec<&'tcx Expr<'tcx>>) {
    match &e.kind {
        ExprKind::Block(b, _) => {
            if let Some(tail) = b.expr {
                tail_exprs(tail, out);
            } else {
                // A block with no tail returns (); for an enum-returning fn
                // that means this path diverges, contributing nothing.
            }
        }
        ExprKind::If(_, then, els) => {
            tail_exprs(then, out);
            if let Some(els) = els {
                tail_exprs(els, out);
            }
        }
        ExprKind::Match(_, arms, _) => {
            for arm in *arms {
                tail_exprs(arm.body, out);
            }
        }
        _ => out.push(e),
    }
}

impl<'tcx> LateLintPass<'tcx> for NarrowedReturn {
    fn check_fn(
        &mut self,
        cx: &LateContext<'tcx>,
        kind: FnKind<'tcx>,
        _decl: &'tcx FnDecl<'tcx>,
        body: &'tcx Body<'tcx>,
        _span: Span,
        def_id: LocalDefId,
    ) {
        if matches!(kind, FnKind::Closure) {
            return;
        }
        // Only fns returning a crate-local enum directly.
        let ret_ty = cx.typeck_results().expr_ty(body.value);
        let ty::Adt(adt, _) = ret_ty.kind() else {
            return;
        };
        if !adt.is_enum() || !adt.did().is_local() {
            return;
        }

        let mut leaves: Vec<&Expr<'_>> = Vec::new();
        tail_exprs(body.value, &mut leaves);
        for_each_expr(cx, body.value, |e: &Expr<'tcx>| {
            if let ExprKind::Ret(Some(v)) = &e.kind {
                tail_exprs(v, &mut leaves);
            }
            std::ops::ControlFlow::<()>::Continue(())
        });

        let mut set = HashSet::new();
        for leaf in leaves {
            // A diverging leaf (panic, return-of-return) yields no value.
            if cx.typeck_results().expr_ty(leaf).is_never() {
                continue;
            }
            match ctor_literal_variant(cx, leaf) {
                Some(v) => {
                    set.insert(v);
                }
                // One non-literal return and the whole set is unknowable.
                None => return,
            }
        }
        if !set.is_empty() {
            self.returns.insert(def_id.to_def_id(), set);
        }
    }

    fn check_expr(&mut self, cx: &LateContext<'tcx>, expr: &'tcx Expr<'tcx>) {
        // `match f(..) { ... E::V => panic!(), ... }` with the call as the
        // direct scrutinee: no aliasing to reason about.
        let ExprKind::Match(scrut, arms, _) = expr.kind else {
            return;
        };
        let ExprKind::Call(callee, _) = &scrut.kind else {
            return;
        };
        let ExprKind::Path(qpath) = &callee.kind else {
            return;
        };
        let Some(def) = cx.qpath_res(qpath, callee.hir_id).opt_def_id() else {
            return;
        };
        if !def.is_local() || !matches!(cx.tcx.def_kind(def), DefKind::Fn | DefKind::AssocFn) {
            return;
        }
        for arm in arms {
            if arm.guard.is_some() {
                continue;
            }
            if let Some(variant) = arm_variant(cx, arm.pat)
                && is_panic_arm(cx, arm.body)
            {
                self.suspects.push((def, variant, arm.span));
            }
        }
    }

    fn check_crate_post(&mut self, cx: &LateContext<'tcx>) {
        for (callee, variant, span) in &self.suspects {
            let Some(returned) = self.returns.get(callee) else {
                continue;
            };
            if returned.contains(variant) {
                continue;
            }
            let mut names: Vec<String> = returned
                .iter()
                .map(|v| format!("`{}`", cx.tcx.item_name(*v)))
                .collect();
            names.sort();
            emit(
                cx,
                NARROWED_RETURN,
                *span,
                format!(
                    "`{}` only ever returns {}; this arm panics on `{}`, which it never constructs",
                    cx.tcx.item_name(*callee),
                    names.join(", "),
                    cx.tcx.item_name(*variant),
                ),
                "the return type promises more than the function delivers; narrow it and this arm becomes unnecessary at compile time",
            );
        }
    }
}
