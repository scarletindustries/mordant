//! The calls between this crate's function bodies, read off MIR, and the
//! helpers that resolve a call to the fn it runs.
//!
//! Each body gets one edge per terminator that runs other code: a call, a
//! drop of a value whose type has `Drop` impls, a failed assertion's panic,
//! and the creation of a closure. Callees are resolved with
//! `Instance::try_resolve` under the caller's typing env, so a trait call
//! whose impl the types pick goes to that impl. Bodies run at compile time
//! (consts, statics) are left out, since their calls are not in the binary.

use std::collections::VecDeque;
use std::collections::hash_map::Entry;
use std::sync::OnceLock;

use rustc_data_structures::fx::{FxHashMap, FxHashSet, FxIndexMap};
use rustc_hir::ConstContext;
use rustc_hir::def_id::{DefId, LocalDefId};
use rustc_middle::mir::{AggregateKind, BasicBlock, Body, Rvalue, StatementKind, TerminatorKind};
use rustc_middle::ty::{self, GenericArgsRef, Instance, Ty, TyCtxt, TypingEnv};
use rustc_span::Span;

use crate::mir_flow::{assert_panic_lang_item, mir_for};

/// What a terminator or statement does to reach `callee`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum EdgeKind {
    /// A `Call` or `TailCall` terminator.
    Call,
    /// A `Drop` terminator, to a `Drop::drop` impl the dropped value surely runs.
    Drop,
    /// A `Drop` terminator, to a `Drop::drop` impl that runs only if the value
    /// holds one: inside an `Option`, a `Vec`, an `Rc`, one enum variant.
    MaybeDrop,
    /// An `Assert` terminator, to the panic fn it calls when it fails.
    Panic,
    /// A closure or coroutine built here. It runs whenever its holder says.
    Closure,
}

/// One edge out of a body.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Edge {
    pub(crate) caller: LocalDefId,
    /// The fn the edge runs: the impl item when the call resolves, or else
    /// the item the call names.
    pub(crate) callee: DefId,
    /// The item the call names in the source: a trait item for a trait call.
    pub(crate) named: DefId,
    /// The callee is the one body this edge runs, not a trait item or shim.
    pub(crate) resolved: bool,
    pub(crate) kind: EdgeKind,
    /// The block whose terminator makes the edge, or holds the statement.
    pub(crate) block: BasicBlock,
    pub(crate) span: Span,
}

impl Edge {
    /// The local body this edge runs, when it runs exactly one.
    pub(crate) fn local_callee(&self) -> Option<LocalDefId> {
        if !self.resolved {
            return None;
        }
        self.callee.as_local()
    }
}

/// Every body's edges, in block order. A body appears even with no edges.
pub(crate) struct CallGraph {
    out: FxIndexMap<LocalDefId, Vec<Edge>>,
}

/// One graph per compilation. Each rustc process compiles one crate.
static GRAPH: OnceLock<CallGraph> = OnceLock::new();

impl CallGraph {
    /// The graph of this crate, built on first use.
    pub(crate) fn get(tcx: TyCtxt<'_>) -> &'static CallGraph {
        GRAPH.get_or_init(|| Self::build(tcx))
    }

    fn build(tcx: TyCtxt<'_>) -> Self {
        let mut out: FxIndexMap<LocalDefId, Vec<Edge>> = FxIndexMap::default();
        for owner in tcx.hir_body_owners() {
            if let Some(ConstContext::Const { .. } | ConstContext::Static(_)) =
                tcx.hir_body_const_context(owner)
            {
                continue;
            }
            let Some(body) = mir_for(tcx, owner) else {
                continue;
            };
            out.insert(owner, body_edges(tcx, owner, &body));
        }
        Self { out }
    }

    /// The bodies in the graph, in HIR order.
    pub(crate) fn bodies(&self) -> impl Iterator<Item = LocalDefId> + '_ {
        self.out.keys().copied()
    }

    /// `def` is a body in the graph.
    pub(crate) fn has_body(&self, def: LocalDefId) -> bool {
        self.out.contains_key(&def)
    }

