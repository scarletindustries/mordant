use std::collections::{HashMap, HashSet, VecDeque};

use clippy_utils::visitors::for_each_expr;
use rustc_hir::def_id::DefId;
use rustc_hir::intravisit::FnKind;
use rustc_hir::{Body, Expr, ExprKind, FnDecl, LangItem};
use rustc_lint::{LateContext, LateLintPass};
use rustc_span::Span;
use rustc_span::def_id::LocalDefId;

use crate::MordantConfig;
use crate::baseline::{emit, emit_with_note};
use crate::hir_shapes::{callee_of, def_path_names};

rustc_session::declare_lint! {
    /// Flags a function that can reach something the `forbidden-reach`
    /// config bans for it ("from `scheduler::pick`, no call path may reach
    /// allocation, locking, or panic"), printing the path of calls that gets
    /// there, each arrow of which is a real call expression in this crate.
    /// Dynamic dispatch and function pointers are invisible to the walk, so
    /// absence of a finding proves nothing, but a finding is a concrete path
    /// that exists.
    ///
    /// **One finding per (root, banned definition).** A root breaking two
    /// entries of its `never` list reports twice; several call paths to the
    /// same definition collapse into one finding, which names how many call
    /// sites it stands for. So a count of findings is a count of bans broken,
    /// never of call sites.
    pub FORBIDDEN_REACH,
    Warn,
    "a banned definition is reachable from a declared root"
}

/// One rule from `dylint.toml`:
///
/// ```toml
/// [[mordant.forbidden-reach]]
/// from = "sched::pick"
/// never = ["core::panicking", "std::vec::Vec::push"]
/// ```
#[derive(Clone, Default, serde::Deserialize)]
#[cfg_attr(test, derive(Debug, PartialEq))]
#[serde(rename_all = "kebab-case", default)]
pub struct ReachRule {
    pub from: String,
    pub never: Vec<String>,
}

#[derive(Default)]
pub struct ForbiddenReach {
    rules: Vec<ReachRule>,
    /// Local function -> (callee, call site) edges.
    calls: HashMap<DefId, Vec<(DefId, Span)>>,
    /// Local functions matching a rule's `from`, per rule index.
    roots: Vec<(usize, DefId, Span)>,
}

rustc_session::impl_lint_pass!(ForbiddenReach => [FORBIDDEN_REACH]);

impl ForbiddenReach {
    pub fn new(config: &MordantConfig) -> Self {
        Self {
            rules: config.forbidden_reach.clone(),
            calls: HashMap::new(),
            roots: Vec::new(),
        }
    }

    /// Path-suffix match on `::`-separated segments, so "Vec::push" matches
    /// `std::vec::Vec::push` but a bare substring never matches by accident.
    fn matches_pattern(name: &str, pattern: &str) -> bool {
        name == pattern || name.ends_with(&format!("::{pattern}"))
    }
}

/// The definition an `ExprKind::Index` reaches, for the same edge table a
/// call or method call feeds.
///
/// `[]` on a type with a user `Index`/`IndexMut` impl is operator-overload
/// resolution, recorded in `typeck_results` exactly like a method call --
/// `type_dependent_def_id` is the same lookup `callee_of` makes for
/// `ExprKind::MethodCall`, just on the index expression's `hir_id` instead of
/// the method call's.
///
/// Built-in slice/array indexing is not overload resolution at all: rustc
/// lowers it straight to a MIR place projection plus an `Assert(BoundsCheck)`
/// terminator during MIR building, so `type_dependent_def_id` comes back
/// `None` and there is no HIR call for a walk to have found in the first
/// place. That `None` is the signal, not the absence of one: it is exactly
/// the shape a caller cannot tell from "not an index at all" without also
/// knowing `e.kind` was `Index`, which is why this only runs from that arm.
/// Substitute the lang item behind that terminator, `core::panicking::
/// panic_bounds_check`, as the edge instead.
fn index_edge<'tcx>(cx: &LateContext<'tcx>, e: &Expr<'tcx>) -> Option<DefId> {
    match cx.typeck_results().type_dependent_def_id(e.hir_id) {
        Some(def) => Some(def),
        None => cx.tcx.lang_items().get(LangItem::PanicBoundsCheck),
    }
}

