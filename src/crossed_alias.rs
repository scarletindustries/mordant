use clippy_utils::source::snippet;
use rustc_hir::def::{DefKind, Res};
use rustc_hir::def_id::DefId;
use rustc_hir::{BinOpKind, Body, Expr, ExprKind, LetStmt, Node, Ty as HirTy};
use rustc_lint::{LateContext, LateLintPass};
use rustc_middle::ty::{self, Ty};

use crate::baseline::emit;
use crate::hir_shapes::{
    Callee, assigned_adt_field, callee_of, declared_ty, field_decl_ty, param_decl_ty,
    return_decl_ty, value_expr, written_alias,
};

rustc_session::declare_lint! {
    /// Flags a value declared under one integer type alias arriving at a
    /// place declared under another: a `DependencyId` local passed as the
    /// `PackageId` parameter, stored in a `PackageId` field, bound by
    /// `let p: PackageId = ..`, returned from a `-> PackageId` function, or
    /// defining a `PackageId` const, where both aliases name the same
    /// primitive integer. Two aliases over one integer exist to tell two
    /// kinds of number apart, and rustc erases both, so nothing rejects the
    /// crossing; a newtype per kind would. The kinds are read off the
    /// written types of this crate's locals, parameters, fields, consts and
    /// signatures, so the lint stays quiet when either side has no alias (a
    /// literal, a plain `u32`, arithmetic between two values), when one
    /// alias is declared as the other, through `as` casts, on aliases that
    /// bottom out in `core`/`std`/`libc` (representation, not identity), and
    /// on declarations in other crates, whose written types it cannot see.
    pub CROSSED_ALIAS,
    Warn,
    "a value declared as one integer alias flowing into a place declared as another"
}

rustc_session::declare_lint_pass!(CrossedAlias => [CROSSED_ALIAS]);

/// An integer alias as written at a slot, and the alias it bottoms out in.
#[derive(Clone, Copy)]
struct Kind<'tcx> {
    written: DefId,
    root: DefId,
    int: Ty<'tcx>,
}

/// `type A = B;` names `B`'s kind; follow the chain while it stays visible.
fn alias_root(cx: &LateContext<'_>, mut did: DefId) -> DefId {
    for _ in 0..8 {
        let Some(next) = did
            .as_local()
            .and_then(|local| cx.tcx.hir_node_by_def_id(local).alias_ty())
            .and_then(written_alias)
        else {
            break;
        };
        did = next;
    }
    did
}

fn is_representation_crate(cx: &LateContext<'_>, did: DefId) -> bool {
    matches!(
        cx.tcx.crate_name(did.krate).as_str(),
        "core" | "std" | "alloc" | "libc"
    )
}

/// The identity a written type claims: an alias, generic-free, whose chain
/// ends at a primitive integer without passing through a std crate.
fn kind_of<'tcx>(cx: &LateContext<'tcx>, ty: &HirTy<'_>) -> Option<Kind<'tcx>> {
    let written = written_alias(ty)?;
    let root = alias_root(cx, written);
    if is_representation_crate(cx, written) || is_representation_crate(cx, root) {
        return None;
    }
    let int = cx
        .tcx
        .type_of(root)
        .instantiate_identity()
        .skip_normalization();
    matches!(int.kind(), ty::Int(_) | ty::Uint(_)).then_some(Kind { written, root, int })
}

fn is_lit(e: &Expr<'_>) -> bool {
    matches!(value_expr(e).kind, ExprKind::Lit(_))
}

/// The kind the value was declared with. An id offset by a literal
/// (`INVALID - 1`, `id + 1`) is still that kind of id; anything else
/// computed is no kind at all.
fn source_kind<'tcx>(cx: &LateContext<'tcx>, e: &'tcx Expr<'tcx>) -> Option<Kind<'tcx>> {
    let e = value_expr(e);
    if let ExprKind::Binary(op, l, r) = e.kind {
        if !matches!(op.node, BinOpKind::Add | BinOpKind::Sub) {
            return None;
        }
        return match (is_lit(l), is_lit(r)) {
            (false, true) => source_kind(cx, l),
            (true, false) => source_kind(cx, r),
            _ => None,
        };
    }
    kind_of(cx, declared_ty(cx, e)?)
}

/// Where the value lands, worded for the message.
enum Slot {
    Param { callee: DefId, idx: usize },
    Field(DefId),
    Local(String),
    Return(DefId),
    Const(DefId),
}

fn def_name(cx: &LateContext<'_>, did: DefId) -> String {
    cx.tcx
        .opt_item_name(did)
        .map_or_else(|| "this closure".to_owned(), |s| s.to_string())
}

