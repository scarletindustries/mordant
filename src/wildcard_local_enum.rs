use clippy_utils::diagnostics::span_lint_hir_and_then;
use clippy_utils::source::snippet_opt;
use rustc_ast::LitKind;
use rustc_errors::Applicability;
use rustc_hir::def::{CtorOf, DefKind, Res};
use rustc_hir::def_id::DefId;
use rustc_hir::{Arm, Expr, ExprKind, MatchSource, Pat, PatExpr, PatExprKind, PatKind, QPath};
use rustc_lint::{LateContext, LateLintPass};
use rustc_middle::ty;
use rustc_span::Span;

use crate::MordantConfig;

rustc_session::declare_lint! {
    /// Flags `_` (or a catch-all binding) matching over a small crate-local
    /// enum. The wildcard absorbs every future variant: adding one compiles
    /// without a whisper, and this match silently routes it to the old
    /// behavior. Listing the variants keeps exhaustiveness checking alive.
    pub WILDCARD_LOCAL_ENUM,
    Warn,
    "wildcard arm over a small crate-local enum"
}

pub struct WildcardLocalEnum {
    max_variants: usize,
}

rustc_session::impl_lint_pass!(WildcardLocalEnum => [WILDCARD_LOCAL_ENUM]);

impl WildcardLocalEnum {
    pub fn new(config: &MordantConfig) -> Self {
        Self {
            max_variants: config.wildcard_local_enum_max_variants,
        }
    }
}

fn is_negative_extractor(cx: &LateContext<'_>, body: &Expr<'_>) -> bool {
    match body.kind {
        ExprKind::Lit(lit) => matches!(lit.node, LitKind::Bool(false)),
        ExprKind::Path(_) => clippy_utils::is_none_expr(cx, body),
        // `&[]` and `[]`: the empty-slice answer.
        ExprKind::AddrOf(_, _, inner) => is_negative_extractor(cx, inner),
        ExprKind::Array(elems) => elems.is_empty(),
        // `Vec::new()` / `String::new()`: an empty collection, constructed
        // fresh, is an answer with no content, not behavior.
        ExprKind::Call(callee, []) => {
            if let ExprKind::Path(qpath) = &callee.kind
                && let Some(def) = cx.qpath_res(qpath, callee.hir_id).opt_def_id()
            {
                let path = cx.tcx.def_path_str(def);
                path.ends_with("Vec::<T>::new")
                    || path.ends_with("Vec::new")
                    || path.ends_with("String::new")
            } else {
                false
            }
        }
        // `return None` / `return false`: the early-exit spelling of the same
        // empty answers.
        ExprKind::Ret(Some(inner)) => is_negative_extractor(cx, inner),
        _ => false,
    }
}

/// The variant a pattern head names, resolved through either the variant path
/// or its constructor.
fn pat_variant(cx: &LateContext<'_>, pat: &Pat<'_>, qpath: &QPath<'_>) -> Option<(DefId, Span)> {
    let res = cx.qpath_res(qpath, pat.hir_id);
    let variant = match res {
        Res::Def(DefKind::Variant, id) => id,
        Res::Def(DefKind::Ctor(CtorOf::Variant, _), id) => cx.tcx.parent(id),
        _ => return None,
    };
    Some((variant, qpath.span()))
}

