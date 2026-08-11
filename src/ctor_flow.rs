//! Dataflow over a constructor's MIR: which fields of the returned `Self`
//! does a failure exit actually depend on?
//!
//! A receiver-less `fn(..) -> Result<Self, E>` / `Option<Self>` can fail for
//! three unrelated reasons: it inspected a value it is about to store and
//! rejected it (an invariant), the input it was reading was malformed (a
//! parser), or the environment refused (allocation, IO). Only the first makes
//! a field's later reassignment dangerous, and the signature cannot tell them
//! apart. The body can:
//!
//! * every point that returns `Err`/`None`, literal or via `?`, is a failure
//!   exit;
//! * the branch conditions an exit is control-dependent on (post-dominator
//!   analysis, so nesting and loops are exact) decide it, and everything those
//!   conditions read, followed back through copies, projections, arithmetic
//!   and predicate calls into their arguments, is the exit's *decision slice*;
//! * every field of the `Self` that reaches the return has a *storage slice*:
//!   the values it is built from, followed back through copies, casts,
//!   arithmetic and nested aggregates, but not through "the `Ok` payload of"
//!   or "the result of a call" -- past those the field's own type or the
//!   callee carries whatever guarantee exists, not this function's check;
//! * a field carries an invariant iff some exit's decision slice meets its
//!   storage slice.
//!
//! Slices are over *places* (`opts.footer`, not `opts`), so a check on one
//! field of an input struct is not a check on its siblings, and `args.port =
//! x; if args.port == 0 { Err }; Ok(args)` implicates `port` alone.
//!
//! `let v = input.next()?; Self { v }` fails on `input` and stores the payload
//! of the checked thing, so the slices never meet. `if n > MAX { Err }; Self {
//! n }` reads and stores `n`. Exits whose error type is a resource error
//! (`AllocError`, `io::Error`, ...) are dropped up front, since "the allocator
//! said no" and "the helper said no" have the same shape.

use std::collections::{HashMap, HashSet, VecDeque};

use rustc_abi::{FieldIdx, VariantIdx};
use rustc_hir::LangItem;
use rustc_hir::def_id::{DefId, LocalDefId};
use rustc_index::IndexVec;
use rustc_lint::LateContext;
use rustc_middle::mir::{
    AggregateKind, BasicBlock, Body, BorrowKind, Local, Operand, Place, ProjectionElem,
    RETURN_PLACE, RawPtrKind, Rvalue, StatementKind, TerminatorKind,
};
use rustc_middle::ty::{self, Ty, TyCtxt};
use rustc_span::{Span, sym};

/// Error types that report the environment refusing, not the value being
/// wrong. An exit failing with one of these is never a validation.
const RESOURCE_ERRORS: &[&str] = &[
    "core::alloc::AllocError",
    "std::alloc::AllocError",
    "alloc::collections::TryReserveError",
    "std::collections::TryReserveError",
    "core::alloc::LayoutError",
    "std::alloc::LayoutError",
    "std::io::Error",
    "std::thread::AccessError",
];

pub(crate) struct FieldCheck {
    pub field: FieldIdx,
    /// The branch whose outcome the field's stored value decides.
    pub check: Span,
}

/// The fields of `self_did` that `ctor` checks before constructing one.
pub(crate) fn checked_fields(
    cx: &LateContext<'_>,
    ctor: LocalDefId,
    self_did: DefId,
    extra_resource_errors: &[String],
) -> Vec<FieldCheck> {
    let mut memo = HashMap::new();
    let mut out: HashMap<FieldIdx, Span> = HashMap::new();
    analyze_body(
        cx.tcx,
        ctor,
        self_did,
        extra_resource_errors,
        &mut memo,
        0,
        &mut out,
    );
    let mut v: Vec<FieldCheck> = out
        .into_iter()
        .map(|(field, check)| FieldCheck { field, check })
        .collect();
    v.sort_by_key(|f| f.field);
    v
}

// ── MIR access ───────────────────────────────────────────────────────────────

fn mir_for<'tcx>(tcx: TyCtxt<'tcx>, def: LocalDefId) -> Option<MirRef<'tcx>> {
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

enum MirRef<'tcx> {
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
struct Atom {
    local: Local,
    path: Vec<u32>,
}

/// Path component for "some element of": indexing conflates positions but
/// keeps the container distinct from its elements, so a length check on a
/// slice is not a check on the element later stored out of it.
const ANY_ELEM: u32 = u32::MAX;

