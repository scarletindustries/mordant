use std::collections::{HashMap, HashSet};

use clippy_utils::res::MaybeResPath;
use clippy_utils::{SpanlessEq, get_parent_expr, hash_expr};
use rustc_hir::def_id::{DefId, LocalDefId};
use rustc_hir::{BorrowKind, Expr, ExprKind, HirId, Mutability, Node, UnOp};
use rustc_lint::{LateContext, LateLintPass};
use rustc_middle::ty::{self, Ty};
use rustc_span::{DesugaringKind, Ident, Symbol};

use crate::adt_facts::field_ty;
use crate::baseline::emit;
use crate::hir_shapes::{assigned_field, field_method_call, indexed_field};

rustc_session::declare_lint! {
    /// Flags two or more growable-sequence fields of one struct (`Vec`,
    /// `VecDeque`, or any type with `push`/`append`, `len` and indexing)
    /// whose lengths the crate only ever changes side by side -- every
    /// `push`, `pop`, `clear`, `truncate`, reassignment or `&mut` borrow of
    /// one sits in the same block as one of the other, on the same value --
    /// and that some function reads at one index (`s.a[i]` with `s.b[i]`,
    /// `s.a.get(i)` with `s.b.get(i)`) or zips together. Element `i` of each
    /// is one record kept in several places by hand: the type admits
    /// sequences of different lengths, and only the discipline of every
    /// writer keeps `a[i]` describing the same thing as `b[i]`. One `Vec` of
    /// a struct with those fields holds the pairing in the type.
    ///
    /// Only fields nothing outside the crate can write are considered: the
    /// struct is private to the crate, or the field is. One length change of
    /// either field without the other beside it disproves the pairing and
    /// the lint stays quiet; so does a pair grown together but never read in
    /// step, and pushes to two different values of the type.
    pub PARALLEL_VECS,
    Warn,
    "sequence fields only ever grown together and read at one index"
}

/// Methods that change a sequence's length.
const LEN_OPS: &[&str] = &[
    "push",
    "push_back",
    "push_front",
    "append",
    "insert",
    "extend",
    "extend_from_slice",
    "extend_from_within",
    "resize",
    "resize_with",
    "pop",
    "pop_back",
    "pop_front",
    "clear",
    "truncate",
    "remove",
    "swap_remove",
    "swap_remove_back",
    "swap_remove_front",
    "drain",
    "retain",
    "retain_mut",
    "dedup",
    "dedup_by",
    "dedup_by_key",
    "split_off",
    "set_len",
    "splice",
    "append_assume_capacity",
];

/// Positional reads with one index argument.
const GET_OPS: &[&str] = &["get", "get_mut", "get_unchecked", "get_unchecked_mut"];

/// Adapters between a sequence and the `zip` that pairs it: `s.a.iter()`.
const ITER_ADAPTERS: &[&str] = &[
    "iter",
    "iter_mut",
    "into_iter",
    "copied",
    "cloned",
    "by_ref",
    "drain",
];

/// The value a sequence field is read off, `s` or `self.tape`: by shape,
/// and by identity too when it is a local binding or a projection of one,
/// since every local hashes alike.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct Place {
    local: Option<HirId>,
    shape: u64,
}

/// Where a length change happens: the body, the nearest block or match arm,
/// and the value it is applied to.
type Site = (DefId, HirId, Place);

/// An indexed read of a sequence field, kept until its body ends.
struct Read {
    owner: LocalDefId,
    adt: DefId,
    field: Symbol,
    place: Place,
    index: HirId,
}

#[derive(Default)]
pub struct ParallelVecs {
    /// Struct -> its sequence fields the crate alone can write; cached, and
    /// empty for structs that do not qualify.
    candidates: HashMap<DefId, Vec<Symbol>>,
    /// (struct, field) -> the sites that change its length.
    writes: HashMap<(DefId, Symbol), HashSet<Site>>,
    /// (struct, field, field), names ordered: read at one index somewhere.
    in_step: HashSet<(DefId, Symbol, Symbol)>,
    reads: Vec<Read>,
}

