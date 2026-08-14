use crate::baseline::emit;
use rustc_hir::def_id::{DefId, LOCAL_CRATE};
use rustc_hir::{Expr, ExprKind};
use rustc_lint::{LateContext, LateLintPass};
use rustc_middle::ty::{self, Ty};
use rustc_span::sym;

use crate::MordantConfig;

rustc_session::declare_lint! {
    /// Flags maps keyed on values that are not the canonical identity of the
    /// thing they name. Which types and expression forms count is declared per
    /// project in `dylint.toml`; with no configuration the lint is silent.
    pub NONIDENTITY_KEY,
    Warn,
    "map keyed on a value that is not the canonical identity of what it names"
}

/// A key-expression form `nonidentity-key-forms` can opt into. These variants
/// are the whole of the accepted vocabulary, so a spelling this lint honours
/// exists in exactly one place; a name that is not one of them selects no form,
/// as it did when this was a pair of string comparisons.
#[derive(Clone, Copy, PartialEq, Eq)]
enum KeyForm {
    /// `f.to_bits()` where `f` is a float.
    ToBits,
    /// A raw pointer cast to an integer.
    PtrCast,
}

impl KeyForm {
    fn parse(name: &str) -> Option<Self> {
        match name {
            "to-bits" => Some(KeyForm::ToBits),
            "ptr-cast" => Some(KeyForm::PtrCast),
            _ => None,
        }
    }
}

pub struct NonidentityKey {
    config: &'static MordantConfig,
    /// The opted-in forms. Membership, not one flag per form: a form the
    /// project did not name is absent rather than false.
    forms: Vec<KeyForm>,
}

rustc_session::impl_lint_pass!(NonidentityKey => [NONIDENTITY_KEY]);

/// Methods whose receiver is a keyed collection and whose first argument (or
/// type parameter) is in key position.
const KEY_METHODS: &[&str] = &[
    "insert",
    "entry",
    "get",
    "contains_key",
    "contains",
    "remove",
];

impl NonidentityKey {
    pub fn new(config: &'static MordantConfig) -> Self {
        Self {
            config,
            forms: config
                .nonidentity_key_forms
                .iter()
                .filter_map(|f| KeyForm::parse(f))
                .collect(),
        }
    }

    fn is_denied<'tcx>(&self, cx: &LateContext<'tcx>, ty: Ty<'tcx>) -> Option<String> {
        configured(
            cx,
            ty.peel_refs().ty_adt_def()?.did(),
            &self.config.nonidentity_key_types,
        )
    }

    fn is_fixing<'tcx>(&self, cx: &LateContext<'tcx>, ty: Ty<'tcx>) -> bool {
        ty.peel_refs().ty_adt_def().is_some_and(|adt| {
            configured(cx, adt.did(), &self.config.nonidentity_key_fixes).is_some()
        })
    }

    /// A direct hit, or (opt-in) a denied type inside a tuple or one level of
    /// struct fields with no declared identity-fixing component beside it.
    fn denied_type_name<'tcx>(&self, cx: &LateContext<'tcx>, key_ty: Ty<'tcx>) -> Option<String> {
        if let Some(path) = self.is_denied(cx, key_ty) {
            return Some(path);
        }
        if !self.config.nonidentity_key_composite {
            return None;
        }
        let components: Vec<Ty<'_>> = match key_ty.peel_refs().kind() {
            ty::Tuple(elems) => elems.iter().collect(),
            ty::Adt(adt, args) if adt.is_struct() => adt
                .non_enum_variant()
                .fields
                .iter()
                .map(|f| f.ty(cx.tcx, args).skip_normalization())
                .collect(),
            _ => return None,
        };
        let hit = components.iter().find_map(|t| self.is_denied(cx, *t))?;
        if components.iter().any(|t| self.is_fixing(cx, *t)) {
            return None;
        }
        // Renders as: map keyed on `(Span, u32)` carrying `Span`, ...
        Some(format!("{key_ty}` carrying `{hit}"))
    }
}

/// `did`'s def path when `list` names it, either as that path or with the
/// local crate's name in front.
fn configured(cx: &LateContext<'_>, did: DefId, list: &[String]) -> Option<String> {
    let path = cx.tcx.def_path_str(did);
    let with_crate = did
        .is_local()
        .then(|| format!("{}::{path}", cx.tcx.crate_name(LOCAL_CRATE)));
    list.iter()
        .any(|d| *d == path || Some(d) == with_crate.as_ref())
        .then_some(path)
}

