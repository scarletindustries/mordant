use std::collections::{HashMap, HashSet};

use crate::adt_facts::{has_fixed_repr, has_positional_fields, private_local_struct, struct_field};
use crate::baseline::emit;
use crate::enum_facts::{arm_variant, ctor_literal_variant};
use clippy_utils::eq_expr_value;
use clippy_utils::{in_automatically_derived, is_default_equivalent};
use rustc_ast::LitKind;
use rustc_hir::def_id::DefId;
use rustc_hir::{
    Arm, BinOpKind, Block, BorrowKind, Expr, ExprKind, Mutability, Node, Pat, PatExpr, PatExprKind,
    PatKind, StmtKind, StructTailExpr, UnOp,
};
use rustc_lint::{LateContext, LateLintPass};
use rustc_middle::ty;
use rustc_span::{Span, Symbol, SyntaxContext, sym};

rustc_session::declare_lint! {
    /// Flags a struct field that is read only where a sibling field is known
    /// to hold one particular value — every read in the crate sits under an
    /// `if`, `match`, `let .. else` or diverging guard that tests the sibling
    /// against the same variant or literal — while a construction site that
    /// gives the sibling any other value fills the field with a placeholder
    /// (`None`, `0`, `""`, `false`, `Default::default()`, a null pointer).
    /// The field is the payload of one case of the sibling, stored flat: the
    /// struct lets every other case carry a value that means nothing and lets
    /// any new reader use it without the test. An enum whose variant carries
    /// the field cannot be built or read that way.
    ///
    /// Only fires on structs private to the crate with named fields and no
    /// explicit `repr`, and only on proof: one read the lint cannot place
    /// under such a test — an accessor, a destructuring pattern, a `Debug`
    /// written by hand, a test on a copy of the sibling rather than the
    /// sibling itself — keeps it quiet, as does a construction that gives
    /// the field a real value beside another value of the sibling, and so
    /// does a sibling that is never given the tested value by any literal or
    /// assignment (the field is then dead, not dependent). Reads in derived
    /// impls are not counted.
    pub DEPENDENT_FIELD,
    Warn,
    "a field that only means something when a sibling field has one value"
}

/// A value a field can be tested against and built with, compared exactly.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Value {
    Variant(DefId),
    Bool(bool),
    Int(u128),
}

impl Value {
    /// The one other value the field can hold, when there is exactly one.
    fn complement(self) -> Option<Value> {
        match self {
            Value::Bool(b) => Some(Value::Bool(!b)),
            Value::Variant(_) | Value::Int(_) => None,
        }
    }
}

/// `sibling == value`, known to hold where a read happens.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct Test {
    sibling: Symbol,
    value: Value,
}

/// What one construction site wrote into one field.
#[derive(Clone, Copy)]
struct Init {
    value: Option<Value>,
    placeholder: bool,
}

#[derive(Default)]
pub struct DependentField {
    /// (struct, field) -> per read, the sibling tests that dominate it. An
    /// empty set is a read nothing is known at.
    reads: HashMap<(DefId, Symbol), Vec<HashSet<Test>>>,
    /// struct -> per literal construction site, what each named field got.
    sites: HashMap<DefId, Vec<HashMap<Symbol, Init>>>,
    /// Fields assigned after construction, whose values the sites do not
    /// bound.
    assigned: HashSet<(DefId, Symbol)>,
}

rustc_session::impl_lint_pass!(DependentField => [DEPENDENT_FIELD]);

/// The struct behind `ty` when every read and construction of it is this
/// crate's to see and its fields have names a message can use.
fn relevant<'tcx>(cx: &LateContext<'tcx>, ty: ty::Ty<'tcx>) -> Option<ty::AdtDef<'tcx>> {
    let adt = private_local_struct(cx, ty)?;
    let v = adt.non_enum_variant();
    (!has_fixed_repr(adt) && !has_positional_fields(v) && v.fields.len() >= 2).then_some(adt)
}

