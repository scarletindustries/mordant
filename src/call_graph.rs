//! The calls between this crate's functions, read off MIR. A call goes in
//! only when it resolves to one local function body. Calls through `dyn`,
//! function pointers, unresolved trait methods and other crates are left out.

use std::collections::VecDeque;
use std::collections::hash_map::Entry;

use rustc_data_structures::fx::{FxHashMap, FxHashSet, FxIndexMap};
use rustc_hir::ConstContext;
use rustc_hir::def_id::LocalDefId;
use rustc_middle::mir::TerminatorKind;
use rustc_middle::ty::{self, TyCtxt};
use rustc_span::Span;

use crate::mir_flow::mir_for;

/// One call in MIR: the body it is in, the local fn it runs, and where.
#[derive(Clone, Copy)]
pub(crate) struct Call {
    pub(crate) caller: LocalDefId,
    pub(crate) callee: LocalDefId,
    pub(crate) span: Span,
}

/// Every resolved call, grouped by caller, in body order. Bodies run at
/// compile time (consts, statics) are skipped, since their calls are not in
/// the binary.
pub(crate) struct CallGraph {
    pub(crate) out: FxIndexMap<LocalDefId, Vec<Call>>,
}

impl CallGraph {
    pub(crate) fn build(tcx: TyCtxt<'_>) -> Self {
        let mut out: FxIndexMap<LocalDefId, Vec<Call>> = FxIndexMap::default();
        for owner in tcx.hir_body_owners() {
            if let Some(ConstContext::Const { .. } | ConstContext::Static(_)) =
                tcx.hir_body_const_context(owner)
            {
                continue;
            }
            let Some(body) = mir_for(tcx, owner) else {
                continue;
            };
            let env = ty::TypingEnv::post_analysis(tcx, owner);
            let mut calls = Vec::new();
            let mut seen: FxHashSet<Span> = FxHashSet::default();
            for data in body.basic_blocks.iter() {
                let Some(terminator) = &data.terminator else {
                    continue;
                };
                let (TerminatorKind::Call { func, .. } | TerminatorKind::TailCall { func, .. }) =
                    &terminator.kind
                else {
                    continue;
                };
                let Some((def, args)) = func.const_fn_def() else {
                    continue;
                };
                let Some(callee) = resolve(tcx, env, def, args) else {
                    continue;
                };
                let span = terminator.source_info.span;
                if seen.insert(span) {
                    calls.push(Call {
                        caller: owner,
                        callee,
                        span,
                    });
                }
            }
            out.insert(owner, calls);
        }
        Self { out }
    }

    /// The calls to each local fn, in the order `build` found them.
    pub(crate) fn callers(&self) -> FxHashMap<LocalDefId, Vec<Call>> {
        let mut into: FxHashMap<LocalDefId, Vec<Call>> = FxHashMap::default();
        for call in self.out.values().flatten() {
            into.entry(call.callee).or_default().push(*call);
        }
        into
    }

    /// The shortest chain of calls that leads from `start` back to `start`,
    /// or `None` when no call path returns to it.
    pub(crate) fn cycle_through(&self, start: LocalDefId) -> Option<Vec<Call>> {
        let mut parent: FxHashMap<LocalDefId, Call> = FxHashMap::default();
        let mut queue = VecDeque::from([start]);
        while let Some(cur) = queue.pop_front() {
            for call in self.out.get(&cur).into_iter().flatten() {
                if call.callee == start {
                    let mut chain = vec![*call];
                    let mut at = cur;
                    while at != start {
                        let step = parent[&at];
                        chain.push(step);
                        at = step.caller;
                    }
                    chain.reverse();
                    return Some(chain);
                }
                if let Entry::Vacant(slot) = parent.entry(call.callee) {
                    slot.insert(*call);
                    queue.push_back(call.callee);
                }
            }
        }
        None
    }
}

/// The local fn body a call runs, if the call names one. A trait method
/// whose impl depends on a type parameter does not resolve here.
fn resolve<'tcx>(
    tcx: TyCtxt<'tcx>,
    env: ty::TypingEnv<'tcx>,
    def: rustc_hir::def_id::DefId,
    args: ty::GenericArgsRef<'tcx>,
) -> Option<LocalDefId> {
    let instance = ty::Instance::try_resolve(tcx, env, def, args).ok()??;
    let ty::InstanceKind::Item(item) = instance.def else {
        return None;
    };
    let item = item.as_local()?;
    tcx.def_kind(item).is_fn_like().then_some(item)
}
