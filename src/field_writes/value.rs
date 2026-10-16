//! The abstract value of one field write, and where in a block an
//! assignment runs.

use std::collections::HashSet;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::ops::ControlFlow;

use clippy_utils::visitors::for_each_expr_without_closures;
use clippy_utils::{hash_expr, is_default_equivalent};
use rustc_ast::LitKind;
use rustc_hir::attrs::lang_items::LangItem;
use rustc_hir::def::{CtorOf, DefKind, Res};
use rustc_hir::def_id::DefId;
use rustc_hir::{BorrowKind, Expr, ExprKind, HirId, Node, QPath, Stmt};
use rustc_lint::LateContext;
use rustc_middle::ty::{self, Ty, TyCtxt};
use rustc_span::{Symbol, sym};

use super::BlockPart;
use crate::adt_facts::is_option_ty;
use crate::enum_facts::ctor_literal_variant;
use crate::hir_shapes::{field_chain, peel_blocks_unsafe};

/// What a write puts into a field, as far as the code spells it out.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub(crate) enum Value {
    Bool(bool),
    /// An unsigned integer literal.
    Int(u128),
    Str(Symbol),
    /// Any other literal, by its debug text.
    OtherLit(String),
    /// A variant of any enum, payload left out. `Some(..)` and `None` are
    /// variants of `Option`.
    Variant(DefId),
    /// A named constant.
    Const(DefId),
    /// `Default::default()` of a type with no simpler default.
    Default,
    Unknown,
}

impl Value {
    pub fn is_some(&self, tcx: TyCtxt<'_>) -> bool {
        matches!(self, Value::Variant(v) if tcx.is_lang_item(*v, LangItem::OptionSome))
    }

    pub fn is_none(&self, tcx: TyCtxt<'_>) -> bool {
        matches!(self, Value::Variant(v) if tcx.is_lang_item(*v, LangItem::OptionNone))
    }
}

/// The value `e` writes into a field of type `ty`, and whether it is a
/// placeholder.
pub(super) fn classify<'tcx>(
    cx: &LateContext<'tcx>,
    e: &Expr<'tcx>,
    ty: Ty<'tcx>,
) -> (Value, bool) {
    (value_of(cx, e, ty), is_placeholder(cx, e))
}

fn value_of<'tcx>(cx: &LateContext<'tcx>, e: &Expr<'tcx>, ty: Ty<'tcx>) -> Value {
    let mut e = peel_blocks_unsafe(e);
    while let ExprKind::AddrOf(BorrowKind::Ref, _, inner) = e.kind {
        e = peel_blocks_unsafe(inner);
    }
    if let Some(v) = ctor_literal_variant(cx, e) {
        return Value::Variant(v);
    }
    match e.kind {
        ExprKind::Lit(lit) => match lit.node {
            LitKind::Bool(b) => Value::Bool(b),
            LitKind::Int(n, _) => Value::Int(n.get()),
            LitKind::Str(s, _) => Value::Str(s),
            ref other => Value::OtherLit(format!("{other:?}")),
        },
        ExprKind::Path(ref qpath) => match cx.qpath_res(qpath, e.hir_id) {
            Res::Def(DefKind::Const { .. } | DefKind::AssocConst { .. }, did) => Value::Const(did),
            _ => Value::Unknown,
        },
        ExprKind::Call(callee, []) if is_default_fn(cx, callee) => default_of(cx, ty),
        _ => Value::Unknown,
    }
}

/// `Default::default`, `T::default` or `<T as Default>::default` as a callee.
fn is_default_fn(cx: &LateContext<'_>, callee: &Expr<'_>) -> bool {
    let ExprKind::Path(ref qpath) = callee.kind else {
        return false;
    };
    let Some(did) = cx.qpath_res(qpath, callee.hir_id).opt_def_id() else {
        return false;
    };
    let tcx = cx.tcx;
    let trait_did = tcx.trait_of_assoc(did).or_else(|| {
        tcx.impl_of_assoc(did)
            .and_then(|i| tcx.impl_opt_trait_id(i))
    });
    trait_did.is_some_and(|t| tcx.is_diagnostic_item(sym::Default, t))
}

/// What `Default::default()` gives for a field of type `ty`.
pub(super) fn default_of<'tcx>(cx: &LateContext<'tcx>, ty: Ty<'tcx>) -> Value {
    if ty.is_bool() {
        return Value::Bool(false);
    }
    if matches!(ty.kind(), ty::Uint(_) | ty::Int(_)) {
        return Value::Int(0);
    }
    if is_option_ty(cx, ty)
        && let Some(none) = cx.tcx.lang_items().get(LangItem::OptionNone)
    {
        return Value::Variant(none);
    }
    if let ty::Adt(adt, _) = ty.kind()
        && adt.is_enum()
        && let Some(v) = derived_default_variant(cx.tcx, ty)
    {
        return Value::Variant(v);
    }
    Value::Default
}

