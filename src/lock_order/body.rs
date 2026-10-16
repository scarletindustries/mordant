//! One body's locks: where each is taken, which blocks run while it is
//! held, and what the body does to its callers' locks (returns a guard,
//! leaves a lock held, or unlocks one it did not take).

use std::cell::RefCell;

use rustc_data_structures::fx::{FxHashMap, FxHashSet};
use rustc_hir::def_id::{DefId, LocalDefId};
use rustc_index::bit_set::DenseBitSet;
use rustc_middle::mir::{
    BasicBlock, Body, Local, Operand, RETURN_PLACE, Rvalue, StatementKind, TerminatorKind,
};
use rustc_middle::ty::{self, Ty, TyCtxt, TypingEnv};
use rustc_span::Span;

use super::place::{LockPlace, Root, Tracer};
use crate::call_graph::{CallGraph, resolve_item};
use crate::hir_shapes::{def_path_names_in, path_matches};

/// The `lock-*` keys of `mordant.toml`.
pub(super) struct Config {
    pub(super) guard_types: Vec<String>,
    pub(super) lock_methods: Vec<String>,
    pub(super) unlock_methods: Vec<String>,
    pub(super) reentrant_types: Vec<String>,
}

/// How a lock is taken.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(super) struct Mode {
    /// A read lock: two of them on one lock do not wait on each other.
    pub(super) shared: bool,
    /// The same thread may take it again.
    pub(super) reentrant: bool,
    /// The call waits for the lock, unlike `try_lock`.
    pub(super) blocking: bool,
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub(super) struct Taken {
    pub(super) place: LockPlace,
    pub(super) mode: Mode,
}

/// What a fn does to locks its caller can see.
#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub(super) struct Prims {
    /// It returns a guard for this lock.
    pub(super) returns: Option<Taken>,
    /// It takes these locks by a lock method and returns with them held.
    pub(super) leaves_held: Vec<Taken>,
    /// It unlocks these locks, which it did not take.
    pub(super) releases: Vec<LockPlace>,
}

/// One acquisition in a body.
pub(super) struct Acq {
    pub(super) block: BasicBlock,
    /// The type the lock call was made on, peeled of pointers.
    pub(super) lock_ty: Option<DefId>,
    pub(super) span: Span,
    pub(super) taken: Taken,
    /// The blocks whose terminator runs while the lock is surely held.
    pub(super) held: DenseBitSet<BasicBlock>,
}

/// One body's locks.
pub(super) struct Facts {
    pub(super) acqs: Vec<Acq>,
    /// For each call or drop, the place each argument points to.
    pub(super) args: FxHashMap<BasicBlock, Vec<LockPlace>>,
    pub(super) prims: Prims,
    /// Callers see its locks through `prims` and must not walk into it.
    pub(super) primitive: bool,
}

/// Names and caches shared by every body.
pub(super) struct Kinds<'c, 'tcx> {
    pub(super) tcx: TyCtxt<'tcx>,
    pub(super) config: &'c Config,
    guards: RefCell<FxHashMap<DefId, Option<Mode>>>,
}

/// Lock guards from std and the common lock crates, by type name.
const STD_GUARDS: &[&str] = &[
    "MutexGuard",
    "MappedMutexGuard",
    "RwLockReadGuard",
    "RwLockWriteGuard",
    "MappedRwLockReadGuard",
    "MappedRwLockWriteGuard",
    "RwLockUpgradableReadGuard",
    "ReentrantLockGuard",
    "ReentrantMutexGuard",
];
const LOCK_CRATES: &[&str] = &["std", "lock_api", "parking_lot", "spin"];

impl<'c, 'tcx> Kinds<'c, 'tcx> {
    pub(super) fn new(tcx: TyCtxt<'tcx>, config: &'c Config) -> Self {
        Self {
            tcx,
            config,
            guards: RefCell::default(),
        }
    }

    fn listed(&self, def: DefId, list: &[String]) -> bool {
        !list.is_empty()
            && def_path_names_in(self.tcx, def)
                .iter()
                .any(|n| list.iter().any(|p| path_matches(n, p)))
    }