rustc_session::impl_lint_pass!(ParallelVecs => [PARALLEL_VECS]);

fn has_inherent_method(cx: &LateContext<'_>, did: DefId, names: &[&str]) -> bool {
    cx.tcx.inherent_impls(did).iter().any(|imp| {
        names.iter().any(|n| {
            cx.tcx
                .associated_items(*imp)
                .filter_by_name_unhygienic(Symbol::intern(n))
                .next()
                .is_some()
        })
    })
}

/// `Vec`, `VecDeque`, or a type that grows (`push`/`append`), reports a
/// `len`, and is read by position (`Index` or `get`).
fn is_sequence<'tcx>(cx: &LateContext<'tcx>, ty: Ty<'tcx>) -> bool {
    let ty::Adt(adt, _) = ty.kind() else {
        return false;
    };
    let did = adt.did();
    if cx
        .tcx
        .get_diagnostic_name(did)
        .is_some_and(|n| matches!(n.as_str(), "Vec" | "VecDeque"))
    {
        return true;
    }
    let indexed = cx
        .tcx
        .lang_items()
        .index_trait()
        .is_some_and(|t| cx.tcx.non_blanket_impls_for_ty(t, ty).next().is_some())
        || has_inherent_method(cx, did, &["get", "at"]);
    indexed
        && has_inherent_method(cx, did, &["push", "push_back", "append"])
        && has_inherent_method(cx, did, &["len"])
}

impl ParallelVecs {
    /// The sequence fields of the local struct behind `ty` that only this
    /// crate can write, when there are at least two of them.
    fn candidates<'tcx>(
        &mut self,
        cx: &LateContext<'tcx>,
        ty: Ty<'tcx>,
    ) -> Option<(DefId, &[Symbol])> {
        let ty::Adt(adt, _) = ty.peel_refs().kind() else {
            return None;
        };
        if !adt.is_struct() || !adt.did().is_local() {
            return None;
        }
        let did = adt.did();
        let fields = self.candidates.entry(did).or_insert_with(|| {
            let exported = cx.effective_visibilities.is_exported(did.expect_local());
            let fields: Vec<Symbol> = adt
                .non_enum_variant()
                .fields
                .iter()
                .filter(|f| (!exported || !f.vis.is_public()) && is_sequence(cx, field_ty(cx, f)))
                .map(|f| f.name)
                .collect();
            if fields.len() >= 2 {
                fields
            } else {
                Vec::new()
            }
        });
        (!fields.is_empty()).then_some((did, fields.as_slice()))
    }

    /// `base.field` as a candidate sequence field: its struct, its name, and
    /// the value it is read off.
    fn sequence_field<'tcx>(
        &mut self,
        cx: &LateContext<'tcx>,
        base: &'tcx Expr<'tcx>,
        field: Ident,
    ) -> Option<(DefId, Symbol, Place)> {
        let (adt, fields) = self.candidates(cx, cx.typeck_results().expr_ty_adjusted(base))?;
        if !fields.contains(&field.name) {
            return None;
        }
        let place = Place {
            local: local_root(base),
            shape: hash_expr(cx, base),
        };
        Some((adt, field.name, place))
    }

    fn record_write<'tcx>(
        &mut self,
        cx: &LateContext<'tcx>,
        at: &'tcx Expr<'tcx>,
        base: &'tcx Expr<'tcx>,
        field: Ident,
    ) {
        let Some((adt, field, place)) = self.sequence_field(cx, base, field) else {
            return;
        };
        let body = cx.tcx.hir_enclosing_body_owner(at.hir_id).to_def_id();
        self.writes.entry((adt, field)).or_default().insert((
            body,
            step_scope(cx, at.hir_id),
            place,
        ));
    }

    fn record_index<'tcx>(
        &mut self,
        cx: &LateContext<'tcx>,
        at: &'tcx Expr<'tcx>,
        base: &'tcx Expr<'tcx>,
        field: Ident,
        index: &'tcx Expr<'tcx>,
    ) {
        let Some((adt, field, place)) = self.sequence_field(cx, base, field) else {
            return;
        };
        let owner = cx.tcx.hir_enclosing_body_owner(at.hir_id);
        for earlier in &self.reads {
            if earlier.owner != owner
                || earlier.adt != adt
                || earlier.field == field
                || earlier.place != place
            {
                continue;
            }
            let other = cx.tcx.hir_expect_expr(earlier.index);
            if SpanlessEq::new(cx).eq_expr(at.span.ctxt(), other, index) {
                self.in_step.insert(ordered(adt, earlier.field, field));
            }
        }
        self.reads.push(Read {
            owner,
            adt,
            field,
            place,
            index: index.hir_id,
        });
    }

    /// `zip` over two sequence fields of one value reads them in step.
    fn record_zip<'tcx>(
        &mut self,
        cx: &LateContext<'tcx>,
        l: &'tcx Expr<'tcx>,
        r: &'tcx Expr<'tcx>,
    ) {
        let (Some((lb, lf)), Some((rb, rf))) = (zipped_field(l), zipped_field(r)) else {
            return;
        };
        let (Some((adt, lf, lp)), Some((radt, rf, rp))) = (
            self.sequence_field(cx, lb, lf),
            self.sequence_field(cx, rb, rf),
        ) else {
            return;
        };
        if adt == radt && lf != rf && lp == rp {
            self.in_step.insert(ordered(adt, lf, rf));
        }
    }
}

