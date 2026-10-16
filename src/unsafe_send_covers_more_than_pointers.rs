use rustc_errors::MultiSpan;
use rustc_hir::{Item, ItemKind};
use rustc_lint::{LateContext, LateLintPass};
use rustc_middle::ty::{self, AdtDef, FieldDef, GenericArgsRef, Ty, TyCtxt, TypeVisitableExt};
use rustc_span::{Span, Symbol, sym};

use crate::MordantConfig;
use crate::adt_facts::{BlockerKind, ThreadBlocker, is_cell, thread_blockers};
use crate::baseline::emit_hir_then;

rustc_lint::declare_lint! {
    /// Flags an `unsafe impl Send` or `unsafe impl Sync` for a type that
    /// would not be `Send` or `Sync` on its own for a reason other than a
    /// raw pointer. The lint finds which fields block the bound, looking
    /// through each field's type. It stays silent when every blocker is a
    /// raw pointer or `NonNull`. It names fields that hold `Cell`,
    /// `RefCell`, `UnsafeCell`, `Rc`, any other type with an `impl !Send`
    /// or `impl !Sync`, a type marked with `PhantomData<*const ()>`, or a
    /// type listed in `thread-affine-types`. The impl turns off the
    /// compiler's check for all of those fields, not only for the pointers.
    ///
    /// A type built to share a cell is left alone: a one-field wrapper of a
    /// cell, a type of only atomics and cells, or a cell of a generic
    /// parameter. Its impl is already the small wrapper the fix asks for.
    pub UNSAFE_SEND_COVERS_MORE_THAN_POINTERS,
    Warn,
    "unsafe Send or Sync impl that covers fields other than raw pointers"
}

pub struct UnsafeSendCoversMoreThanPointers {
    pub config: &'static MordantConfig,
}

rustc_lint::impl_lint_pass!(UnsafeSendCoversMoreThanPointers => [UNSAFE_SEND_COVERS_MORE_THAN_POINTERS]);

impl<'tcx> LateLintPass<'tcx> for UnsafeSendCoversMoreThanPointers {
    fn check_item(&mut self, cx: &LateContext<'tcx>, item: &'tcx Item<'tcx>) {
        let ItemKind::Impl(imp) = item.kind else {
            return;
        };
        if imp.of_trait.is_none()
            || item.span.in_external_macro(cx.tcx.sess.source_map())
            || clippy_utils::in_automatically_derived(cx.tcx, item.hir_id())
        {
            return;
        }
        let tcx = cx.tcx;
        let did = item.owner_id.to_def_id();
        if tcx.impl_polarity(did) != ty::ImplPolarity::Positive {
            return;
        }
        let trait_did = tcx.impl_trait_ref(did).skip_binder().def_id;
        let Some(name @ (sym::Send | sym::Sync)) = tcx.get_diagnostic_name(trait_did) else {
            return;
        };
        let self_ty = tcx.type_of(did).instantiate_identity().skip_normalization();
        let ty::Adt(adt, args) = *self_ty.kind() else {
            return;
        };
        if !adt.did().is_local() {
            return;
        }
        let listed = &self.config.thread_affine_types;
        let primitive = primitive_shape(tcx, adt, args);
        let mut real: Vec<(Span, String)> = Vec::new();
        let (mut has_pointer, mut has_cell) = (false, false);
        for (f, blockers) in thread_blockers(tcx, adt, args, trait_did, listed) {
            let fty = f.ty(tcx, args).skip_normalization();
            for b in blockers {
                if b.kind == BlockerKind::Pointer {
                    has_pointer = true;
                    continue;
                }
                let own = b.kind == BlockerKind::Declared && b.bound == trait_did;
                if own && shared_cell(tcx, primitive, f, args, b.ty) {
                    continue;
                }
                has_cell |= is_cell(tcx, b.ty);
                // One label per field: the first blocker found in it.
                let span = tcx.def_span(f.did);
                if real.last().is_none_or(|(s, _)| *s != span) {
                    real.push((span, describe(tcx, &b, fty, name)));
                }
            }
        }
        if real.is_empty() {
            return;
        }
        let ty_name = tcx.item_name(adt.did());
        let mut notes = MultiSpan::from_spans(real.iter().map(|(s, _)| *s).collect());
        for (span, label) in real {
            notes.push_span_label(span, label);
        }
        let mut help = if has_pointer {
            format!(
                "wrap only the raw pointer fields in a small type with its own `unsafe impl {name}`. Then remove this impl so the compiler checks the other fields"
            )
        } else {
            format!("remove this impl and make these fields `{name}`")
        };
        if has_cell {
            help.push_str(". Move a cell that a lock guards into that `Mutex`, or use an atomic");
        }
        emit_hir_then(
            cx,
            UNSAFE_SEND_COVERS_MORE_THAN_POINTERS,
            item.hir_id(),
            tcx.def_span(did),
            format!(
                "`unsafe impl {name} for {ty_name}` covers fields that are not `{name}` for a reason other than a raw pointer"
            ),
            |diag| {
                diag.span_note(notes, format!("these fields are not `{name}` on their own"));
                diag.help(help);
            },
        );
    }
}

