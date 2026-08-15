use std::collections::{HashMap, HashSet};

use clippy_utils::higher::Range;
use clippy_utils::res::MaybeResPath;
use rustc_ast::LitKind;
use rustc_hir::def::{DefKind, Res};
use rustc_hir::def_id::DefId;
use rustc_hir::{BinOpKind, Expr, ExprKind, HirId, LetStmt, PatKind, QPath, UnOp};
use rustc_lint::{LateContext, LateLintPass};
use rustc_span::{Span, Symbol};

use crate::adt_facts::{field_ty, struct_field};
use crate::baseline::emit;
use crate::hir_shapes::{assigned_field, callee_of, peel_blocks_unsafe};

rustc_session::declare_lint! {
    /// Flags an integer struct field that the crate itself treats as
    /// sometimes absent — some function compares it `==`/`!=` against
    /// `T::MAX`, `-1`, or a constant named `INVALID`/`NONE`/`SENTINEL` — and
    /// that another function indexes with (`v[x.f as usize]`, a slice range
    /// end, `buf[off..off + len]`, `get_unchecked`) or offsets a pointer by,
    /// with no test of the field anywhere in that function. One reader knows
    /// the magic value means "none"; the other turns it into an out-of-bounds
    /// index or a wild pointer. The type is `u32` when the value set is
    /// `Option<u32>`, and only convention tells the readers apart.
    ///
    /// Reported on the unchecked reader. A function counts as checking the
    /// field if it compares the field, or a local read off it, against
    /// anything (`==`, `!=`, an ordering test against a length), clamps it
    /// (`min`, `checked_add`, ..), or directly calls a predicate (a
    /// `bool`-returning function) that does; a function all of whose visible
    /// callers check is their unchecked half and stays quiet too. `.get(i)`
    /// already answers for an index that is not there and is not counted.
    /// Plain arithmetic on the field is not a use: positions and lengths are
    /// summed everywhere and the sum is only wrong where it meets memory. A
    /// field only ever *assigned* `MAX` and never compared to it is a bound,
    /// not a missing value, and is left alone.
    pub SENTINEL_INT,
    Warn,
    "an integer field compared to a sentinel by one reader and indexed with unchecked by another"
}

/// A struct (local or not) and one of its integer fields.
type Field = (DefId, Symbol);

#[derive(Default)]
struct Evidence {
    /// How the first comparison seen spells the sentinel.
    spelling: String,
    compared: usize,
}

#[derive(Clone, Copy)]
enum Use {
    Index,
    Offset,
}

struct Read {
    body: DefId,
    span: Span,
    how: Use,
}

#[derive(Default)]
pub struct SentinelInt {
    evidence: HashMap<Field, Evidence>,
    reads: HashMap<Field, Vec<Read>>,
    /// The function tests the field, or a value read off it, somewhere.
    checked: HashSet<(DefId, Field)>,
    /// `let i = x.f as usize`: locals that carry a field's value.
    locals: HashMap<HirId, Field>,
    /// Function -> local `bool`-returning functions it calls directly.
    calls: HashMap<DefId, HashSet<DefId>>,
    /// Local function -> functions that call it directly.
    callers: HashMap<DefId, HashSet<DefId>>,
    /// Local functions referenced other than by a direct call: their caller
    /// set is unknowable.
    poisoned: HashSet<DefId>,
}

rustc_session::impl_lint_pass!(SentinelInt => [SENTINEL_INT]);

enum Sentinel {
    Max,
    MinusOne,
    Named(DefId),
}

/// `INVALID_ID`, `Slot::NONE`, `NOT_SET_SENTINEL`: a constant whose name says
/// the value stands for no value.
fn names_absence(name: &str) -> bool {
    name.split('_')
        .any(|w| matches!(w, "INVALID" | "NONE" | "SENTINEL"))
}

