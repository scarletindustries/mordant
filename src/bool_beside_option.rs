use std::collections::{HashMap, HashSet};

use crate::adt_facts::{field_ty, has_fixed_repr, is_option_ty, struct_field};
use crate::baseline::emit;
use crate::hir_shapes::{assigned_field, peel_blocks_unsafe};
use clippy_utils::{as_some_expr, get_enclosing_block, get_parent_expr, is_none_expr};
use rustc_ast::LitKind;
use rustc_hir::def_id::DefId;
use rustc_hir::{BorrowKind, Expr, ExprKind, HirId, Mutability};
use rustc_lint::{LateContext, LateLintPass};
use rustc_middle::ty::adjustment::{Adjust, AutoBorrow, AutoBorrowMutability};
use rustc_middle::ty::{self, AdtDef};
use rustc_span::{Span, Symbol};

rustc_session::declare_lint! {
    /// Flags a bool field that stores whether a sibling `Option` field is
    /// `Some`: every write to either of them, struct literal or field
    /// assignment, sits beside a write to the other in the same struct
    /// expression or block, `true` always with `Some(..)` and `false` always
    /// with `None` (or always the reverse). The flag is the `Option`'s
    /// discriminant kept a second time, and nothing but that habit keeps the
    /// two agreeing.
    ///
    /// Only fires on fields nothing outside the crate can name, when every
    /// write to the pair is a literal `true`/`false` and `Some(..)`/`None` and
    /// both polarities occur. A lone write to either field, a computed value,
    /// a compound assignment, or a `&mut` borrow of either (`.take()`,
    /// `mem::replace`, `&mut s.opt`) is unprovable and silences it; `as_mut`
    /// and `as_deref_mut` cannot change the discriminant and do not.
    pub BOOL_BESIDE_OPTION,
    Warn,
    "bool field that repeats whether a sibling Option field is Some"
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Bool,
    Opt,
}

struct FieldWrites {
    kind: Kind,
    /// Some write to the field is not a literal of its kind, or the field is
    /// mutably borrowed: its value is no longer a function of the sites.
    unprovable: bool,
    /// (site, set): the struct expression or enclosing block that writes the
    /// field, and whether it writes `true` / `Some(..)` there.
    sites: HashSet<(HirId, bool)>,
}

#[derive(Default)]
pub struct BoolBesideOption {
    writes: HashMap<(DefId, Symbol), FieldWrites>,
}

rustc_session::impl_lint_pass!(BoolBesideOption => [BOOL_BESIDE_OPTION]);

/// A bool or `Option` field of a local struct that nothing outside the crate
/// can write: the struct with the field's kind, or None for anything else.
fn tracked_field<'tcx>(
    cx: &LateContext<'tcx>,
    ty: ty::Ty<'tcx>,
    name: Symbol,
) -> Option<(AdtDef<'tcx>, Kind)> {
    let ty::Adt(adt, _) = ty.peel_refs().kind() else {
        return None;
    };
    if !adt.is_struct() || !adt.did().is_local() || has_fixed_repr(*adt) {
        return None;
    }
    let f = struct_field(*adt, name)?;
    if cx.effective_visibilities.is_exported(f.did.expect_local()) {
        return None;
    }
    let fty = field_ty(cx, f);
    let kind = if fty.is_bool() {
        Kind::Bool
    } else if is_option_ty(cx, fty) {
        Kind::Opt
    } else {
        return None;
    };
    Some((*adt, kind))
}

/// Whether `value` writes `true`/`Some(..)` (Some(true)) or `false`/`None`
/// (Some(false)); None for anything computed.
fn polarity(cx: &LateContext<'_>, kind: Kind, value: &Expr<'_>) -> Option<bool> {
    let value = peel_blocks_unsafe(value);
    match kind {
        Kind::Bool => match value.kind {
            ExprKind::Lit(lit) => match lit.node {
                LitKind::Bool(b) => Some(b),
                _ => None,
            },
            _ => None,
        },
        Kind::Opt => {
            if as_some_expr(cx, value).is_some() {
                Some(true)
            } else {
                is_none_expr(cx, value).then_some(false)
            }
        }
    }
}

impl BoolBesideOption {
    fn facts(&mut self, adt: AdtDef<'_>, name: Symbol, kind: Kind) -> &mut FieldWrites {
        self.writes
            .entry((adt.did(), name))
            .or_insert_with(|| FieldWrites {
                kind,
                unprovable: false,
                sites: HashSet::new(),
            })
    }

    fn record<'tcx>(
        &mut self,
        cx: &LateContext<'tcx>,
        base_ty: ty::Ty<'tcx>,
        name: Symbol,
        site: HirId,
        value: &Expr<'_>,
    ) {
        let Some((adt, kind)) = tracked_field(cx, base_ty, name) else {
            return;
        };
        let facts = self.facts(adt, name, kind);
        match polarity(cx, kind, value) {
            Some(set) => {
                facts.sites.insert((site, set));
            }
            None => facts.unprovable = true,
        }
    }

    fn poison<'tcx>(&mut self, cx: &LateContext<'tcx>, base_ty: ty::Ty<'tcx>, name: Symbol) {
        if let Some((adt, kind)) = tracked_field(cx, base_ty, name) {
            self.facts(adt, name, kind).unprovable = true;
        }
    }
}

