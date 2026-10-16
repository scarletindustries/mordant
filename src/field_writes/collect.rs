//! The pass that records every field write of the crate.

use clippy_utils::{get_enclosing_block, get_parent_expr};
use rustc_hir::def_id::DefId;
use rustc_hir::{
    BindingMode, BorrowKind, ByRef, Expr, ExprField, ExprKind, HirId, Mutability, Pat, PatKind,
    QPath, StructTailExpr, UnOp,
};
use rustc_lint::{LateContext, LateLintPass};
use rustc_middle::ty::adjustment::{Adjust, AutoBorrow, AutoBorrowMutability};
use rustc_middle::ty::{AdtDef, Ty, VariantDef};
use rustc_span::{Span, Symbol};

use super::value::{block_part, classify, default_of, is_derived_default, is_placeholder};
use super::{Access, Build, FACTS, FieldWrites, Init, Value, Write, WriteKind};
use crate::adt_facts::{buildable_in_other_crates, field_ty, private_local_struct};
use crate::hir_shapes::{enclosing_fn, in_derived_clone, peel_blocks_unsafe};

/// Methods that borrow a field mutably but cannot replace it whole, so they
/// cannot change which variant it holds.
const PROJECTING_MUT_METHODS: &[&str] = &["as_mut", "as_deref_mut", "as_pin_mut", "iter_mut"];

/// Records the field writes of the crate, then publishes them for
/// `field_writes::facts`. It declares no lints, so it always runs, and it is
/// registered before the lints that read the facts.
#[derive(Default)]
pub struct FieldWritesPass {
    facts: FieldWrites,
}

rustc_lint::impl_lint_pass!(FieldWritesPass => []);

impl<'tcx> LateLintPass<'tcx> for FieldWritesPass {
    fn check_expr(&mut self, cx: &LateContext<'tcx>, expr: &'tcx Expr<'tcx>) {
        if in_derived_clone(cx, expr.hir_id) {
            return;
        }
        match expr.kind {
            ExprKind::Struct(qpath, fields, tail) => self.build(cx, expr, qpath, fields, tail),
            ExprKind::Assign(place, value, _) => {
                self.write_to(cx, expr, place, WriteKind::Assign, Some(value));
            }
            ExprKind::AssignOp(_, place, _) => {
                self.write_to(cx, expr, place, WriteKind::Assign, None);
            }
            ExprKind::AddrOf(BorrowKind::Ref | BorrowKind::Raw, Mutability::Mut, inner) => {
                self.write_to(cx, expr, inner, WriteKind::MutBorrow, None);
            }
            // The `&mut` a method call takes on its receiver.
            ExprKind::Field(..) if borrowed_mut(cx, expr) => {
                self.write_to(cx, expr, expr, WriteKind::MutBorrow, None);
            }
            ExprKind::Match(scrut, arms, _)
                if arms.iter().any(|a| binds_whole_by_mut_ref(cx, a.pat)) =>
            {
                self.write_to(cx, expr, scrut, WriteKind::MutBorrow, None);
            }
            ExprKind::Let(l) if binds_whole_by_mut_ref(cx, l.pat) => {
                self.write_to(cx, expr, l.init, WriteKind::MutBorrow, None);
            }
            _ => {}
        }
    }

    // `let S { ref mut f, .. } = x`, or `let S { f, .. } = self` on a
    // `&mut self`: the binding is a `&mut` to the field.
    fn check_pat(&mut self, cx: &LateContext<'tcx>, pat: &'tcx Pat<'tcx>) {
        let PatKind::Struct(ref qpath, fields, _) = pat.kind else {
            return;
        };
        let Some(typeck) = cx.typeck_results else {
            return;
        };
        if in_derived_clone(cx, pat.hir_id) {
            return;
        }
        let Some(adt) = local_adt(typeck.pat_ty(pat)) else {
            return;
        };
        let variant = adt.variant_of_res(cx.qpath_res(qpath, pat.hir_id));
        for field in fields {
            let mut by_mut_ref = false;
            field.pat.walk(|p| {
                by_mut_ref |= is_mut_ref_binding(cx, p);
                !by_mut_ref
            });
            if by_mut_ref {
                let w = self.write(cx, pat.hir_id, field.span, WriteKind::MutBorrow);
                self.push(cx, adt, variant, field.ident.name, w);
            }
        }
    }

