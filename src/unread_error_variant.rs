use std::collections::{HashMap, HashSet};

use rustc_hir::def::{CtorOf, DefKind, Res};
use rustc_hir::def_id::DefId;
use rustc_hir::{Expr, ExprKind, Pat, PatExpr, PatExprKind, PatKind};
use rustc_lint::{LateContext, LateLintPass};
use rustc_middle::ty;

use crate::baseline::emit;

rustc_session::declare_lint! {
    /// Flags a crate-private enum variant that is constructed somewhere but
    /// never named by a pattern outside the enum's own trait impls. Trait
    /// impls (`Display`, `Debug`, `From`, derives) must match every variant to
    /// exist, so they prove nothing; a pattern anywhere else, including the
    /// enum's own inherent methods, is the crate reading the structure. A
    /// variant that fails this test only ever reaches anyone through a
    /// catch-all or a string rendering.
    pub UNREAD_ERROR_VARIANT,
    Warn,
    "enum variant constructed but never distinguished by a pattern"
}

#[derive(Default)]
struct EnumFacts {
    /// variant -> first construction site.
    constructed: HashMap<DefId, rustc_span::Span>,
    /// Variants named by a pattern outside the enum's own impls.
    named: HashSet<DefId>,
}

#[derive(Default)]
pub struct UnreadErrorVariant {
    enums: HashMap<DefId, EnumFacts>,
}

rustc_session::impl_lint_pass!(UnreadErrorVariant => [UNREAD_ERROR_VARIANT]);

impl UnreadErrorVariant {
    pub fn new() -> Self {
        Self::default()
    }
}

/// The crate-private local enum owning `variant_res`, with the variant.
fn variant_of_private_enum(cx: &LateContext<'_>, res: Res) -> Option<(DefId, DefId)> {
    let variant = match res {
        Res::Def(DefKind::Variant, id) => id,
        Res::Def(DefKind::Ctor(CtorOf::Variant, _), id) => cx.tcx.parent(id),
        _ => return None,
    };
    let enum_did = cx.tcx.parent(variant);
    let local = enum_did.as_local()?;
    if cx.effective_visibilities.is_exported(local) {
        return None;
    }
    Some((enum_did, variant))
}

/// True when `hir_id` sits inside a TRAIT impl whose self type is `enum_did`.
/// `Display`, `Debug`, `From` and derive expansions must match every variant
/// to exist, so their patterns prove nothing. Inherent methods are not
/// excluded: an accessor like `fn tenths(&self)` is the crate genuinely
/// reading the structure.
fn inside_own_trait_impl(cx: &LateContext<'_>, hir_id: rustc_hir::HirId, enum_did: DefId) -> bool {
    let mut cur = hir_id.owner.def_id.to_def_id();
    loop {
        if matches!(
            cx.tcx.def_kind(cur),
            rustc_hir::def::DefKind::Impl { of_trait: true }
        ) {
            let self_ty = cx
                .tcx
                .type_of(cur)
                .instantiate_identity()
                .skip_normalization();
            if let ty::Adt(adt, _) = self_ty.kind()
                && adt.did() == enum_did
            {
                return true;
            }
        }
        match cx.tcx.opt_parent(cur) {
            Some(p) => cur = p,
            None => return false,
        }
    }
}

impl<'tcx> LateLintPass<'tcx> for UnreadErrorVariant {
    fn check_expr(&mut self, cx: &LateContext<'tcx>, expr: &'tcx Expr<'tcx>) {
        let res = match expr.kind {
            ExprKind::Call(callee, _) => {
                let ExprKind::Path(qpath) = &callee.kind else {
                    return;
                };
                cx.qpath_res(qpath, callee.hir_id)
            }
            ExprKind::Path(ref qpath) => cx.qpath_res(qpath, expr.hir_id),
            ExprKind::Struct(qpath, ..) => cx.qpath_res(qpath, expr.hir_id),
            _ => return,
        };
        let Some((enum_did, variant)) = variant_of_private_enum(cx, res) else {
            return;
        };
        self.enums
            .entry(enum_did)
            .or_default()
            .constructed
            .entry(variant)
            .or_insert(expr.span);
    }

    fn check_pat(&mut self, cx: &LateContext<'tcx>, pat: &'tcx Pat<'tcx>) {
        let qpath = match &pat.kind {
            PatKind::TupleStruct(qpath, ..) | PatKind::Struct(qpath, ..) => qpath,
            PatKind::Expr(PatExpr {
                kind: PatExprKind::Path(qpath),
                ..
            }) => qpath,
            _ => return,
        };
        let res = cx.qpath_res(qpath, pat.hir_id);
        let Some((enum_did, variant)) = variant_of_private_enum(cx, res) else {
            return;
        };
        if inside_own_trait_impl(cx, pat.hir_id, enum_did) {
            return;
        }
        self.enums
            .entry(enum_did)
            .or_default()
            .named
            .insert(variant);
    }

    fn check_crate_post(&mut self, cx: &LateContext<'tcx>) {
        for (enum_did, facts) in &self.enums {
            // The crate must distinguish at least one variant by a counting
            // pattern; otherwise matching is simply not how this enum is
            // consumed, and "never matched" proves nothing about any variant.
            if facts.named.is_empty() {
                continue;
            }
            for (variant, span) in &facts.constructed {
                if facts.named.contains(variant) {
                    continue;
                }
                emit(
                    cx,
                    UNREAD_ERROR_VARIANT,
                    *span,
                    format!(
                        "`{}` is constructed here, but no pattern outside `{}`'s trait impls ever names it",
                        cx.tcx.def_path_str(*variant),
                        cx.tcx.item_name(*enum_did),
                    ),
                    "the variant's structure is never read; handle it distinctly or collapse it into another variant",
                );
            }
        }
    }
}