/// The sentinel an expression spells, if it is one of the three forms.
fn sentinel_of<'tcx>(cx: &LateContext<'tcx>, e: &'tcx Expr<'tcx>) -> Option<Sentinel> {
    let e = peel_blocks_unsafe(e);
    if !cx.typeck_results().expr_ty(e).is_integral() {
        return None;
    }
    match e.kind {
        ExprKind::Unary(UnOp::Neg, inner) => match peel_blocks_unsafe(inner).kind {
            ExprKind::Lit(lit) if matches!(lit.node, LitKind::Int(v, _) if v.get() == 1) => {
                Some(Sentinel::MinusOne)
            }
            _ => None,
        },
        ExprKind::Path(ref qpath) => match cx.qpath_res(qpath, e.hir_id) {
            Res::Def(DefKind::Const { .. } | DefKind::AssocConst { .. }, did) => {
                let name = cx.tcx.item_name(did);
                if name.as_str() == "MAX" && cx.tcx.crate_name(did.krate).as_str() == "core" {
                    Some(Sentinel::Max)
                } else if names_absence(name.as_str()) {
                    Some(Sentinel::Named(did))
                } else {
                    None
                }
            }
            _ => None,
        },
        _ => None,
    }
}

fn spelling(cx: &LateContext<'_>, s: &Sentinel, e: &Expr<'_>) -> String {
    match s {
        Sentinel::Max => format!("{}::MAX", cx.typeck_results().expr_ty(e)),
        Sentinel::MinusOne => "-1".to_owned(),
        Sentinel::Named(did) => cx.tcx.item_name(*did).to_string(),
    }
}

/// `base.name` as a struct and its integer field.
fn field_key(cx: &LateContext<'_>, base: &Expr<'_>, name: Symbol) -> Option<Field> {
    let adt = cx
        .typeck_results()
        .expr_ty_adjusted(base)
        .peel_refs()
        .ty_adt_def()?;
    if !adt.is_struct() {
        return None;
    }
    let f = struct_field(adt, name)?;
    field_ty(cx, f).is_integral().then_some((adt.did(), name))
}

/// The function an expression belongs to, with closures folded into the
/// function that wrote them: a check before a `.map(|..| v[x.f])` covers it.
fn owner_fn(cx: &LateContext<'_>, hir_id: HirId) -> DefId {
    let mut did = cx.tcx.hir_enclosing_body_owner(hir_id).to_def_id();
    while cx.tcx.is_closure_like(did) || matches!(cx.tcx.def_kind(did), DefKind::InlineConst) {
        did = cx.tcx.parent(did);
    }
    did
}

/// Value-preserving wrappers a field read is still visible through.
const ADAPTERS: &[&str] = &[
    "clone",
    "into",
    "try_into",
    "unwrap",
    "expect",
    "cast_signed",
    "cast_unsigned",
];

/// Calls that index their receiver by their one argument and do not answer
/// for an index that is not there.
const INDEXERS: &[&str] = &[
    "get_unchecked",
    "get_unchecked_mut",
    "split_at",
    "split_at_mut",
    "split_off",
    "remove",
    "swap_remove",
];

const OFFSETS: &[&str] = &[
    "add",
    "sub",
    "offset",
    "byte_add",
    "byte_sub",
    "byte_offset",
];

