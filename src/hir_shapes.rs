//! Shapes several lints read off HIR the same way: the literal `self`
//! receiver, a field of it, a condition with its negations peeled, a branch
//! that ends in `return`, and the definition a call expression invokes. Each
//! lint applies its own filters on top; what lives here is only the part they
//! spelled identically.

use rustc_hir::def_id::DefId;
use rustc_hir::{Block, Expr, ExprKind, QPath, Stmt, StmtKind, UnOp};
use rustc_lint::LateContext;
use rustc_middle::ty::AdtDef;
use rustc_span::symbol::kw;
use rustc_span::{Ident, Symbol};

/// The bare `self` path.
pub(crate) fn is_self_path(e: &Expr<'_>) -> bool {
    matches!(&e.kind, ExprKind::Path(QPath::Resolved(None, p))
        if p.segments.len() == 1 && p.segments[0].ident.name == kw::SelfLower)
}

/// `self.field`, as the `self` expression it is read off and the field name.
pub(crate) fn self_field<'h>(e: &Expr<'h>) -> Option<(&'h Expr<'h>, Ident)> {
    match e.kind {
        ExprKind::Field(base, ident) if is_self_path(base) => Some((base, ident)),
        _ => None,
    }
}

/// The `base.field` an assignment target writes, through any number of
/// explicit derefs: `x.f`, `(*this).f`, `*s.f`. Returned as `base`, the
/// field name, and the `Field` expression itself.
pub(crate) fn assigned_field<'h>(
    mut place: &'h Expr<'h>,
) -> Option<(&'h Expr<'h>, Ident, &'h Expr<'h>)> {
    while let ExprKind::Unary(UnOp::Deref, inner) | ExprKind::DropTemps(inner) = place.kind {
        place = inner;
    }
    match place.kind {
        ExprKind::Field(base, ident) => Some((base, ident, place)),
        _ => None,
    }
}

/// `root.a.b` as `root` and `[a, b]`, read through `&`, `*` and HIR
/// temporaries at any level, which all name the same place.
pub(crate) fn field_chain<'h>(mut e: &'h Expr<'h>) -> (&'h Expr<'h>, Vec<Symbol>) {
    let mut fields = Vec::new();
    loop {
        match e.kind {
            ExprKind::Field(inner, ident) => {
                fields.push(ident.name);
                e = inner;
            }
            ExprKind::AddrOf(_, _, inner)
            | ExprKind::Unary(UnOp::Deref, inner)
            | ExprKind::DropTemps(inner) => e = inner,
            _ => {
                fields.reverse();
                return (e, fields);
            }
        }
    }
}

/// `head.a.b`; from an empty `head`, `a.b`.
pub(crate) fn dotted(mut head: String, fields: &[Symbol]) -> String {
    for f in fields {
        if !head.is_empty() {
            head.push('.');
        }
        head.push_str(f.as_str());
    }
    head
}

/// `assigned_field` with `base` resolved to the ADT behind its ADJUSTED
/// type, so a write through a `Box`, a guard or any other `Deref` container
/// reaches the type it holds.
pub(crate) fn assigned_adt_field<'tcx>(
    cx: &LateContext<'tcx>,
    place: &'tcx Expr<'tcx>,
) -> Option<(AdtDef<'tcx>, Ident, &'tcx Expr<'tcx>)> {
    let (base, ident, field) = assigned_field(place)?;
    let adt = cx
        .typeck_results()
        .expr_ty_adjusted(base)
        .peel_refs()
        .ty_adt_def()?;
    Some((adt, ident, field))
}

/// `e` with every `!` and HIR condition wrapper stripped, in any
/// interleaving, and whether at least one `!` was among them. `!!x` reports
/// negated: the callers ask whether the condition is spelled as a bail-out,
/// not what it evaluates to.
pub(crate) fn peel_not<'h>(mut e: &'h Expr<'h>) -> (&'h Expr<'h>, bool) {
    let mut negated = false;
    loop {
        match e.kind {
            ExprKind::Unary(UnOp::Not, inner) => {
                negated = true;
                e = inner;
            }
            ExprKind::DropTemps(inner) => e = inner,
            _ => return (e, negated),
        }
    }
}

/// `e` under any nesting of `{ tail }` / `unsafe { tail }` (no statements)
/// and HIR temporaries. `clippy_utils::peel_blocks` stops at `unsafe`.
pub(crate) fn peel_blocks_unsafe<'h>(mut e: &'h Expr<'h>) -> &'h Expr<'h> {
    while let ExprKind::DropTemps(inner)
    | ExprKind::Block(
        &Block {
            stmts: [],
            expr: Some(inner),
            ..
        },
        None,
    ) = e.kind
    {
        e = inner;
    }
    e
}

/// The block's only expression: `{ e }` with no statements, or `{ e; }` /
/// `{ e }` as a single expression statement with no tail. Anything else is
/// None.
pub(crate) fn sole_expr<'h>(b: &'h Block<'h>) -> Option<&'h Expr<'h>> {
    match (b.stmts, b.expr) {
        ([], Some(e)) => Some(e),
        (
            [
                Stmt {
                    kind: StmtKind::Semi(e) | StmtKind::Expr(e),
                    ..
                },
            ],
            None,
        ) => Some(e),
        _ => None,
    }
}

/// The statement's expression, for `let` its initializer.
pub(crate) fn stmt_expr<'tcx>(stmt: &'tcx Stmt<'tcx>) -> Option<&'tcx Expr<'tcx>> {
    match stmt.kind {
        StmtKind::Expr(e) | StmtKind::Semi(e) => Some(e),
        StmtKind::Let(l) => l.init,
        StmtKind::Item(_) => None,
    }
}

