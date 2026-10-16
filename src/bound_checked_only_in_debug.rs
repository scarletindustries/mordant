use std::collections::HashMap;
use std::ops::ControlFlow;

use clippy_utils::SpanlessEq;
use clippy_utils::res::MaybeResPath;
use clippy_utils::source::snippet;
use clippy_utils::visitors::{Descend, for_each_expr};
use rustc_hir::{BinOpKind, Block, Expr, ExprKind, HirId, Mutability, Pat, PatKind, UnOp};
use rustc_lint::{LateContext, LateLintPass, LintContext};
use rustc_middle::ty::adjustment::{Adjust, AutoBorrow, AutoBorrowMutability};
use rustc_middle::ty::{self, Ty};
use rustc_span::{Span, SyntaxContext, sym};

use crate::baseline::emit_with_note;
use crate::hir_shapes::{debug_only, debug_only_conditions, enclosing_fn, stmt_expr};
use crate::int_facts::{IntLayout, const_value, int_layout, peel_widening};

rustc_lint::declare_lint! {
    /// Flags an integer that a `debug_assert!` bounds and that the same
    /// function then narrows: an `as` cast to a smaller type, a `& MASK` of
    /// low bits, or a `<< N` into a fixed-width word. Release builds drop the
    /// check, so a value over the bound wraps or loses bits with no error.
    ///
    /// The debug check must compare the same local, field or `.len()` against
    /// a constant, in a statement before the narrowing in the same block.
    /// The bound must fit the narrowed width, which shows the check is what
    /// makes the narrowing safe. Nothing may write the value in between. The
    /// lint stays silent when the function also bounds the value in release
    /// code: a comparison with a constant, `try_from`, `try_into`, `min`,
    /// `clamp`, a `checked_` or `saturating_` method, a range pattern, or an
    /// index into a fixed-size array that panics past its length.
    pub BOUND_CHECKED_ONLY_IN_DEBUG,
    Warn,
    "an integer bounded only by a debug assertion and then narrowed with a cast, mask or shift"
}

#[derive(Default)]
pub struct BoundCheckedOnlyInDebug {
    /// Narrowing span to (check span, message, help). A later check of the
    /// same value replaces an earlier one.
    findings: HashMap<Span, (Span, String, String)>,
}

rustc_lint::impl_lint_pass!(BoundCheckedOnlyInDebug => [BOUND_CHECKED_ONLY_IN_DEBUG]);

/// `place <= max`, checked only in debug builds.
struct Bound<'tcx> {
    place: &'tcx Expr<'tcx>,
    root: HirId,
    layout: IntLayout,
    max: u128,
}

enum Cut<'tcx> {
    Cast(Ty<'tcx>),
    Mask(u128),
    Shift(u128, Ty<'tcx>),
}

/// The local a place is read from: `x`, `x.f.g`, `*x`, or `x.len()`.
/// `None` for anything else, and for reads through a raw pointer, whose
/// target other code can write.
fn place_root(cx: &LateContext<'_>, e: &Expr<'_>) -> Option<HirId> {
    match e.kind {
        ExprKind::Field(base, _) | ExprKind::Index(base, ..) => place_root(cx, base),
        ExprKind::Unary(UnOp::Deref, base) => {
            if cx.typeck_results().expr_ty(base).is_raw_ptr() {
                return None;
            }
            place_root(cx, base)
        }
        ExprKind::MethodCall(seg, recv, [], _) if seg.ident.name == sym::len => {
            place_root(cx, recv)
        }
        _ => e.res_local_id(),
    }
}

/// The upper bounds a debug condition gives: `x <= C`, `x < C`, `C >= x`,
/// `C > x`, and each side of `&&`.
fn bounds_in<'tcx>(cx: &LateContext<'tcx>, cond: &'tcx Expr<'tcx>, out: &mut Vec<Bound<'tcx>>) {
    let ExprKind::Binary(op, l, r) = cond.kind else {
        if let ExprKind::DropTemps(inner) = cond.kind {
            bounds_in(cx, inner, out);
        }
        return;
    };
    let (place, limit, strict) = match op.node {
        BinOpKind::And => {
            bounds_in(cx, l, out);
            bounds_in(cx, r, out);
            return;
        }
        BinOpKind::Le => (l, r, false),
        BinOpKind::Lt => (l, r, true),
        BinOpKind::Ge => (r, l, false),
        BinOpKind::Gt => (r, l, true),
        _ => return,
    };
    let place = peel_widening(cx, place);
    let Some(limit) = const_value(cx, limit) else {
        return;
    };
    let max = if strict {
        limit.checked_sub(1)
    } else {
        Some(limit)
    };
    let layout = int_layout(cx, cx.typeck_results().expr_ty(place));
    if let (Some(max), Some(layout), Some(root)) = (max, layout, place_root(cx, place))
        && !layout.signed
    {
        out.push(Bound {
            place,
            root,
            layout,
            max,
        });
    }
}

fn is_place<'tcx>(cx: &LateContext<'tcx>, e: &'tcx Expr<'tcx>, place: &Expr<'_>) -> bool {
    SpanlessEq::new(cx).eq_expr(SyntaxContext::root(), peel_widening(cx, e), place)
}