    /// The mode of a guard type, or `None` when `def` is not a guard.
    fn guard(&self, def: DefId) -> Option<Mode> {
        if let Some(known) = self.guards.borrow().get(&def) {
            return *known;
        }
        let tcx = self.tcx;
        let name = tcx.item_name(def);
        let name = name.as_str();
        let krate = tcx.crate_name(def.krate);
        #[allow(
            deprecated,
            reason = "the attribute is a tool attribute with no parsed form"
        )]
        let marked = clippy_utils::attrs::get_builtin_attr(
            tcx.get_all_attrs(def),
            clippy_utils::sym::has_significant_drop,
        )
        .next()
        .is_some();
        let is_guard = (LOCK_CRATES.contains(&krate.as_str()) && STD_GUARDS.contains(&name))
            || marked
            || self.listed(def, &self.config.guard_types);
        let mode = is_guard.then(|| Mode {
            shared: name.contains("Read"),
            reentrant: name.starts_with("Reentrant")
                || self.listed(def, &self.config.reentrant_types),
            blocking: true,
        });
        self.guards.borrow_mut().insert(def, mode);
        mode
    }

    /// The guard a value of type `ty` owns, looking through enums, tuples
    /// and the type arguments of other types, but not through pointers.
    pub(super) fn owned_guard(&self, ty: Ty<'tcx>) -> Option<Mode> {
        self.owned_guard_at(ty, 0)
    }

    fn owned_guard_at(&self, ty: Ty<'tcx>, depth: usize) -> Option<Mode> {
        if depth > 4 {
            return None;
        }
        match ty.kind() {
            ty::Adt(adt, args) => self
                .guard(adt.did())
                .or_else(|| args.types().find_map(|t| self.owned_guard_at(t, depth + 1))),
            ty::Tuple(items) => items.iter().find_map(|t| self.owned_guard_at(t, depth + 1)),
            ty::Array(elem, _) => self.owned_guard_at(*elem, depth + 1),
            _ => None,
        }
    }

    fn reentrant_type(&self, ty: Ty<'tcx>) -> bool {
        match peel(ty).kind() {
            ty::Adt(adt, _) => {
                self.tcx.item_name(adt.did()).as_str().contains("Reentrant")
                    || self.listed(adt.did(), &self.config.reentrant_types)
            }
            _ => false,
        }
    }
}