impl SentinelInt {
    /// The field whose value `e` carries: the field itself through casts,
    /// borrows, derefs and value-preserving adapters, or a local bound to one.
    fn read_of<'tcx>(&self, cx: &LateContext<'tcx>, mut e: &'tcx Expr<'tcx>) -> Option<Field> {
        loop {
            e = peel_blocks_unsafe(e);
            match e.kind {
                ExprKind::Cast(inner, _)
                | ExprKind::AddrOf(_, _, inner)
                | ExprKind::Unary(UnOp::Deref, inner) => e = inner,
                ExprKind::MethodCall(seg, recv, args, _)
                    if args.len() <= 1 && ADAPTERS.contains(&seg.ident.as_str()) =>
                {
                    e = recv;
                }
                // `usize::from(x.f)`, `u32::try_from(x.f)`.
                ExprKind::Call(callee, [arg])
                    if matches!(callee.kind, ExprKind::Path(QPath::TypeRelative(_, seg))
                        if matches!(seg.ident.as_str(), "from" | "try_from")) =>
                {
                    e = arg;
                }
                // `off + len`, `idx - 1`: the sum still carries the sentinel.
                ExprKind::Binary(op, l, r)
                    if matches!(op.node, BinOpKind::Add | BinOpKind::Sub | BinOpKind::Mul) =>
                {
                    return self.read_of(cx, l).or_else(|| self.read_of(cx, r));
                }
                ExprKind::Field(base, ident) => return field_key(cx, base, ident.name),
                ExprKind::Path(_) => {
                    return e
                        .res_local_id()
                        .and_then(|id| self.locals.get(&id).copied());
                }
                _ => return None,
            }
        }
    }

    fn compared(&mut self, cx: &LateContext<'_>, field: Field, s: &Sentinel, at: &Expr<'_>) {
        let ev = self.evidence.entry(field).or_default();
        if ev.compared == 0 {
            ev.spelling = spelling(cx, s, at);
        }
        ev.compared += 1;
    }

    fn read<'tcx>(
        &mut self,
        cx: &LateContext<'tcx>,
        operand: &'tcx Expr<'tcx>,
        span: Span,
        how: Use,
    ) {
        if let Some(field) = self.read_of(cx, operand) {
            let body = owner_fn(cx, operand.hir_id);
            self.reads
                .entry(field)
                .or_default()
                .push(Read { body, span, how });
        }
    }

    /// An index operand: the value itself, or either end of a range.
    fn indexed<'tcx>(&mut self, cx: &LateContext<'tcx>, idx: &'tcx Expr<'tcx>, span: Span) {
        match Range::hir(cx, idx) {
            Some(range) => {
                for end in [range.start, range.end].into_iter().flatten() {
                    self.read(cx, end, span, Use::Index);
                }
            }
            None => self.read(cx, idx, span, Use::Index),
        }
    }

    fn checks(&self, body: DefId, field: Field) -> bool {
        self.checked.contains(&(body, field))
            || self
                .calls
                .get(&body)
                .is_some_and(|cs| cs.iter().any(|c| self.checked.contains(&(*c, field))))
    }

    /// Every visible caller of `body` checks the field first: `body` is the
    /// unchecked half of a checked pair, not an unchecked reader.
    fn callers_check(&self, body: DefId, field: Field) -> bool {
        if self.poisoned.contains(&body) {
            return false;
        }
        self.callers
            .get(&body)
            .is_some_and(|cs| !cs.is_empty() && cs.iter().all(|c| self.checks(*c, field)))
    }

    fn record_call<'tcx>(&mut self, cx: &LateContext<'tcx>, expr: &'tcx Expr<'tcx>) {
        match callee_of(cx, expr) {
            Some(callee) => {
                let def = callee.def();
                if def.is_local() && matches!(cx.tcx.def_kind(def), DefKind::Fn | DefKind::AssocFn)
                {
                    let body = owner_fn(cx, expr.hir_id);
                    self.callers.entry(def).or_default().insert(body);
                    // Only a predicate (`is_root()`, `has_parent()`) stands
                    // in for a comparison; a call that happens to compare
                    // inside says nothing about the caller's own reads.
                    let returns_bool = cx
                        .tcx
                        .fn_sig(def)
                        .instantiate_identity()
                        .skip_normalization()
                        .output()
                        .skip_binder()
                        .is_bool();
                    if returns_bool {
                        self.calls.entry(body).or_default().insert(def);
                    }
                }
            }
            None => {
                let ExprKind::Path(qpath) = &expr.kind else {
                    return;
                };
                if matches!(
                    clippy_utils::get_parent_expr(cx, expr),
                    Some(Expr { kind: ExprKind::Call(callee, _), .. }) if callee.hir_id == expr.hir_id
                ) {
                    return;
                }
                if let Res::Def(DefKind::Fn | DefKind::AssocFn, def) =
                    cx.qpath_res(qpath, expr.hir_id)
                    && def.is_local()
                {
                    self.poisoned.insert(def);
                }
            }
        }
    }
}

impl<'tcx> LateLintPass<'tcx> for SentinelInt {
    fn check_local(&mut self, cx: &LateContext<'tcx>, local: &'tcx LetStmt<'tcx>) {
        if let Some(init) = local.init
            && let PatKind::Binding(_, id, _, None) = local.pat.kind
            && let Some(field) = self.read_of(cx, init)
        {
            self.locals.insert(id, field);
        }
    }