/// The local binding a place expression projects from, through fields,
/// indexing, `&` and `*`.
fn local_root(mut e: &Expr<'_>) -> Option<HirId> {
    loop {
        match e.kind {
            ExprKind::Field(inner, _)
            | ExprKind::Index(inner, _, _)
            | ExprKind::AddrOf(_, _, inner)
            | ExprKind::Unary(UnOp::Deref, inner)
            | ExprKind::DropTemps(inner) => e = inner,
            _ => return e.res_local_id(),
        }
    }
}

fn ordered(adt: DefId, a: Symbol, b: Symbol) -> (DefId, Symbol, Symbol) {
    if a.as_str() <= b.as_str() {
        (adt, a, b)
    } else {
        (adt, b, a)
    }
}

/// The nearest block or match arm around `hir_id`: two length changes are
/// side by side when they share it.
fn step_scope(cx: &LateContext<'_>, hir_id: HirId) -> HirId {
    for (id, node) in cx.tcx.hir_parent_iter(hir_id) {
        match node {
            Node::Block(_) | Node::Arm(_) => return id,
            Node::Item(_) | Node::ImplItem(_) | Node::TraitItem(_) => break,
            _ => {}
        }
    }
    hir_id
}

/// The `base.field` a `zip` operand iterates: `&s.a`, `s.a.iter()`,
/// `s.a.iter().copied()`.
fn zipped_field<'h>(mut e: &'h Expr<'h>) -> Option<(&'h Expr<'h>, Ident)> {
    loop {
        match e.kind {
            ExprKind::AddrOf(_, _, inner) | ExprKind::DropTemps(inner) => e = inner,
            ExprKind::MethodCall(seg, recv, _, _)
                if ITER_ADAPTERS.contains(&seg.ident.name.as_str()) =>
            {
                e = recv;
            }
            ExprKind::Field(base, ident) => return Some((base, ident)),
            _ => return None,
        }
    }
}

/// `&mut s.a` in a `for` head iterates; anywhere else it is handed to code
/// that may change the length.
fn is_for_loop_head(cx: &LateContext<'_>, e: &Expr<'_>) -> bool {
    get_parent_expr(cx, e).is_some_and(|p| {
        matches!(p.kind, ExprKind::Call(..)) && p.span.is_desugaring(DesugaringKind::ForLoop)
    })
}