/// The key type of a keyed std/indexmap collection, or None.
fn keyed_collection_key<'tcx>(cx: &LateContext<'tcx>, recv_ty: Ty<'tcx>) -> Option<Ty<'tcx>> {
    let ty::Adt(adt, args) = recv_ty.peel_refs().kind() else {
        return None;
    };
    let did = adt.did();
    let is_std_keyed = [
        sym::HashMap,
        sym::HashSet,
        sym::BTreeMap,
        clippy_utils::sym::BTreeSet,
    ]
    .iter()
    .any(|s| cx.tcx.is_diagnostic_item(*s, did));
    let is_indexmap = cx.tcx.crate_name(did.krate).as_str() == "indexmap";
    if is_std_keyed || is_indexmap {
        args.types().next()
    } else {
        None
    }
}

fn is_float_to_bits(cx: &LateContext<'_>, expr: &Expr<'_>) -> bool {
    if let ExprKind::MethodCall(seg, recv, [], _) = expr.kind
        && seg.ident.as_str() == "to_bits"
    {
        cx.typeck_results()
            .expr_ty(recv)
            .peel_refs()
            .is_floating_point()
    } else {
        false
    }
}

fn is_ptr_to_int_cast(cx: &LateContext<'_>, expr: &Expr<'_>) -> bool {
    if let ExprKind::Cast(inner, _) = expr.kind {
        cx.typeck_results().expr_ty(inner).is_raw_ptr()
            && cx.typeck_results().expr_ty(expr).is_integral()
    } else {
        false
    }
}

impl<'tcx> LateLintPass<'tcx> for NonidentityKey {
    fn check_expr(&mut self, cx: &LateContext<'tcx>, expr: &'tcx Expr<'tcx>) {
        let ExprKind::MethodCall(seg, recv, args, _) = expr.kind else {
            return;
        };
        if !KEY_METHODS.contains(&seg.ident.as_str()) {
            return;
        }
        let recv_ty = cx.typeck_results().expr_ty_adjusted(recv);
        let Some(key_ty) = keyed_collection_key(cx, recv_ty) else {
            return;
        };

        if let Some(path) = self.denied_type_name(cx, key_ty) {
            emit(
                cx,
                NONIDENTITY_KEY,
                expr.span,
                format!("map keyed on `{path}`, which this project declares is not an identity"),
                "key on the canonical identity of the thing this names",
            );
            return;
        }

        // Expression-form checks apply where the key value itself is written:
        // `insert` and `entry`.
        if !matches!(seg.ident.as_str(), "insert" | "entry") {
            return;
        }
        let Some(key_expr) = args.first() else {
            return;
        };
        if self.forms.contains(&KeyForm::ToBits) && is_float_to_bits(cx, key_expr) {
            emit(
                cx,
                NONIDENTITY_KEY,
                key_expr.span,
                "map keyed on `to_bits()` of a float",
                "for boxed or interned values the bits are a pointer, not the value; key on the canonical identity",
            );
        }
        // Project-declared methods whose result is not an identity (e.g. a
        // NaN-boxing `Value::to_bits`, where boxed values yield pointer bits).
        let deny_methods = &self.config.nonidentity_key_methods;
        if !deny_methods.is_empty()
            && let ExprKind::MethodCall(kseg, _, _, _) = key_expr.kind
            && let Some(mdid) = cx.typeck_results().type_dependent_def_id(key_expr.hir_id)
            && configured(cx, mdid, deny_methods).is_some()
        {
            emit(
                cx,
                NONIDENTITY_KEY,
                key_expr.span,
                format!(
                    "map keyed on `{}()`, which this project declares is not an identity",
                    kseg.ident
                ),
                "key on the canonical identity of the thing this names",
            );
        }
        if self.forms.contains(&KeyForm::PtrCast) && is_ptr_to_int_cast(cx, key_expr) {
            emit(
                cx,
                NONIDENTITY_KEY,
                key_expr.span,
                "map keyed on a pointer cast to an integer",
                "pointer bits name an allocation, not a value; key on the canonical identity",
            );
        }
    }
}
