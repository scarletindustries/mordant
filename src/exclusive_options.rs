use std::collections::HashMap;

use crate::adt_facts::{field_ty, is_option_ty, private_local_struct};
use crate::baseline::emit;
use crate::hir_shapes::assigned_field;
use rustc_hir::def_id::DefId;
use rustc_hir::{Expr, ExprKind, StructTailExpr};
use rustc_lint::{LateContext, LateLintPass};
use rustc_middle::ty;
use rustc_span::Symbol;

use crate::MordantConfig;

rustc_session::declare_lint! {
    /// Flags structs where several `Option` fields together encode one state:
    /// every construction site in the crate sets at most one of them to
    /// `Some`. The type permits combinations the crate never builds; an enum
    /// with one variant per field says what the code means.
    ///
    /// Only fires on structs private to the crate, with every construction a
    /// literal `Some(..)`/`None` struct expression and no later field
    /// assignment, whether direct or through a `Box`, guard or other `Deref`
    /// container — anything else is unprovable and stays silent.
    pub EXCLUSIVE_OPTIONS,
    Warn,
    "struct whose Option fields are never populated together"
}

#[derive(Default)]
struct Facts {
    unprovable: bool,
    /// Per construction site: which option fields were `Some`.
    sites: Vec<Vec<Symbol>>,
}

pub struct ExclusiveOptions {
    min_fields: usize,
    structs: HashMap<DefId, Facts>,
}

rustc_session::impl_lint_pass!(ExclusiveOptions => [EXCLUSIVE_OPTIONS]);

impl ExclusiveOptions {
    pub fn new(config: &MordantConfig) -> Self {
        Self {
            min_fields: config.exclusive_options_min_fields,
            structs: HashMap::new(),
        }
    }
}

fn option_fields(cx: &LateContext<'_>, adt: ty::AdtDef<'_>) -> Vec<Symbol> {
    adt.non_enum_variant()
        .fields
        .iter()
        .filter(|f| is_option_ty(cx, field_ty(cx, f)))
        .map(|f| f.name)
        .collect()
}

/// The struct this expression constructs or field-assigns into, if it is a
/// crate-local, crate-private struct with enough Option fields to care about.
fn relevant_adt<'tcx>(
    cx: &LateContext<'tcx>,
    ty: ty::Ty<'tcx>,
    min_fields: usize,
) -> Option<(ty::AdtDef<'tcx>, Vec<Symbol>)> {
    let adt = private_local_struct(cx, ty)?;
    let opts = option_fields(cx, adt);
    (opts.len() >= min_fields).then_some((adt, opts))
}

impl<'tcx> LateLintPass<'tcx> for ExclusiveOptions {
    fn check_expr(&mut self, cx: &LateContext<'tcx>, expr: &'tcx Expr<'tcx>) {
        match expr.kind {
            ExprKind::Struct(_, fields, tail) => {
                let ty = cx.typeck_results().expr_ty(expr);
                let Some((adt, opts)) = relevant_adt(cx, ty, self.min_fields) else {
                    return;
                };
                let facts = self.structs.entry(adt.did()).or_default();
                if !matches!(tail, StructTailExpr::None) {
                    facts.unprovable = true;
                    return;
                }
                let mut somes = Vec::new();
                for field in fields {
                    if !opts.contains(&field.ident.name) {
                        continue;
                    }
                    if clippy_utils::as_some_expr(cx, field.expr).is_some() {
                        somes.push(field.ident.name);
                    } else if !clippy_utils::is_none_expr(cx, field.expr) {
                        facts.unprovable = true;
                        return;
                    }
                }
                facts.sites.push(somes);
            }
            // A later `s.field = ...` write re-opens every combination; the
            // construction sites alone no longer prove anything.
            ExprKind::Assign(place, _, _) | ExprKind::AssignOp(_, place, _) => {
                let Some((base, ident, _)) = assigned_field(place) else {
                    return;
                };
                let ty = cx.typeck_results().expr_ty_adjusted(base);
                let Some((adt, opts)) = relevant_adt(cx, ty, self.min_fields) else {
                    return;
                };
                if opts.contains(&ident.name) {
                    self.structs.entry(adt.did()).or_default().unprovable = true;
                }
            }
            _ => {}
        }
    }

    fn check_crate_post(&mut self, cx: &LateContext<'tcx>) {
        for (did, facts) in &self.structs {
            if facts.unprovable || facts.sites.len() < 2 {
                continue;
            }
            if facts.sites.iter().any(|s| s.len() > 1) {
                continue;
            }
            // At least two distinct fields must actually appear as Some;
            // otherwise this is one live field and dead siblings, not a state.
            let mut seen: Vec<Symbol> = Vec::new();
            for site in &facts.sites {
                for f in site {
                    if !seen.contains(f) {
                        seen.push(*f);
                    }
                }
            }
            if seen.len() < 2 {
                continue;
            }
            let names: Vec<String> = seen.iter().map(|s| format!("`{s}`")).collect();
            emit(
                cx,
                EXCLUSIVE_OPTIONS,
                cx.tcx.def_span(*did),
                format!(
                    "no construction of `{}` sets more than one of {} to `Some` ({} sites checked)",
                    cx.tcx.def_path_str(*did),
                    names.join(", "),
                    facts.sites.len(),
                ),
                "these fields encode one state; an enum with one variant per field represents it",
            );
        }
    }
}
