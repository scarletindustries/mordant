use std::collections::{HashMap, HashSet, VecDeque};

use clippy_utils::visitors::for_each_expr;
use rustc_hir::def_id::DefId;
use rustc_hir::intravisit::FnKind;
use rustc_hir::{Body, Expr, ExprKind, FnDecl};
use rustc_lint::{LateContext, LateLintPass};
use rustc_span::Span;
use rustc_span::def_id::LocalDefId;

use crate::MordantConfig;
use crate::baseline::emit;

rustc_session::declare_lint! {
    /// Config-declared reachability bans: "from `scheduler::pick`, no call
    /// path may reach allocation, locking, or panic". On a violation the
    /// finding prints the witness path, every edge of which is a real call
    /// expression in this crate. Dynamic dispatch and function pointers are
    /// invisible to the walk, so absence of a finding proves nothing — but a
    /// finding is a concrete path that exists.
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

    fn names_of(&self, cx: &LateContext<'_>, def: DefId) -> [String; 2] {
        let path = strip_generic_segments(&cx.tcx.def_path_str(def));
        let with_crate = format!("{}::{}", cx.tcx.crate_name(def.krate), path);
        [path, with_crate]
    }

    /// Path-suffix match on `::`-separated segments, so "Vec::push" matches
    /// `std::vec::Vec::push` but a bare substring never matches by accident.
    fn matches_pattern(name: &str, pattern: &str) -> bool {
        name == pattern || name.ends_with(&format!("::{pattern}"))
    }
}

/// `std::vec::Vec::<T>::push` -> `std::vec::Vec::push`: generic-argument
/// segments would defeat suffix matching, and no ban is per-instantiation.
fn strip_generic_segments(path: &str) -> String {
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
            let callee = match &e.kind {
                ExprKind::Call(callee, _) => {
                    if let ExprKind::Path(qpath) = &callee.kind {
                        cx.qpath_res(qpath, callee.hir_id).opt_def_id()
                    } else {
                        None
                    }
                }
                ExprKind::MethodCall(..) => cx.typeck_results().type_dependent_def_id(e.hir_id),
                _ => None,
            };
            if let Some(callee) = callee {
                edges.push((callee, e.span));
            }
            std::ops::ControlFlow::<()>::Continue(())
        });
        self.calls.insert(caller, edges);

        for (i, rule) in self.rules.iter().enumerate() {
            if self
                .names_of(cx, caller)
                .iter()
                .any(|n| Self::matches_pattern(n, &rule.from))
            {
                self.roots.push((i, caller, span));
            }
        }
    }

    fn check_crate_post(&mut self, cx: &LateContext<'tcx>) {
        for &(rule_idx, root, root_span) in &self.roots {
            let rule = &self.rules[rule_idx];
            // BFS with parent links; the first banned hit yields the witness.
            let mut parent: HashMap<DefId, (DefId, Span)> = HashMap::new();
            let mut seen: HashSet<DefId> = HashSet::new();
            let mut queue = VecDeque::new();
            queue.push_back(root);
            seen.insert(root);
            let mut hit: Option<(DefId, Span)> = None;
            'bfs: while let Some(cur) = queue.pop_front() {
                let Some(edges) = self.calls.get(&cur) else {
                    continue;
                };
                for &(callee, at) in edges {
                    let banned = self
                        .names_of(cx, callee)
                        .iter()
                        .any(|n| rule.never.iter().any(|p| Self::matches_pattern(n, p)));
                    if banned {
                        parent.insert(callee, (cur, at));
                        hit = Some((callee, at));
                        break 'bfs;
                    }
                    if callee.is_local() && seen.insert(callee) {
                        parent.insert(callee, (cur, at));
                        queue.push_back(callee);
                    }
                }
            }
            let Some((banned, at)) = hit else {
                continue;
            };
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
            emit(
                cx,
                FORBIDDEN_REACH,
                root_span,
                format!(
                    "`{}` reaches `{}`, which this project bans from it: {}",
                    cx.tcx.def_path_str(root),
                    cx.tcx.def_path_str(banned),
                    chain.join(" -> "),
                ),
                "every arrow is a real call in this crate; break the chain or amend the rule",
            );
            let _ = at;
        }
    }
}