impl<'tcx> LateLintPass<'tcx> for ForbiddenReach {
    fn check_fn(
        &mut self,
        cx: &LateContext<'tcx>,
        kind: FnKind<'tcx>,
        _decl: &'tcx FnDecl<'tcx>,
        body: &'tcx Body<'tcx>,
        span: Span,
        def_id: LocalDefId,
    ) {
        if self.rules.is_empty() || matches!(kind, FnKind::Closure) {
            return;
        }
        let caller = def_id.to_def_id();
        let mut edges = Vec::new();
        for_each_expr(cx, body.value, |e: &Expr<'tcx>| {
            if let Some(callee) = callee_of(cx, e) {
                edges.push((callee.def(), e.span));
            } else if matches!(e.kind, ExprKind::Index(..))
                && let Some(def) = index_edge(cx, e)
            {
                edges.push((def, e.span));
            }
            std::ops::ControlFlow::<()>::Continue(())
        });
        self.calls.insert(caller, edges);

        let names = def_path_names(cx, caller);
        for (i, rule) in self.rules.iter().enumerate() {
            if names.iter().any(|n| Self::matches_pattern(n, &rule.from)) {
                self.roots.push((i, caller, span));
            }
        }
    }

    fn check_crate_post(&mut self, cx: &LateContext<'tcx>) {
        for &(rule_idx, root, root_span) in &self.roots {
            let rule = &self.rules[rule_idx];
            // BFS with parent links. The walk runs to exhaustion instead of stopping at the
            // first banned callee, so a root breaking several `never` entries reports each
            // of them; BFS order makes the parent chain recorded for a definition the
            // shortest witness to it.
            let mut parent: HashMap<DefId, (DefId, Span)> = HashMap::new();
            let mut seen: HashSet<DefId> = HashSet::new();
            let mut queue = VecDeque::new();
            queue.push_back(root);
            seen.insert(root);
            // (banned definition, call sites reaching it). One entry per definition, in walk
            // order, so the findings a root emits do not depend on hash iteration.
            let mut hits: Vec<(DefId, usize)> = Vec::new();
            while let Some(cur) = queue.pop_front() {
                let Some(edges) = self.calls.get(&cur) else {
                    continue;
                };
                for &(callee, at) in edges {
                    let banned = def_path_names(cx, callee)
                        .iter()
                        .any(|n| rule.never.iter().any(|p| Self::matches_pattern(n, p)));
                    if banned {
                        match hits.iter_mut().find(|(def, _)| *def == callee) {
                            Some((_, sites)) => *sites += 1,
                            None => {
                                parent.insert(callee, (cur, at));
                                hits.push((callee, 1));
                            }
                        }
                    }
                    if callee.is_local() && seen.insert(callee) {
                        parent.insert(callee, (cur, at));
                        queue.push_back(callee);
                    }
                }
            }
            for (banned, sites) in hits {
                // Rebuild the witness path root -> ... -> banned.
                let mut chain = vec![cx.tcx.def_path_str(banned)];
                let mut cur = banned;
                while cur != root {
                    let Some(&(prev, _)) = parent.get(&cur) else {
                        break;
                    };
                    chain.push(cx.tcx.def_path_str(prev));
                    cur = prev;
                }
                chain.reverse();
                // Only the shortest path is printed, so say when the finding stands for more
                // than the one call site on it.
                let more = match sites {
                    0 | 1 => String::new(),
                    2 => " (1 more call site reaches it)".to_string(),
                    n => format!(" ({} more call sites reach it)", n - 1),
                };
                let (root_name, banned_name) =
                    (cx.tcx.def_path_str(root), cx.tcx.def_path_str(banned));
                let msg = format!(
                    "`{root_name}` can reach `{banned_name}`, which the `forbidden-reach` config \
                     bans for it. Path: {}{more}",
                    chain.join(" -> "),
                );
                let help = format!(
                    "break the path (each arrow is a real call in this crate), or relax the \
                     `forbidden-reach` rule for `{}` if the call is fine",
                    rule.from,
                );
                match parent.get(&banned) {
                    Some(&(_, at)) => emit_with_note(
                        cx,
                        FORBIDDEN_REACH,
                        root_span,
                        msg,
                        at,
                        format!("the last call in that path, to `{banned_name}`"),
                        help,
                    ),
                    None => emit(cx, FORBIDDEN_REACH, root_span, msg, help),
                }
            }
        }
    }
}