/// The shape of a type built to share a cell between threads.
#[derive(Clone, Copy, PartialEq)]
enum Primitive {
    No,
    /// One field: the type wraps what that field names.
    Wrapper,
    /// Every field is an atomic or a cell: the atomics guard the cells.
    AtomicsAndCells,
}

fn primitive_shape<'tcx>(
    tcx: TyCtxt<'tcx>,
    adt: AdtDef<'tcx>,
    args: GenericArgsRef<'tcx>,
) -> Primitive {
    let tys: Vec<Ty<'tcx>> = adt
        .all_fields()
        .map(|f| f.ty(tcx, args).skip_normalization())
        .collect();
    if tys.len() == 1 {
        Primitive::Wrapper
    } else if tys.iter().any(|t| is_atomic(tcx, *t))
        && tys.iter().all(|t| is_cell(tcx, *t) || is_atomic(tcx, *t))
    {
        Primitive::AtomicsAndCells
    } else {
        Primitive::No
    }
}

/// A cell the type exists to share: a cell of a generic parameter, a cell
/// named in the field type of a one-field wrapper, or a cell beside only
/// atomics.
fn shared_cell<'tcx>(
    tcx: TyCtxt<'tcx>,
    primitive: Primitive,
    field: &FieldDef,
    args: GenericArgsRef<'tcx>,
    cell: Ty<'tcx>,
) -> bool {
    if !is_cell(tcx, cell) {
        return false;
    }
    match primitive {
        _ if cell.has_param() => true,
        Primitive::AtomicsAndCells => true,
        Primitive::Wrapper => {
            let named = field.ty(tcx, args).skip_normalization();
            named.walk().any(|g| g.as_type() == Some(cell))
        }
        Primitive::No => false,
    }
}

fn is_atomic(tcx: TyCtxt<'_>, t: Ty<'_>) -> bool {
    let ty::Adt(adt, _) = t.kind() else {
        return false;
    };
    tcx.crate_name(adt.did().krate) == sym::core
        && tcx.item_name(adt.did()).as_str().starts_with("Atomic")
}

/// The label on a field: what it holds, where, and why that blocks `name`.
fn describe<'tcx>(
    tcx: TyCtxt<'tcx>,
    b: &ThreadBlocker<'tcx>,
    field_ty: Ty<'tcx>,
    name: Symbol,
) -> String {
    let t = b.ty;
    let at = match b.inside {
        Some(o) if o != field_ty => format!(" inside `{o}`"),
        _ => String::new(),
    };
    match b.kind {
        BlockerKind::Declared => {
            format!(
                "holds `{t}`{at}, which is declared not `{}`",
                tcx.item_name(b.bound)
            )
        }
        BlockerKind::Marker => {
            format!("holds `{t}`{at}, which `PhantomData<*const ()>` marks as not `{name}`")
        }
        BlockerKind::Listed => {
            format!("holds `{t}`{at}, which `thread-affine-types` lists as tied to one thread")
        }
        BlockerKind::Pointer => format!("holds the raw pointer `{t}`{at}"),
    }
}