/// `ty` with references and raw pointers taken off.
fn peel(mut ty: Ty<'_>) -> Ty<'_> {
    while let Some(inner) = ty.builtin_deref(true) {
        ty = inner;
    }
    ty
}

/// What one call terminator does to locks, before regions are known.
enum CallKind {
    /// The lock taken, whether by a lock method, and the lock's type.
    Acquire(Taken, bool, Option<DefId>),
    Release(LockPlace),
    Other,
}

/// An acquisition before its region is known: block, span, the lock, whether
/// a lock method took it, the local the guard lands in, and the lock's type.
type Found = (BasicBlock, Span, Taken, bool, Option<Local>, Option<DefId>);

/// Reads one body. `prims` holds what is known of local callees.
pub(super) fn scan<'tcx>(
    kinds: &Kinds<'_, 'tcx>,
    graph: &CallGraph,
    def: LocalDefId,
    body: &Body<'tcx>,
    prims: &FxHashMap<LocalDefId, Prims>,
    accessors: &FxHashMap<LocalDefId, LockPlace>,
) -> Facts {
    let tcx = kinds.tcx;
    let env = TypingEnv::post_analysis(tcx, def);
    let tracer = Tracer::new(tcx, body, env, accessors);
    let mut args_of: FxHashMap<BasicBlock, Vec<LockPlace>> = FxHashMap::default();
    let mut found: Vec<Found> = Vec::new();
    let mut releases: Vec<(BasicBlock, LockPlace)> = Vec::new();
    for (block, data) in body.basic_blocks.iter_enumerated() {
        if data.is_cleanup {
            continue;
        }
        let terminator = data.terminator();
        let span = terminator.source_info.span;
        match &terminator.kind {
            TerminatorKind::Call {
                func,
                args,
                destination,
                target,
                ..
            } => {
                let places: Vec<LockPlace> = args.iter().map(|a| tracer.pointee(&a.node)).collect();
                let kind = classify(kinds, env, body, func, args, *destination, &places, prims);
                args_of.insert(block, places);
                match kind {
                    CallKind::Acquire(taken, raw, lock_ty) if target.is_some() => {
                        let dest = destination.as_local().filter(|_| !raw);
                        found.push((block, span, taken, raw, dest, lock_ty));
                    }
                    CallKind::Acquire(..) | CallKind::Other => {}
                    CallKind::Release(place) => releases.push((block, place)),
                }
            }
            TerminatorKind::Drop { place, .. } => {
                args_of.insert(block, vec![tracer.place(*place)]);
            }
            _ => {}
        }
    }
    // A lock method's acquisition counts only when every unlock that might
    // be of the same lock is surely of it or surely not.
    let unlock_unseen = unlocks_in_closure(kinds, graph, def);
    let unsure = |taken: &LockPlace| {
        releases.iter().any(|(_, r)| {
            let same_field = match (r.steps.last(), taken.steps.last()) {
                (Some(a), Some(b)) => a.adt == b.adt && a.field == b.field,
                _ => true,
            };
            same_field && !r.same_lock(taken)
        })
    };
    let doms = body.basic_blocks.dominators();
    let mut acqs = Vec::new();
    let mut returns = None;
    let mut leaves_held = Vec::new();
    for (block, span, taken, raw, dest, lock_ty) in found {
        let Some(target) = body.basic_blocks[block].terminator().successors().next() else {
            continue;
        };
        let ends: Vec<BasicBlock> = if raw {
            if unlock_unseen || !taken.place.exact || unsure(&taken.place) {
                continue;
            }
            releases
                .iter()
                .filter(|(_, p)| p.same_lock(&taken.place))
                .map(|(b, _)| *b)
                .collect()
        } else {
            let Some(dest) = dest else { continue };
            let (ends, returned) = guard_ends(kinds, body, dest);
            if returned && returns.is_none() {
                returns = Some(param_relative(&taken));
            }
            ends
        };
        let held = region(body, doms, block, target, &ends);
        if raw && param_or_static(&taken.place) && returns_while(body, &held) {
            leaves_held.push(taken.clone());
        }
        acqs.push(Acq {
            block,
            lock_ty,
            span,
            taken,
            held,
        });
    }
    let own: Vec<&LockPlace> = acqs.iter().map(|a| &a.taken.place).collect();
    let released: Vec<LockPlace> = releases
        .into_iter()
        .map(|(_, p)| p)
        .filter(|p| param_or_static(p) && !own.iter().any(|o| o.same_lock(p)))
        .collect();
    let prims = Prims {
        returns,
        leaves_held,
        releases: released,
    };
    let primitive = prims != Prims::default()
        || kinds
            .owned_guard(body.local_decls[RETURN_PLACE].ty)
            .is_some();
    Facts {
        acqs,
        args: args_of,
        prims,
        primitive,
    }
}

