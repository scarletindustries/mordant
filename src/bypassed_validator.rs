use std::collections::HashMap;

use crate::adt_facts::impl_self_adt;
use crate::baseline::emit_with_note;
use crate::ctor_flow::{self, FieldCheck};
use clippy_utils::ty::ty_from_hir_ty;
use rustc_abi::FieldIdx;
use rustc_hir::def_id::DefId;
use rustc_hir::{Expr, ExprKind, FnRetTy, ImplItem, ImplItemKind, ItemKind};
use rustc_lint::{LateContext, LateLintPass};
use rustc_middle::ty;
use rustc_span::{Span, Symbol, sym};

rustc_session::declare_lint! {
    /// Flags struct literals that bypass a validating constructor: a
    /// receiver-less associated function returning `Result<Self, _>` or
    /// `Option<Self>` whose body rejects some value it then stores in a field
    /// (see `ctor_flow`). A literal outside the type's own module constructs
    /// it without that check ever running. Constructors that only fail
    /// because their input did not parse or a resource ran out check nothing
    /// about the fields and are not validators.
    pub BYPASSED_VALIDATOR,
    Warn,
    "struct literal bypasses the type's validating constructor"
}

struct Validator {
    ctor: Symbol,
    checks: Vec<FieldCheck>,
}

pub struct BypassedValidator {
    extra_resource_errors: Vec<String>,
    /// struct -> constructors that check at least one stored field.
    validators: HashMap<DefId, Vec<Validator>>,
    /// Literal constructions outside the struct's own module and impls.
    literals: Vec<(DefId, Span)>,
}

rustc_session::declare_lint! {
    /// Flags a field whose value a validating constructor checks (see
    /// `ctor_flow`) but which is visible outside the type's own module. Any
    /// holder can assign the field directly, so the constructor's check holds
    /// only until the first write. Fields the constructor never inspects are
    /// not reported, whatever their visibility.
    pub PUB_INVARIANT_FIELDS,
    Warn,
    "checked field assignable outside its module"
}

rustc_session::impl_lint_pass!(BypassedValidator => [BYPASSED_VALIDATOR, PUB_INVARIANT_FIELDS]);

impl BypassedValidator {
    pub fn new(config: &crate::MordantConfig) -> Self {
        Self {
            extra_resource_errors: config.validator_resource_errors.clone(),
            validators: HashMap::new(),
            literals: Vec::new(),
        }
    }
}

/// The struct an impl block is for, when it is a crate-local struct.
fn impl_self_struct(cx: &LateContext<'_>, impl_did: DefId) -> Option<DefId> {
    let adt = impl_self_adt(cx, impl_did)?;
    (adt.is_struct() && adt.did().is_local()).then(|| adt.did())
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
        if !wraps_self {
            return;
        }
        let checks = ctor_flow::checked_fields(
            cx,
            item.owner_id.def_id,
            struct_did,
            &self.extra_resource_errors,
        );
        if !checks.is_empty() {
            self.validators
                .entry(struct_did)
                .or_default()
                .push(Validator {
                    ctor: item.ident.name,
                    checks,
                });
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
        for (did, ctors) in validators {
            let parent_mod = cx.tcx.parent_module_from_def_id(did.expect_local());
            let fields = &cx.tcx.adt_def(*did).non_enum_variant().fields;
            let mut reported: Vec<FieldIdx> = Vec::new();
            for v in ctors {
                for check in &v.checks {
                    if reported.contains(&check.field) {
                        continue;
                    }
                    let field = &fields[check.field];
                    // A private field is Restricted to exactly the parent
                    // module; anything else widens the write surface past
                    // the check.
                    if cx.tcx.visibility(field.did)
                        == ty::Visibility::Restricted(parent_mod.to_def_id())
                    {
                        continue;
                    }
                    reported.push(check.field);
                    emit_with_note(
                        cx,
                        PUB_INVARIANT_FIELDS,
                        cx.tcx.def_span(field.did),
                        format!(
                            "`{}::{}` rejects some values of `{}` before storing it, but the field is assignable outside its module",
                            cx.tcx.item_name(*did),
                            v.ctor,
                            field.name,
                        ),
                        check.check,
                        "the check a direct write skips",
                        "make the field private; the checked invariant otherwise holds only until the first outside write",
                    );
                }
            }
        }
        for (did, span) in &self.literals {
            let Some(ctors) = self.validators.get(did) else {
                continue;
            };
            let fields = &cx.tcx.adt_def(*did).non_enum_variant().fields;
            let v = &ctors[0];
            let names: Vec<String> = v
                .checks
                .iter()
                .map(|c| format!("`{}`", fields[c.field].name))
                .collect();
            emit_with_note(
                cx,
                BYPASSED_VALIDATOR,
                *span,
                format!(
                    "`{}` is constructed by literal here, but `{}::{}` checks {} before constructing one",
                    cx.tcx.def_path_str(*did),
                    cx.tcx.item_name(*did),
                    v.ctor,
                    names.join(", "),
                ),
                v.checks[0].check,
                "the check this literal never runs",
                "construct through the validating function, or move this literal into the type's module",
            );
        }
    }
}
