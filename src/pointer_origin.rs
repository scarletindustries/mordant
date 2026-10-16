//! Finds where a pointer or reference built in a fn body comes from: a
//! static, a thread-local, or one of the fn's arguments. Nothing here
//! decides what to report.
//!
//! The search follows a value back through copies, casts, borrows and
//! field reads. It also enters the bodies of the fns it calls, so
//! `X.get()` on a static cell leads back to `X`. A value made by integer
//! arithmetic, by a call whose body is not available, or by a fresh
//! allocation has no root. The search keeps every root found on any path.

use rustc_data_structures::fx::FxHashMap;
use rustc_hir::def_id::DefId;
use rustc_index::IndexVec;
use rustc_index::bit_set::DenseBitSet;
use rustc_middle::mir::{
    BasicBlock, BinOp, Body, Local, Location, Operand, Place, ProjectionElem, Rvalue,
    StatementKind, TerminatorKind,
};
use rustc_middle::ty::{self, Ty, TyCtxt};
use rustc_span::{Span, sym};

use crate::mir_flow::{MirRef, mir_for};

/// Calls followed into other bodies before the search gives up.
const MAX_DEPTH: usize = 8;
/// Bodies from other crates with more blocks than this are not entered.
const MAX_FOREIGN_BLOCKS: usize = 64;

/// What a value can be reached from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Root {
    Static(DefId),
    ThreadLocal,
    /// An argument of the body, by its local.
    Arg(Local),
}

/// A root, and where the body being searched reads it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Source {
    pub root: Root,
    pub span: Span,
}

/// The search, with a cache of what each called fn returns.
pub(crate) struct Origins<'tcx> {
    tcx: TyCtxt<'tcx>,
    /// `None` while the fn is being searched, so a cycle adds nothing.
    returns: FxHashMap<DefId, Option<Vec<Root>>>,
}

impl<'tcx> Origins<'tcx> {
    pub(crate) fn new(tcx: TyCtxt<'tcx>) -> Self {
        Self {
            tcx,
            returns: FxHashMap::default(),
        }
    }

    /// The roots of the value `body` returns, each at the place in `body`
    /// where it is read.
    pub(crate) fn of_return(&mut self, body: &Body<'tcx>) -> Vec<Source> {
        Walk::new(self, body, 0).run()
    }

    /// The roots of what `def` returns, with no spans. Empty when its body
    /// is not available or the search is too deep.
    fn returns_of(&mut self, def: DefId, depth: usize) -> Vec<Root> {
        if depth > MAX_DEPTH {
            return Vec::new();
        }
        match self.returns.get(&def) {
            Some(Some(roots)) => return roots.clone(),
            Some(None) => return Vec::new(),
            None => {}
        }
        let Some(body) = body_of(self.tcx, def) else {
            return Vec::new();
        };
        self.returns.insert(def, None);
        let roots: Vec<Root> = Walk::new(self, &body, depth)
            .run()
            .into_iter()
            .map(|s| s.root)
            .collect();
        self.returns.insert(def, Some(roots.clone()));
        roots
    }
}

fn body_of(tcx: TyCtxt<'_>, def: DefId) -> Option<MirRef<'_>> {
    if let Some(local) = def.as_local() {
        return mir_for(tcx, local);
    }
    if !tcx.def_kind(def).is_fn_like() || !tcx.is_mir_available(def) {
        return None;
    }
    let body = tcx.optimized_mir(def);
    (body.basic_blocks.len() <= MAX_FOREIGN_BLOCKS).then_some(MirRef::Opt(body))
}

/// Where a local gets its value: a statement, or a call's result.
#[derive(Clone, Copy)]
enum Def {
    Assign(Location),
    Call(BasicBlock),
}

struct Walk<'a, 'b, 'tcx> {
    origins: &'a mut Origins<'tcx>,
    body: &'b Body<'tcx>,
    defs: IndexVec<Local, Vec<Def>>,
    seen: DenseBitSet<Local>,
    depth: usize,
    found: Vec<Source>,
}

/// A write to the local itself or to a field of it, not through a pointer.
fn whole_local(place: Place<'_>) -> Option<Local> {
    let through_ptr = place
        .projection
        .iter()
        .any(|p| matches!(p, ProjectionElem::Deref));
    (!through_ptr).then_some(place.local)
}

