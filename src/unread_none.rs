use std::collections::HashMap;

use rustc_hir::def_id::DefId;
use rustc_hir::{Expr, ExprKind, Node};
use rustc_lint::{LateContext, LateLintPass};
use rustc_middle::ty;
use rustc_span::{Symbol, sym};

use crate::baseline::emit;

rustc_session::declare_lint! {
    /// Flags an `Option` field every read of which assumes `Some`: two or
    /// more `unwrap`/`expect` reads and not one site that handles `None`.
    /// The type admits a state no reader survives, which usually means a
    /// two-phase object: `None` exists only between construction and setup.
    /// Splitting the phases into types (a builder that yields the ready
    /// shape, or a plain `T` field) deletes every one of those panics
    /// structurally.
    pub UNREAD_NONE,
    Warn,
    "Option field whose None is never handled by any reader"
}

#[derive(Default)]
struct FieldFacts {
    panicking: Vec<rustc_span::Span>,
    handled: usize,
}

#[derive(Default)]
pub struct UnreadNone {
    fields: HashMap<(DefId, Symbol), FieldFacts>,
}

rustc_session::impl_lint_pass!(UnreadNone => [UNREAD_NONE]);

impl UnreadNone {
    pub fn new() -> Self {
        Self::default()
    }
}

/// The crate-private local struct owning this field access, when the field is
/// an `Option`.
fn option_field_of<'tcx>(
    cx: &LateContext<'tcx>,
    base: &Expr<'tcx>,
    field: Symbol,
) -> Option<DefId> {
    let ty::Adt(adt, _) = cx
        .typeck_results()
        .expr_ty_adjusted(base)
        .peel_refs()
        .kind()
    else {
        return None;
    };
    if !adt.is_struct() || !adt.did().is_local() {
        return None;
    }
    if cx
        .effective_visibilities
        .is_exported(adt.did().expect_local())
    {
        return None;
    }
    let is_option = adt.non_enum_variant().fields.iter().any(|f| {
        f.name == field
            && matches!(
                cx.tcx
                    .type_of(f.did)
                    .instantiate_identity()
                    .skip_normalization()
                    .kind(),
                ty::Adt(fadt, _) if cx.tcx.is_diagnostic_item(sym::Option, fadt.did())
            )
    });
    is_option.then(|| adt.did())
}

enum ReadKind {
    Panicking,
    Handled,
    Write,
}

/// Walk up from the field access through transparent adapters to see what
/// finally consumes it. Anything that is not literally `unwrap`/`expect` is
/// treated as handling `None` — the conservative direction: uncounted
/// handling produces silence, never a finding.
fn classify<'tcx>(cx: &LateContext<'tcx>, field_expr: &'tcx Expr<'tcx>) -> ReadKind {
    let mut current = field_expr.hir_id;
    for (parent_id, node) in cx.tcx.hir_parent_iter(field_expr.hir_id).take(6) {
        let Node::Expr(parent) = node else {
            return ReadKind::Handled;
        };
        match &parent.kind {
            ExprKind::MethodCall(seg, recv, _, _) if recv.hir_id == current => {
                match seg.ident.as_str() {
                    "unwrap" | "expect" => return ReadKind::Panicking,
                    // Transparent adapters: keep looking at what consumes the
                    // adapted value.
                    "as_ref" | "as_mut" | "clone" => {
                        current = parent.hir_id;
                    }
                    _ => return ReadKind::Handled,
                }
            }
            ExprKind::AddrOf(_, _, inner) if inner.hir_id == current => {
                current = parent.hir_id;
            }
            ExprKind::Assign(lhs, ..) | ExprKind::AssignOp(_, lhs, _) if lhs.hir_id == current => {
                return ReadKind::Write;
            }
            _ => return ReadKind::Handled,
        }
        let _ = parent_id;
    }
    ReadKind::Handled
}

impl<'tcx> LateLintPass<'tcx> for UnreadNone {
    fn check_expr(&mut self, cx: &LateContext<'tcx>, expr: &'tcx Expr<'tcx>) {
        let ExprKind::Field(base, ident) = expr.kind else {
            return;
        };
        let Some(adt_did) = option_field_of(cx, base, ident.name) else {
            return;
        };
        let facts = self.fields.entry((adt_did, ident.name)).or_default();
        match classify(cx, expr) {
            ReadKind::Panicking => facts.panicking.push(expr.span),
            ReadKind::Handled => facts.handled += 1,
            ReadKind::Write => {}
        }
    }

    fn check_crate_post(&mut self, cx: &LateContext<'tcx>) {
        for ((adt, field), facts) in &self.fields {
            if facts.handled > 0 || facts.panicking.len() < 2 {
                continue;
            }
            let Some(fdef) = cx
                .tcx
                .adt_def(*adt)
                .non_enum_variant()
                .fields
                .iter()
                .find(|f| f.name == *field)
            else {
                continue;
            };
            emit(
                cx,
                UNREAD_NONE,
                cx.tcx.def_span(fdef.did),
                format!(
                    "every read of `{}.{field}` assumes `Some` ({} unwraps, 0 sites handle `None`)",
                    cx.tcx.item_name(*adt),
                    facts.panicking.len(),
                ),
                "`None` is a state no reader survives; split the phases into types, or store `T` directly",
            );
        }
    }
}