    fn check_crate_post(&mut self, _: &LateContext<'tcx>) {
        // One compiler session lints one crate, so this is set once.
        let _ = FACTS.set(std::mem::take(&mut self.facts));
    }
}

/// The ADT behind `ty`, when this crate defines it and it is not a union.
fn local_adt(ty: Ty<'_>) -> Option<AdtDef<'_>> {
    let adt = ty.peel_refs().ty_adt_def()?;
    (adt.did().is_local() && !adt.is_union()).then_some(adt)
}

fn is_mut_ref_binding(cx: &LateContext<'_>, p: &Pat<'_>) -> bool {
    matches!(p.kind, PatKind::Binding(..))
        && matches!(
            cx.typeck_results().pat_binding_modes().get(p.hir_id),
            Some(BindingMode(ByRef::Yes(_, Mutability::Mut), _))
        )
}

/// `ref mut b` as the whole pattern, so `b` is a `&mut` to all of the
/// matched place.
fn binds_whole_by_mut_ref(cx: &LateContext<'_>, pat: &Pat<'_>) -> bool {
    match pat.kind {
        PatKind::Binding(_, _, _, None) => is_mut_ref_binding(cx, pat),
        PatKind::Or(pats) => pats.iter().any(|p| binds_whole_by_mut_ref(cx, p)),
        _ => false,
    }
}

/// A field expression with an automatic `&mut` on it that can change it
/// whole.
fn borrowed_mut(cx: &LateContext<'_>, expr: &Expr<'_>) -> bool {
    let adjusted = cx.typeck_results().expr_adjustments(expr).iter().any(|a| {
        matches!(
            a.kind,
            Adjust::Borrow(AutoBorrow::Ref(AutoBorrowMutability::Mut { .. }))
                | Adjust::Borrow(AutoBorrow::RawPtr(Mutability::Mut))
        )
    });
    adjusted
        && !matches!(
            get_parent_expr(cx, expr).map(|p| &p.kind),
            Some(ExprKind::MethodCall(seg, ..))
                if PROJECTING_MUT_METHODS.contains(&seg.ident.name.as_str())
        )
}

impl FieldWritesPass {
    fn build<'tcx>(
        &mut self,
        cx: &LateContext<'tcx>,
        expr: &'tcx Expr<'tcx>,
        qpath: &QPath<'tcx>,
        fields: &'tcx [ExprField<'tcx>],
        tail: StructTailExpr<'tcx>,
    ) {
        let Some(adt) = local_adt(cx.typeck_results().expr_ty(expr)) else {
            return;
        };
        if matches!(tail, StructTailExpr::NoneWithError(_)) {
            return;
        }
        let variant = adt.variant_of_res(cx.qpath_res(qpath, expr.hir_id));
        // What `..` fills the fields the literal leaves out with.
        let rest = |cx: &LateContext<'tcx>, ty| match tail {
            StructTailExpr::Base(b) if is_derived_default(cx, b, adt.did()) => {
                (default_of(cx, ty), true)
            }
            StructTailExpr::Base(b) => (Value::Unknown, is_placeholder(cx, b)),
            _ => (Value::Unknown, true),
        };
        let inits = variant
            .fields
            .iter()
            .map(|f| {
                let ty = field_ty(cx, f);
                match fields.iter().find(|ef| ef.ident.name == f.name) {
                    Some(ef) => {
                        let (value, placeholder) = classify(cx, ef.expr, ty);
                        Init {
                            name: f.name,
                            value,
                            placeholder,
                            span: ef.span,
                        }
                    }
                    None => {
                        let (value, placeholder) = rest(cx, ty);
                        Init {
                            name: f.name,
                            value,
                            placeholder,
                            span: expr.span,
                        }
                    }
                }
            })
            .collect();
        self.note_variant(cx, adt, variant);
        self.facts
            .builds
            .entry(variant.def_id)
            .or_default()
            .push(Build {
                span: expr.span,
                hir_id: expr.hir_id,
                func: enclosing_fn(cx, expr.hir_id),
                derived: clippy_utils::in_automatically_derived(cx.tcx, expr.hir_id),
                fields: inits,
            });
    }

    /// Records a write of `kind` to the field `place` names, if it names a
    /// field of a local struct. `place` may be wrapped in derefs.
    fn write_to<'tcx>(
        &mut self,
        cx: &LateContext<'tcx>,
        expr: &'tcx Expr<'tcx>,
        place: &'tcx Expr<'tcx>,
        mut kind: WriteKind,
        value: Option<&'tcx Expr<'tcx>>,
    ) {
        let mut field = peel_blocks_unsafe(place);
        let mut through_deref = false;
        while let ExprKind::Unary(UnOp::Deref, inner) = field.kind {
            field = peel_blocks_unsafe(inner);
            through_deref = true;
        }
        let ExprKind::Field(base, ident) = field.kind else {
            return;
        };
        let Some(adt) = local_adt(cx.typeck_results().expr_ty_adjusted(base)) else {
            return;
        };
        if adt.is_enum() {
            return;
        }
        let variant = adt.non_enum_variant();
        let Some(fdef) = variant.fields.iter().find(|f| f.name == ident.name) else {
            return;
        };
        // `*x.f = v` writes what `f` points to, and may leave `f` itself
        // anything.
        if through_deref && matches!(kind, WriteKind::Assign) {
            kind = WriteKind::MutBorrow;
        }
        let mut w = self.write(cx, expr.hir_id, expr.span, kind);
        if matches!(kind, WriteKind::Assign) {
            w.block_part = block_part(cx, expr.hir_id, base);
            if let Some(value) = value {
                (w.value, w.placeholder) = classify(cx, value, field_ty(cx, fdef));
            }
        }
        self.push(cx, adt, variant, ident.name, w);
    }

    fn write(&self, cx: &LateContext<'_>, hir_id: HirId, span: Span, kind: WriteKind) -> Write {
        Write {
            kind,
            value: Value::Unknown,
            placeholder: false,
            span,
            hir_id,
            body: cx.tcx.hir_enclosing_body_owner(hir_id).to_def_id(),
            block: get_enclosing_block(cx, hir_id).map(|b| b.hir_id),
            block_part: None,
        }
    }

    fn push(
        &mut self,
        cx: &LateContext<'_>,
        adt: AdtDef<'_>,
        variant: &VariantDef,
        field: Symbol,
        w: Write,
    ) {
        self.note_variant(cx, adt, variant);
        self.facts
            .writes
            .entry((variant.def_id, field))
            .or_default()
            .push(w);
    }

    /// Who outside the crate can build `variant` or write its fields.
    fn note_variant(&mut self, cx: &LateContext<'_>, adt: AdtDef<'_>, variant: &VariantDef) {
        let did: DefId = variant.def_id;
        if self.facts.access.contains_key(&did) {
            return;
        }
        let ty = cx
            .tcx
            .type_of(adt.did())
            .instantiate_identity()
            .skip_normalization();
        self.facts.access.insert(
            did,
            Access {
                private: private_local_struct(cx, ty).is_some(),
                others_build: buildable_in_other_crates(cx, adt, variant),
            },
        );
        let exported = variant
            .fields
            .iter()
            .filter(|f| {
                f.did
                    .as_local()
                    .is_some_and(|l| cx.effective_visibilities.is_exported(l))
            })
            .map(|f| f.name)
            .collect();
        self.facts.exported.insert(did, exported);
    }
}
