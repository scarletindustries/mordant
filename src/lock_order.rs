use std::collections::HashMap;

use clippy_utils::visitors::{Descend, for_each_expr};
use rustc_hir::def_id::DefId;
use rustc_hir::intravisit::FnKind;
use rustc_hir::{Body, Expr, ExprKind, FnDecl, PatKind, QPath, Stmt, StmtKind};
use rustc_lint::{LateContext, LateLintPass};
use rustc_middle::ty;
use rustc_span::Span;
use rustc_span::def_id::LocalDefId;

use crate::baseline::emit;
use crate::hir_shapes::{is_self_path, stmt_expr};

rustc_session::declare_lint! {
    /// Flags two lock acquisitions the crate performs in both orders: one
    /// body takes `a` then `b` while `a`'s guard is still live, another takes
    /// `b` then `a`. Each order alone is fine; both together are the shape of
    /// a deadlock waiting for the right interleaving. The claim is only that
    /// both orders exist, with the two locations named.
    ///
    /// Conservative on purpose: only `.lock()`, `.read()` and `.write()` on
    /// receivers whose type is a `Mutex` or `RwLock`, only guards bound by
    /// `let` in the same block, and only second acquisitions in later
    /// statements of that block with no `drop(guard)` in between. A lock is
    /// a field path off `self` together with `self`'s type, so `self.state`
    /// of two different types are two different locks. An acquisition inside
    /// a closure built while the guard is held does not count as a second
    /// lock: when the closure runs is the holder's business, and the closure
    /// body is judged on its own when it takes two locks itself.
    pub LOCK_ORDER,
    Warn,
    "two locks acquired in conflicting orders"
}

/// One lock: the field path off `self`, owned by `self`'s type.
#[derive(Clone, PartialEq, Eq, Hash)]
struct Lock {
    owner: DefId,
    path: String,
}

#[derive(Default)]
pub struct LockOrder {
    /// (first lock, second lock) -> where the pair was seen.
    pairs: HashMap<(Lock, Lock), Span>,
}

rustc_session::impl_lint_pass!(LockOrder => [LOCK_ORDER]);

impl LockOrder {
    pub fn new() -> Self {
        Self::default()
    }
}

/// `self.a.b.lock()` (or `.read()` / `.write()`) on a `Mutex`/`RwLock`
/// receiver: the lock's identity is the field path and the type of `self`.
fn lock_acquisition(cx: &LateContext<'_>, e: &Expr<'_>) -> Option<Lock> {
    let ExprKind::MethodCall(seg, recv, [], _) = &e.kind else {
        return None;
    };
    if !matches!(seg.ident.as_str(), "lock" | "read" | "write") {
        return None;
    }
    let recv_ty = cx.typeck_results().expr_ty_adjusted(recv);
    let ty::Adt(adt, _) = recv_ty.peel_refs().kind() else {
        return None;
    };
    let ty_path = cx.tcx.def_path_str(adt.did());
    if !(ty_path.ends_with("Mutex") || ty_path.ends_with("RwLock")) {
        return None;
    }
    let (root, path) = field_path(recv)?;
    // Adjusted: `self` is the base of the first field access, so this is the
    // type the path is projected from, through `&self` or `self: Arc<Self>`.
    let owner = cx
        .typeck_results()
        .expr_ty_adjusted(root)
        .peel_refs()
        .ty_adt_def()?
        .did();
    Some(Lock { owner, path })
}

/// `self.a.b` rendered as "a.b", with the `self` it hangs off; `None` for
/// anything that is not a plain field chain off `self`, since only those
/// have a stable identity.
fn field_path<'h>(e: &'h Expr<'h>) -> Option<(&'h Expr<'h>, String)> {
    match &e.kind {
        ExprKind::Field(base, ident) => {
            let (root, prefix) = field_path(base)?;
            if prefix.is_empty() {
                Some((root, ident.name.to_string()))
            } else {
                Some((root, format!("{prefix}.{}", ident.name)))
            }
        }
        ExprKind::Path(_) if is_self_path(e) => Some((e, String::new())),
        ExprKind::AddrOf(_, _, inner) | ExprKind::Unary(_, inner) => field_path(inner),
        _ => None,
    }
}