fn derived_default_impl<'tcx>(tcx: TyCtxt<'tcx>, ty: Ty<'tcx>) -> Option<DefId> {
    let default = tcx.get_diagnostic_item(sym::Default)?;
    tcx.non_blanket_impls_for_ty(default, ty)
        .find(|i| tcx.is_automatically_derived(*i))
}

fn has_derived_default<'tcx>(tcx: TyCtxt<'tcx>, ty: Ty<'tcx>) -> bool {
    derived_default_impl(tcx, ty).is_some()
}

/// The `#[default]` variant of a local enum with a derived `Default`: the
/// variant the derived `default` fn returns.
fn derived_default_variant<'tcx>(tcx: TyCtxt<'tcx>, ty: Ty<'tcx>) -> Option<DefId> {
    let imp = derived_default_impl(tcx, ty)?;
    let item = tcx.associated_item_def_ids(imp).first()?.as_local()?;
    let body = tcx.hir_maybe_body_owned_by(item)?;
    let e = peel_blocks_unsafe(body.value);
    let ExprKind::Path(ref qpath) = e.kind else {
        return None;
    };
    // The derive writes `Self::V`, which only type checking resolves.
    match tcx.typeck(item).qpath_res(qpath, e.hir_id) {
        Res::Def(DefKind::Ctor(CtorOf::Variant, _), ctor) => Some(tcx.parent(ctor)),
        _ => None,
    }
}

/// Whether `base` of a `..base` literal of `adt` is its derived `Default`.
pub(super) fn is_derived_default(cx: &LateContext<'_>, base: &Expr<'_>, adt: DefId) -> bool {
    let ExprKind::Call(callee, []) = peel_blocks_unsafe(base).kind else {
        return false;
    };
    if !is_default_fn(cx, callee) {
        return false;
    }
    let ty = cx
        .tcx
        .type_of(adt)
        .instantiate_identity()
        .skip_normalization();
    has_derived_default(cx.tcx, ty)
}

/// A don't-care value: whatever `Default` would give, or a null pointer.
pub(crate) fn is_placeholder(cx: &LateContext<'_>, e: &Expr<'_>) -> bool {
    if is_default_equivalent(cx, e) {
        return true;
    }
    if let ExprKind::Call(f, []) = e.kind
        && let ExprKind::Path(qpath) = &f.kind
        && let Some(def) = cx.qpath_res(qpath, f.hir_id).opt_def_id()
    {
        return matches!(
            cx.tcx.get_diagnostic_name(def),
            Some(sym::ptr_null | sym::ptr_null_mut)
        );
    }
    false
}

/// The part of a block that the assignment `id` runs in: the
/// innermost block around it, and how many statements before it can leave
/// early (`return`, `?`, `break`, `continue`). None when a match arm, a
/// closure or the body itself holds the assignment with no block of its own.
pub(super) fn block_part<'tcx>(
    cx: &LateContext<'tcx>,
    id: HirId,
    base: &Expr<'_>,
) -> Option<BlockPart> {
    let mut from = id;
    for (parent_id, node) in cx.tcx.hir_parent_iter(id) {
        match node {
            Node::Block(block) => {
                let exits_before = block
                    .stmts
                    .iter()
                    .take_while(|s| s.hir_id != from)
                    .filter(|&s| leaves_early(s))
                    .count();
                return Some(BlockPart {
                    block: parent_id,
                    exits_before,
                    place: place_key(cx, base),
                });
            }
            Node::Expr(e) if matches!(e.kind, ExprKind::Closure(_)) => return None,
            Node::Expr(_) | Node::ExprField(_) | Node::Stmt(_) | Node::LetStmt(_) => {
                from = parent_id;
            }
            _ => return None,
        }
    }
    None
}

/// Whether the statement can leave its block. A `break` or `continue` aimed
/// at a loop inside the statement stays inside it.
fn leaves_early<'tcx>(s: &'tcx Stmt<'tcx>) -> bool {
    let mut own_loops = HashSet::new();
    for_each_expr_without_closures(s, |e: &'tcx Expr<'tcx>| {
        match e.kind {
            ExprKind::Ret(_) => return ControlFlow::Break(()),
            ExprKind::Loop(..) => {
                own_loops.insert(e.hir_id);
            }
            ExprKind::Break(dest, _) | ExprKind::Continue(dest)
                if !dest.target_id.is_ok_and(|l| own_loops.contains(&l)) =>
            {
                return ControlFlow::Break(());
            }
            _ => {}
        }
        ControlFlow::Continue(())
    })
    .is_some()
}

/// The place `base` names, as a hash of the local at its root and the
/// fields read off it. `SpanlessHash` hashes every local alike, which would
/// put `a.f` beside `b.g`.
pub(crate) fn place_key(cx: &LateContext<'_>, base: &Expr<'_>) -> u64 {
    let chain = field_chain(base);
    let mut h = DefaultHasher::new();
    if let ExprKind::Path(QPath::Resolved(None, path)) = chain.root.kind
        && let Res::Local(local) = path.res
    {
        local.hash(&mut h);
    } else {
        hash_expr(cx, chain.root).hash(&mut h);
    }
    chain.fields.hash(&mut h);
    h.finish()
}