    /// The edges out of `caller`, or none for a body not in the graph.
    pub(crate) fn edges(&self, caller: LocalDefId) -> &[Edge] {
        self.out.get(&caller).map_or(&[], Vec::as_slice)
    }

    /// Calls from `caller` to local bodies, one per call site.
    pub(crate) fn local_calls(&self, caller: LocalDefId) -> impl Iterator<Item = Edge> + '_ {
        let mut seen: FxHashSet<Span> = FxHashSet::default();
        self.edges(caller)
            .iter()
            .filter(|e| e.kind == EdgeKind::Call && e.local_callee().is_some())
            .filter(move |e| seen.insert(e.span))
            .copied()
    }

    /// The calls to each local fn, one per call site, in graph order.
    pub(crate) fn callers(&self) -> FxHashMap<LocalDefId, Vec<Edge>> {
        let mut into: FxHashMap<LocalDefId, Vec<Edge>> = FxHashMap::default();
        for caller in self.bodies() {
            for call in self.local_calls(caller) {
                if let Some(callee) = call.local_callee() {
                    into.entry(callee).or_default().push(call);
                }
            }
        }
        into
    }

    /// The shortest chain of calls that leads from `start` back to `start`,
    /// or `None` when no call path returns to it.
    pub(crate) fn cycle_through(&self, start: LocalDefId) -> Option<Vec<Edge>> {
        let mut parent: FxHashMap<LocalDefId, Edge> = FxHashMap::default();
        let mut queue = VecDeque::from([start]);
        while let Some(cur) = queue.pop_front() {
            for call in self.local_calls(cur) {
                let Some(callee) = call.local_callee() else {
                    continue;
                };
                if callee == start {
                    let mut chain = vec![call];
                    let mut at = cur;
                    while at != start {
                        let step = parent[&at];
                        chain.push(step);
                        at = step.caller;
                    }
                    chain.reverse();
                    return Some(chain);
                }
                if let Entry::Vacant(slot) = parent.entry(callee) {
                    slot.insert(call);
                    queue.push_back(callee);
                }
            }
        }
        None
    }
}

fn body_edges<'tcx>(tcx: TyCtxt<'tcx>, owner: LocalDefId, body: &Body<'tcx>) -> Vec<Edge> {
    let env = TypingEnv::post_analysis(tcx, owner);
    let mut edges = Vec::new();
    let mut push = |callee: DefId, named: DefId, resolved: bool, kind, block, span| {
        edges.push(Edge {
            caller: owner,
            callee,
            named,
            resolved,
            kind,
            block,
            span,
        });
    };
    for (block, data) in body.basic_blocks.iter_enumerated() {
        if data.is_cleanup {
            continue;
        }
        for statement in &data.statements {
            if let StatementKind::Assign(assign) = &statement.kind
                && let Rvalue::Aggregate(kind, _) = &assign.1
                && let AggregateKind::Closure(def, _)
                | AggregateKind::Coroutine(def, _)
                | AggregateKind::CoroutineClosure(def, _) = **kind
            {
                push(
                    def,
                    def,
                    true,
                    EdgeKind::Closure,
                    block,
                    statement.source_info.span,
                );
            }
        }
        let Some(terminator) = &data.terminator else {
            continue;
        };
        let span = terminator.source_info.span;
        match &terminator.kind {
            TerminatorKind::Call { func, .. } | TerminatorKind::TailCall { func, .. } => {
                let Some((def, args)) = func.const_fn_def() else {
                    continue;
                };
                match resolve_item(tcx, env, def, args) {
                    Some(item) => push(item, def, true, EdgeKind::Call, block, span),
                    None => push(def, def, false, EdgeKind::Call, block, span),
                }
            }
            TerminatorKind::Drop { place, .. } => {
                let ty = place.ty(&body.local_decls, tcx).ty;
                for (drop, sure) in drop_fns(tcx, env, ty) {
                    let kind = if sure {
                        EdgeKind::Drop
                    } else {
                        EdgeKind::MaybeDrop
                    };
                    push(drop, drop, true, kind, block, span);
                }
            }
            TerminatorKind::Assert { msg, .. } => {
                if let Some(def) = tcx.lang_items().get(assert_panic_lang_item(msg)) {
                    push(def, def, true, EdgeKind::Panic, block, span);
                }
            }
            _ => {}
        }
    }
    edges
}