/// Whether a call takes a lock, releases one, or neither.
#[allow(clippy::too_many_arguments, reason = "one call's parts, read once")]
fn classify<'tcx>(
    kinds: &Kinds<'_, 'tcx>,
    env: TypingEnv<'tcx>,
    body: &Body<'tcx>,
    func: &Operand<'tcx>,
    args: &[rustc_span::Spanned<Operand<'tcx>>],
    destination: rustc_middle::mir::Place<'tcx>,
    places: &[LockPlace],
    prims: &FxHashMap<LocalDefId, Prims>,
) -> CallKind {
    let tcx = kinds.tcx;
    let Some((named, generic_args)) = func.const_fn_def() else {
        return CallKind::Other;
    };
    let callee = resolve_item(tcx, env, named, generic_args).unwrap_or(named);
    let local = callee.as_local().and_then(|l| prims.get(&l));
    let receiver = places.first().cloned().unwrap_or_else(LockPlace::opaque);
    let receiver_ty = args
        .first()
        .map(|a| peel(a.node.ty(&body.local_decls, tcx)));
    let receiver_adt = receiver_ty
        .and_then(|t| t.ty_adt_def())
        .map(|adt| adt.did());
    // A local fn that locks a field of its receiver makes the receiver a
    // lock type only when the receiver is a newtype around that lock.
    let wrapper = receiver_adt.filter(|&d| {
        let adt = tcx.adt_def(d);
        adt.is_struct() && adt.non_enum_variant().fields.len() == 1
    });
    let name = tcx.item_name(named);
    let blocking = !name.as_str().starts_with("try_");
    let reentrant = receiver_ty.is_some_and(|t| kinds.reentrant_type(t));
    if let Some(mode) = kinds.owned_guard(destination.ty(&body.local_decls, tcx).ty) {
        // A call that is handed a guard (`unwrap`, `Condvar::wait`) or a
        // pointer to one (`Option::take`) passes it on: it takes no lock.
        let moves_guard = args.iter().any(|a| {
            let ty = a.node.ty(&body.local_decls, tcx);
            kinds.owned_guard(peel(ty)).is_some()
        });
        if moves_guard {
            return CallKind::Other;
        }
        let (place, mode, lock_ty) = match local.and_then(|p| p.returns.as_ref()) {
            Some(ret) => (ret.place.in_caller(places), ret.mode, wrapper),
            None => (receiver, mode, receiver_adt),
        };
        let mode = Mode {
            reentrant: mode.reentrant || reentrant,
            blocking: mode.blocking && blocking,
            ..mode
        };
        return CallKind::Acquire(Taken { place, mode }, false, lock_ty);
    }
    if let Some(p) = local {
        if let Some(held) = p.leaves_held.first() {
            let taken = Taken {
                place: held.place.in_caller(places),
                mode: held.mode,
            };
            return CallKind::Acquire(taken, true, wrapper);
        }
        if let Some(released) = p.releases.first() {
            return CallKind::Release(released.in_caller(places));
        }
    }
    let config = kinds.config;
    if kinds.listed(named, &config.lock_methods) || kinds.listed(callee, &config.lock_methods) {
        let lower = name.as_str().to_ascii_lowercase();
        let mode = Mode {
            shared: lower.contains("shared") || lower.contains("read"),
            reentrant,
            blocking,
        };
        let taken = Taken {
            place: receiver,
            mode,
        };
        return CallKind::Acquire(taken, true, receiver_adt);
    }
    if kinds.listed(named, &config.unlock_methods) || kinds.listed(callee, &config.unlock_methods) {
        return CallKind::Release(receiver);
    }
    CallKind::Other
}

/// A closure built in `def` calls an unlock method itself, so a lock may
/// be released where this body cannot see it.
fn unlocks_in_closure(kinds: &Kinds<'_, '_>, graph: &CallGraph, def: LocalDefId) -> bool {
    let unlocks = &kinds.config.unlock_methods;
    if unlocks.is_empty() {
        return false;
    }
    graph
        .edges(def)
        .iter()
        .filter(|e| e.kind == crate::call_graph::EdgeKind::Closure)
        .filter_map(|e| e.local_callee())
        .any(|closure| {
            graph
                .edges(closure)
                .iter()
                .any(|e| kinds.listed(e.named, unlocks) || kinds.listed(e.callee, unlocks))
        })
}

