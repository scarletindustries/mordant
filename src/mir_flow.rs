//! MIR machinery shared by the body-level analyses, with no opinion about
//! what any of it means for a lint.
//!
//! It answers four questions about a body:
//!
//! * which MIR to read at all (`mir_for`: the pre-optimization body, so `?`
//!   is still a `Try::branch` call and every aggregate is intact);
//! * what a place names, as an [`Atom`] -- a local plus its leading field
//!   path -- and how faithfully (`place_info`, [`Exactness`]);
//! * which branches a block is control-dependent on (`build_cfg`,
//!   `post_dominators`, `control_deps`), over a CFG in which blocks that
//!   cannot reach a `return` do not exist, so nothing is "decided" by an
//!   `assert!`;
//! * what a branch switches on (`switch_operand_atoms`).
//!
//! What the answers mean is the caller's business: `ctor_flow` combines them
//! into "does a failure exit depend on a stored field", `unchecked_input_len`
//! and `variant_flow` use only `mir_for` and trace the body their own way.

use std::collections::{HashSet, VecDeque};

use rustc_hir::def_id::LocalDefId;
use rustc_index::IndexVec;
use rustc_index::bit_set::DenseBitSet;
use rustc_middle::mir::{
    BasicBlock, BasicBlockData, Body, Local, Place, ProjectionElem, TerminatorKind,
};
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
        self.local == stored.local && self.path.starts_with(&stored.path)
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

// ── control dependence ───────────────────────────────────────────────────────

/// The body's CFG over normal (non-unwind) edges, with every block that
/// cannot reach a `return` pruned: a panic, abort or `unreachable!()` is
/// "does not happen" here, otherwise everything after `assert!(x)` would be
/// control-dependent on `x`.
pub(crate) struct Cfg {
    succs: IndexVec<BasicBlock, Vec<BasicBlock>>,
    /// The virtual exit node, one past the last block.
    exit: BasicBlock,
}

/// Normal (non-unwind) successors of a non-cleanup block; none otherwise.
fn raw_successors(body: &Body<'_>, data: &BasicBlockData<'_>) -> Vec<BasicBlock> {
    let Some(term) = data.terminator.as_ref().filter(|_| !data.is_cleanup) else {
        return Vec::new();
    };
    // Unwind targets are always cleanup blocks, so this leaves the normal edges.
    let mut v: Vec<_> = term
        .successors()
        .filter(|b| !body.basic_blocks[*b].is_cleanup)
        .collect();
    v.sort();
    v.dedup();
    v
}

pub(crate) fn build_cfg(body: &Body<'_>) -> Cfg {
    let exit = BasicBlock::from_usize(body.basic_blocks.len());
    let raw: IndexVec<BasicBlock, Vec<BasicBlock>> = body
        .basic_blocks
        .iter()
        .map(|data| raw_successors(body, data))
        .collect();
    let mut can_return: IndexVec<BasicBlock, bool> = body
        .basic_blocks
        .iter()
        .map(|data| {
            matches!(
                data.terminator.as_ref().map(|t| &t.kind),
                Some(TerminatorKind::Return | TerminatorKind::TailCall { .. })
            )
        })
        .collect();
    let mut changed = true;
    while changed {
        changed = false;
        for b in raw.indices() {
            if !can_return[b] && raw[b].iter().any(|s| can_return[*s]) {
                can_return[b] = true;
                changed = true;
            }
        }
    }
    let succs = raw
        .iter()
        .map(|r| {
            let live: Vec<BasicBlock> = r.iter().copied().filter(|s| can_return[*s]).collect();
            if live.is_empty() { vec![exit] } else { live }
        })
        .collect();
    Cfg { succs, exit }
}

type Bits = DenseBitSet<BasicBlock>;
/// Post-dominator sets over blocks plus the virtual exit.
pub(crate) type Pdoms = IndexVec<BasicBlock, Bits>;

pub(crate) fn post_dominators(cfg: &Cfg) -> Pdoms {
    let size = cfg.exit.as_usize() + 1;
    let mut pdom = IndexVec::from_elem_n(Bits::new_filled(size), size);
    pdom[cfg.exit] = Bits::new_empty(size);
    pdom[cfg.exit].insert(cfg.exit);
    let mut changed = true;
    while changed {
        changed = false;
        for (b, succs) in cfg.succs.iter_enumerated() {
            let mut acc = Bits::new_filled(size);
            for &s in succs {
                acc.intersect(&pdom[s]);
            }
            acc.insert(b);
            if acc != pdom[b] {
                pdom[b] = acc;
                changed = true;
            }
        }
    }
    pdom
}

/// Branch blocks that `t` is directly control-dependent on, in block order:
/// `t` does not post-dominate them but does post-dominate one of their
/// successors, so their outcome sends control to `t` or away from it. A
/// branch that only decides whether one of those is reached (an early
/// `return Ok` guard in front of it) is not among them; `control_deps` has
/// those too.
pub(crate) fn direct_control_deps(cfg: &Cfg, pdom: &Pdoms, t: BasicBlock) -> Vec<BasicBlock> {
    cfg.succs
        .iter_enumerated()
        .filter(|(a, succs)| {
            let strictly = pdom[*a].contains(t) && *a != t;
            succs.len() >= 2 && !strictly && succs.iter().any(|s| pdom[*s].contains(t))
        })
        .map(|(a, _)| a)
        .collect()
}

/// Branch blocks that `target` is transitively control-dependent on,
/// innermost first.
pub(crate) fn control_deps(cfg: &Cfg, pdom: &Pdoms, target: BasicBlock) -> Vec<BasicBlock> {
    let mut seen = HashSet::new();
    let mut q = VecDeque::from([target]);
    let mut deps = Vec::new();
    while let Some(t) = q.pop_front() {
        for a in direct_control_deps(cfg, pdom, t) {
            if seen.insert(a) {
                deps.push(a);
                q.push_back(a);
            }
        }
    }
    deps
}

pub(crate) fn switch_operand_atoms(body: &Body<'_>, bb: BasicBlock) -> Vec<Atom> {
    match &body.basic_blocks[bb].terminator().kind {
        TerminatorKind::SwitchInt { discr, .. } => discr
            .place()
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
