//! Shared facts about the structs and enums a crate defines: which struct a
//! type or an impl block is about, what a field's declared type is, and the
//! shape questions (explicit `repr`, positional fields, `Result`'s error type)
//! that several lints ask of the same definition. Every filter that makes a
//! lint fire or not -- privacy, `is_struct`, a minimum field count -- stays in
//! the lint, so this module only ever answers, never decides.

use rustc_data_structures::fx::FxHashSet;
use rustc_hir::def::DefKind;
use rustc_hir::def_id::DefId;
use rustc_hir::{HirId, find_attr};
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

/// Can code in another crate build `variant` of `adt` with a literal? Then
/// this crate does not see every place a value is built.
pub(crate) fn buildable_in_other_crates(
    cx: &LateContext<'_>,
    adt: AdtDef<'_>,
    variant: &VariantDef,
) -> bool {
    adt.did()
        .as_local()
        .is_some_and(|did| cx.effective_visibilities.is_exported(did))
        && !variant.is_field_list_non_exhaustive()
        && variant.fields.iter().all(|f| f.vis.is_public())
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

/// Whether the code at `at` is the ADT's own: inside the module defining it
/// (when this crate defines it) or inside any impl block, inherent or trait,
/// whose self type it is.
pub(crate) fn in_own_code_of(cx: &LateContext<'_>, at: HirId, adt: DefId) -> bool {
    if let Some(local) = adt.as_local()
        && cx.tcx.parent_module(at) == cx.tcx.parent_module_from_def_id(local)
    {
        return true;
    }
    let mut cur = cx.tcx.hir_enclosing_body_owner(at).to_def_id();
    while let Some(parent) = cx.tcx.opt_parent(cur) {
        if matches!(cx.tcx.def_kind(parent), DefKind::Impl { .. })
            && impl_self_adt(cx, parent).is_some_and(|a| a.did() == adt)
        {
            return true;
        }
        cur = parent;
    }
    false
}

/// True when `hir_id` sits inside a TRAIT impl whose self type is `adt_did`.
/// `Display`, `Debug`, `From` and derive expansions must match every variant
/// to exist, so their patterns prove nothing. Inherent methods are not
/// excluded: an accessor like `fn tenths(&self)` is the crate genuinely
/// reading the structure.
pub(crate) fn inside_own_trait_impl(cx: &LateContext<'_>, hir_id: HirId, adt_did: DefId) -> bool {
    let mut cur = hir_id.owner.def_id.to_def_id();
    loop {
        if matches!(cx.tcx.def_kind(cur), DefKind::Impl { of_trait: true })
            && impl_self_adt(cx, cur).is_some_and(|adt| adt.did() == adt_did)
        {
            return true;
        }
        match cx.tcx.opt_parent(cur) {
            Some(p) => cur = p,
            None => return false,
        }
    }
}

/// The definition, or a module enclosing it, carries a `#[cfg]`: what it
/// names is chosen per platform or feature rather than fixed by the program.
pub(crate) fn cfg_selected(cx: &LateContext<'_>, mut did: DefId) -> bool {
    loop {
        if find_attr!(cx.tcx, did, CfgTrace(..)) {
            return true;
        }
        match cx.tcx.opt_parent(did) {
            Some(p) => did = p,
            None => return false,
        }
    }
}

/// What kind of type stops a field from being `Send` or `Sync` on its own.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum BlockerKind {
    /// A raw pointer, `NonNull`, an extern type, an `UnsafeCell` of nothing
    /// (an opaque FFI handle), or a cell that a `Send` value only shares
    /// through a reference.
    Pointer,
    /// A type with an `impl !Send` or `impl !Sync`, such as `Rc` or `Cell`.
    Declared,
    /// A type that holds `PhantomData<*const ()>` to opt out on purpose.
    Marker,
    /// A type the project lists as tied to one thread.
    Listed,
}