impl<'tcx> LateLintPass<'tcx> for ParallelVecs {
    fn check_expr(&mut self, cx: &LateContext<'tcx>, expr: &'tcx Expr<'tcx>) {
        match expr.kind {
            ExprKind::MethodCall(seg, recv, [arg], _) if seg.ident.name.as_str() == "zip" => {
                self.record_zip(cx, recv, arg);
            }
            ExprKind::Call(callee, [l, r])
                if matches!(callee.kind, ExprKind::Path(ref qp)
                    if cx.qpath_res(qp, callee.hir_id).opt_def_id()
                        .is_some_and(|d| cx.tcx.opt_item_name(d)
                            .is_some_and(|n| n.as_str() == "zip"))) =>
            {
                self.record_zip(cx, l, r);
            }
            ExprKind::MethodCall(..) => {
                let Some(call) = field_method_call(expr) else {
                    return;
                };
                let name = call.method.name.as_str();
                if LEN_OPS.contains(&name) {
                    self.record_write(cx, expr, call.base, call.field);
                } else if GET_OPS.contains(&name)
                    && let [index] = call.args
                {
                    self.record_index(cx, expr, call.base, call.field, index);
                }
            }
            ExprKind::Index(..) => {
                if let Some(read) = indexed_field(expr) {
                    self.record_index(cx, expr, read.base, read.field, read.index);
                }
            }
            ExprKind::Assign(place, _, _) | ExprKind::AssignOp(_, place, _) => {
                if let Some((base, field, _)) = assigned_field(place) {
                    self.record_write(cx, expr, base, field);
                }
            }
            ExprKind::AddrOf(BorrowKind::Ref, Mutability::Mut, inner) => {
                if let Some((base, field, _)) = assigned_field(inner)
                    && !is_for_loop_head(cx, expr)
                {
                    self.record_write(cx, expr, base, field);
                }
            }
            _ => {}
        }
    }

    fn check_body_post(&mut self, cx: &LateContext<'tcx>, body: &rustc_hir::Body<'tcx>) {
        let owner = cx.tcx.hir_body_owner_def_id(body.id());
        self.reads.retain(|r| r.owner != owner);
    }

    fn check_crate_post(&mut self, cx: &LateContext<'tcx>) {
        let mut per_struct: HashMap<DefId, Vec<(Symbol, HashSet<Site>)>> = HashMap::new();
        for ((adt, field), sites) in self.writes.drain() {
            per_struct.entry(adt).or_default().push((field, sites));
        }
        let mut findings: Vec<(DefId, Vec<Symbol>, usize)> = Vec::new();
        for (adt, mut fields) in per_struct {
            fields.sort_by_key(|(f, _)| f.as_str().to_owned());
            let mut groups: Vec<(&HashSet<Site>, Vec<Symbol>)> = Vec::new();
            for (field, sites) in &fields {
                match groups.iter_mut().find(|(s, _)| *s == sites) {
                    Some((_, members)) => members.push(*field),
                    None => groups.push((sites, vec![*field])),
                }
            }
            for (sites, members) in groups {
                let read_in_step = members.iter().any(|a| {
                    members
                        .iter()
                        .any(|b| self.in_step.contains(&ordered(adt, *a, *b)))
                });
                if members.len() >= 2 && read_in_step {
                    findings.push((adt, members, sites.len()));
                }
            }
        }
        findings.sort_by_key(|(adt, ..)| cx.tcx.def_span(*adt).lo());
        for (adt, members, sites) in findings {
            let names: Vec<String> = members.iter().map(|m| format!("`{m}`")).collect();
            emit(
                cx,
                PARALLEL_VECS,
                cx.tcx.def_span(adt),
                format!(
                    "parallel vecs: the fields {} of `{}` only change length together ({} {}) and are read at one index, so element `i` of each is one record kept in {} places",
                    names.join(", "),
                    cx.tcx.def_path_str(adt),
                    sites,
                    if sites == 1 { "block" } else { "blocks" },
                    members.len(),
                ),
                "one `Vec` of a struct with these fields holds the pairing in the type and cannot let the lengths differ",
            );
        }
    }
}