/// `e` under HIR temporaries and shared borrows: `&x.g` names what `x.g`
/// does for a comparison or a `match`.
fn strip_ref<'h>(mut e: &'h Expr<'h>) -> &'h Expr<'h> {
    while let ExprKind::DropTemps(inner)
    | ExprKind::AddrOf(BorrowKind::Ref, Mutability::Not, inner) = e.kind
    {
        e = inner;
    }
    e
}

/// `e` with `!`s removed, and whether they flip it (their parity, which
/// `hir_shapes::peel_not` does not keep).
fn peel_parity<'h>(mut e: &'h Expr<'h>) -> (&'h Expr<'h>, bool) {
    let mut flipped = false;
    loop {
        match e.kind {
            ExprKind::Unary(UnOp::Not, inner) => {
                flipped = !flipped;
                e = inner;
            }
            ExprKind::DropTemps(inner) => e = inner,
            _ => return (e, flipped),
        }
    }
}

fn lit_value(lit: &LitKind) -> Option<Value> {
    match *lit {
        LitKind::Bool(b) => Some(Value::Bool(b)),
        LitKind::Int(n, _) => Some(Value::Int(n.get())),
        _ => None,
    }
}

/// The value `e` spells out: a variant literal, a bool or an unsigned
/// integer literal. Named constants are not values here: two names can be
/// one value.
fn expr_value(cx: &LateContext<'_>, e: &Expr<'_>) -> Option<Value> {
    let e = strip_ref(e);
    if let Some(v) = ctor_literal_variant(cx, e) {
        return Some(Value::Variant(v));
    }
    match e.kind {
        ExprKind::Lit(lit) => lit_value(&lit.node),
        _ => None,
    }
}

/// The one value a pattern admits at its head; or-patterns, ranges,
/// bindings without a sub-pattern and wildcards admit more than one.
fn pat_value(cx: &LateContext<'_>, pat: &Pat<'_>) -> Option<Value> {
    match pat.kind {
        PatKind::Binding(.., Some(sub))
        | PatKind::Ref(sub, ..)
        | PatKind::Deref(sub)
        | PatKind::Box(sub) => pat_value(cx, sub),
        PatKind::Expr(PatExpr {
            kind:
                PatExprKind::Lit {
                    lit,
                    negated: false,
                },
            ..
        }) => lit_value(&lit.node),
        _ => arm_variant(cx, pat).map(Value::Variant),
    }
}

/// Whether variant `v` has no fields, so `!= E::V` rules the variant out and
/// not just one payload of it.
fn negation_rules_out(cx: &LateContext<'_>, v: Value) -> bool {
    match v {
        Value::Bool(_) | Value::Int(_) => true,
        Value::Variant(did) => cx
            .tcx
            .adt_def(cx.tcx.parent(did))
            .variant_with_id(did)
            .fields
            .is_empty(),
    }
}

