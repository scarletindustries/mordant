use clippy_utils::visitors::for_each_expr;
use rustc_hir::{Block, Expr, ExprKind, QPath, Stmt, StmtKind};
use rustc_lint::{LateContext, LateLintPass};
use rustc_middle::ty;
use rustc_span::sym;
use rustc_span::symbol::kw;

use crate::baseline::emit;

rustc_session::declare_lint! {
    /// Flags `map.get(&k).unwrap()` when the presence it bets on was proved a
    /// few statements up by `map.insert(k, ..)`, with nothing in between that
    /// could touch the map or the key: no calls, no assignments to either.
    /// The unwrap re-fetches a value the code already held, and the panic
    /// path plus the second hash both disappear by keeping it.
    pub INSERT_THEN_UNWRAP,
    Warn,
    "unwrap of a lookup proven by an insert just above"
}

rustc_session::declare_lint_pass!(InsertThenUnwrap => [INSERT_THEN_UNWRAP]);

/// A stable textual identity for the small expressions worth tracking:
/// `self.a.b` chains, plain locals, and literals. Anything else is `None`,
/// and untrackable means unprovable means silent.
fn identity(e: &Expr<'_>) -> Option<String> {
    match &e.kind {
        ExprKind::Field(base, ident) => Some(format!("{}.{}", identity(base)?, ident.name)),
        ExprKind::Path(QPath::Resolved(None, p)) if p.segments.len() == 1 => {
            let seg = p.segments[0].ident;
            if seg.name == kw::SelfLower {
                Some("self".to_string())
            } else {
                Some(seg.name.to_string())
            }
        }
        ExprKind::Lit(lit) => Some(format!("lit:{:?}", lit.node)),
        ExprKind::AddrOf(_, _, inner) | ExprKind::Unary(_, inner) => identity(inner),
        _ => None,
    }
}

fn is_map(cx: &LateContext<'_>, recv: &Expr<'_>) -> bool {
    let ty::Adt(adt, _) = cx
        .typeck_results()
        .expr_ty_adjusted(recv)
        .peel_refs()
        .kind()
    else {
        return false;
    };
    cx.tcx.is_diagnostic_item(sym::HashMap, adt.did())
        || cx.tcx.is_diagnostic_item(sym::BTreeMap, adt.did())
}

/// `map.insert(k, _)` as (map identity, key identity).
fn insert_of(cx: &LateContext<'_>, e: &Expr<'_>) -> Option<(String, String)> {
    let ExprKind::MethodCall(seg, recv, [k, _], _) = &e.kind else {
        return None;
    };
    if seg.ident.as_str() != "insert" || !is_map(cx, recv) {
        return None;
    }
    Some((identity(recv)?, identity(k)?))
}

/// `map.get(&k).unwrap()` (or `.expect(..)`) as (map, key, whole span).
fn get_unwrap_of<'tcx>(
    cx: &LateContext<'tcx>,
    e: &'tcx Expr<'tcx>,
) -> Option<(String, String, String, rustc_span::Span)> {
    let ExprKind::MethodCall(useg, urecv, _, _) = &e.kind else {
        return None;
    };
    if !matches!(useg.ident.as_str(), "unwrap" | "expect") {
        return None;
    }
    let ExprKind::MethodCall(gseg, grecv, [k], _) = &urecv.kind else {
        return None;
    };
    if !matches!(gseg.ident.as_str(), "get" | "get_mut") || !is_map(cx, grecv) {
        return None;
    }
    let shown = clippy_utils::source::snippet_opt(cx, k.span)
        .unwrap_or_else(|| "..".to_string())
        .trim_start_matches('&')
        .to_string();
    Some((identity(grecv)?, identity(k)?, shown, e.span))
}

/// True when this statement could invalidate a tracked (map, key) fact:
/// any call at all, or any assignment. Purity is not analyzed; anything
/// callable is assumed able to touch the map, which can only cause silence.
fn may_disturb<'tcx>(cx: &LateContext<'tcx>, e: &'tcx Expr<'tcx>) -> bool {
    let mut disturbs = false;
    for_each_expr(cx, e, |inner: &Expr<'tcx>| {
        match &inner.kind {
            ExprKind::MethodCall(..) | ExprKind::Call(..) => {
                // The proven lookup itself is a call; the caller filters it
                // out by checking lookups before disturbance.
                disturbs = true;
            }
            ExprKind::Assign(..) | ExprKind::AssignOp(..) => disturbs = true,
            _ => {}
        }
        std::ops::ControlFlow::<()>::Continue(())
    });
    disturbs
}

impl<'tcx> LateLintPass<'tcx> for InsertThenUnwrap {
    fn check_block(&mut self, cx: &LateContext<'tcx>, block: &'tcx Block<'tcx>) {
        for (i, stmt) in block.stmts.iter().enumerate() {
            let (StmtKind::Expr(e) | StmtKind::Semi(e)) = stmt.kind else {
                continue;
            };
            let Some((map, key)) = insert_of(cx, e) else {
                continue;
            };
            for later in &block.stmts[i + 1..] {
                let le = match later.kind {
                    StmtKind::Expr(le) | StmtKind::Semi(le) => Some(le),
                    StmtKind::Let(l) => l.init,
                    StmtKind::Item(_) => None,
                };
                let Some(le) = le else { continue };
                // The lookup must be the statement's own top-level shape (or
                // the let initializer); a lookup buried under other calls is
                // checked as a disturbance instead.
                if let Some((gmap, gkey, shown, at)) = get_unwrap_of(cx, le)
                    && gmap == map
                    && gkey == key
                {
                    let map_shown = map.strip_prefix("self.").unwrap_or(&map);
                    emit(
                        cx,
                        INSERT_THEN_UNWRAP,
                        at,
                        format!(
                            "this unwrap re-fetches `{map_shown}[{shown}]`, which the insert above just proved present"
                        ),
                        "keep the inserted value, or use the entry API; the panic path and the second lookup both vanish",
                    );
                    return;
                }
                if may_disturb(cx, le) {
                    break;
                }
            }
        }
    }
}