/// The lock acquired by this statement's `let` initializer, with the bound
/// name, when the whole initializer is (or trivially wraps) the acquisition.
fn stmt_lock_binding(cx: &LateContext<'_>, stmt: &Stmt<'_>) -> Option<(Lock, rustc_span::Symbol)> {
    let StmtKind::Let(l) = stmt.kind else {
        return None;
    };
    let init = l.init?;
    let inner = match &init.kind {
        // `.unwrap()` / `.expect(..)` around the lock call.
        ExprKind::MethodCall(seg, recv, ..)
            if matches!(seg.ident.as_str(), "unwrap" | "expect") =>
        {
            recv
        }
        _ => init,
    };
    let lock = lock_acquisition(cx, inner)?;
    let PatKind::Binding(_, _, name, None) = l.pat.kind else {
        return None;
    };
    Some((lock, name.name))
}

fn is_drop_of(e: &Expr<'_>, name: rustc_span::Symbol) -> bool {
    if let ExprKind::Call(callee, [arg]) = &e.kind
        && let ExprKind::Path(QPath::Resolved(_, p)) = &callee.kind
        && p.segments
            .last()
            .is_some_and(|s| s.ident.as_str() == "drop")
        && let ExprKind::Path(QPath::Resolved(None, ap)) = &arg.kind
    {
        return ap.segments.len() == 1 && ap.segments[0].ident.name == name;
    }
    false
}

impl<'tcx> LateLintPass<'tcx> for LockOrder {
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
        // Walk every block: a `let guard = <lock>` makes later statements of
        // the same block "while holding", until a `drop(guard)`.
        for_each_expr(cx, body.value, |e: &Expr<'tcx>| {
            if let ExprKind::Block(block, _) = e.kind {
                for (i, stmt) in block.stmts.iter().enumerate() {
                    let Some((first, guard_name)) = stmt_lock_binding(cx, stmt) else {
                        continue;
                    };
                    for le in block.stmts[i + 1..].iter().filter_map(stmt_expr) {
                        if is_drop_of(le, guard_name) {
                            break;
                        }
                        let mut second: Option<(Lock, Span)> = None;
                        for_each_expr(cx, le, |inner: &Expr<'_>| {
                            // A closure built here runs whenever its holder
                            // decides, possibly after the guard is gone; what
                            // it locks is not locked now.
                            if matches!(inner.kind, ExprKind::Closure(..)) {
                                return std::ops::ControlFlow::<(), Descend>::Continue(Descend::No);
                            }
                            if let Some(l) = lock_acquisition(cx, inner)
                                && l != first
                            {
                                second = Some((l, inner.span));
                            }
                            std::ops::ControlFlow::<(), Descend>::Continue(Descend::Yes)
                        });
                        if let Some((second, at)) = second {
                            self.pairs.entry((first.clone(), second)).or_insert(at);
                        }
                    }
                }
            }
            std::ops::ControlFlow::<()>::Continue(())
        });
    }

    fn check_crate_post(&mut self, cx: &LateContext<'tcx>) {
        let sm = cx.tcx.sess.source_map();
        for ((a, b), span) in &self.pairs {
            // Report each conflicting pair once, from the lexically smaller
            // side, so the two orders produce one finding, not two.
            if a.path >= b.path {
                continue;
            }
            if let Some(rev_span) = self.pairs.get(&(b.clone(), a.clone())) {
                let (a, b) = (&a.path, &b.path);
                emit(
                    cx,
                    LOCK_ORDER,
                    *span,
                    format!(
                        "`{a}` is locked before `{b}` here, but `{b}` before `{a}` at {}",
                        sm.span_to_diagnostic_string(*rev_span),
                    ),
                    "both orders existing is the shape of a deadlock; pick one order and hold to it everywhere",
                );
            }
        }
    }
}
