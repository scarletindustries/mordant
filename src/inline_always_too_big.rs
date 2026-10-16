use rustc_hir::def::DefKind;
use rustc_hir::def_id::LocalDefId;
use rustc_lint::{LateContext, LateLintPass};
use rustc_middle::ty::TyCtxt;

use crate::MordantConfig;
use crate::baseline::emit_hir_then;
use crate::call_graph::{Call, CallGraph};
use crate::mir_flow::{counted_size, mir_for};

rustc_lint::declare_lint! {
    /// Finds `#[inline(always)]` on a function where the attribute cannot work
    /// or makes the binary much larger. There are two cases.
    ///
    /// The function calls itself, directly or through other functions in
    /// this crate. The compiler cannot inline every call in that cycle, and
    /// each call from outside the cycle still gets a full copy of the body.
    ///
    /// The body is large and this crate calls the function from many places.
    /// Each call gets its own copy of the whole body. The finding gives the
    /// body size times the call count as an estimate of the cost.
    ///
    /// Size is counted in MIR statements before optimization, as
    /// `generic_body_not_generic` counts it. Overflow checks add to it. A
    /// call counts only when it names one local function. Calls through
    /// `dyn`, function pointers, and trait methods that depend on a type
    /// parameter are not seen. Calls from other crates are not seen either,
    /// so the count is a lower bound.
    ///
    /// `inline-always-too-big-min-statements` (default 100) and
    /// `inline-always-too-big-min-call-sites` (default 3) set the second case.
    pub(crate) INLINE_ALWAYS_TOO_BIG,
    Warn,
    "`#[inline(always)]` on a function that calls itself, or on a large function called from many places"
}

pub(crate) struct InlineAlwaysTooBig {
    min_statements: usize,
    min_call_sites: usize,
}

rustc_lint::impl_lint_pass!(InlineAlwaysTooBig => [INLINE_ALWAYS_TOO_BIG]);

impl InlineAlwaysTooBig {
    pub(crate) fn new(config: &MordantConfig) -> Self {
        Self {
            min_statements: config.inline_always_too_big_min_statements,
            min_call_sites: config.inline_always_too_big_min_call_sites,
        }
    }
}

/// A plain fn or method marked `#[inline(always)]`, not from an external macro.
fn marked(tcx: TyCtxt<'_>, def: LocalDefId) -> bool {
    matches!(tcx.def_kind(def), DefKind::Fn | DefKind::AssocFn)
        && tcx.codegen_fn_attrs(def).inline.always()
        && !tcx.def_span(def).in_external_macro(tcx.sess.source_map())
}

enum Finding {
    Cycle(Vec<Call>),
    Copies { size: usize, sites: Vec<Call> },
}

impl<'tcx> LateLintPass<'tcx> for InlineAlwaysTooBig {
    fn check_crate_post(&mut self, cx: &LateContext<'tcx>) {
        let tcx = cx.tcx;
        let marked_fns: Vec<LocalDefId> =
            tcx.hir_body_owners().filter(|&d| marked(tcx, d)).collect();
        if marked_fns.is_empty() {
            return;
        }
        let graph = CallGraph::build(tcx);
        let callers = graph.callers();
        let mut findings: Vec<(LocalDefId, Finding)> = Vec::new();
        for def in marked_fns {
            if let Some(cycle) = graph.cycle_through(def) {
                findings.push((def, Finding::Cycle(cycle)));
                continue;
            }
            let sites = callers.get(&def).cloned().unwrap_or_default();
            if sites.len() < self.min_call_sites {
                continue;
            }
            let Some(size) = mir_for(tcx, def).map(|body| counted_size(&body)) else {
                continue;
            };
            if size >= self.min_statements {
                findings.push((def, Finding::Copies { size, sites }));
            }
        }
        findings.sort_by_key(|(def, _)| tcx.def_span(*def).lo());
        for (def, finding) in findings {
            report(cx, def, finding);
        }
    }
}

fn report(cx: &LateContext<'_>, def: LocalDefId, finding: Finding) {
    let tcx = cx.tcx;
    let name = tcx.def_path_str(def);
    let hir_id = tcx.local_def_id_to_hir_id(def);
    let span = tcx.def_span(def);
    match finding {
        Finding::Cycle(chain) => {
            let path: Vec<String> = std::iter::once(name.clone())
                .chain(chain.iter().map(|c| tcx.def_path_str(c.callee)))
                .collect();
            let back = chain[chain.len() - 1].span;
            let msg = format!(
                "`{name}` is `#[inline(always)]` but calls itself: {}. Not every call in the \
                 cycle can be inlined, and each call from outside gets a copy of the body",
                path.join(" -> "),
            );
            emit_hir_then(cx, INLINE_ALWAYS_TOO_BIG, hir_id, span, msg, |diag| {
                diag.span_note(back, format!("this call goes back to `{name}`"));
                diag.help(
                    "use `#[inline]` and let the compiler decide, or move the recursive part \
                     into an `#[inline(never)]` fn",
                );
            });
        }
        Finding::Copies { size, sites } => {
            let count = sites.len();
            let first = sites.iter().map(|c| c.span).min_by_key(|s| s.lo());
            let msg = format!(
                "`{name}` is `#[inline(always)]` with {size} MIR statements and {count} call \
                 {} in this crate, so its body is copied {count} times (about {} statements)",
                if count == 1 { "site" } else { "sites" },
                size * count,
            );
            emit_hir_then(cx, INLINE_ALWAYS_TOO_BIG, hir_id, span, msg, |diag| {
                if let Some(first) = first {
                    diag.span_note(first, "the first call site");
                }
                diag.help(
                    "use `#[inline]` and let the compiler decide, or keep a small \
                     `#[inline(always)]` fast path and move the rest into an `#[inline(never)]` fn",
                );
            });
        }
    }
}