/// Methods that borrow an `Option` or bool field mutably without being able
/// to replace it whole.
const PROJECTING_MUT_METHODS: &[&str] = &["as_mut", "as_deref_mut", "as_pin_mut", "iter_mut"];

impl<'tcx> LateLintPass<'tcx> for BoolBesideOption {
    fn check_expr(&mut self, cx: &LateContext<'tcx>, expr: &'tcx Expr<'tcx>) {
        match expr.kind {
            ExprKind::Struct(_, fields, _) => {
                // A derived `Clone`/`Default` writes every field from the same
                // field or the same default: it preserves whatever the
                // hand-written sites establish and proves nothing itself.
                if expr.span.in_derive_expansion() {
                    return;
                }
                let ty = cx.typeck_results().expr_ty(expr);
                for field in fields {
                    self.record(cx, ty, field.ident.name, expr.hir_id, field.expr);
                }
            }
            ExprKind::Assign(place, value, _) => {
                let Some((base, ident, _)) = assigned_field(place) else {
                    return;
                };
                let base_ty = cx.typeck_results().expr_ty_adjusted(base);
                match get_enclosing_block(cx, expr.hir_id) {
                    Some(block) => self.record(cx, base_ty, ident.name, block.hir_id, value),
                    None => self.poison(cx, base_ty, ident.name),
                }
            }
            ExprKind::AssignOp(_, place, _) => {
                if let Some((base, ident, _)) = assigned_field(place) {
                    self.poison(cx, cx.typeck_results().expr_ty_adjusted(base), ident.name);
                }
            }
            ExprKind::AddrOf(BorrowKind::Ref | BorrowKind::Raw, Mutability::Mut, inner) => {
                if let ExprKind::Field(base, ident) = peel_blocks_unsafe(inner).kind {
                    self.poison(cx, cx.typeck_results().expr_ty_adjusted(base), ident.name);
                }
            }
            // The auto-`&mut` a mutating method call takes on its receiver.
            ExprKind::Field(base, ident) => {
                let mutably_borrowed = cx.typeck_results().expr_adjustments(expr).iter().any(|a| {
                    matches!(
                        a.kind,
                        Adjust::Borrow(AutoBorrow::Ref(AutoBorrowMutability::Mut { .. }))
                            | Adjust::Borrow(AutoBorrow::RawPtr(Mutability::Mut))
                    )
                });
                if !mutably_borrowed {
                    return;
                }
                let projecting = matches!(
                    get_parent_expr(cx, expr).map(|p| &p.kind),
                    Some(ExprKind::MethodCall(seg, ..))
                        if PROJECTING_MUT_METHODS.contains(&seg.ident.name.as_str())
                );
                if !projecting {
                    self.poison(cx, cx.typeck_results().expr_ty_adjusted(base), ident.name);
                }
            }
            _ => {}
        }
    }

    fn check_crate_post(&mut self, cx: &LateContext<'tcx>) {
        let mut per_struct: HashMap<DefId, Vec<(Symbol, FieldWrites)>> = HashMap::new();
        for ((did, name), facts) in self.writes.drain() {
            if !facts.unprovable && !facts.sites.is_empty() {
                per_struct.entry(did).or_default().push((name, facts));
            }
        }
        let mut findings: Vec<(Span, String)> = Vec::new();
        for (did, mut fields) in per_struct {
            fields.sort_by_key(|(name, _)| name.as_str().to_owned());
            let adt = cx.tcx.adt_def(did);
            for (flag, flag_facts) in fields.iter().filter(|(_, f)| f.kind == Kind::Bool) {
                // A flag only ever `false` (or only ever `true`) is a constant,
                // not a copy of anything.
                if !flag_facts.sites.iter().any(|&(_, set)| set)
                    || !flag_facts.sites.iter().any(|&(_, set)| !set)
                {
                    continue;
                }
                for (opt, opt_facts) in fields.iter().filter(|(_, f)| f.kind == Kind::Opt) {
                    let inverted: HashSet<(HirId, bool)> =
                        opt_facts.sites.iter().map(|&(s, set)| (s, !set)).collect();
                    let reading = if flag_facts.sites == opt_facts.sites {
                        "is_some"
                    } else if flag_facts.sites == inverted {
                        "is_none"
                    } else {
                        continue;
                    };
                    let (with_true, with_false) = match reading {
                        "is_some" => ("Some(..)", "None"),
                        _ => ("None", "Some(..)"),
                    };
                    let Some(field) = struct_field(adt, *flag) else {
                        continue;
                    };
                    findings.push((
                        cx.tcx.def_span(field.did),
                        format!(
                            "`{flag}` is only ever written beside `{opt}` of `{}`, `true` with `{with_true}` and `false` with `{with_false}` ({} sites): it stores `{opt}.{reading}()` a second time",
                            cx.tcx.def_path_str(did),
                            flag_facts.sites.len(),
                        ),
                    ));
                    break;
                }
            }
        }
        findings.sort_by_key(|(span, _)| span.lo());
        for (span, msg) in findings {
            emit(
                cx,
                BOOL_BESIDE_OPTION,
                span,
                msg,
                "the `Option` already carries this state; drop the flag and ask the `Option`, so the two cannot disagree",
            );
        }
    }
}