impl<'a, 'b, 'tcx> Walk<'a, 'b, 'tcx> {
    fn new(origins: &'a mut Origins<'tcx>, body: &'b Body<'tcx>, depth: usize) -> Self {
        let mut defs: IndexVec<Local, Vec<Def>> =
            IndexVec::from_elem_n(Vec::new(), body.local_decls.len());
        for (bb, data) in body.basic_blocks.iter_enumerated() {
            for (i, stmt) in data.statements.iter().enumerate() {
                if let StatementKind::Assign(assign) = &stmt.kind
                    && let Some(local) = whole_local(assign.0)
                {
                    let at = Location {
                        block: bb,
                        statement_index: i,
                    };
                    defs[local].push(Def::Assign(at));
                }
            }
            if let TerminatorKind::Call { destination, .. } = &data.terminator().kind
                && let Some(local) = whole_local(*destination)
            {
                defs[local].push(Def::Call(bb));
            }
        }
        Self {
            origins,
            body,
            defs,
            seen: DenseBitSet::new_empty(body.local_decls.len()),
            depth,
            found: Vec::new(),
        }
    }

    fn run(mut self) -> Vec<Source> {
        self.local(rustc_middle::mir::RETURN_PLACE);
        self.found
    }

    fn push(&mut self, root: Root, span: Span) {
        if !self.found.iter().any(|s| s.root == root) {
            self.found.push(Source { root, span });
        }
    }

    fn local(&mut self, local: Local) {
        if !self.seen.insert(local) {
            return;
        }
        if (1..=self.body.arg_count).contains(&local.as_usize()) {
            let span = self.body.local_decls[local].source_info.span;
            self.push(Root::Arg(local), span);
        }
        for def in self.defs[local].clone() {
            match def {
                Def::Assign(at) => self.assign(at),
                Def::Call(bb) => self.call(bb),
            }
        }
    }

    fn assign(&mut self, at: Location) {
        let stmt = &self.body.basic_blocks[at.block].statements[at.statement_index];
        let StatementKind::Assign(assign) = &stmt.kind else {
            return;
        };
        let span = stmt.source_info.span;
        match &assign.1 {
            Rvalue::Use(op, ..) | Rvalue::Cast(_, op, _) | Rvalue::WrapUnsafeBinder(op, _) => {
                self.operand(op);
            }
            Rvalue::CopyForDeref(place) | Rvalue::Ref(_, _, place) | Rvalue::RawPtr(_, place) => {
                self.local(place.local);
            }
            Rvalue::ThreadLocalRef(_) => self.push(Root::ThreadLocal, span),
            Rvalue::BinaryOp(BinOp::Offset, ops) => self.operand(&ops.0),
            // Only parts that hold a pointer: a slice built from a fresh
            // pointer and a length read from `self` is not reached from
            // `self`.
            Rvalue::Aggregate(_, ops) => {
                for op in ops {
                    if holds_pointer(op.ty(self.body, self.origins.tcx)) {
                        self.operand(op);
                    }
                }
            }
            _ => {}
        }
    }

    fn operand(&mut self, op: &Operand<'tcx>) {
        match op {
            Operand::Copy(place) | Operand::Move(place) => self.local(place.local),
            Operand::Constant(c) => {
                if let Some(def) = c.check_static_ptr(self.origins.tcx) {
                    self.push(Root::Static(def), c.span);
                }
            }
            Operand::RuntimeChecks(_) => {}
        }
    }

    fn call(&mut self, bb: BasicBlock) {
        let term = self.body.basic_blocks[bb].terminator();
        let TerminatorKind::Call {
            func,
            args,
            fn_span,
            ..
        } = &term.kind
        else {
            return;
        };
        let Some((def, generic_args)) = func.const_fn_def() else {
            return;
        };
        let tcx = self.origins.tcx;
        let span = *fn_span;
        if let Some(intrinsic) = tcx.intrinsic(def) {
            let keeps_first = [
                sym::transmute,
                sym::transmute_unchecked,
                sym::offset,
                sym::arith_offset,
            ];
            if keeps_first.contains(&intrinsic.name)
                && let Some(first) = args.first()
            {
                self.operand(&first.node);
            }
            return;
        }
        if is_local_key_with(tcx, def) {
            if let Some(closure) = args.get(1) {
                self.local_key_closure(closure.node.ty(self.body, tcx), span);
            }
            return;
        }
        let Some(callee) = self.resolve(def, generic_args) else {
            return;
        };
        if !callee.is_local() && !tcx.is_mir_available(callee) {
            if callee == def
                && points_into_own_cell(tcx, def, generic_args)
                && let Some(first) = args.first()
            {
                self.operand(&first.node);
            }
            return;
        }
        for root in self.origins.returns_of(callee, self.depth + 1) {
            match root {
                Root::Arg(arg) => {
                    if let Some(a) = args.get(arg.as_usize() - 1) {
                        self.operand(&a.node);
                    }
                }
                other @ (Root::Static(_) | Root::ThreadLocal) => self.push(other, span),
            }
        }
    }