impl Atom {
    fn whole(local: Local) -> Self {
        Atom {
            local,
            path: Vec::new(),
        }
    }
    fn extended(&self, tail: &[u32]) -> Self {
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
    fn overlaps(&self, other: &Atom) -> bool {
        self.local == other.local && self.path.iter().zip(&other.path).all(|(a, b)| a == b)
    }
    /// `self` (a decision atom) reads `stored` or a part of it. The reverse,
    /// a decision on the whole of something only part of which is stored
    /// (`lexer.next()?` then `log: lexer.log`), is not evidence about the part.
    fn inspects(&self, stored: &Atom) -> bool {
        self.local == stored.local && is_prefix(&stored.path, &self.path)
    }
}

struct PlaceInfo {
    atom: Atom,
    /// Projection was pure field access (the atom is exact).
    pure: bool,
    /// A `Downcast` appeared: this reads a variant payload of `atom`.
    downcast: bool,
    index_locals: Vec<Local>,
}

fn place_info(place: Place<'_>) -> PlaceInfo {
    let mut path = Vec::new();
    let mut pure = true;
    let mut downcast = false;
    let mut index_locals = Vec::new();
    for elem in place.projection.iter() {
        match elem {
            ProjectionElem::Field(f, _) if pure => path.push(f.as_u32()),
            ProjectionElem::Field(..) => {}
            // `(*r).f`: the reference is the value for slicing purposes, so
            // a deref neither ends the field path nor makes it inexact.
            ProjectionElem::Deref => {}
            ProjectionElem::Downcast(..) => {
                downcast = true;
                pure = false;
            }
            ProjectionElem::Index(v) => {
                index_locals.push(v);
                if pure {
                    path.push(ANY_ELEM);
                }
            }
            ProjectionElem::ConstantIndex { .. } | ProjectionElem::Subslice { .. } if pure => {
                path.push(ANY_ELEM);
            }
            _ => pure = false,
        }
    }
    PlaceInfo {
        atom: Atom {
            local: place.local,
            path,
        },
        pure,
        downcast,
        index_locals,
    }
}

fn operand_place<'tcx>(op: &Operand<'tcx>) -> Option<Place<'tcx>> {
    match op {
        Operand::Copy(p) | Operand::Move(p) => Some(*p),
        Operand::Constant(_) | Operand::RuntimeChecks(_) => None,
    }
}

// ── def facts ────────────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
enum Read {
    /// Value identity: a copy/move/borrow of the place. Paths compose.
    Same(Atom),
    /// Computed from the whole operand (cast, arithmetic, repeat).
    Derived(Atom),
    /// Operand `i` of an aggregate written to the dest. Paths compose past `i`.
    AggField(u32, Atom),
    /// A variant payload of this atom was read (`(x as Some).0`).
    Payload(Atom),
    /// `discriminant(x)`: decides, never stores.
    Discr(Atom),
    /// An index local: decides, never stores.
    Index(Local),
    /// Argument of the call whose result this is: decides unless the result
    /// only ever had its payload stored.
    CallArg(Atom),
    /// An argument of a call that operates on a `&mut` argument (insert,
    /// write, advance): the call failing is about the operation, not a
    /// verdict on any value handed to it. Never followed.
    CallArgMut,
    /// Argument of a call that could write through a `&mut` to the dest.
    ViaMut(Atom),
}

#[derive(Clone, Debug)]
struct Def {
    /// Field path of the destination place within its local.
    dest: Vec<u32>,
    reads: Vec<Read>,
}

/// A call to a crate-local fn: callee and argument atoms by position.
type LocalCall = (LocalDefId, Vec<Option<Atom>>);

/// A `Result`/`Option` aggregate assigned to a whole local: which variant,
/// its first operand, and that operand's type (the error, for `Err`).
struct WrapperAggregate<'tcx> {
    dest: Local,
    adt: DefId,
    variant: VariantIdx,
    operand: Option<Atom>,
    operand_ty: Option<Ty<'tcx>>,
    block: BasicBlock,
}

struct Facts {
    defs: IndexVec<Local, Vec<Def>>,
    /// Whole-local moves (`_a = move _b`) and `Try::branch` look-through.
    alias: IndexVec<Local, Vec<Local>>,
    /// Locals whose variant payload was moved whole into this local.
    payload_alias: IndexVec<Local, Vec<Local>>,
    /// Atoms holding a `Self` value that reaches the return.
    self_roots: Vec<Atom>,
    local_calls: HashMap<Local, Vec<LocalCall>>,
    /// Blocks that assign a failure into the return, minus resource errors.
    failure_blocks: Vec<BasicBlock>,
    /// Closures defined in the body whose signature can yield `Self`.
    closures: Vec<LocalDefId>,
}