    fn check_expr(&mut self, cx: &LateContext<'tcx>, expr: &'tcx Expr<'tcx>) {
        self.record_call(cx, expr);
        match expr.kind {
            ExprKind::Struct(_, fields, _) => {
                let Some(adt) = cx.typeck_results().expr_ty(expr).ty_adt_def() else {
                    return;
                };
                if !adt.is_struct() {
                    return;
                }
                for init in fields {
                    if let Some(s) = sentinel_of(cx, init.expr)
                        && let Some(f) = struct_field(adt, init.ident.name)
                        && field_ty(cx, f).is_integral()
                    {
                        // A literal that writes the sentinel is where the
                        // spelling is clearest, but it is not a check.
                        let ev = self
                            .evidence
                            .entry((adt.did(), init.ident.name))
                            .or_default();
                        if ev.spelling.is_empty() {
                            ev.spelling = spelling(cx, &s, init.expr);
                        }
                    }
                }
            }
            ExprKind::Assign(place, val, _) => {
                if let Some((base, ident, _)) = assigned_field(place)
                    && let Some(field) = field_key(cx, base, ident.name)
                    && let Some(s) = sentinel_of(cx, val)
                {
                    let ev = self.evidence.entry(field).or_default();
                    if ev.spelling.is_empty() {
                        ev.spelling = spelling(cx, &s, val);
                    }
                }
            }
            ExprKind::Binary(op, l, r) => match op.node {
                BinOpKind::Eq | BinOpKind::Ne => {
                    let body = owner_fn(cx, expr.hir_id);
                    for (a, b) in [(l, r), (r, l)] {
                        if let Some(field) = self.read_of(cx, a) {
                            self.checked.insert((body, field));
                            if let Some(s) = sentinel_of(cx, b) {
                                self.compared(cx, field, &s, b);
                            }
                        }
                    }
                }
                BinOpKind::Lt | BinOpKind::Le | BinOpKind::Gt | BinOpKind::Ge => {
                    let body = owner_fn(cx, expr.hir_id);
                    for a in [l, r] {
                        if let Some(field) = self.read_of(cx, a) {
                            self.checked.insert((body, field));
                        }
                    }
                }
                _ => {}
            },
            ExprKind::Index(_, idx, _) => self.indexed(cx, idx, expr.span),
            ExprKind::MethodCall(seg, recv, args, _) => {
                let name = seg.ident.as_str();
                // The author deciding what an out-of-range value does:
                // overflow-aware arithmetic on it, or a clamp of it.
                let bounded = ["checked_", "saturating_", "wrapping_", "overflowing_"]
                    .iter()
                    .any(|p| name.starts_with(p))
                    || matches!(name, "min" | "max" | "clamp");
                if bounded {
                    for operand in std::iter::once(recv).chain(args.iter()) {
                        if let Some(field) = self.read_of(cx, operand) {
                            self.checked.insert((owner_fn(cx, expr.hir_id), field));
                        }
                    }
                }
                if let [arg] = args {
                    if INDEXERS.contains(&name) {
                        self.indexed(cx, arg, expr.span);
                    } else if OFFSETS.contains(&name)
                        && cx.typeck_results().expr_ty_adjusted(recv).is_raw_ptr()
                    {
                        self.read(cx, arg, expr.span, Use::Offset);
                    }
                }
            }
            _ => {}
        }
    }

    fn check_crate_post(&mut self, cx: &LateContext<'tcx>) {
        let mut findings: Vec<(Span, String)> = Vec::new();
        for (field, reads) in &self.reads {
            let Some(ev) = self.evidence.get(field) else {
                continue;
            };
            if ev.compared == 0 {
                continue;
            }
            // One report per unchecked function, at its first use.
            let mut first: HashMap<DefId, &Read> = HashMap::new();
            for read in reads {
                first
                    .entry(read.body)
                    .and_modify(|r| {
                        if read.span.lo() < r.span.lo() {
                            *r = read;
                        }
                    })
                    .or_insert(read);
            }
            for (body, read) in first {
                if self.checks(body, *field) || self.callers_check(body, *field) {
                    continue;
                }
                let how = match read.how {
                    Use::Index => "indexes with",
                    Use::Offset => "offsets a pointer by",
                };
                let reader = match cx.tcx.opt_item_name(body) {
                    Some(name) => format!("`{name}`"),
                    None => "this body".to_owned(),
                };
                findings.push((
                    read.span,
                    format!(
                        "sentinel `{}` can reach this use: {reader} {how} `{}.{}` and nothing in it tests the field, which the crate compares against that sentinel at {} other site{}",
                        ev.spelling,
                        cx.tcx.item_name(field.0),
                        field.1,
                        ev.compared,
                        if ev.compared == 1 { "" } else { "s" },
                    ),
                ));
            }
        }
        findings.sort_by_key(|(span, _)| span.lo());
        findings.dedup_by_key(|(span, _)| *span);
        for (span, msg) in findings {
            emit(
                cx,
                SENTINEL_INT,
                span,
                msg,
                "the field's value set is `Option<int>`; store that (a `NonZero`/`NonMax` niche keeps the size) and no reader can index without deciding the empty case",
            );
        }
    }
}