/// The expression's final action is a `return`.
pub(crate) fn ends_in_return(e: &Expr<'_>) -> bool {
    match e.kind {
        ExprKind::Ret(_) => true,
        ExprKind::Block(b, _) => match (b.stmts.last(), b.expr) {
            (_, Some(tail)) => ends_in_return(tail),
            (
                Some(Stmt {
                    kind: StmtKind::Expr(s) | StmtKind::Semi(s),
                    ..
                }),
                None,
            ) => ends_in_return(s),
            _ => false,
        },
        _ => false,
    }
}

/// What a call expression invokes.
pub(crate) enum Callee<'h> {
    /// `f(..)`, `T::f(..)`, `E::V(..)`: whatever the callee path resolves to.
    Path { def: DefId, args: &'h [Expr<'h>] },
    /// `recv.m(..)`: the method type-dependent resolution picked.
    Method {
        def: DefId,
        recv: &'h Expr<'h>,
        args: &'h [Expr<'h>],
    },
}

impl Callee<'_> {
    pub(crate) fn def(&self) -> DefId {
        match *self {
            Callee::Path { def, .. } | Callee::Method { def, .. } => def,
        }
    }
}

/// Resolves a call or method call. `def` is unfiltered — constructors and
/// foreign definitions come back too — because each caller wants a different
/// subset. A call through anything but a path (a closure value, a field
/// holding a fn pointer) resolves to nothing.
pub(crate) fn callee_of<'tcx>(cx: &LateContext<'tcx>, e: &Expr<'tcx>) -> Option<Callee<'tcx>> {
    match e.kind {
        ExprKind::Call(callee, args) => {
            let ExprKind::Path(qpath) = &callee.kind else {
                return None;
            };
            let def = cx.qpath_res(qpath, callee.hir_id).opt_def_id()?;
            Some(Callee::Path { def, args })
        }
        ExprKind::MethodCall(_, recv, args, _) => {
            let def = cx.typeck_results().type_dependent_def_id(e.hir_id)?;
            Some(Callee::Method { def, recv, args })
        }
        _ => None,
    }
}

/// A definition's path with generic segments stripped, bare and prefixed
/// with its crate name: the two spellings a config pattern may name it by.
pub(crate) fn def_path_names(cx: &LateContext<'_>, def: DefId) -> [String; 2] {
    let path = strip_generic_segments(&cx.tcx.def_path_str(def));
    let with_crate = format!("{}::{}", cx.tcx.crate_name(def.krate), path);
    [path, with_crate]
}

/// `std::vec::Vec::<T>::push` -> `std::vec::Vec::push`: generic-argument
/// segments would defeat suffix matching, and no pattern is per-instantiation.
pub(crate) fn strip_generic_segments(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    let mut depth = 0usize;
    let mut chars = path.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '<' => depth += 1,
            '>' => depth = depth.saturating_sub(1),
            _ if depth > 0 => {}
            ':' if chars.peek() == Some(&':') && out.ends_with("::") => {
                // Collapse the `::` that wrapped a stripped `::<..>`.
                chars.next();
            }
            _ => out.push(c),
        }
    }
    out
}

/// The identifier an expression is called by: the last segment of a path,
/// the field of a field access, the method of a method call, the callee's
/// last segment of a call. Casts, `&`, unary operators and HIR temporaries
/// are transparent: they change representation, not what the name asserts.
pub(crate) fn value_name(mut e: &Expr<'_>) -> Option<Ident> {
    loop {
        match &e.kind {
            ExprKind::Cast(inner, _)
            | ExprKind::AddrOf(_, _, inner)
            | ExprKind::Unary(_, inner)
            | ExprKind::DropTemps(inner) => e = inner,
            ExprKind::Field(_, ident) => return Some(*ident),
            ExprKind::Path(QPath::Resolved(_, path)) => return Some(path.segments.last()?.ident),
            ExprKind::MethodCall(seg, ..) => return Some(seg.ident),
            ExprKind::Call(callee, _) => {
                let ExprKind::Path(QPath::Resolved(_, path)) = &callee.kind else {
                    return None;
                };
                return Some(path.segments.last()?.ident);
            }
            _ => return None,
        }
    }
}

/// `base.field.method(args)`: a method call whose receiver is a field, split
/// at that field; `field_chain(base)` names the rest of the place.
pub(crate) struct FieldMethodCall<'h> {
    pub base: &'h Expr<'h>,
    pub field: Ident,
    pub method: Ident,
    /// Receiver excluded.
    pub args: &'h [Expr<'h>],
}

/// A method call on a field (`self.items.push(x)`, `(*this).a.b.len()`),
/// through explicit derefs of the receiver; None when the receiver is not a
/// field.
pub(crate) fn field_method_call<'h>(e: &'h Expr<'h>) -> Option<FieldMethodCall<'h>> {
    let ExprKind::MethodCall(seg, recv, args, _) = e.kind else {
        return None;
    };
    let (base, field, _) = assigned_field(recv)?;
    Some(FieldMethodCall {
        base,
        field,
        method: seg.ident,
        args,
    })
}

/// `base.field[index]`: an index expression whose base is a field.
pub(crate) struct IndexedField<'h> {
    pub base: &'h Expr<'h>,
    pub field: Ident,
    pub index: &'h Expr<'h>,
}

/// An index expression on a field, through explicit derefs of the indexed
/// place; None when what is indexed is not a field.
pub(crate) fn indexed_field<'h>(e: &'h Expr<'h>) -> Option<IndexedField<'h>> {
    let ExprKind::Index(place, index, _) = e.kind else {
        return None;
    };
    let (base, field, _) = assigned_field(place)?;
    Some(IndexedField { base, field, index })
}
