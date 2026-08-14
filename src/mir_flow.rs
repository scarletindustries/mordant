//! MIR machinery shared by the body-level analyses, with no opinion about
//! what any of it means for a lint.
//!
//! It answers five questions about a body:
//!
//! * which MIR to read at all (`mir_for`: the pre-optimization body, so `?`
//!   is still a `Try::branch` call and every aggregate is intact);
//! * what a place names, as an [`Atom`] -- a local plus its leading field
//!   path -- and how faithfully (`place_info`, [`Exactness`]);
//! * which branches a block is control-dependent on (`build_cfg`,
//!   `post_dominators`, `control_deps`), over a CFG in which blocks that
//!   cannot reach a `return` do not exist, so nothing is "decided" by an
//!   `assert!`;
//! * whether every path to a block passes through another (`dominates`).
//!   This is a different relation from control dependence: a clamp's branch
//!   dominates the use after it without deciding whether the use runs;
//! * what a branch switches on (`switch_operand_atoms`).
//!
//! What the answers mean is the caller's business: `ctor_flow` combines them
//! into "does a failure exit depend on a stored field", `unchecked_input_len`
//! asks whether a comparison dominates a use, `variant_flow` uses only
//! `mir_for` and traces returned variants its own way.

use std::collections::{HashSet, VecDeque};

use rustc_hir::def_id::LocalDefId;
use rustc_middle::mir::{BasicBlock, Body, Local, Operand, Place, ProjectionElem, TerminatorKind};
use rustc_middle::ty::TyCtxt;

// ── MIR access ───────────────────────────────────────────────────────────────

pub(crate) fn mir_for<'tcx>(tcx: TyCtxt<'tcx>, def: LocalDefId) -> Option<MirRef<'tcx>> {
    if !tcx.def_kind(def).is_fn_like() || !tcx.is_mir_available(def.to_def_id()) {
        return None;
    }
    // The pre-optimization body keeps `?` as `Try::branch` calls and every
    // aggregate intact regardless of the build's opt level. It is stolen once
    // `optimized_mir` runs, which nothing before codegen asks for; fall back
    // if some other driver did.
    let steal = tcx.mir_drops_elaborated_and_const_checked(def);
    if steal.is_stolen() {
        Some(MirRef::Opt(tcx.optimized_mir(def.to_def_id())))
    } else {
        Some(MirRef::Steal(steal.borrow()))
    }
}

pub(crate) enum MirRef<'tcx> {
    Steal(rustc_data_structures::sync::MappedReadGuard<'tcx, Body<'tcx>>),
    Opt(&'tcx Body<'tcx>),
}

impl<'tcx> std::ops::Deref for MirRef<'tcx> {
    type Target = Body<'tcx>;
    fn deref(&self) -> &Body<'tcx> {
        match self {
            MirRef::Steal(g) => g,
            MirRef::Opt(b) => b,
        }
    }
}

// ── places as atoms ──────────────────────────────────────────────────────────

/// A local plus the leading run of field projections: `_3.1.0`. Anything past
/// the first deref/index/downcast is folded into the prefix before it.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub(crate) struct Atom {
    pub(crate) local: Local,
    pub(crate) path: Vec<u32>,
}

/// Path component for "some element of": indexing conflates positions but
/// keeps the container distinct from its elements, so a length check on a
/// slice is not a check on the element later stored out of it.
pub(crate) const ANY_ELEM: u32 = u32::MAX;

impl Atom {
    pub(crate) fn whole(local: Local) -> Self {
        Atom {
            local,
            path: Vec::new(),
        }
    }
    pub(crate) fn extended(&self, tail: &[u32]) -> Self {
        // `node = node.next` in a loop composes without bound; past a few
        // levels the distinction stops mattering, so the path saturates.
        const MAX_PATH: usize = 6;
        let mut path = self.path.clone();
        let room = MAX_PATH.saturating_sub(path.len());
        path.extend_from_slice(&tail[..tail.len().min(room)]);
        Atom {
            local: self.local,
            path,
        }
    }
    pub(crate) fn overlaps(&self, other: &Atom) -> bool {
        self.local == other.local && self.path.iter().zip(&other.path).all(|(a, b)| a == b)
    }
    /// `self` (a decision atom) reads `stored` or a part of it. The reverse,
    /// a decision on the whole of something only part of which is stored
    /// (`lexer.next()?` then `log: lexer.log`), is not evidence about the part.
    pub(crate) fn inspects(&self, stored: &Atom) -> bool {
        self.local == stored.local && is_prefix(&stored.path, &self.path)
    }
}

/// How well `PlaceInfo::atom` names what the projection actually read.
///
/// A `Downcast` is absorbing: it ends the exact field path and no later
/// projection puts it back, so "payload" and "exact" cannot hold at once and
/// every caller below branches payload-first. Three states, not four.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Exactness {
    /// Pure field access: the atom is exact.
    Exact,
    /// A `Downcast` appeared: this reads a variant payload of the atom.
    VariantPayload,
    /// Some other projection: the atom names more than was read.
    Inexact,
}

