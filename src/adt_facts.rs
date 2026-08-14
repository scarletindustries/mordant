//! Shared facts about the structs and enums a crate defines: which struct a
//! type or an impl block is about, what a field's declared type is, what a
//! struct literal constructs, and the shape questions (explicit `repr`,
//! positional fields, `Result`'s error type) that several lints ask of the
//! same definition. Every filter that makes a lint fire or not -- privacy,
//! `is_struct`, the tail of a literal, a minimum field count -- stays in the
//! lint, so this module only ever answers, never decides.

use rustc_hir::def_id::DefId;
use rustc_hir::{Expr, ExprField, ExprKind, StructTailExpr};
use rustc_lint::LateContext;
use rustc_middle::ty::{self, AdtDef, FieldDef, Ty, TyCtxt, VariantDef};
use rustc_span::{Symbol, sym};

/// The struct behind `ty` (through any references) when this crate defines
/// it and nothing outside the crate can name it: the structs whose every
/// construction and field write the crate can see, which is what lets a lint
/// claim "never" about them.
pub(crate) fn private_local_struct<'tcx>(
    cx: &LateContext<'tcx>,
    ty: Ty<'tcx>,
) -> Option<AdtDef<'tcx>> {
    let ty::Adt(adt, _) = ty.peel_refs().kind() else {
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
    Some(*adt)
}

/// A field's declared type, with the struct's own generics left in place.
pub(crate) fn field_ty<'tcx>(cx: &LateContext<'tcx>, f: &FieldDef) -> Ty<'tcx> {
    cx.tcx
        .type_of(f.did)
        .instantiate_identity()
        .skip_normalization()
}

/// The field of a struct (or union) called `name`.
pub(crate) fn struct_field<'tcx>(adt: AdtDef<'tcx>, name: Symbol) -> Option<&'tcx FieldDef> {
    adt.non_enum_variant()
        .fields
        .iter()
        .find(|f| f.name == name)
}

/// The ADT an impl block is for, whatever its origin; a blanket or foreign
/// impl, or one on a primitive, is None.
pub(crate) fn impl_self_adt<'tcx>(cx: &LateContext<'tcx>, impl_did: DefId) -> Option<AdtDef<'tcx>> {
    cx.tcx
        .type_of(impl_did)
        .instantiate_identity()
        .skip_normalization()
        .ty_adt_def()
}

/// A `Name { .. }` expression, resolved: the ADT it builds, the variant of it
/// (the only one, for a struct or union), the fields it spells out, and its
/// `..base` tail if any.
pub(crate) struct StructLiteral<'tcx> {
    pub(crate) adt: AdtDef<'tcx>,
    pub(crate) variant: &'tcx VariantDef,
    pub(crate) fields: &'tcx [ExprField<'tcx>],
    pub(crate) tail: StructTailExpr<'tcx>,
}

/// Resolve a struct expression; anything else, including a tuple-struct call,
/// is None. The type comes from typeck rather than the path, so an alias or
/// `Self` resolves to what it names.
pub(crate) fn struct_literal<'tcx>(
    cx: &LateContext<'tcx>,
    e: &'tcx Expr<'tcx>,
) -> Option<StructLiteral<'tcx>> {
    let ExprKind::Struct(qpath, fields, tail) = e.kind else {
        return None;
    };
    let adt = cx.typeck_results().expr_ty(e).ty_adt_def()?;
    let variant = adt.variant_of_res(cx.qpath_res(qpath, e.hir_id));
    Some(StructLiteral {
        adt,
        variant,
        fields,
        tail,
    })
}

pub(crate) fn is_option_ty(cx: &LateContext<'_>, ty: Ty<'_>) -> bool {
    matches!(ty.kind(), ty::Adt(adt, _) if cx.tcx.is_diagnostic_item(sym::Option, adt.did()))
}

/// An explicit `repr` means something outside Rust fixes the layout, so the
/// value combinations a lint would call unreachable may all be real.
pub(crate) fn has_fixed_repr(adt: AdtDef<'_>) -> bool {
    let repr = adt.repr();
    repr.c() || repr.packed() || repr.transparent() || repr.simd() || repr.int.is_some()
}

/// Tuple fields are named "0", "1", ...; a message that wants field names has
/// nothing to say about them.
pub(crate) fn has_positional_fields(v: &VariantDef) -> bool {
    v.fields
        .iter()
        .any(|f| f.name.as_str().starts_with(|c: char| c.is_ascii_digit()))
}

/// Whether a configured list names `did`. An entry may be the full def path,
/// a `::`-suffix of it, the bare item name, or `crate::Name` -- the last for
/// a re-export whose def path runs through a private module
/// (`bun_sys::error::Error` configured as `bun_sys::Error`).
pub(crate) fn matches_config_path<'a>(
    tcx: TyCtxt<'_>,
    did: DefId,
    mut entries: impl Iterator<Item = &'a str>,
) -> bool {
    let path = tcx.def_path_str(did);
    let name = tcx.item_name(did);
    let krate = tcx.crate_name(did.krate);
    entries.any(|e| {
        path == e
            || path.ends_with(&format!("::{e}"))
            || match e.rsplit_once("::") {
                None => name.as_str() == e,
                Some((k, n)) => !k.contains("::") && krate.as_str() == k && name.as_str() == n,
            }
    })
}

/// `Result<_, E>` -> `E`; anything else, `Option` included, is None.
pub(crate) fn result_err_ty<'tcx>(tcx: TyCtxt<'tcx>, ty: Ty<'tcx>) -> Option<Ty<'tcx>> {
    let ty::Adt(adt, args) = ty.kind() else {
        return None;
    };
    (tcx.is_diagnostic_item(sym::Result, adt.did()) && args.len() == 2).then(|| args.type_at(1))
}
