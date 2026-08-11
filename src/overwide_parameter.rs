use std::collections::{HashMap, HashSet};

use clippy_utils::visitors::for_each_expr;
use rustc_hir::def::{CtorOf, DefKind, Res};
use rustc_hir::def_id::DefId;
use rustc_hir::intravisit::FnKind;
use rustc_hir::{Body, Expr, ExprKind, FnDecl, HirId, Pat, PatExpr, PatExprKind, PatKind, QPath};
use rustc_lint::{LateContext, LateLintPass};
use rustc_middle::ty;
use rustc_span::Span;
use rustc_span::def_id::LocalDefId;

use crate::baseline::emit;

rustc_session::declare_lint! {
    /// Flags a panicking match arm for an enum variant that no existing call
    /// site can send: the function takes the full enum, panics on `E::C`, and
    /// every call site in the crate provably passes a different variant
    /// (constructor literals only — anything else makes the set unknowable
    /// and the lint silent). The parameter type is wider than the function's
    /// real domain; narrowing it turns the panic into a compile error for
    /// future callers.
    pub OVERWIDE_PARAMETER,
    Warn,
    "panicking arm for a variant no existing call site passes"
}

struct EnumParam {
    /// Variants with a diverging panic arm in this fn's body.
    panicking: Vec<(DefId, Span)>,
}

#[derive(Default)]
struct CallFacts {
    passed: HashSet<DefId>,
    sites: usize,
    unknown: bool,
}

#[derive(Default)]
pub struct OverwideParameter {
    /// fn -> per-param enum facts (None for params this lint ignores).
    fns: HashMap<DefId, Vec<Option<EnumParam>>>,
    calls: HashMap<(DefId, usize), CallFacts>,
    /// Functions referenced other than by direct call: indirect callers are
    /// invisible, so their call-site sets are unknowable.
    poisoned: HashSet<DefId>,
}

rustc_session::impl_lint_pass!(OverwideParameter => [OVERWIDE_PARAMETER]);

impl OverwideParameter {
    pub fn new() -> Self {
        Self::default()
    }
}