/// How `e` narrows the bound value, and the largest value that survives it.
fn narrowing<'tcx>(
    cx: &LateContext<'tcx>,
    e: &'tcx Expr<'tcx>,
    place: &Expr<'_>,
) -> Option<(Cut<'tcx>, u128)> {
    let ty = cx.typeck_results().expr_ty(e);
    match e.kind {
        ExprKind::Cast(op, _) if is_place(cx, op, place) => {
            Some((Cut::Cast(ty), int_layout(cx, ty)?.max()))
        }
        ExprKind::Binary(op, l, r) if op.node == BinOpKind::BitAnd => {
            let mask = if is_place(cx, l, place) {
                const_value(cx, r)?
            } else if is_place(cx, r, place) {
                const_value(cx, l)?
            } else {
                return None;
            };
            // Only a mask of low bits keeps values up to some limit intact.
            (mask.checked_add(1).is_none_or(u128::is_power_of_two))
                .then_some((Cut::Mask(mask), mask))
        }
        ExprKind::Binary(op, l, r) if op.node == BinOpKind::Shl && is_place(cx, l, place) => {
            let shift = const_value(cx, r)?;
            let room = int_layout(cx, ty)?
                .max()
                .checked_shr(u32::try_from(shift).ok()?)?;
            Some((Cut::Shift(shift, ty), room))
        }
        _ => None,
    }
}

/// The root local is written, or borrowed mutably, by `e`.
fn writes_root(cx: &LateContext<'_>, e: &Expr<'_>, root: HirId) -> bool {
    match e.kind {
        ExprKind::Assign(lhs, ..) | ExprKind::AssignOp(_, lhs, _) => {
            place_root(cx, lhs) == Some(root)
        }
        ExprKind::AddrOf(_, Mutability::Mut, inner) => place_root(cx, inner) == Some(root),
        _ => {
            place_root(cx, e) == Some(root)
                && cx.typeck_results().expr_adjustments(e).iter().any(|a| {
                    matches!(
                        a.kind,
                        Adjust::Borrow(AutoBorrow::Ref(AutoBorrowMutability::Mut { .. }))
                    )
                })
        }
    }
}

fn has_range_pat(pat: &Pat<'_>) -> bool {
    let mut found = false;
    pat.walk_always(|p| found |= matches!(p.kind, PatKind::Range(..)));
    found
}

/// The indexes of `e` when it indexes a fixed-size array, which panics past
/// its length: `n` for `buf[n]`, and each end of `buf[a..b]`.
fn array_indexes<'tcx>(cx: &LateContext<'tcx>, e: &'tcx Expr<'tcx>) -> Vec<&'tcx Expr<'tcx>> {
    let ExprKind::Index(base, idx, _) = e.kind else {
        return Vec::new();
    };
    let base_ty = cx.typeck_results().expr_ty_adjusted(base).peel_refs();
    if !matches!(base_ty.kind(), ty::Array(..)) {
        return Vec::new();
    }
    match idx.kind {
        ExprKind::Struct(_, fields, _) => fields.iter().map(|f| f.expr).collect(),
        _ => vec![idx],
    }
}

/// Release code in the function bounds `place` too, so the debug check is
/// not its only guard.
fn bounded_in_release<'tcx>(
    cx: &LateContext<'tcx>,
    body: &'tcx Expr<'tcx>,
    place: &Expr<'_>,
) -> bool {
    // Locals that index a fixed-size array: `i < place` with `buf[i]` in the
    // loop bounds `place` by the array's length.
    let mut indexes = Vec::new();
    for_each_expr(cx.tcx, body, |e| {
        indexes.extend(
            array_indexes(cx, e)
                .into_iter()
                .filter_map(|i| i.res_local_id()),
        );
        ControlFlow::<()>::Continue(())
    });
    let is_index = |e: &Expr<'_>| e.res_local_id().is_some_and(|id| indexes.contains(&id));
    let bounds = |e: &'tcx Expr<'tcx>| match e.kind {
        ExprKind::Binary(op, l, r)
            if matches!(
                op.node,
                BinOpKind::Lt | BinOpKind::Le | BinOpKind::Gt | BinOpKind::Ge
            ) =>
        {
            let (small, big) = match op.node {
                BinOpKind::Lt | BinOpKind::Le => (l, r),
                _ => (r, l),
            };
            (is_place(cx, l, place) && const_value(cx, r).is_some())
                || (is_place(cx, r, place) && const_value(cx, l).is_some())
                || (is_place(cx, big, place) && is_index(small))
        }
        ExprKind::MethodCall(seg, recv, ..) if is_place(cx, recv, place) => {
            let name = seg.ident.as_str();
            matches!(name, "min" | "clamp" | "try_into")
                || name.starts_with("checked_")
                || name.starts_with("saturating_")
        }
        ExprKind::Call(callee, [arg]) if is_place(cx, arg, place) => {
            matches!(callee.kind, ExprKind::Path(rustc_hir::QPath::TypeRelative(_, seg)) if seg.ident.as_str() == "try_from")
        }
        ExprKind::Match(scrut, arms, _) => {
            arms.iter().any(|a| has_range_pat(a.pat)) && is_place(cx, scrut, place)
        }
        ExprKind::Let(l) => has_range_pat(l.pat) && is_place(cx, l.init, place),
        ExprKind::Index(..) => array_indexes(cx, e).iter().any(|i| is_place(cx, i, place)),
        _ => false,
    };
    for_each_expr(cx.tcx, body, |e| {
        if debug_only(cx, e).is_some() {
            return ControlFlow::Continue(Descend::No);
        }
        if bounds(e) {
            return ControlFlow::Break(());
        }
        ControlFlow::Continue(Descend::Yes)
    })
    .is_some()
}