/// One type inside a field that stops the field from being `Send` or `Sync`.
#[derive(Clone, Copy)]
pub(crate) struct ThreadBlocker<'tcx> {
    pub kind: BlockerKind,
    pub ty: Ty<'tcx>,
    /// The bound `ty` fails: `Sync` for a `Cell` reached through `&`.
    pub bound: DefId,
    /// The struct or enum whose field holds `ty`, when the walk went through
    /// one.
    pub inside: Option<Ty<'tcx>>,
}

/// For each field of `adt` with `args`, the types inside it that stop it
/// from being `Send` or `Sync` (`auto_trait`) on its own. Generic parameters
/// and associated types count as meeting the bound. So does an impl whose
/// bounds the walk cannot map, so the answer can miss a blocker but never
/// invents one. `listed` names types tied to one thread, spelled as
/// `matches_config_path` reads them.
pub(crate) fn thread_blockers<'tcx>(
    tcx: TyCtxt<'tcx>,
    adt: AdtDef<'tcx>,
    args: ty::GenericArgsRef<'tcx>,
    auto_trait: DefId,
    listed: &[String],
) -> Vec<(&'tcx FieldDef, Vec<ThreadBlocker<'tcx>>)> {
    let mut out = Vec::new();
    for f in adt.all_fields() {
        let mut walk = BlockerWalk {
            tcx,
            listed,
            root: auto_trait,
            seen: Default::default(),
            found: Vec::new(),
        };
        walk.ty(f.ty(tcx, args).skip_normalization(), auto_trait, None, 0);
        if !walk.found.is_empty() {
            out.push((f, walk.found));
        }
    }
    out
}

/// `Cell`, `RefCell` or `UnsafeCell`.
pub(crate) fn is_cell(tcx: TyCtxt<'_>, ty: Ty<'_>) -> bool {
    let ty::Adt(adt, _) = ty.kind() else {
        return false;
    };
    adt.is_unsafe_cell()
        || matches!(
            tcx.get_diagnostic_name(adt.did()),
            Some(sym::Cell | sym::RefCell)
        )
}

struct BlockerWalk<'a, 'tcx> {
    tcx: TyCtxt<'tcx>,
    listed: &'a [String],
    /// The bound the walk started from.
    root: DefId,
    seen: FxHashSet<(Ty<'tcx>, DefId)>,
    found: Vec<ThreadBlocker<'tcx>>,
}

impl<'tcx> BlockerWalk<'_, 'tcx> {
    fn push(&mut self, kind: BlockerKind, ty: Ty<'tcx>, bound: DefId, inside: Option<Ty<'tcx>>) {
        self.found.push(ThreadBlocker {
            kind,
            ty,
            bound,
            inside,
        });
    }

    /// `owner` is the struct or enum whose field holds `ty`.
    fn ty(&mut self, ty: Ty<'tcx>, tr: DefId, owner: Option<Ty<'tcx>>, depth: usize) {
        if depth > 24 || !self.seen.insert((ty, tr)) {
            return;
        }
        match *ty.kind() {
            ty::RawPtr(..) | ty::Foreign(_) => self.push(BlockerKind::Pointer, ty, tr, owner),
            // `&T` is `Send` only when `T` is `Sync`. `&mut T` follows `T`.
            ty::Ref(_, inner, m) => {
                let sync = self.tcx.get_diagnostic_item(sym::Sync);
                let next = match (m, sync) {
                    (ty::Mutability::Not, Some(s)) => s,
                    _ => tr,
                };
                self.ty(inner, next, owner, depth + 1);
            }
            ty::Array(e, _) | ty::Slice(e) | ty::Pat(e, _) => self.ty(e, tr, owner, depth + 1),
            ty::Tuple(tys) => {
                for t in tys {
                    self.ty(t, tr, owner, depth + 1);
                }
            }
            ty::Adt(adt, args) => self.adt(ty, adt, args, tr, owner, depth),
            // Primitives, fn types, parameters, trait objects and closures
            // count as meeting the bound.
            _ => {}
        }
    }