fn is_resource_error(tcx: TyCtxt<'_>, ty: Ty<'_>, extra: &[String]) -> bool {
    let ty::Adt(adt, _) = ty.peel_refs().kind() else {
        return false;
    };
    let path = tcx.def_path_str(adt.did());
    let name = tcx.item_name(adt.did());
    let krate = tcx.crate_name(adt.did().krate);
    RESOURCE_ERRORS
        .iter()
        .copied()
        .chain(extra.iter().map(String::as_str))
        .any(|e| {
            // Full def path, bare name, or `crate::Name` for a re-export whose
            // def path runs through a private module (`bun_sys::error::Error`
            // configured as `bun_sys::Error`).
            path == e
                || path.ends_with(&format!("::{e}"))
                || match e.rsplit_once("::") {
                    None => name.as_str() == e,
                    Some((k, n)) => !k.contains("::") && krate.as_str() == k && name.as_str() == n,
                }
        })
}

/// `Result<_, E>` -> `E`; `Option<_>` -> None.
fn error_type<'tcx>(tcx: TyCtxt<'tcx>, ty: Ty<'tcx>) -> Option<Ty<'tcx>> {
    let ty::Adt(adt, args) = ty.kind() else {
        return None;
    };
    (tcx.is_diagnostic_item(sym::Result, adt.did()) && args.len() == 2).then(|| args.type_at(1))
}

/// Trait methods whose result is the receiver's value seen differently.
fn is_view_call(tcx: TyCtxt<'_>, callee: DefId) -> bool {
    let Some(tr) = tcx.trait_of_assoc(callee) else {
        return false;
    };
    let li = tcx.lang_items();
    [
        li.deref_trait(),
        li.deref_mut_trait(),
        li.index_trait(),
        li.index_mut_trait(),
        li.clone_trait(),
    ]
    .contains(&Some(tr))
        || [
            sym::Borrow,
            sym::BorrowMut,
            sym::AsRef,
            sym::AsMut,
            rustc_span::Symbol::intern("ToOwned"),
        ]
        .iter()
        .any(|s| tcx.is_diagnostic_item(*s, tr))
}

fn is_self_ty(ty: Ty<'_>, self_did: DefId) -> bool {
    matches!(ty.kind(), ty::Adt(a, _) if a.did() == self_did)
}

fn mentions_self(ty: Ty<'_>, self_did: DefId) -> bool {
    ty.walk()
        .any(|arg| arg.as_type().is_some_and(|t| is_self_ty(t, self_did)))
}

/// Reads of one operand/place in value position.
fn reads_of_place(place: Place<'_>, same: bool, out: &mut Vec<Read>) {
    let info = place_info(place);
    out.extend(info.index_locals.iter().map(|l| Read::Index(*l)));
    if info.downcast {
        out.push(Read::Payload(info.atom));
    } else if same && info.pure {
        out.push(Read::Same(info.atom));
    } else {
        out.push(Read::Derived(info.atom));
    }
}

fn reads_of_operand(op: &Operand<'_>, same: bool, out: &mut Vec<Read>) {
    if let Some(p) = operand_place(op) {
        reads_of_place(p, same, out);
    }
}

