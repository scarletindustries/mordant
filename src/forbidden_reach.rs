use std::collections::{HashMap, HashSet, VecDeque};

use rustc_hir::def_id::DefId;
use rustc_hir::intravisit::FnKind;
use rustc_hir::{Body, FnDecl};
use rustc_lint::{LateContext, LateLintPass};
use rustc_span::Span;
use rustc_span::def_id::LocalDefId;

use crate::MordantConfig;
use crate::baseline::{emit, emit_with_note};
use crate::call_graph::{CallGraph, Edge, EdgeKind};
use crate::hir_shapes::{def_path_names, path_matches};

rustc_lint::declare_lint! {
    /// Flags a function that can reach something the `forbidden-reach`
    /// config bans for it, such as "from `scheduler::pick`, no call path may
    /// reach allocation, locking, or panic". The finding prints the path of
    /// calls that gets there. Each arrow is a real call, drop or panic in
    /// this crate.
    ///
    /// The walk reads MIR. It follows calls that resolve to one local body,
    /// trait calls included when the types pick the impl. It follows the
    /// `Drop` impls a dropped value runs, and the bodies of closures a
    /// function builds. A failed bounds check, overflow check or division by
    /// zero counts as a call to the `core::panicking` function it runs.
    /// Overflow checks exist only with `-C overflow-checks` on, so a release
    /// profile hides them. Calls through `dyn`, function pointers and other
    /// crates are not followed, so no finding does not prove no path.
    ///
    /// There is one finding per root and banned definition. Several call
    /// paths to the same definition make one finding, which says how many
    /// call sites it stands for.
    pub FORBIDDEN_REACH,
    Warn,
    "a banned definition is reachable from a declared root"
}

/// One rule from `mordant.toml`:
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
    /// Local functions matching a rule's `from`, per rule index.
    roots: Vec<(usize, LocalDefId, Span)>,
}

rustc_lint::impl_lint_pass!(ForbiddenReach => [FORBIDDEN_REACH]);

impl ForbiddenReach {
    pub fn new(config: &MordantConfig) -> Self {
        Self {
            rules: config.forbidden_reach.clone(),
            roots: Vec::new(),
        }
    }
}

fn banned(cx: &LateContext<'_>, rule: &ReachRule, def: DefId) -> bool {
    def_path_names(cx, def)
        .iter()
        .any(|n| rule.never.iter().any(|p| path_matches(n, p)))
}

/// The local body the walk goes on into after `edge`, if any. A panic is
/// always a leaf.
fn next_body(graph: &CallGraph, edge: &Edge) -> Option<LocalDefId> {
    match edge.kind {
        EdgeKind::Call | EdgeKind::Drop | EdgeKind::MaybeDrop | EdgeKind::Closure => {
            edge.local_callee().filter(|&def| graph.has_body(def))
        }
        EdgeKind::Panic => None,
    }
}

impl<'tcx> LateLintPass<'tcx> for ForbiddenReach {
    fn check_fn(
        &mut self,
        cx: &LateContext<'tcx>,
        kind: FnKind<'tcx>,
        _decl: &'tcx FnDecl<'tcx>,
        _body: &'tcx Body<'tcx>,
        span: Span,
        def_id: LocalDefId,
    ) {
        if self.rules.is_empty() || matches!(kind, FnKind::Closure) {
            return;
        }
        let names = def_path_names(cx, def_id.to_def_id());
        for (i, rule) in self.rules.iter().enumerate() {
            if names.iter().any(|n| path_matches(n, &rule.from)) {
                self.roots.push((i, def_id, span));
            }
        }
    }

    fn check_crate_post(&mut self, cx: &LateContext<'tcx>) {
        if self.roots.is_empty() {
            return;
        }
        let graph = CallGraph::get(cx.tcx);
        for &(rule_idx, root, root_span) in &self.roots {
            report_root(cx, graph, &self.rules[rule_idx], root, root_span);
        }
    }
}

/// Walks every edge reachable from `root` and reports each banned
/// definition once, with the shortest path to it.
fn report_root(
    cx: &LateContext<'_>,
    graph: &CallGraph,
    rule: &ReachRule,
    root: LocalDefId,
    root_span: Span,
) {
    // Breadth first, so the parent chain to each definition is a shortest
    // path. The walk does not stop at the first ban, so a root that breaks
    // several `never` entries reports each one.
    let mut parent: HashMap<DefId, (DefId, Span)> = HashMap::new();
    let mut seen: HashSet<LocalDefId> = HashSet::from([root]);
    let mut queue = VecDeque::from([root]);
    // (banned definition, call sites reaching it), in walk order.
    let mut hits: Vec<(DefId, usize)> = Vec::new();
    let mut sites: HashSet<(DefId, Span)> = HashSet::new();
    while let Some(cur) = queue.pop_front() {
        for edge in graph.edges(cur) {
            if !sites.insert((edge.callee, edge.span)) {
                continue;
            }
            let banned = banned(cx, rule, edge.callee)
                || (edge.named != edge.callee && banned(cx, rule, edge.named));
            if banned {
                match hits.iter_mut().find(|(def, _)| *def == edge.callee) {
                    Some((_, count)) => *count += 1,
                    None => {
                        parent.insert(edge.callee, (cur.to_def_id(), edge.span));
                        hits.push((edge.callee, 1));
                    }
                }
            }
            if let Some(next) = next_body(graph, edge)
                && seen.insert(next)
            {
                parent
                    .entry(next.to_def_id())
                    .or_insert((cur.to_def_id(), edge.span));
                queue.push_back(next);
            }
        }
    }
    let root = root.to_def_id();
    for (banned, count) in hits {
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
        // Only the shortest path is printed, so say when the finding stands
        // for more than the one call site on it.
        let more = match count {
            0 | 1 => String::new(),
            2 => " (1 more call site reaches it)".to_string(),
            n => format!(" ({} more call sites reach it)", n - 1),
        };
        let (root_name, banned_name) = (cx.tcx.def_path_str(root), cx.tcx.def_path_str(banned));
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