/// The blocks that end a guard's life, and whether the body returns it.
/// The guard moves from `dest` into other locals. It ends where one of them
/// is dropped or passed to a call that does not hand a guard back.
fn guard_ends<'tcx>(
    kinds: &Kinds<'_, 'tcx>,
    body: &Body<'tcx>,
    dest: Local,
) -> (Vec<BasicBlock>, bool) {
    let tcx = kinds.tcx;
    let mut holders: FxHashSet<Local> = FxHashSet::from_iter([dest]);
    // Guards are not `Copy`; a copy reads some other part of a holder.
    let moved = |op: &Operand<'_>, holders: &FxHashSet<Local>| matches!(op, Operand::Move(p) if holders.contains(&p.local));
    loop {
        let before = holders.len();
        for data in body.basic_blocks.iter() {
            for statement in &data.statements {
                let StatementKind::Assign(assign) = &statement.kind else {
                    continue;
                };
                let (place, rvalue) = &**assign;
                if place.is_indirect() {
                    continue;
                }
                let takes = match rvalue {
                    Rvalue::Use(op, _) | Rvalue::Cast(_, op, _) => moved(op, &holders),
                    Rvalue::Aggregate(_, ops) => ops.iter().any(|op| moved(op, &holders)),
                    _ => false,
                };
                if takes {
                    holders.insert(place.local);
                }
            }
            if let TerminatorKind::Call {
                args, destination, ..
            } = &data.terminator().kind
                && args.iter().any(|a| moved(&a.node, &holders))
                && kinds
                    .owned_guard(destination.ty(&body.local_decls, tcx).ty)
                    .is_some()
                && !destination.is_indirect()
            {
                holders.insert(destination.local);
            }
        }
        if holders.len() == before {
            break;
        }
    }
    let mut ends = Vec::new();
    for (block, data) in body.basic_blocks.iter_enumerated() {
        if data.is_cleanup {
            continue;
        }
        let ends_here = match &data.terminator().kind {
            TerminatorKind::Drop { place, .. } => holders.contains(&place.local),
            TerminatorKind::Call {
                args, destination, ..
            } => {
                args.iter()
                    .any(|a| matches!(&a.node, Operand::Move(p) if holders.contains(&p.local)))
                    && !holders.contains(&destination.local)
            }
            _ => false,
        };
        if ends_here {
            ends.push(block);
        }
    }
    (ends, holders.contains(&RETURN_PLACE))
}

/// The blocks that run while a lock taken at `block`'s terminator is held.
/// Every path to such a block passes `target`, the block after the
/// acquisition, and no end of the lock reaches it without passing the
/// acquisition again. The end blocks are left out, since they release it.
fn region(
    body: &Body<'_>,
    doms: &rustc_data_structures::graph::dominators::Dominators<BasicBlock>,
    block: BasicBlock,
    target: BasicBlock,
    ends: &[BasicBlock],
) -> DenseBitSet<BasicBlock> {
    let blocks = &body.basic_blocks;
    let mut after_end = DenseBitSet::new_empty(blocks.len());
    let mut stack: Vec<BasicBlock> = Vec::new();
    let normal_succs = |b: BasicBlock| {
        blocks[b]
            .terminator()
            .successors()
            .filter(|s| !blocks[*s].is_cleanup)
    };
    for &end in ends {
        stack.extend(normal_succs(end));
    }
    while let Some(b) = stack.pop() {
        if !after_end.insert(b) || b == block {
            continue;
        }
        stack.extend(normal_succs(b));
    }
    let mut held = DenseBitSet::new_empty(blocks.len());
    if !doms.is_reachable(target) {
        return held;
    }
    for (b, data) in blocks.iter_enumerated() {
        if data.is_cleanup || !doms.is_reachable(b) {
            continue;
        }
        if doms.dominates(target, b) && !after_end.contains(b) && !ends.contains(&b) {
            held.insert(b);
        }
    }
    held
}

fn param_or_static(place: &LockPlace) -> bool {
    place.exact && matches!(place.root, Root::Param(_) | Root::Static(_))
}

/// `taken` as a caller can use it: its own locals mean nothing there.
fn param_relative(taken: &Taken) -> Taken {
    if param_or_static(&taken.place) {
        return taken.clone();
    }
    Taken {
        place: LockPlace {
            root: Root::Opaque,
            steps: taken.place.steps.clone(),
            exact: false,
        },
        mode: taken.mode,
    }
}

/// Some `return` runs while the lock is held.
fn returns_while(body: &Body<'_>, held: &DenseBitSet<BasicBlock>) -> bool {
    held.iter().any(|b| {
        matches!(
            body.basic_blocks[b].terminator().kind,
            TerminatorKind::Return
        )
    })
}

/// Bodies whose locks are not worth reading: derives and external macros.
pub(super) fn skipped(tcx: TyCtxt<'_>, def: LocalDefId) -> bool {
    let hir_id = tcx.local_def_id_to_hir_id(def);
    tcx.def_span(def).in_external_macro(tcx.sess.source_map())
        || clippy_utils::in_automatically_derived(tcx, hir_id)
}