fn gather<'tcx>(
    tcx: TyCtxt<'tcx>,
    body: &Body<'tcx>,
    self_did: DefId,
    extra_resource_errors: &[String],
) -> Facts {
    let n = body.local_decls.len();
    let mut defs: IndexVec<Local, Vec<Def>> = IndexVec::from_elem_n(Vec::new(), n);
    let mut alias: IndexVec<Local, Vec<Local>> = IndexVec::from_elem_n(Vec::new(), n);
    let mut payload_alias: IndexVec<Local, Vec<Local>> = IndexVec::from_elem_n(Vec::new(), n);
    let mut mut_ref_to: HashMap<Local, Atom> = HashMap::new();
    let mut wrappers: Vec<WrapperAggregate<'tcx>> = Vec::new();
    let mut local_calls: HashMap<Local, Vec<LocalCall>> = HashMap::new();
    let mut residual_calls: Vec<(Local, Option<Ty<'tcx>>, BasicBlock)> = Vec::new();
    let mut closures = Vec::new();

    for (bb, data) in body.basic_blocks.iter_enumerated() {
        if data.is_cleanup {
            continue;
        }
        for stmt in &data.statements {
            let StatementKind::Assign(assign) = &stmt.kind else {
                continue;
            };
            let (dest, rvalue) = &**assign;
            let dinfo = place_info(*dest);
            let mut reads = Vec::new();
            match rvalue {
                Rvalue::Use(op, _) => {
                    if dest.projection.is_empty()
                        && let Some(p) = operand_place(op)
                    {
                        let pinfo = place_info(p);
                        if p.projection.is_empty() {
                            alias[dest.local].push(p.local);
                        } else if pinfo.downcast {
                            payload_alias[dest.local].push(p.local);
                        }
                    }
                    reads_of_operand(op, true, &mut reads);
                }
                Rvalue::Repeat(op, _)
                | Rvalue::Cast(_, op, _)
                | Rvalue::UnaryOp(_, op)
                | Rvalue::WrapUnsafeBinder(op, _) => reads_of_operand(op, false, &mut reads),
                Rvalue::Ref(_, kind, place) => {
                    if matches!(kind, BorrowKind::Mut { .. }) && dest.projection.is_empty() {
                        mut_ref_to.insert(dest.local, place_info(*place).atom);
                    }
                    reads_of_place(*place, true, &mut reads);
                }
                Rvalue::RawPtr(kind, place) => {
                    if matches!(kind, RawPtrKind::Mut) && dest.projection.is_empty() {
                        mut_ref_to.insert(dest.local, place_info(*place).atom);
                    }
                    reads_of_place(*place, true, &mut reads);
                }
                Rvalue::Reborrow(_, m, place) => {
                    if m.is_mut() && dest.projection.is_empty() {
                        mut_ref_to.insert(dest.local, place_info(*place).atom);
                    }
                    reads_of_place(*place, true, &mut reads);
                }
                Rvalue::CopyForDeref(place) => reads_of_place(*place, true, &mut reads),
                Rvalue::BinaryOp(_, ops) => {
                    let (a, b) = &**ops;
                    reads_of_operand(a, false, &mut reads);
                    reads_of_operand(b, false, &mut reads);
                }
                Rvalue::Discriminant(place) => {
                    let info = place_info(*place);
                    reads.extend(info.index_locals.iter().map(|l| Read::Index(*l)));
                    reads.push(Read::Discr(info.atom));
                }
                Rvalue::Aggregate(kind, ops) => {
                    let kind = &**kind;
                    for (i, op) in ops.iter().enumerate() {
                        if let Some(p) = operand_place(op) {
                            let info = place_info(p);
                            reads.extend(info.index_locals.iter().map(|l| Read::Index(*l)));
                            if info.downcast {
                                reads.push(Read::Payload(info.atom));
                            } else if info.pure {
                                reads.push(Read::AggField(i as u32, info.atom));
                            } else {
                                reads.push(Read::Derived(info.atom));
                            }
                        }
                    }
                    match kind {
                        AggregateKind::Adt(did, vidx, _, _, None)
                            if dest.projection.is_empty()
                                && (tcx.is_diagnostic_item(sym::Result, *did)
                                    || tcx.is_diagnostic_item(sym::Option, *did)) =>
                        {
                            let first = ops.iter().next();
                            wrappers.push(WrapperAggregate {
                                dest: dest.local,
                                adt: *did,
                                variant: *vidx,
                                operand: first.and_then(operand_place).map(|p| place_info(p).atom),
                                operand_ty: first.map(|o| o.ty(&body.local_decls, tcx)),
                                block: bb,
                            });
                        }
                        AggregateKind::Closure(cdid, args) => {
                            let out = args.as_closure().sig().output().skip_binder();
                            if mentions_self(out, self_did)
                                && let Some(l) = cdid.as_local()
                            {
                                closures.push(l);
                            }
                        }
                        _ => {}
                    }
                }
                Rvalue::ThreadLocalRef(_) => {}
            }
            defs[dest.local].push(Def {
                dest: dinfo.atom.path,
                reads,
            });
        }

        let Some(term) = &data.terminator else {
            continue;
        };
        if let TerminatorKind::Call {
            func,
            args,
            destination,
            ..
        } = &term.kind
        {
            let arg_atoms: Vec<Option<Atom>> = args
                .iter()
                .map(|a| operand_place(&a.node).map(|p| place_info(p).atom))
                .collect();
            // A call handed a `&mut` is an operation on that argument (insert,
            // write, advance); its other arguments are the data operated
            // with, and its failing judges none of them.
            let operation = arg_atoms
                .iter()
                .flatten()
                .any(|a| a.path.is_empty() && mut_ref_to.contains_key(&a.local));
            let call_arg = |a: &Atom| {
                if operation {
                    Read::CallArgMut
                } else {
                    Read::CallArg(a.clone())
                }
            };
            let mut reads: Vec<Read> = arg_atoms.iter().flatten().map(call_arg).collect();
            // A `&mut x` handed to the call: whatever the callee decides from
            // its arguments can land in `x`.
            for a in arg_atoms.iter().flatten() {
                if a.path.is_empty()
                    && let Some(target) = mut_ref_to.get(&a.local)
                {
                    defs[target.local].push(Def {
                        dest: target.path.clone(),
                        reads: arg_atoms
                            .iter()
                            .flatten()
                            .cloned()
                            .map(Read::ViaMut)
                            .collect(),
                    });
                }
            }
            if let Some((callee, cargs)) = func.const_fn_def() {
                if is_view_call(tcx, callee)
                    && let Some(Some(recv)) = arg_atoms.first()
                {
                    // `deref`, `index`, `clone`, `as_ref`, `borrow`: the result
                    // is (a view of) the receiver's value, not a verdict on it.
                    reads.clear();
                    let li = tcx.lang_items();
                    let tr = tcx.trait_of_assoc(callee);
                    if tr.is_some() && (tr == li.index_trait() || tr == li.index_mut_trait()) {
                        reads.push(Read::Same(recv.extended(&[ANY_ELEM])));
                    } else {
                        reads.push(Read::Same(recv.clone()));
                    }
                    for a in arg_atoms.iter().skip(1).flatten() {
                        if a.path.is_empty() {
                            reads.push(Read::Index(a.local));
                        }
                    }
                } else if tcx.is_lang_item(callee, LangItem::TryTraitFromResidual) {
                    let residual = cargs
                        .get(1)
                        .and_then(|g| g.as_type())
                        .or_else(|| args.first().map(|a| a.node.ty(&body.local_decls, tcx)));
                    residual_calls.push((
                        destination.local,
                        residual.and_then(|r| error_type(tcx, r)),
                        bb,
                    ));
                } else if tcx.is_lang_item(callee, LangItem::TryTraitBranch) {
                    if let Some(Some(r)) = arg_atoms.first()
                        && r.path.is_empty()
                    {
                        alias[destination.local].push(r.local);
                    }
                } else if let Some(l) = callee.as_local()
                    && destination.projection.is_empty()
                {
                    local_calls
                        .entry(destination.local)
                        .or_default()
                        .push((l, arg_atoms.clone()));
                    reads.clear();
                    reads.extend(arg_atoms.iter().flatten().map(call_arg));
                }
            }
            let dinfo = place_info(*destination);
            defs[destination.local].push(Def {
                dest: dinfo.atom.path,
                reads,
            });
        }
    }

    let returned = closure_over(RETURN_PLACE, |l| alias[l].clone());

    let mut failure_blocks = Vec::new();
    let mut self_roots = Vec::new();
    for w in &wrappers {
        if !returned.contains(&w.dest) {
            continue;
        }
        let name = tcx.adt_def(w.adt).variant(w.variant).name;
        if name == sym::Err || name == sym::None {
            let resource = name == sym::Err
                && w.operand_ty
                    .is_some_and(|t| is_resource_error(tcx, t, extra_resource_errors));
            if !resource {
                failure_blocks.push(w.block);
            }
        } else if let Some(a) = &w.operand {
            self_roots.push(a.clone());
        }
    }
    for (dest, err, bb) in &residual_calls {
        if returned.contains(dest)
            && !err.is_some_and(|t| is_resource_error(tcx, t, extra_resource_errors))
        {
            failure_blocks.push(*bb);
        }
    }
    // A body returning bare `Self` (a helper, a closure): the return place
    // itself is the self value.
    if is_self_ty(body.local_decls[RETURN_PLACE].ty, self_did) {
        self_roots.push(Atom::whole(RETURN_PLACE));
    }
    failure_blocks.sort();
    failure_blocks.dedup();

    Facts {
        defs,
        alias,
        payload_alias,
        self_roots,
        local_calls,
        failure_blocks,
        closures,
    }
}

fn closure_over(start: Local, mut succ: impl FnMut(Local) -> Vec<Local>) -> HashSet<Local> {
    let mut seen = HashSet::new();
    let mut q = VecDeque::from([start]);
    while let Some(l) = q.pop_front() {
        if seen.insert(l) {
            q.extend(succ(l));
        }
    }
    seen
}

// ── slices ───────────────────────────────────────────────────────────────────

fn is_prefix(a: &[u32], b: &[u32]) -> bool {
    a.len() <= b.len() && a.iter().zip(b).all(|(x, y)| x == y)
}

/// Storage slice from `roots`: every atom whose value ends up (as itself, or
/// arithmetically combined) in a root. Returns the slice and the atoms whose
/// variant payload was consumed on the way (produced, not inspected).
fn storage_slice(defs: &IndexVec<Local, Vec<Def>>, roots: &[Atom]) -> (Vec<Atom>, Vec<Atom>) {
    let mut seen: HashSet<Atom> = HashSet::new();
    let mut produced: Vec<Atom> = Vec::new();
    let mut q: VecDeque<Atom> = roots.iter().cloned().collect();
    while let Some(at) = q.pop_front() {
        if !seen.insert(at.clone()) {
            continue;
        }
        for def in &defs[at.local] {
            let (below, rem): (bool, &[u32]) = if is_prefix(&def.dest, &at.path) {
                (true, &at.path[def.dest.len()..])
            } else if is_prefix(&at.path, &def.dest) {
                (false, &[])
            } else {
                continue;
            };
            for r in &def.reads {
                match r {
                    Read::Same(a) => q.push_back(if below { a.extended(rem) } else { a.clone() }),
                    Read::Derived(a) => q.push_back(a.clone()),
                    Read::AggField(i, a) => {
                        if !below || rem.is_empty() {
                            q.push_back(a.clone());
                        } else if rem[0] == *i {
                            q.push_back(a.extended(&rem[1..]));
                        }
                    }
                    Read::Payload(a) => produced.push(a.clone()),
                    Read::Discr(_)
                    | Read::Index(_)
                    | Read::CallArg(_)
                    | Read::CallArgMut
                    | Read::ViaMut(_) => {}
                }
            }
        }
    }
    (seen.into_iter().collect(), produced)
}

/// Decision slice from a branch operand. `opaque` holds atoms whose defining
/// call must not be entered: produced-payload bases (the branch consumed the
/// callee's verdict, it did not inspect the arguments) and stored values (a
/// method failing on a stored value implicates that value, not everything
/// its constructor was handed).
fn decision_slice(defs: &IndexVec<Local, Vec<Def>>, roots: &[Atom], opaque: &[Atom]) -> Vec<Atom> {
    let mut seen: HashSet<Atom> = HashSet::new();
    let mut q: VecDeque<Atom> = roots.iter().cloned().collect();
    while let Some(at) = q.pop_front() {
        if !seen.insert(at.clone()) {
            continue;
        }
        let is_produced = opaque.iter().any(|p| p.overlaps(&at));
        for def in &defs[at.local] {
            let (below, rem): (bool, &[u32]) = if is_prefix(&def.dest, &at.path) {
                (true, &at.path[def.dest.len()..])
            } else if is_prefix(&at.path, &def.dest) {
                (false, &[])
            } else {
                continue;
            };
            for r in &def.reads {
                match r {
                    Read::Same(a) => q.push_back(if below { a.extended(rem) } else { a.clone() }),
                    Read::AggField(i, a) => {
                        if !below || rem.is_empty() {
                            q.push_back(a.clone());
                        } else if rem[0] == *i {
                            q.push_back(a.extended(&rem[1..]));
                        }
                    }
                    Read::Derived(a) | Read::Payload(a) | Read::Discr(a) | Read::ViaMut(a) => {
                        q.push_back(a.clone())
                    }
                    Read::Index(l) => q.push_back(Atom::whole(*l)),
                    Read::CallArg(a) => {
                        if !is_produced {
                            q.push_back(a.clone());
                        }
                    }
                    Read::CallArgMut => {}
                }
            }
        }
    }
    seen.into_iter().collect()
}

fn slices_meet(storage: &[Atom], decision: &[Atom]) -> bool {
    let mut by_local: HashMap<Local, Vec<&Atom>> = HashMap::new();
    for d in decision {
        by_local.entry(d.local).or_default().push(d);
    }
    storage.iter().any(|s| {
        by_local
            .get(&s.local)
            .is_some_and(|ds| ds.iter().any(|d| d.inspects(s)))
    })
}

// ── control dependence ───────────────────────────────────────────────────────

/// The body's CFG over normal (non-unwind) edges, with every block that
/// cannot reach a `return` pruned: a panic, abort or `unreachable!()` is
/// "does not happen" here, otherwise everything after `assert!(x)` would be
/// control-dependent on `x`.
struct Cfg {
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

fn build_cfg(body: &Body<'_>) -> Cfg {
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

struct Bits(Vec<u64>);
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
fn post_dominators(cfg: &Cfg) -> Vec<Bits> {
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

/// Branch blocks that `target` is transitively control-dependent on,
/// innermost first.
fn control_deps(cfg: &Cfg, pdom: &[Bits], target: BasicBlock) -> Vec<BasicBlock> {
    let branches: Vec<usize> = (0..cfg.exit).filter(|a| cfg.succs[*a].len() >= 2).collect();
    let direct = |t: usize| -> Vec<usize> {
        branches
            .iter()
            .copied()
            .filter(|a| {
                let strictly = pdom[*a].has(t) && *a != t;
                !strictly && cfg.succs[*a].iter().any(|s| pdom[*s].has(t))
            })
            .collect()
    };
    let mut seen = HashSet::new();
    let mut q = VecDeque::from([target.as_usize()]);
    let mut deps = Vec::new();
    while let Some(t) = q.pop_front() {
        for a in direct(t) {
            if seen.insert(a) {
                deps.push(BasicBlock::from_usize(a));
                q.push_back(a);
            }
        }
    }
    deps
}

/// The branch at `bb` switches on the discriminant of a `Result<_, E>` whose
/// `E` is a resource error: its outcome is the environment's, whatever the
/// constructor then returns.
fn decides_on_resource<'tcx>(
    tcx: TyCtxt<'tcx>,
    body: &Body<'tcx>,
    facts: &Facts,
    bb: BasicBlock,
    extra: &[String],
) -> bool {
    let TerminatorKind::SwitchInt { discr, .. } = &body.basic_blocks[bb].terminator().kind else {
        return false;
    };
    let Some(p) = operand_place(discr) else {
        return false;
    };
    // `_d = discriminant(_r); switchInt(move _d)`: look one def back.
    let mut candidates = vec![p.local];
    for def in &facts.defs[p.local] {
        for r in &def.reads {
            if let Read::Discr(a) = r {
                candidates.push(a.local);
                // `?`: `_t = Try::branch(_r)`; the residual type sits on `_r`.
                candidates.extend(facts.alias[a.local].iter().copied());
            }
        }
    }
    candidates.into_iter().any(|l| {
        error_type(tcx, body.local_decls[l].ty).is_some_and(|e| is_resource_error(tcx, e, extra))
    })
}

fn switch_operand_atoms(body: &Body<'_>, bb: BasicBlock) -> Vec<Atom> {
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

// ── per-field roots, helpers, and the verdict ────────────────────────────────

type Summary = HashMap<FieldIdx, Vec<usize>>;
type Memo = HashMap<LocalDefId, Option<Summary>>;

/// Which parameters of helper `def` (returning `Self`, `Result<Self, _>` or
/// `Option<Self>`) flow into which field of the `Self` it builds.
fn helper_summary<'tcx>(
    tcx: TyCtxt<'tcx>,
    def: LocalDefId,
    self_did: DefId,
    memo: &mut Memo,
    depth: usize,
) -> Option<Summary> {
    if let Some(m) = memo.get(&def) {
        return m.clone();
    }
    if depth > 3 {
        return None;
    }
    memo.insert(def, None);
    let body = mir_for(tcx, def)?;
    let facts = gather(tcx, &body, self_did, &[]);
    let nfields = tcx.adt_def(self_did).non_enum_variant().fields.len();
    let roots = field_roots(tcx, &body, &facts, self_did, nfields, memo, depth + 1);
    let argc = body.arg_count;
    let mut out: Summary = HashMap::new();
    for (f, r) in roots {
        let (slice, _) = storage_slice(&facts.defs, &r);
        let params: Vec<usize> = (1..=argc)
            .filter(|i| slice.iter().any(|a| a.local.as_usize() == *i))
            .map(|i| i - 1)
            .collect();
        if !params.is_empty() {
            out.entry(f).or_default().extend(params);
        }
    }
    memo.insert(def, Some(out.clone()));
    Some(out)
}

/// field -> root atoms: `carrier.f` for every `Self`-typed local the returned
/// value passes through, plus helper-call arguments the helper stores in `f`.
fn field_roots<'tcx>(
    tcx: TyCtxt<'tcx>,
    body: &Body<'tcx>,
    facts: &Facts,
    self_did: DefId,
    nfields: usize,
    memo: &mut Memo,
    depth: usize,
) -> HashMap<FieldIdx, Vec<Atom>> {
    let mut roots: HashMap<FieldIdx, Vec<Atom>> = HashMap::new();
    let mut carriers: HashSet<Local> = HashSet::new();
    for r in &facts.self_roots {
        if r.path.is_empty() {
            carriers.extend(closure_over(r.local, |l| {
                let mut v = facts.alias[l].clone();
                v.extend(facts.payload_alias[l].iter().copied());
                v
            }));
        } else {
            // `Ok(pair.1)`-style: the self value is a sub-place; root there.
            for f in 0..nfields {
                roots
                    .entry(FieldIdx::from_usize(f))
                    .or_default()
                    .push(r.extended(&[f as u32]));
            }
        }
    }
    for &c in &carriers {
        if is_self_ty(body.local_decls[c].ty, self_did) {
            for f in 0..nfields {
                roots
                    .entry(FieldIdx::from_usize(f))
                    .or_default()
                    .push(Atom {
                        local: c,
                        path: vec![f as u32],
                    });
            }
        }
        if let Some(calls) = facts.local_calls.get(&c) {
            for (callee, args) in calls {
                let ret = tcx
                    .fn_sig(callee.to_def_id())
                    .instantiate_identity()
                    .skip_normalization()
                    .output()
                    .skip_binder();
                if !mentions_self(ret, self_did) {
                    continue;
                }
                if let Some(summary) = helper_summary(tcx, *callee, self_did, memo, depth) {
                    for (f, params) in summary {
                        for p in params {
                            if let Some(Some(a)) = args.get(p) {
                                roots.entry(f).or_default().push(a.clone());
                            }
                        }
                    }
                }
            }
        }
    }
    roots
}

fn analyze_body<'tcx>(
    tcx: TyCtxt<'tcx>,
    def: LocalDefId,
    self_did: DefId,
    extra_resource_errors: &[String],
    memo: &mut Memo,
    depth: usize,
    out: &mut HashMap<FieldIdx, Span>,
) {
    if depth > 3 {
        return;
    }
    let Some(body) = mir_for(tcx, def) else {
        return;
    };
    if body.tainted_by_errors.is_some() {
        return;
    }
    if let Some(e) = error_type(tcx, body.local_decls[RETURN_PLACE].ty)
        && is_resource_error(tcx, e, extra_resource_errors)
    {
        return;
    }
    let facts = gather(tcx, &body, self_did, extra_resource_errors);
    let closures = facts.closures.clone();
    if !facts.failure_blocks.is_empty() {
        let nfields = tcx.adt_def(self_did).non_enum_variant().fields.len();
        let roots = field_roots(tcx, &body, &facts, self_did, nfields, memo, depth);
        let mut slices: Vec<(FieldIdx, Vec<Atom>)> = Vec::new();
        let mut opaque: Vec<Atom> = Vec::new();
        for (f, r) in roots {
            let (s, p) = storage_slice(&facts.defs, &r);
            opaque.extend(p);
            opaque.extend(s.iter().cloned());
            slices.push((f, s));
        }
        if !slices.is_empty() {
            let cfg = build_cfg(&body);
            let pdom = post_dominators(&cfg);
            let mut switch_slices: HashMap<BasicBlock, Vec<Atom>> = HashMap::new();
            for &fb in &facts.failure_blocks {
                for branch in control_deps(&cfg, &pdom, fb) {
                    if decides_on_resource(tcx, &body, &facts, branch, extra_resource_errors) {
                        continue;
                    }
                    let ds = switch_slices.entry(branch).or_insert_with(|| {
                        decision_slice(&facts.defs, &switch_operand_atoms(&body, branch), &opaque)
                    });
                    for (f, ss) in &slices {
                        if !out.contains_key(f) && slices_meet(ss, ds) {
                            out.insert(*f, body.basic_blocks[branch].terminator().source_info.span);
                        }
                    }
                }
            }
        }
    }
    drop(body);
    // A closure that can yield `Self` (`.and_then(|v| ...)`, `try_parse(|i|
    // ...)`) is a constructor body of its own.
    for c in closures {
        analyze_body(
            tcx,
            c,
            self_did,
            extra_resource_errors,
            memo,
            depth + 1,
            out,
        );
    }
}