    fn adt(
        &mut self,
        ty: Ty<'tcx>,
        adt: AdtDef<'tcx>,
        args: ty::GenericArgsRef<'tcx>,
        tr: DefId,
        owner: Option<Ty<'tcx>>,
        depth: usize,
    ) {
        let tcx = self.tcx;
        if matches_config_path(tcx, adt.did(), self.listed.iter().map(String::as_str)) {
            self.push(BlockerKind::Listed, ty, tr, owner);
            return;
        }
        if adt.is_phantom_data() {
            self.ty(args.type_at(0), tr, owner, depth + 1);
            return;
        }
        // A nested type that opts out with `PhantomData<*const ()>`. At the
        // impl's own type the marker is only a pointer field.
        if has_opt_out_marker(tcx, adt, args) {
            self.push(BlockerKind::Marker, ty, tr, owner);
            return;
        }
        // A label names the nearest owner the user wrote, not `RawVec`.
        let std_lib = [sym::core, sym::alloc, sym::std].contains(&tcx.crate_name(adt.did().krate));
        let here = if std_lib { owner } else { Some(ty) };
        // With no impl of its own, the type is `Send` when its fields are.
        let Some(imp) = tcx.non_blanket_impls_for_ty(tr, ty).next() else {
            for f in adt.all_fields() {
                let fty = f.ty(tcx, args).skip_normalization();
                self.ty(fty, tr, here, depth + 1);
            }
            return;
        };
        if tcx.impl_polarity(imp) == ty::ImplPolarity::Negative {
            // An `UnsafeCell<[u8; 0]>` holds no data: it is an opaque FFI
            // handle. A cell that a `Send` value reaches only through a
            // reference is shared the way a raw pointer shares it.
            let opaque = adt.is_unsafe_cell() && is_zero_sized(tcx, args.type_at(0));
            let shared = tr != self.root && is_cell(tcx, ty);
            let kind = if opaque || shared || is_non_null(tcx, adt.did()) {
                BlockerKind::Pointer
            } else {
                BlockerKind::Declared
            };
            self.push(kind, ty, tr, owner);
            return;
        }
        // A positive impl such as `unsafe impl<T: Send> Sync for Mutex<T>`:
        // follow each `Send` or `Sync` bound on an impl parameter to the
        // argument in that position.
        let impl_ty = tcx.type_of(imp).instantiate_identity().skip_normalization();
        let ty::Adt(_, impl_args) = *impl_ty.kind() else {
            return;
        };
        for (clause, _) in tcx.clauses_of(imp).clauses {
            let Some(tc) = clause.as_trait_clause() else {
                continue;
            };
            let bound = tc.def_id();
            if !matches!(tcx.get_diagnostic_name(bound), Some(sym::Send | sym::Sync)) {
                continue;
            }
            let bounded = tc.skip_binder().self_ty();
            let at = impl_args.iter().position(|a| a.as_type() == Some(bounded));
            if let Some(arg) = at.and_then(|i| args.get(i)).and_then(|a| a.as_type()) {
                self.ty(arg, bound, here, depth + 1);
            }
        }
    }
}

/// A field of type `PhantomData<*const ()>` or `PhantomData<*mut ()>`.
fn has_opt_out_marker<'tcx>(
    tcx: TyCtxt<'tcx>,
    adt: AdtDef<'tcx>,
    args: ty::GenericArgsRef<'tcx>,
) -> bool {
    adt.all_fields().any(|f| {
        let ty::Adt(p, pargs) = *f.ty(tcx, args).skip_normalization().kind() else {
            return false;
        };
        p.is_phantom_data() && matches!(pargs.type_at(0).kind(), ty::RawPtr(t, _) if t.is_unit())
    })
}

/// `()` or an array of length zero.
fn is_zero_sized<'tcx>(tcx: TyCtxt<'tcx>, ty: Ty<'tcx>) -> bool {
    match ty.kind() {
        ty::Tuple(tys) => tys.is_empty(),
        ty::Array(_, len) => len.try_to_target_usize(tcx) == Some(0),
        _ => false,
    }
}

fn is_non_null(tcx: TyCtxt<'_>, did: DefId) -> bool {
    tcx.crate_name(did.krate) == sym::core && tcx.item_name(did).as_str() == "NonNull"
}