/// The variant a constructor-literal argument passes, or None for anything
/// short of a literal constructor.
fn ctor_literal_variant(cx: &LateContext<'_>, e: &Expr<'_>) -> Option<DefId> {
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
fn arm_variant(cx: &LateContext<'_>, pat: &Pat<'_>) -> Option<DefId> {
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
fn is_panic_arm(cx: &LateContext<'_>, body: &Expr<'_>) -> bool {
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

impl<'tcx> LateLintPass<'tcx> for OverwideParameter {
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
        // External callers are invisible; only a crate-private fn has a
        // knowable call-site set.
        if cx.effective_visibilities.is_exported(def_id) {
            return;
        }
        // Param locals whose type is a crate-local enum.
        let mut params: Vec<Option<(HirId, DefId)>> = Vec::new();
        for param in body.params {
            let PatKind::Binding(_, hir_id, _, None) = param.pat.kind else {
                params.push(None);
                continue;
            };
            let pty = cx.typeck_results().pat_ty(param.pat).peel_refs();
            match pty.kind() {
                ty::Adt(adt, _) if adt.is_enum() && adt.did().is_local() => {
                    params.push(Some((hir_id, adt.did())));
                }
                _ => params.push(None),
            }
        }
        if params.iter().all(Option::is_none) {
            return;
        }
        // Panicking arms in matches whose scrutinee is exactly the param.
        let mut facts: Vec<Option<EnumParam>> = params
            .iter()
            .map(|p| {
                p.map(|_| EnumParam {
                    panicking: Vec::new(),
                })
            })
            .collect();
        for_each_expr(cx, body.value, |e: &Expr<'tcx>| {
            if let ExprKind::Match(scrut, arms, _) = e.kind
                && let ExprKind::Path(QPath::Resolved(None, path)) = &scrut.kind
                && let Res::Local(scrut_local) = path.res
            {
                for (idx, p) in params.iter().enumerate() {
                    let Some((param_hir, _)) = p else { continue };
                    if *param_hir != scrut_local {
                        continue;
                    }
                    for arm in arms {
                        if arm.guard.is_some() {
                            continue;
                        }
                        if let Some(variant) = arm_variant(cx, arm.pat)
                            && is_panic_arm(cx, arm.body)
                            && let Some(f) = &mut facts[idx]
                        {
                            f.panicking.push((variant, arm.span));
                        }
                    }
                }
            }
            std::ops::ControlFlow::<()>::Continue(())
        });
        self.fns.insert(def_id.to_def_id(), facts);
    }

    fn check_expr(&mut self, cx: &LateContext<'tcx>, expr: &'tcx Expr<'tcx>) {
        match &expr.kind {
            ExprKind::Call(callee, args) => {
                let ExprKind::Path(qpath) = &callee.kind else {
                    return;
                };
                let Some(def) = cx.qpath_res(qpath, callee.hir_id).opt_def_id() else {
                    return;
                };
                if !def.is_local()
                    || !matches!(cx.tcx.def_kind(def), DefKind::Fn | DefKind::AssocFn)
                {
                    return;
                }
                for (i, arg) in args.iter().enumerate() {
                    let facts = self.calls.entry((def, i)).or_default();
                    facts.sites += 1;
                    match ctor_literal_variant(cx, arg) {
                        Some(v) => {
                            facts.passed.insert(v);
                        }
                        None => facts.unknown = true,
                    }
                }
            }
            ExprKind::MethodCall(_, _, args, _) => {
                let Some(def) = cx.typeck_results().type_dependent_def_id(expr.hir_id) else {
                    return;
                };
                if !def.is_local() {
                    return;
                }
                // Method args start after the receiver, which is param 0 in
                // the body's param list only for free fns; for methods the
                // receiver occupies body param 0, so args map to 1..
                for (i, arg) in args.iter().enumerate() {
                    let facts = self.calls.entry((def, i + 1)).or_default();
                    facts.sites += 1;
                    match ctor_literal_variant(cx, arg) {
                        Some(v) => {
                            facts.passed.insert(v);
                        }
                        None => facts.unknown = true,
                    }
                }
            }
            // A bare reference to a local fn (fn pointer, higher-order use):
            // its future call sites are invisible. The callee position of a
            // direct call is not a bare reference; that call is counted above.
            ExprKind::Path(qpath) => {
                let is_direct_callee =
                    cx.tcx
                        .hir_parent_iter(expr.hir_id)
                        .next()
                        .is_some_and(|(_, node)| {
                            matches!(
                                node,
                                rustc_hir::Node::Expr(Expr {
                                    kind: ExprKind::Call(callee, _),
                                    ..
                                }) if callee.hir_id == expr.hir_id
                            )
                        });
                if is_direct_callee {
                    return;
                }
                if let Res::Def(DefKind::Fn | DefKind::AssocFn, def) =
                    cx.qpath_res(qpath, expr.hir_id)
                    && def.is_local()
                {
                    self.poisoned.insert(def);
                }
            }
            _ => {}
        }
    }

    fn check_crate_post(&mut self, cx: &LateContext<'tcx>) {
        for (fn_def, params) in &self.fns {
            if self.poisoned.contains(fn_def) {
                continue;
            }
            for (idx, facts) in params.iter().enumerate() {
                let Some(facts) = facts else { continue };
                let Some(calls) = self.calls.get(&(*fn_def, idx)) else {
                    // Never called: nothing provable about its domain.
                    continue;
                };
                if calls.unknown || calls.sites == 0 {
                    continue;
                }
                for (variant, span) in &facts.panicking {
                    if calls.passed.contains(variant) {
                        continue;
                    }
                    let mut passed: Vec<String> = calls
                        .passed
                        .iter()
                        .map(|v| format!("`{}`", cx.tcx.item_name(*v)))
                        .collect();
                    passed.sort();
                    emit(
                        cx,
                        OVERWIDE_PARAMETER,
                        *span,
                        format!(
                            "all {} call sites of `{}` pass {}; this arm panics on `{}`, which no existing caller sends",
                            calls.sites,
                            cx.tcx.item_name(*fn_def),
                            passed.join(", "),
                            cx.tcx.item_name(*variant),
                        ),
                        "the parameter is wider than the function's domain; narrow the type and the panic becomes a compile error for future callers",
                    );
                }
            }
        }
    }
}
