use std::collections::HashMap;

use crate::baseline::emit;
use clippy_utils::ty::ty_from_hir_ty;
use rustc_hir::def_id::DefId;
use rustc_hir::{Expr, ExprKind, FnRetTy, ImplItem, ImplItemKind, ItemKind};
use rustc_lint::{LateContext, LateLintPass};
use rustc_middle::ty;
use rustc_span::{Span, Symbol, sym};

rustc_session::declare_lint! {
    /// Flags struct literals that bypass a validating constructor. A type with
    /// an associated function returning `Result<Self, _>` or `Option<Self>`
    /// promises "construction can fail"; a literal outside the type's own
    /// impls constructs it without that check ever running.
    pub BYPASSED_VALIDATOR,
    Warn,
    "struct literal bypasses the type's validating constructor"
}

#[derive(Default)]
pub struct BypassedValidator {
    /// struct -> name of one validating constructor.
    validators: HashMap<DefId, Symbol>,
    /// Literal constructions outside the struct's own impls.
    literals: Vec<(DefId, Span)>,
}

rustc_session::declare_lint! {
    /// Flags a field of a validated type (one with a `Result<Self, _>` or
    /// `Option<Self>` constructor) that is visible outside the type's own
    /// module. Any holder can assign the field directly, so the constructor's
    /// check holds only until the first write.
    pub PUB_INVARIANT_FIELDS,
    Warn,
    "field of a validated type assignable outside its module"
}

rustc_session::impl_lint_pass!(BypassedValidator => [BYPASSED_VALIDATOR, PUB_INVARIANT_FIELDS]);

impl BypassedValidator {
    pub fn new() -> Self {
        Self::default()
    }
}

/// The struct an impl block is for, when it is a crate-local struct.
fn impl_self_struct(cx: &LateContext<'_>, impl_did: DefId) -> Option<DefId> {
    let self_ty = cx
        .tcx
        .type_of(impl_did)
        .instantiate_identity()
        .skip_normalization();
    if let ty::Adt(adt, _) = self_ty.kind()
        && adt.is_struct()
        && adt.did().is_local()
    {
        Some(adt.did())
    } else {
        None
    }
}

impl<'tcx> LateLintPass<'tcx> for BypassedValidator {
    fn check_impl_item(&mut self, cx: &LateContext<'tcx>, item: &'tcx ImplItem<'tcx>) {
        // Only inherent-impl functions count as validators; a trait impl's
        // signature is the trait's idea, not this type's promise.
        let parent = cx.tcx.local_parent(item.owner_id.def_id);
        let ItemKind::Impl(imp) = cx.tcx.hir_expect_item(parent).kind else {
            return;
        };
        if imp.of_trait.is_some() {
            return;
        }
        let Some(struct_did) = impl_self_struct(cx, parent.to_def_id()) else {
            return;
        };
        let ImplItemKind::Fn(sig, _) = &item.kind else {
            return;
        };
        // A constructor has no receiver. `fn parent(&self) -> Option<&Self>`
        // and `fn clone(&self) -> Result<Self, _>` navigate or copy a value
        // that already passed whatever check exists; they establish nothing.
        if cx.tcx.associated_item(item.owner_id).is_method() {
            return;
        }
        let FnRetTy::Return(ret_hir_ty) = sig.decl.output else {
            return;
        };
        let output = ty_from_hir_ty(cx, ret_hir_ty);
        let ty::Adt(adt, args) = output.kind() else {
            return;
        };
        // `Self` by value only: `Option<&Self>` from a receiver-less fn is a
        // lookup into a table of existing values, not construction.
        let wraps_self = (cx.tcx.is_diagnostic_item(sym::Result, adt.did())
            || cx.tcx.is_diagnostic_item(sym::Option, adt.did()))
            && matches!(args.type_at(0).kind(), ty::Adt(inner, _) if inner.did() == struct_did);
        if wraps_self {
            self.validators.entry(struct_did).or_insert(item.ident.name);
        }
    }

    fn check_expr(&mut self, cx: &LateContext<'tcx>, expr: &'tcx Expr<'tcx>) {
        if !matches!(expr.kind, ExprKind::Struct(..)) {
            return;
        }
        let ty = cx.typeck_results().expr_ty(expr);
        let ty::Adt(adt, _) = ty.kind() else {
            return;
        };
        let Some(struct_local) = adt.did().as_local() else {
            return;
        };
        if !adt.is_struct() {
            return;
        }
        // The type's own module can write the literal whatever the field
        // visibility, so a literal there is the author's (a static table the
        // `Option<Self>` lookup searches, a sibling helper), not a bypass.
        // This is the same boundary `pub_invariant_fields` holds fields to.
        if cx.tcx.parent_module(expr.hir_id) == cx.tcx.parent_module_from_def_id(struct_local) {
            return;
        }
        // Literals inside the type's own impls (constructors, Default,
        // builders) are the implementation even from another module.
        let owner = cx.tcx.hir_enclosing_body_owner(expr.hir_id);
        let mut cur = owner.to_def_id();
        while let Some(parent) = cx.tcx.opt_parent(cur) {
            if matches!(
                cx.tcx.def_kind(parent),
                rustc_hir::def::DefKind::Impl { .. }
            ) && impl_self_struct(cx, parent) == Some(adt.did())
            {
                return;
            }
            cur = parent;
        }
        self.literals.push((adt.did(), expr.span));
    }

    fn check_crate_post(&mut self, cx: &LateContext<'tcx>) {
        let mut validators: Vec<_> = self.validators.iter().collect();
        validators.sort_by_key(|(did, _)| cx.tcx.def_span(**did).lo());
        for (did, ctor) in validators {
            let parent_mod = cx.tcx.parent_module_from_def_id(did.expect_local());
            for field in cx.tcx.adt_def(*did).non_enum_variant().fields.iter() {
                let vis = cx.tcx.visibility(field.did);
                // A private field is Restricted to exactly the parent module;
                // anything else widens the write surface past the validator.
                if vis == ty::Visibility::Restricted(parent_mod.to_def_id()) {
                    continue;
                }
                emit(
                    cx,
                    PUB_INVARIANT_FIELDS,
                    cx.tcx.def_span(field.did),
                    format!(
                        "`{}` is validated by `{}::{ctor}`, but this field is assignable outside its module",
                        cx.tcx.def_path_str(*did),
                        cx.tcx.item_name(*did),
                    ),
                    "make the field private; the validated invariant otherwise holds only until the first write",
                );
            }
        }
        for (did, span) in &self.literals {
            let Some(ctor) = self.validators.get(did) else {
                continue;
            };
            emit(
                cx,
                BYPASSED_VALIDATOR,
                *span,
                format!(
                    "`{}` is constructed by literal here, but `{}::{ctor}` validates construction",
                    cx.tcx.def_path_str(*did),
                    cx.tcx.item_name(*did),
                ),
                "construct through the validating function, or move this literal into the type's impl",
            );
        }
    }
}