/// A don't-care initialiser: whatever `Default` would give, or a null
/// pointer.
fn is_placeholder(cx: &LateContext<'_>, e: &Expr<'_>) -> bool {
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

/// Which sibling of the read field `e` names: `e` must be `<base>.g` over
/// the very place expression the read was `<base>.f` over.
fn sibling_of(cx: &LateContext<'_>, base: &Expr<'_>, e: &Expr<'_>) -> Option<Symbol> {
    let ExprKind::Field(other, ident) = strip_ref(e).kind else {
        return None;
    };
    eq_expr_value(cx, SyntaxContext::root(), base, other).then_some(ident.name)
}

/// Every `sibling == value` fact that follows from `cond` evaluating to
/// `holds`, for siblings read off `base`.
fn cond_tests(
    cx: &LateContext<'_>,
    base: &Expr<'_>,
    cond: &Expr<'_>,
    holds: bool,
    out: &mut HashSet<Test>,
) {
    let (inner, flipped) = peel_parity(cond);
    let holds = holds != flipped;
    match inner.kind {
        ExprKind::Binary(op, l, r) if op.node == BinOpKind::And && holds => {
            cond_tests(cx, base, l, true, out);
            cond_tests(cx, base, r, true, out);
        }
        ExprKind::Binary(op, l, r) if op.node == BinOpKind::Or && !holds => {
            cond_tests(cx, base, l, false, out);
            cond_tests(cx, base, r, false, out);
        }
        ExprKind::Binary(op, l, r) if matches!(op.node, BinOpKind::Eq | BinOpKind::Ne) => {
            let equal = (op.node == BinOpKind::Eq) == holds;
            let sides = sibling_of(cx, base, l)
                .map(|s| (s, r))
                .or_else(|| sibling_of(cx, base, r).map(|s| (s, l)));
            let Some((sibling, ve)) = sides else {
                return;
            };
            let Some(value) = expr_value(cx, ve) else {
                return;
            };
            known(cx, sibling, value, equal, out);
        }
        ExprKind::Let(l) if holds => {
            if let Some(sibling) = sibling_of(cx, base, l.init)
                && let Some(value) = pat_value(cx, l.pat)
            {
                known(cx, sibling, value, true, out);
            }
        }
        // `matches!(base.g, P)`.
        ExprKind::Match(scrut, [yes, no], _)
            if is_bool_lit(yes.body, true)
                && is_bool_lit(no.body, false)
                && matches!(no.pat.kind, PatKind::Wild)
                && (holds || yes.guard.is_none()) =>
        {
            if let Some(sibling) = sibling_of(cx, base, scrut)
                && let Some(value) = pat_value(cx, yes.pat)
            {
                known(cx, sibling, value, holds, out);
            }
        }
        _ => {
            if let Some(sibling) = sibling_of(cx, base, inner)
                && cx.typeck_results().expr_ty(inner).is_bool()
            {
                known(cx, sibling, Value::Bool(holds), true, out);
            }
        }
    }
}

fn is_bool_lit(e: &Expr<'_>, b: bool) -> bool {
    matches!(strip_ref(e).kind, ExprKind::Lit(lit) if lit.node == LitKind::Bool(b))
}

/// Record `sibling == value` (or, for `equal == false`, what `!=` pins down,
/// which is something only when the value has exactly one complement).
fn known(
    cx: &LateContext<'_>,
    sibling: Symbol,
    value: Value,
    equal: bool,
    out: &mut HashSet<Test>,
) {
    if equal {
        out.insert(Test { sibling, value });
    } else if negation_rules_out(cx, value)
        && let Some(value) = value.complement()
    {
        out.insert(Test { sibling, value });
    }
}

fn is_never(cx: &LateContext<'_>, e: &Expr<'_>) -> bool {
    cx.typeck_results().expr_ty(e).is_never()
}

/// Facts established by the statements of `block` that run before `child`:
/// a guard whose failing branch diverges, a `let .. else`, a `match` whose
/// every other arm diverges.
fn preceding_guards(
    cx: &LateContext<'_>,
    base: &Expr<'_>,
    block: &Block<'_>,
    child: rustc_hir::HirId,
    out: &mut HashSet<Test>,
) {
    let pos = block
        .stmts
        .iter()
        .position(|s| s.hir_id == child || matches!(s.kind, StmtKind::Let(l) if l.hir_id == child))
        .unwrap_or_else(|| {
            if block.expr.is_some_and(|e| e.hir_id == child) {
                block.stmts.len()
            } else {
                0
            }
        });
    for stmt in &block.stmts[..pos] {
        match stmt.kind {
            StmtKind::Let(l) => {
                if l.els.is_some()
                    && let Some(init) = l.init
                    && let Some(sibling) = sibling_of(cx, base, init)
                    && let Some(value) = pat_value(cx, l.pat)
                {
                    known(cx, sibling, value, true, out);
                }
            }
            StmtKind::Expr(e) | StmtKind::Semi(e) => match e.kind {
                ExprKind::If(cond, then, els) => {
                    let then_diverges = is_never(cx, then);
                    let else_diverges = els.is_some_and(|x| is_never(cx, x));
                    if then_diverges && !else_diverges {
                        cond_tests(cx, base, cond, false, out);
                    } else if else_diverges && !then_diverges {
                        cond_tests(cx, base, cond, true, out);
                    }
                }
                ExprKind::Match(scrut, arms, _) => {
                    let mut live = arms.iter().filter(|a| !is_never(cx, a.body));
                    if let (Some(only), None) = (live.next(), live.next())
                        && only.guard.is_none()
                        && let Some(sibling) = sibling_of(cx, base, scrut)
                        && let Some(value) = pat_value(cx, only.pat)
                    {
                        known(cx, sibling, value, true, out);
                    }
                }
                _ => {}
            },
            StmtKind::Item(_) => {}
        }
    }
}

/// Every sibling test that dominates `read` (a `<base>.f` expression):
/// enclosing `if` conditions and `match` arms on `<base>.g`, and guards
/// that ran earlier in an enclosing block.
fn dominating_tests(cx: &LateContext<'_>, read: &Expr<'_>, base: &Expr<'_>) -> HashSet<Test> {
    let mut out = HashSet::new();
    let mut child = read.hir_id;
    let mut via_arm: Option<&Arm<'_>> = None;
    for (id, node) in cx.tcx.hir_parent_iter(read.hir_id) {
        match node {
            Node::Expr(e) => match e.kind {
                ExprKind::If(cond, then, els) => {
                    if then.hir_id == child {
                        cond_tests(cx, base, cond, true, &mut out);
                    } else if els.is_some_and(|x| x.hir_id == child) {
                        cond_tests(cx, base, cond, false, &mut out);
                    }
                }
                ExprKind::Match(scrut, ..) => {
                    if let Some(arm) = via_arm.take()
                        && let Some(sibling) = sibling_of(cx, base, scrut)
                        && let Some(value) = pat_value(cx, arm.pat)
                    {
                        known(cx, sibling, value, true, &mut out);
                    }
                }
                _ => {}
            },
            Node::Arm(arm) => via_arm = Some(arm),
            Node::Block(b) => preceding_guards(cx, base, b, child, &mut out),
            Node::Item(_) | Node::ImplItem(_) | Node::TraitItem(_) | Node::ForeignItem(_) => break,
            _ => {}
        }
        child = id;
    }
    out
}

impl DependentField {
    fn record_read(&mut self, adt: DefId, field: Symbol, tests: HashSet<Test>) {
        self.reads.entry((adt, field)).or_default().push(tests);
    }
}

impl<'tcx> LateLintPass<'tcx> for DependentField {
    fn check_expr(&mut self, cx: &LateContext<'tcx>, expr: &'tcx Expr<'tcx>) {
        match expr.kind {
            ExprKind::Struct(_, fields, tail) => {
                let Some(adt) = relevant(cx, cx.typeck_results().expr_ty(expr)) else {
                    return;
                };
                if !matches!(tail, StructTailExpr::None) {
                    return;
                }
                let site = fields
                    .iter()
                    .map(|f| {
                        let init = Init {
                            value: expr_value(cx, f.expr),
                            placeholder: is_placeholder(cx, f.expr),
                        };
                        (f.ident.name, init)
                    })
                    .collect();
                self.sites.entry(adt.did()).or_default().push(site);
            }
            ExprKind::Field(base, ident) => {
                let Some(adt) = relevant(cx, cx.typeck_results().expr_ty_adjusted(base)) else {
                    return;
                };
                if struct_field(adt, ident.name).is_none()
                    || in_automatically_derived(cx.tcx, expr.hir_id)
                {
                    return;
                }
                // `base.f = ..` writes; everything else, `base.f += ..` and
                // `&mut base.f` included, reads.
                if let Node::Expr(parent) = cx.tcx.parent_hir_node(expr.hir_id)
                    && let ExprKind::Assign(lhs, ..) = parent.kind
                    && lhs.hir_id == expr.hir_id
                {
                    self.assigned.insert((adt.did(), ident.name));
                    return;
                }
                let tests = dominating_tests(cx, expr, base);
                self.record_read(adt.did(), ident.name, tests);
            }
            _ => {}
        }
    }

    fn check_pat(&mut self, cx: &LateContext<'tcx>, pat: &'tcx Pat<'tcx>) {
        let PatKind::Struct(_, fields, _) = pat.kind else {
            return;
        };
        let Some(typeck) = cx.maybe_typeck_results() else {
            return;
        };
        let Some(adt) = relevant(cx, typeck.pat_ty(pat)) else {
            return;
        };
        if in_automatically_derived(cx.tcx, pat.hir_id) {
            return;
        }
        for f in fields {
            self.record_read(adt.did(), f.ident.name, HashSet::new());
        }
    }

    fn check_crate_post(&mut self, cx: &LateContext<'tcx>) {
        let mut findings: Vec<(Span, String)> = Vec::new();
        for ((did, field), reads) in &self.reads {
            // The tests every read agrees on; one bare read empties it.
            let mut common: Option<HashSet<Test>> = None;
            for tests in reads {
                common = Some(match common {
                    None => tests.clone(),
                    Some(c) => c.intersection(tests).copied().collect(),
                });
            }
            let Some(common) = common else { continue };
            let adt = cx.tcx.adt_def(*did);
            let order: Vec<Symbol> = adt
                .non_enum_variant()
                .fields
                .iter()
                .map(|f| f.name)
                .collect();
            let mut candidates: Vec<Test> =
                common.into_iter().filter(|t| t.sibling != *field).collect();
            candidates.sort_by_key(|t| order.iter().position(|n| *n == t.sibling));
            let sites = self.sites.get(did).map_or(&[][..], Vec::as_slice);
            for test in candidates {
                // The tested case must be one the crate makes: a literal site
                // with that value, a site whose value is not spelled out, or
                // any later assignment. Otherwise the field is never read at
                // all, which is not this lint's claim.
                let reached = self.assigned.contains(&(*did, test.sibling))
                    || sites.iter().any(|s| {
                        s.get(&test.sibling)
                            .is_some_and(|i| i.value.is_none_or(|v| v == test.value))
                    });
                if !reached {
                    continue;
                }
                // Sites that give the sibling some other spelled-out value.
                let elsewhere = sites.iter().filter(|s| {
                    s.get(&test.sibling)
                        .and_then(|i| i.value)
                        .is_some_and(|v| v != test.value)
                });
                let (mut placeholders, mut real) = (0usize, false);
                for site in elsewhere {
                    match site.get(field) {
                        Some(init) if init.placeholder => placeholders += 1,
                        Some(_) => real = true,
                        None => {}
                    }
                }
                if placeholders == 0 || real {
                    continue;
                }
                let Some(fdef) = struct_field(adt, *field) else {
                    continue;
                };
                let holds = match test.value {
                    Value::Variant(v) => format!(
                        "{} == {}::{}",
                        test.sibling,
                        cx.tcx.item_name(cx.tcx.parent(v)),
                        cx.tcx.item_name(v)
                    ),
                    Value::Bool(true) => test.sibling.to_string(),
                    Value::Bool(false) => format!("!{}", test.sibling),
                    Value::Int(n) => format!("{} == {n}", test.sibling),
                };
                findings.push((
                    cx.tcx.def_span(fdef.did),
                    format!(
                        "`{field}` is only read where `{holds}` has been tested ({} read{}), and every `{}` made with another `{}` fills it with a placeholder ({placeholders} site{})",
                        reads.len(),
                        if reads.len() == 1 { "" } else { "s" },
                        cx.tcx.item_name(*did),
                        test.sibling,
                        if placeholders == 1 { "" } else { "s" },
                    ),
                ));
                break;
            }
        }
        findings.sort_by_key(|(span, _)| span.lo());
        for (span, msg) in findings {
            emit(
                cx,
                DEPENDENT_FIELD,
                span,
                msg,
                "the field is the payload of that one case, stored flat; an enum variant carrying it leaves the other cases nothing to fill in or misread",
            );
        }
    }
}
