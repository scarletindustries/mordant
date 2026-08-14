//! Shapes several lints read off HIR the same way: the literal `self`
//! receiver, a field of it, a condition with its negations peeled, a branch
//! that ends in `return`, and the definition a call expression invokes. Each
//! lint applies its own filters on top; what lives here is only the part they
//! spelled identically.

use rustc_hir::def_id::DefId;
use rustc_hir::{Expr, ExprKind, QPath, Stmt, StmtKind, UnOp};
use rustc_lint::LateContext;
use rustc_middle::ty::AdtDef;
use rustc_span::Ident;
use rustc_span::symbol::kw;

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