/// Every variant the non-catch-all arms cover, or None when an arm's shape is
/// beyond this analysis (then no fix is offered). `qspan` is a variant path
/// span to copy the file's path style from.
fn covered_variants(
    cx: &LateContext<'_>,
    arms: &[Arm<'_>],
    skip: &Pat<'_>,
) -> Option<(Vec<DefId>, Option<Span>)> {
    let mut covered = Vec::new();
    let mut qspan = None;
    for arm in arms {
        if arm.pat.hir_id == skip.hir_id {
            continue;
        }
        // A guarded arm covers nothing: its variant still reaches the
        // catch-all when the guard fails.
        if arm.guard.is_some() {
            continue;
        }
        collect(cx, arm.pat, &mut covered, &mut qspan)?;
    }
    return Some((covered, qspan));

    fn collect(
        cx: &LateContext<'_>,
        pat: &Pat<'_>,
        covered: &mut Vec<DefId>,
        qspan: &mut Option<Span>,
    ) -> Option<()> {
        match &pat.kind {
            PatKind::Expr(PatExpr {
                kind: PatExprKind::Path(qpath),
                ..
            })
            | PatKind::TupleStruct(qpath, ..)
            | PatKind::Struct(qpath, ..) => {
                let (variant, span) = pat_variant(cx, pat, qpath)?;
                covered.push(variant);
                qspan.get_or_insert(span);
                Some(())
            }
            PatKind::Or(pats) => {
                for p in *pats {
                    collect(cx, p, covered, qspan)?;
                }
                Some(())
            }
            _ => None,
        }
    }
}

/// `Enum::Variant`, `Enum::Variant(..)`, or `Enum::Variant { .. }`, spelled
/// with the same path prefix the match's other arms use.
fn render_variant(_cx: &LateContext<'_>, prefix: &str, variant: &ty::VariantDef) -> String {
    let name = variant.name;
    match variant.ctor_kind() {
        Some(rustc_hir::def::CtorKind::Const) => format!("{prefix}{name}"),
        Some(rustc_hir::def::CtorKind::Fn) => format!("{prefix}{name}(..)"),
        None => format!("{prefix}{name} {{ .. }}"),
    }
}

impl<'tcx> LateLintPass<'tcx> for WildcardLocalEnum {
    fn check_expr(&mut self, cx: &LateContext<'tcx>, expr: &'tcx Expr<'tcx>) {
        let ExprKind::Match(scrut, arms, MatchSource::Normal) = expr.kind else {
            return;
        };
        // A single-arm match is a destructuring, not a dispatch.
        if arms.len() < 2 {
            return;
        }
        let ty::Adt(adt, _) = cx
            .typeck_results()
            .expr_ty_adjusted(scrut)
            .peel_refs()
            .kind()
        else {
            return;
        };
        if !adt.is_enum() || !adt.did().is_local() {
            return;
        }
        // No `#[non_exhaustive]` exemption, deliberately. `is_local()` above
        // means the enum is defined in the crate being compiled, and a
        // `LateLintPass` only ever walks that same crate's bodies — so every
        // match this lint can reach is a match in the defining crate, where
        // `#[non_exhaustive]` has no effect and rustc checks exhaustiveness
        // normally. It constrains downstream crates only. An earlier exemption
        // here read "already opted out of exhaustiveness", which was false for
        // every match the lint sees, and it silenced the lint on exactly the
        // enums annotated *because* the author expects new variants.
        let n = adt.variants().len();
        if n > self.max_variants {
            return;
        }
        for arm in arms {
            if arm.guard.is_some() {
                continue;
            }
            let catch_all = matches!(
                arm.pat.kind,
                PatKind::Wild | PatKind::Binding(_, _, _, None)
            );
            // `_ => None` and `_ => false` are extractors asking "is it this
            // one shape?" — future variants are correctly not that shape, so
            // absorption is the right behavior, not a hazard.
            if is_negative_extractor(cx, arm.body) {
                continue;
            }
            if !catch_all {
                continue;
            }
            if crate::baseline::suppressed(cx, WILDCARD_LOCAL_ENUM, arm.pat.span) {
                continue;
            }
            // The fix replaces the catch-all with the uncovered variants,
            // spelled with the same path prefix the sibling arms use. Offered
            // only when every sibling arm's coverage is provable.
            let fix = covered_variants(cx, arms, arm.pat).and_then(|(covered, qspan)| {
                let missing: Vec<&ty::VariantDef> = adt
                    .variants()
                    .iter()
                    .filter(|v| !covered.contains(&v.def_id))
                    .collect();
                if missing.is_empty() {
                    return None;
                }
                let prefix =
                    qspan
                        .and_then(|s| snippet_opt(cx, s))
                        .map(|snip| match snip.rfind("::") {
                            Some(i) => snip[..i + 2].to_string(),
                            None => String::new(),
                        })?;
                let list = missing
                    .iter()
                    .map(|v| render_variant(cx, &prefix, v))
                    .collect::<Vec<_>>()
                    .join(" | ");
                Some(match arm.pat.kind {
                    PatKind::Binding(_, _, ident, None) => format!("{ident} @ ({list})"),
                    _ => list,
                })
            });
            // Emitted against the ARM's hir id, so an #[allow] placed on the
            // arm itself is honored; a plain span emission would only see the
            // enclosing match's lint level.
            span_lint_hir_and_then(
                cx,
                WILDCARD_LOCAL_ENUM,
                arm.hir_id,
                arm.pat.span,
                format!(
                    "this arm absorbs every future variant of `{}` ({n} variants today)",
                    cx.tcx.def_path_str(adt.did()),
                ),
                |diag| {
                    let msg =
                        "list the remaining variants; the compiler then flags every new one added";
                    match &fix {
                        Some(sugg) => {
                            diag.span_suggestion(
                                arm.pat.span,
                                msg,
                                sugg,
                                Applicability::MachineApplicable,
                            );
                        }
                        None => {
                            diag.help(msg);
                        }
                    }
                },
            );
        }
    }
}