impl Exactness {
    /// A projection this does not model. It can only lose precision, and it
    /// cannot un-see a `Downcast`.
    fn blur(self) -> Self {
        match self {
            Exactness::Exact | Exactness::Inexact => Exactness::Inexact,
            Exactness::VariantPayload => Exactness::VariantPayload,
        }
    }
}

pub(crate) struct PlaceInfo {
    pub(crate) atom: Atom,
    pub(crate) exactness: Exactness,
    pub(crate) index_locals: Vec<Local>,
}

pub(crate) fn place_info(place: Place<'_>) -> PlaceInfo {
    let mut path = Vec::new();
    let mut exactness = Exactness::Exact;
    let mut index_locals = Vec::new();
    for elem in place.projection.iter() {
        match elem {
            ProjectionElem::Field(f, _) if exactness == Exactness::Exact => path.push(f.as_u32()),
            ProjectionElem::Field(..) => {}
            // `(*r).f`: the reference is the value for slicing purposes, so
            // a deref neither ends the field path nor makes it inexact.
            ProjectionElem::Deref => {}
            ProjectionElem::Downcast(..) => exactness = Exactness::VariantPayload,
            ProjectionElem::Index(v) => {
                index_locals.push(v);
                if exactness == Exactness::Exact {
                    path.push(ANY_ELEM);
                }
            }
            ProjectionElem::ConstantIndex { .. } | ProjectionElem::Subslice { .. }
                if exactness == Exactness::Exact =>
            {
                path.push(ANY_ELEM);
            }
            _ => exactness = exactness.blur(),
        }
    }
    PlaceInfo {
        atom: Atom {
            local: place.local,
            path,
        },
        exactness,
        index_locals,
    }
}

pub(crate) fn operand_place<'tcx>(op: &Operand<'tcx>) -> Option<Place<'tcx>> {
    match op {
        Operand::Copy(p) | Operand::Move(p) => Some(*p),
        Operand::Constant(_) | Operand::RuntimeChecks(_) => None,
    }
}

pub(crate) fn is_prefix(a: &[u32], b: &[u32]) -> bool {
    a.len() <= b.len() && a.iter().zip(b).all(|(x, y)| x == y)
}

// ── control dependence ───────────────────────────────────────────────────────

/// The body's CFG over normal (non-unwind) edges, with every block that
/// cannot reach a `return` pruned: a panic, abort or `unreachable!()` is
/// "does not happen" here, otherwise everything after `assert!(x)` would be
/// control-dependent on `x`.
pub(crate) struct Cfg {
    succs: Vec<Vec<usize>>,
    /// Index of the virtual exit node.
    exit: usize,
}

fn raw_successors(body: &Body<'_>, bb: BasicBlock) -> Vec<BasicBlock> {
    let Some(term) = &body.basic_blocks[bb].terminator else {
        return Vec::new();
    };
    let mut v = match &term.kind {
        TerminatorKind::Goto { target } => vec![*target],
        TerminatorKind::SwitchInt { targets, .. } => targets.all_targets().to_vec(),
        TerminatorKind::Drop { target, .. } | TerminatorKind::Assert { target, .. } => {
            vec![*target]
        }
        TerminatorKind::Call { target, .. } => target.iter().copied().collect(),
        TerminatorKind::Yield { resume, .. } => vec![*resume],
        TerminatorKind::FalseEdge { real_target, .. }
        | TerminatorKind::FalseUnwind { real_target, .. } => {
            vec![*real_target]
        }
        TerminatorKind::InlineAsm { targets, .. } => targets.to_vec(),
        TerminatorKind::Return
        | TerminatorKind::Unreachable
        | TerminatorKind::UnwindResume
        | TerminatorKind::UnwindTerminate(_)
        | TerminatorKind::CoroutineDrop
        | TerminatorKind::TailCall { .. } => Vec::new(),
    };
    v.retain(|b| !body.basic_blocks[*b].is_cleanup);
    v.sort();
    v.dedup();
    v
}