// ── resolution ───────────────────────────────────────────────────────────────

/// The instance a call to `def` with `args` runs, with the arguments
/// normalized first. `None` when the impl depends on a type parameter.
pub(crate) fn resolve_instance<'tcx>(
    tcx: TyCtxt<'tcx>,
    env: TypingEnv<'tcx>,
    def: DefId,
    args: GenericArgsRef<'tcx>,
) -> Option<Instance<'tcx>> {
    let args = tcx
        .try_normalize_erasing_regions(env, ty::Unnormalized::new_wip(args))
        .ok()?;
    Instance::try_resolve(tcx, env, def, args).ok()?
}

/// The fn item a call to `def` runs: the impl's item for a trait method
/// the types pick, or `def` itself. `None` for `dyn` calls, shims and trait
/// calls that depend on a type parameter.
pub(crate) fn resolve_item<'tcx>(
    tcx: TyCtxt<'tcx>,
    env: TypingEnv<'tcx>,
    def: DefId,
    args: GenericArgsRef<'tcx>,
) -> Option<DefId> {
    match resolve_instance(tcx, env, def, args)?.def {
        ty::InstanceKind::Item(item) => Some(item),
        _ => None,
    }
}

// ── drop glue ────────────────────────────────────────────────────────────────

/// The `Drop::drop` fns that dropping a `ty` can run, outermost first, each
/// with whether it surely runs. A local type adds its fields. A type from
/// another crate adds its type arguments, since a container drops what it
/// holds, but maybe not: an `Option` can be `None`, an `Rc` can be shared.
/// The fields of one enum variant maybe run too. `ManuallyDrop`, references
/// and pointers stop the walk.
pub(crate) fn drop_fns<'tcx>(
    tcx: TyCtxt<'tcx>,
    env: TypingEnv<'tcx>,
    ty: Ty<'tcx>,
) -> Vec<(DefId, bool)> {
    let mut found = Vec::new();
    let mut seen: FxHashSet<Ty<'tcx>> = FxHashSet::default();
    walk_drop(tcx, env, ty, true, 0, &mut seen, &mut found);
    found
}

fn walk_drop<'tcx>(
    tcx: TyCtxt<'tcx>,
    env: TypingEnv<'tcx>,
    ty: Ty<'tcx>,
    sure: bool,
    depth: usize,
    seen: &mut FxHashSet<Ty<'tcx>>,
    found: &mut Vec<(DefId, bool)>,
) {
    const MAX_DEPTH: usize = 8;
    if depth > MAX_DEPTH || !seen.insert(ty) || !ty.needs_drop(tcx, env) {
        return;
    }
    let mut next = |t: Ty<'tcx>, sure: bool, found: &mut Vec<(DefId, bool)>| {
        walk_drop(tcx, env, t, sure, depth + 1, seen, found);
    };
    match *ty.kind() {
        ty::Adt(adt, args) => {
            if adt.is_manually_drop() {
                return;
            }
            if let Some(dtor) = adt.destructor(tcx) {
                match found.iter_mut().find(|(d, _)| *d == dtor.did) {
                    Some((_, was)) => *was |= sure,
                    None => found.push((dtor.did, sure)),
                }
            }
            if adt.did().is_local() {
                let sure = sure && adt.variants().len() == 1;
                for field in adt.all_fields() {
                    let field_ty = field.ty(tcx, args).skip_norm_wip();
                    let field_ty = tcx
                        .try_normalize_erasing_regions(env, ty::Unnormalized::new_wip(field_ty))
                        .unwrap_or(field_ty);
                    next(field_ty, sure, found);
                }
            } else {
                for arg in args.types() {
                    next(arg, false, found);
                }
            }
        }
        ty::Tuple(items) => {
            for item in items {
                next(item, sure, found);
            }
        }
        ty::Array(elem, _) | ty::Slice(elem) => next(elem, false, found),
        ty::Closure(_, args) => {
            for upvar in args.as_closure().upvar_tys() {
                next(upvar, sure, found);
            }
        }
        _ => {}
    }
}