    /// `KEY.with(|v| ..)`: what the closure returns from `v` is reached from
    /// the thread-local.
    fn local_key_closure(&mut self, closure: Ty<'tcx>, span: Span) {
        let ty::Closure(closure, _) = *closure.kind() else {
            return;
        };
        for root in self.origins.returns_of(closure, self.depth + 1) {
            match root {
                Root::Arg(arg) if arg.as_usize() == 2 => self.push(Root::ThreadLocal, span),
                Root::Arg(_) => {}
                other @ (Root::Static(_) | Root::ThreadLocal) => self.push(other, span),
            }
        }
    }

    /// The fn a call runs: the impl's item for a trait method whose impl
    /// the argument types pick.
    fn resolve(&self, def: DefId, args: ty::GenericArgsRef<'tcx>) -> Option<DefId> {
        let tcx = self.origins.tcx;
        if tcx.trait_of_assoc(def).is_none() {
            return Some(def);
        }
        let env = ty::TypingEnv::post_analysis(tcx, self.body.source.def_id());
        let args = tcx
            .try_normalize_erasing_regions(env, ty::Unnormalized::new_wip(args))
            .ok()?;
        let instance = ty::Instance::try_resolve(tcx, env, def, args).ok()??;
        match instance.def {
            ty::InstanceKind::Item(item) => Some(item),
            _ => None,
        }
    }
}

/// `LocalKey::with` and its `with_borrow` forms, which pass the
/// thread-local's value to a closure.
fn is_local_key_with(tcx: TyCtxt<'_>, def: DefId) -> bool {
    let Some(impl_def) = tcx.impl_of_assoc(def) else {
        return false;
    };
    let self_ty = tcx
        .type_of(impl_def)
        .instantiate_identity()
        .skip_normalization();
    self_ty
        .ty_adt_def()
        .is_some_and(|adt| tcx.is_diagnostic_item(sym::LocalKey, adt.did()))
        && tcx.item_name(def).as_str().starts_with("with")
}

/// Whether a value of this type can carry a pointer.
fn holds_pointer(ty: Ty<'_>) -> bool {
    ty.walk().any(|arg| {
        arg.as_type()
            .is_some_and(|t| t.is_ref() || t.is_raw_ptr() || t.is_fn_ptr())
    })
}

/// A fn of another crate whose body this build cannot read (a check build
/// stores none), shaped `fn(&self) -> *mut U` where `Self` holds an
/// `UnsafeCell<U>`. A cell type's accessor of that shape returns a pointer
/// to its own contents.
fn points_into_own_cell<'tcx>(
    tcx: TyCtxt<'tcx>,
    def: DefId,
    args: ty::GenericArgsRef<'tcx>,
) -> bool {
    let sig = tcx
        .fn_sig(def)
        .instantiate(tcx, args)
        .skip_normalization()
        .skip_binder();
    let [receiver] = sig.inputs() else {
        return false;
    };
    let (ty::Ref(_, owner, ty::Mutability::Not), ty::RawPtr(pointee, ty::Mutability::Mut)) =
        (*receiver.kind(), *sig.output().kind())
    else {
        return false;
    };
    let pointee = tcx.erase_and_anonymize_regions(pointee);
    holds_cell_of(tcx, owner, pointee, 4)
}

/// Whether `ty` holds an `UnsafeCell<inner>` in its fields, `depth` levels
/// down at most.
fn holds_cell_of<'tcx>(tcx: TyCtxt<'tcx>, ty: Ty<'tcx>, inner: Ty<'tcx>, depth: usize) -> bool {
    let ty::Adt(adt, args) = *ty.kind() else {
        return false;
    };
    if adt.is_unsafe_cell() {
        return tcx.erase_and_anonymize_regions(args.type_at(0)) == inner;
    }
    depth > 0
        && adt.is_struct()
        && adt
            .all_fields()
            .any(|f| holds_cell_of(tcx, f.ty(tcx, args).skip_normalization(), inner, depth - 1))
}