pub(crate) fn build_cfg(body: &Body<'_>) -> Cfg {
    let n = body.basic_blocks.len();
    let raw: Vec<Vec<usize>> = (0..n)
        .map(|i| {
            let bb = BasicBlock::from_usize(i);
            if body.basic_blocks[bb].is_cleanup {
                Vec::new()
            } else {
                raw_successors(body, bb)
                    .into_iter()
                    .map(|b| b.as_usize())
                    .collect()
            }
        })
        .collect();
    let mut can_return: Vec<bool> = (0..n)
        .map(|i| {
            matches!(
                body.basic_blocks[BasicBlock::from_usize(i)]
                    .terminator
                    .as_ref()
                    .map(|t| &t.kind),
                Some(TerminatorKind::Return | TerminatorKind::TailCall { .. })
            )
        })
        .collect();
    let mut changed = true;
    while changed {
        changed = false;
        for b in 0..n {
            if !can_return[b] && raw[b].iter().any(|s| can_return[*s]) {
                can_return[b] = true;
                changed = true;
            }
        }
    }
    let succs = (0..n)
        .map(|b| {
            let live: Vec<usize> = raw[b].iter().copied().filter(|s| can_return[*s]).collect();
            if live.is_empty() { vec![n] } else { live }
        })
        .collect();
    Cfg { succs, exit: n }
}

pub(crate) struct Bits(Vec<u64>);
impl Bits {
    fn full(n: usize) -> Self {
        let mut v = vec![!0u64; n.div_ceil(64)];
        if !n.is_multiple_of(64) {
            *v.last_mut().unwrap() = (1u64 << (n % 64)) - 1;
        }
        Bits(v)
    }
    fn empty(n: usize) -> Self {
        Bits(vec![0; n.div_ceil(64)])
    }
    fn set(&mut self, i: usize) {
        self.0[i / 64] |= 1 << (i % 64);
    }
    fn has(&self, i: usize) -> bool {
        self.0[i / 64] & (1 << (i % 64)) != 0
    }
    fn and_assign(&mut self, o: &Bits) {
        for (a, b) in self.0.iter_mut().zip(&o.0) {
            *a &= *b;
        }
    }
}

/// Post-dominator sets over blocks plus the virtual exit.
pub(crate) fn post_dominators(cfg: &Cfg) -> Vec<Bits> {
    let n = cfg.exit;
    let mut pdom: Vec<Bits> = (0..=n).map(|_| Bits::full(n + 1)).collect();
    pdom[n] = Bits::empty(n + 1);
    pdom[n].set(n);
    let mut changed = true;
    while changed {
        changed = false;
        for b in 0..n {
            let mut acc = Bits::full(n + 1);
            for &s in &cfg.succs[b] {
                acc.and_assign(&pdom[s]);
            }
            acc.set(b);
            if acc.0 != pdom[b].0 {
                pdom[b] = acc;
                changed = true;
            }
        }
    }
    pdom
}

/// Branches (`t` not post-dominating them, but post-dominating one of their
/// successors) whose outcome decides whether `t` runs, in block order.
fn direct_deps(cfg: &Cfg, pdom: &[Bits], t: usize) -> Vec<usize> {
    (0..cfg.exit)
        .filter(|a| {
            let strictly = pdom[*a].has(t) && *a != t;
            cfg.succs[*a].len() >= 2 && !strictly && cfg.succs[*a].iter().any(|s| pdom[*s].has(t))
        })
        .collect()
}

/// Branch blocks that `target` is directly control-dependent on: the ones
/// whose outcome sends control to `target` or away from it. A branch that
/// only decides whether one of those is reached (an early `return Ok` guard
/// in front of it) is not among them; `control_deps` has those too.
pub(crate) fn direct_control_deps(cfg: &Cfg, pdom: &[Bits], target: BasicBlock) -> Vec<BasicBlock> {
    direct_deps(cfg, pdom, target.as_usize())
        .into_iter()
        .map(BasicBlock::from_usize)
        .collect()
}

/// Branch blocks that `target` is transitively control-dependent on,
/// innermost first.
pub(crate) fn control_deps(cfg: &Cfg, pdom: &[Bits], target: BasicBlock) -> Vec<BasicBlock> {
    let mut seen = HashSet::new();
    let mut q = VecDeque::from([target.as_usize()]);
    let mut deps = Vec::new();
    while let Some(t) = q.pop_front() {
        for a in direct_deps(cfg, pdom, t) {
            if seen.insert(a) {
                deps.push(BasicBlock::from_usize(a));
                q.push_back(a);
            }
        }
    }
    deps
}

pub(crate) fn switch_operand_atoms(body: &Body<'_>, bb: BasicBlock) -> Vec<Atom> {
    match &body.basic_blocks[bb].terminator().kind {
        TerminatorKind::SwitchInt { discr, .. } => operand_place(discr)
            .map(|p| {
                let info = place_info(p);
                let mut v: Vec<Atom> = info.index_locals.into_iter().map(Atom::whole).collect();
                v.push(info.atom);
                v
            })
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

// ── dominance ────────────────────────────────────────────────────────────────

/// Every path from the entry to `at` passes through `check` (rustc's forward
/// dominators over the full CFG). Reflexive; false when `at` is unreachable.
pub(crate) fn dominates(body: &Body<'_>, check: BasicBlock, at: BasicBlock) -> bool {
    let d = body.basic_blocks.dominators();
    d.is_reachable(at) && d.dominates(check, at)
}
