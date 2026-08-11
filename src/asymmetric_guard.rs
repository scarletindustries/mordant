use std::collections::{HashMap, HashSet};

use clippy_utils::visitors::for_each_expr;
use rustc_hir::def_id::DefId;
use rustc_hir::intravisit::FnKind;
use rustc_hir::{Block, Body, Expr, ExprKind, FnDecl, QPath, Stmt, StmtKind, UnOp};
use rustc_lint::{LateContext, LateLintPass};
use rustc_middle::ty;
use rustc_span::def_id::LocalDefId;
use rustc_span::symbol::kw;
use rustc_span::{Span, Symbol};

use crate::baseline::emit;

rustc_session::declare_lint! {
    /// Flags a guarded call whose guard cannot be sound: `self.can_x()` gates
    /// `self.y()`, but `y` touches a field of `self` that `can_x` never reads,
    /// directly or through anything it calls. A predicate blind to part of the
    /// state its action manipulates answers a different question than the one
    /// being asked. The guard/mutator pair drifting apart is how a live
    /// connection gets evicted by a donation its guard approved.
    pub ASYMMETRIC_GUARD,
    Warn,
    "guarded call touches state its guard never reads"
}

#[derive(Default)]
struct MethodFacts {
    /// `self.field` occurrences, read or written.
    touched: HashSet<Symbol>,
    /// Same-type inherent methods called on `self`.
    calls: HashSet<DefId>,
}

struct Gate {
    guard: DefId,
    action: DefId,
    span: Span,
}

#[derive(Default)]
pub struct AsymmetricGuard {
    facts: HashMap<DefId, MethodFacts>,
    gates: Vec<Gate>,
}

rustc_session::impl_lint_pass!(AsymmetricGuard => [ASYMMETRIC_GUARD]);

impl AsymmetricGuard {
    pub fn new() -> Self {
        Self::default()
    }
}

fn is_self_path(e: &Expr<'_>) -> bool {
    matches!(&e.kind, ExprKind::Path(QPath::Resolved(None, p))
        if p.segments.len() == 1 && p.segments[0].ident.name == kw::SelfLower)
}

/// The inherent method a `self.m(..)` call resolves to, with its self type,
/// when that type is a crate-local ADT.
fn self_method_call(cx: &LateContext<'_>, e: &Expr<'_>) -> Option<(DefId, DefId)> {
    let ExprKind::MethodCall(_, recv, ..) = &e.kind else {
        return None;
    };
    if !is_self_path(recv) {
        return None;
    }
    let method = cx.typeck_results().type_dependent_def_id(e.hir_id)?;
    let impl_did = cx.tcx.impl_of_assoc(method)?;
    if !matches!(
        cx.tcx.def_kind(impl_did),
        rustc_hir::def::DefKind::Impl { of_trait: false }
    ) {
        return None;
    }
    let self_ty = cx
        .tcx
        .type_of(impl_did)
        .instantiate_identity()
        .skip_normalization();
    if let ty::Adt(adt, _) = self_ty.kind()
        && adt.did().is_local()
    {
        Some((method, adt.did()))
    } else {
        None
    }
}

/// A guard is a permission-flavored boolean method: the names that promise
/// "this action is safe to take".
fn is_guard_name(cx: &LateContext<'_>, method: DefId) -> bool {
    let name = cx.tcx.item_name(method);
    let n = name.as_str();
    n.starts_with("can_") || n.starts_with("may_") || n.starts_with("check_")
}

/// Peel `!` and grouping to the guard call, if the condition is exactly one.
fn guard_call_of<'tcx>(cond: &'tcx Expr<'tcx>) -> (&'tcx Expr<'tcx>, bool) {
    match cond.kind {
        ExprKind::Unary(UnOp::Not, inner) => {
            let (e, _) = guard_call_of(inner);
            (e, true)
        }
        ExprKind::DropTemps(inner) => guard_call_of(inner),
        _ => (cond, false),
    }
}

fn block_ends_in_return(e: &Expr<'_>) -> bool {
    match e.kind {
        ExprKind::Ret(_) => true,
        ExprKind::Block(b, _) => match (b.stmts.last(), b.expr) {
            (_, Some(tail)) => block_ends_in_return(tail),
            (
                Some(Stmt {
                    kind: StmtKind::Expr(s) | StmtKind::Semi(s),
                    ..
                }),
                None,
            ) => block_ends_in_return(s),
            _ => false,
        },
        _ => false,
    }
}