/// The message and help for one narrowing.
fn describe(cut: &Cut<'_>, shown: &str, from: Ty<'_>) -> (String, String) {
    let only = "Only a debug assertion bounds it, and release mode removes that check";
    let keep = "Use `assert!` so the check also runs in release mode";
    let packed = "or make the packed value with a constructor that returns `Option`";
    match cut {
        Cut::Cast(to) => (
            format!(
                "`{shown}` is a `{from}` cast to `{to}` with `as`, which wraps silently. {only}"
            ),
            format!("{keep}, or convert with `{to}::try_from({shown})` and handle the error"),
        ),
        Cut::Mask(mask) => (
            format!(
                "`{shown}` is masked with `& {mask:#x}`, which drops its high bits silently. {only}"
            ),
            format!("{keep}, {packed}"),
        ),
        Cut::Shift(shift, to) => (
            format!(
                "`{shown}` is shifted left by {shift} in a `{to}`, which drops its high bits silently. {only}"
            ),
            format!("{keep}, {packed}"),
        ),
    }
}

impl BoundCheckedOnlyInDebug {
    fn check_after<'tcx>(
        &mut self,
        cx: &LateContext<'tcx>,
        block: &'tcx Block<'tcx>,
        after: usize,
        check: Span,
        bound: &Bound<'tcx>,
    ) {
        let rest = (&block.stmts[after..], block.expr);
        let mut writes = Vec::new();
        let mut loops = Vec::new();
        let mut cuts = Vec::new();
        for_each_expr(cx.tcx, rest, |e| {
            if debug_only(cx, e).is_some() {
                return ControlFlow::<(), _>::Continue(Descend::No);
            }
            if writes_root(cx, e, bound.root) {
                writes.push(e.span);
            }
            if matches!(e.kind, ExprKind::Loop(..)) {
                loops.push(e.span);
            }
            if !e.span.from_expansion()
                && let Some((cut, room)) = narrowing(cx, e, bound.place)
                && room < bound.layout.max()
                && bound.max <= room
            {
                cuts.push((e.span, cut));
            }
            ControlFlow::Continue(Descend::Yes)
        });
        let shown = snippet(cx, bound.place.span, "..").into_owned();
        let from = cx.typeck_results().expr_ty(bound.place);
        for (at, cut) in cuts {
            // A write before the narrowing, or anywhere in a loop around it,
            // may break the bound.
            let written = writes.iter().any(|w| {
                w.lo() < at.lo() || loops.iter().any(|l| l.contains(at) && l.contains(*w))
            });
            if written {
                continue;
            }
            let (msg, help) = describe(&cut, &shown, from);
            self.findings
                .entry(at)
                .and_modify(|old| {
                    if old.0.lo() < check.lo() {
                        *old = (check, msg.clone(), help.clone());
                    }
                })
                .or_insert((check, msg, help));
        }
    }
}

impl<'tcx> LateLintPass<'tcx> for BoundCheckedOnlyInDebug {
    fn check_block(&mut self, cx: &LateContext<'tcx>, block: &'tcx Block<'tcx>) {
        if block.span.in_external_macro(cx.sess().source_map())
            || clippy_utils::in_automatically_derived(cx.tcx, block.hir_id)
        {
            return;
        }
        for (i, stmt) in block.stmts.iter().enumerate() {
            let Some(e) = stmt_expr(stmt) else {
                continue;
            };
            let mut bounds = Vec::new();
            for cond in debug_only_conditions(cx, e) {
                bounds_in(cx, cond, &mut bounds);
            }
            if bounds.is_empty() {
                continue;
            }
            let Some(body) = enclosing_fn(cx, block.hir_id)
                .as_local()
                .and_then(|def| cx.tcx.hir_maybe_body_owned_by(def))
            else {
                return;
            };
            for bound in &bounds {
                if !bounded_in_release(cx, body.value, bound.place) {
                    self.check_after(cx, block, i + 1, stmt.span, bound);
                }
            }
        }
    }

    fn check_crate_post(&mut self, cx: &LateContext<'tcx>) {
        let mut findings: Vec<_> = self.findings.drain().collect();
        findings.sort_by_key(|(at, _)| at.lo());
        for (at, (check, msg, help)) in findings {
            emit_with_note(
                cx,
                BOUND_CHECKED_ONLY_IN_DEBUG,
                at,
                msg,
                check,
                "the only bound check, which runs with debug assertions on and nowhere else",
                help,
            );
        }
    }
}