fn check_edge<'tcx>(cx: &LateContext<'tcx>, src: &'tcx Expr<'tcx>, dest: &HirTy<'_>, slot: Slot) {
    let Some(to) = kind_of(cx, dest) else {
        return;
    };
    let Some(from) = source_kind(cx, src) else {
        return;
    };
    if from.root == to.root || from.int != to.int {
        return;
    }
    let to_name = def_name(cx, to.written);
    let lands = match slot {
        Slot::Param { callee, idx } => {
            let param = cx
                .tcx
                .fn_arg_idents(callee)
                .get(idx)
                .copied()
                .flatten()
                .map_or_else(|| format!("#{idx}"), |i| i.to_string());
            format!(
                "is passed as `{to_name}` parameter `{param}` of `{}`",
                def_name(cx, callee)
            )
        }
        Slot::Field(f) => format!("is stored in `{to_name}` field `{}`", def_name(cx, f)),
        Slot::Local(pat) => format!("is bound to `{pat}: {to_name}`"),
        Slot::Return(f) => format!("is returned from `{}` as `{to_name}`", def_name(cx, f)),
        Slot::Const(c) => format!("defines `{to_name}` const `{}`", def_name(cx, c)),
    };
    let src = value_expr(src);
    emit(
        cx,
        CROSSED_ALIAS,
        src.span,
        format!(
            "`{}` is declared `{}` but {lands}; both are `{}`, so nothing rejects the crossing",
            snippet(cx, src.span, ".."),
            def_name(cx, from.written),
            to.int,
        ),
        "a newtype per id kind makes this a type error",
    );
}

impl<'tcx> LateLintPass<'tcx> for CrossedAlias {
    fn check_expr(&mut self, cx: &LateContext<'tcx>, expr: &'tcx Expr<'tcx>) {
        match expr.kind {
            ExprKind::Call(..) | ExprKind::MethodCall(..) => {
                let (def, first, args) = match callee_of(cx, expr) {
                    Some(Callee::Path { def, args }) => (def, 0, args),
                    Some(Callee::Method { def, args, .. }) => (def, 1, args),
                    None => return,
                };
                if !matches!(cx.tcx.def_kind(def), DefKind::Fn | DefKind::AssocFn) {
                    return;
                }
                for (i, arg) in args.iter().enumerate() {
                    let idx = first + i;
                    if let Some(dest) = param_decl_ty(cx, def, idx) {
                        check_edge(cx, arg, dest, Slot::Param { callee: def, idx });
                    }
                }
            }
            ExprKind::Struct(qpath, fields, _) => {
                let Some(adt) = cx.typeck_results().expr_ty(expr).ty_adt_def() else {
                    return;
                };
                let res = cx.qpath_res(qpath, expr.hir_id);
                if !matches!(
                    res,
                    Res::Def(
                        DefKind::Struct
                            | DefKind::Union
                            | DefKind::Variant
                            | DefKind::TyAlias
                            | DefKind::AssocTy
                            | DefKind::Ctor(..),
                        _
                    ) | Res::SelfTyAlias { .. }
                        | Res::SelfTyParam { .. }
                        | Res::SelfCtor(_)
                ) {
                    return;
                }
                let variant = adt.variant_of_res(res);
                for field in fields {
                    let idx = cx.typeck_results().field_index(field.hir_id);
                    let def = variant.fields[idx].did;
                    if let Some(dest) = field_decl_ty(cx, def) {
                        check_edge(cx, field.expr, dest, Slot::Field(def));
                    }
                }
            }
            ExprKind::Assign(place, value, _) => {
                let Some((adt, ident, _)) = assigned_adt_field(cx, place) else {
                    return;
                };
                if adt.is_enum() {
                    return;
                }
                let Some(field) = crate::adt_facts::struct_field(adt, ident.name) else {
                    return;
                };
                if let Some(dest) = field_decl_ty(cx, field.did) {
                    check_edge(cx, value, dest, Slot::Field(field.did));
                }
            }
            ExprKind::Ret(Some(value)) => {
                let owner = cx.tcx.hir_enclosing_body_owner(expr.hir_id).to_def_id();
                if let Some(dest) = return_decl_ty(cx, owner) {
                    check_edge(cx, value, dest, Slot::Return(owner));
                }
            }
            _ => {}
        }
    }

    fn check_local(&mut self, cx: &LateContext<'tcx>, local: &'tcx LetStmt<'tcx>) {
        if let (Some(dest), Some(init)) = (local.ty, local.init) {
            let pat = snippet(cx, local.pat.span, "..").into_owned();
            check_edge(cx, init, dest, Slot::Local(pat));
        }
    }

    /// The body's value against its owner's written type: a function's tail
    /// against its return type, a const or static's initializer against its
    /// declaration.
    fn check_body(&mut self, cx: &LateContext<'tcx>, body: &Body<'tcx>) {
        let owner = cx.tcx.hir_body_owner_def_id(body.id());
        let node = cx.tcx.hir_node_by_def_id(owner);
        if node.fn_decl().is_some() {
            if let Some(dest) = return_decl_ty(cx, owner.to_def_id()) {
                check_edge(cx, body.value, dest, Slot::Return(owner.to_def_id()));
            }
        } else if matches!(node, Node::Item(_) | Node::ImplItem(_) | Node::TraitItem(_))
            && matches!(
                cx.tcx.def_kind(owner),
                DefKind::Const { .. } | DefKind::Static { .. } | DefKind::AssocConst { .. }
            )
            && let Some(dest) = node.ty()
        {
            check_edge(cx, body.value, dest, Slot::Const(owner.to_def_id()));
        }
    }
}