impl AsymmetricGuard {
    fn collect_actions<'tcx>(
        &mut self,
        cx: &LateContext<'tcx>,
        scope: &'tcx Expr<'tcx>,
        guard: DefId,
        adt: DefId,
    ) {
        let mut gates = Vec::new();
        for_each_expr(cx, scope, |e: &Expr<'_>| {
            if let Some((m, m_adt)) = self_method_call(cx, e)
                && m_adt == adt
                && m != guard
            {
                gates.push(Gate {
                    guard,
                    action: m,
                    span: e.span,
                });
            }
            std::ops::ControlFlow::<()>::Continue(())
        });
        self.gates.extend(gates);
    }
}

impl<'tcx> LateLintPass<'tcx> for AsymmetricGuard {
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
        // Facts: which self-fields this method touches, and what it calls.
        let method = def_id.to_def_id();
        let mut facts = MethodFacts::default();
        for_each_expr(cx, body.value, |e: &Expr<'_>| {
            match &e.kind {
                ExprKind::Field(base, ident) if is_self_path(base) => {
                    facts.touched.insert(ident.name);
                }
                _ => {
                    if let Some((m, _)) = self_method_call(cx, e) {
                        facts.calls.insert(m);
                    }
                }
            }
            std::ops::ControlFlow::<()>::Continue(())
        });
        self.facts.insert(method, facts);
    }

    fn check_block(&mut self, cx: &LateContext<'tcx>, block: &'tcx Block<'tcx>) {
        // `if !self.can_x() { return } ... self.y()` — the guard covers the
        // rest of the block.
        for (i, stmt) in block.stmts.iter().enumerate() {
            let (StmtKind::Expr(e) | StmtKind::Semi(e)) = stmt.kind else {
                continue;
            };
            let ExprKind::If(cond, then, None) = e.kind else {
                continue;
            };
            let (gexpr, negated) = guard_call_of(cond);
            let Some((guard, adt)) = self_method_call(cx, gexpr) else {
                continue;
            };
            if !is_guard_name(cx, guard) {
                continue;
            }
            if negated && block_ends_in_return(then) {
                for later in &block.stmts[i + 1..] {
                    match later.kind {
                        StmtKind::Expr(le) | StmtKind::Semi(le) => {
                            self.collect_actions(cx, le, guard, adt);
                        }
                        // The real-world shape binds the result:
                        // `let connections = self.detach_fds(&victim);`.
                        StmtKind::Let(l) => {
                            if let Some(init) = l.init {
                                self.collect_actions(cx, init, guard, adt);
                            }
                        }
                        StmtKind::Item(_) => {}
                    }
                }
                if let Some(tail) = block.expr {
                    self.collect_actions(cx, tail, guard, adt);
                }
            } else if !negated {
                // `if self.can_x() { self.y() }` — the then-branch is gated.
                self.collect_actions(cx, then, guard, adt);
            }
        }
    }

    fn check_crate_post(&mut self, cx: &LateContext<'tcx>) {
        for gate in &self.gates {
            let Some(action) = self.facts.get(&gate.action) else {
                continue;
            };
            // Only mutations: a guarded read cannot invalidate anything. The
            // honest mutator signal is the receiver: `&mut self`.
            let sig = cx
                .tcx
                .fn_sig(gate.action)
                .instantiate_identity()
                .skip_binder();
            let mutates = sig
                .inputs()
                .first()
                .is_some_and(|t| matches!(t.kind(), ty::Ref(_, _, m) if m.is_mut()));
            if !mutates {
                continue;
            }
            // The guard's coverage is everything it touches transitively
            // through same-type calls, so a guard delegating to helpers is
            // never accused falsely.
            let mut covered: HashSet<Symbol> = HashSet::new();
            let mut queue = vec![gate.guard];
            let mut seen: HashSet<DefId> = HashSet::new();
            while let Some(m) = queue.pop() {
                if !seen.insert(m) {
                    continue;
                }
                if let Some(f) = self.facts.get(&m) {
                    covered.extend(f.touched.iter().copied());
                    queue.extend(f.calls.iter().copied());
                }
            }
            let mut missed: Vec<&Symbol> = action
                .touched
                .iter()
                .filter(|f| !covered.contains(f))
                .collect();
            if missed.is_empty() {
                continue;
            }
            missed.sort_by_key(|s| s.as_str().to_owned());
            let fields: Vec<String> = missed.iter().map(|s| format!("`{s}`")).collect();
            emit(
                cx,
                ASYMMETRIC_GUARD,
                gate.span,
                format!(
                    "`{}` is gated by `{}`, but touches {} which the guard never reads",
                    cx.tcx.item_name(gate.action),
                    cx.tcx.item_name(gate.guard),
                    fields.join(", "),
                ),
                "a guard blind to part of the state its action manipulates cannot be sound; align what the pair reads",
            );
        }
    }
}
