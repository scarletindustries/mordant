//! Which lock a call acts on. The pointer passed to `lock` (or `unlock`) is
//! traced back through borrows, casts and `Deref` calls to a root (a
//! parameter, a static or a local) and the fields taken from it.

use rustc_abi::{FIRST_VARIANT, FieldIdx, VariantIdx};
use rustc_data_structures::fx::{FxHashMap, FxHashSet};
use rustc_hir::attrs::lang_items::LangItem;
use rustc_hir::def_id::{DefId, LocalDefId};
use rustc_index::IndexVec;
use rustc_middle::mir::{
    Body, Local, Operand, Place, PlaceRef, ProjectionElem, RETURN_PLACE, Rvalue, StatementKind,
    TerminatorKind,
};
use rustc_middle::ty::{self, TyCtxt, TypingEnv};
use rustc_span::sym;

use crate::call_graph::resolve_item;
use crate::mir_flow::local_name;

/// Steps past this many fields lose their exact identity.
const MAX_STEPS: usize = 8;

/// Where a lock place starts.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub(super) enum Root {
    /// What the fn's parameter `i` points to. Never reassigned in the body.
    Param(usize),
    Static(DefId),
    /// A local of this body, assigned at most once.
    Local(Local),
    /// What the pointer in a local points to, when the pointer came from
    /// memory or a call. The local is assigned once, so it is one place.
    Pointee(Local),
    /// Anything else: a value read from memory, a call result.
    Opaque,
}

/// One field taken on the way from the root to the lock.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(super) struct Step {
    /// The struct or enum the field belongs to, or `None` for a tuple.
    pub(super) adt: Option<DefId>,
    pub(super) variant: VariantIdx,
    pub(super) field: FieldIdx,
    /// The struct or enum the field's own type is, if any.
    pub(super) ty: Option<DefId>,
}

/// A lock, as the place it lives at.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub(super) struct LockPlace {
    pub(super) root: Root,
    pub(super) steps: Vec<Step>,
    /// The root and steps name one lock: no index, no reassigned root.
    pub(super) exact: bool,
}

/// A lock as the order check sees it: the struct field it lives in, so that
/// two functions that lock `self.a` on the same struct agree. A lock with
/// no field is named by its static.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub(super) enum Class {
    Field(DefId, VariantIdx, FieldIdx),
    Static(DefId, Vec<FieldIdx>),
}

impl LockPlace {
    pub(super) fn opaque() -> Self {
        Self {
            root: Root::Opaque,
            steps: Vec::new(),
            exact: false,
        }
    }

    fn rooted(root: Root, exact: bool) -> Self {
        Self {
            root,
            steps: Vec::new(),
            exact,
        }
    }

    /// The class of the lock. A lock type can wrap another (a `Mutex<T>`
    /// newtype, `Guarded<T>` around a raw mutex), so the class is the
    /// outermost field whose type is one of `lock_types`. Failing that, the
    /// last field, and failing that, the static.
    pub(super) fn class(&self, lock_types: &FxHashSet<DefId>) -> Option<Class> {
        let named = |s: &Step| s.adt.map(|adt| Class::Field(adt, s.variant, s.field));
        let outer = self
            .steps
            .iter()
            .find(|s| s.adt.is_some() && s.ty.is_some_and(|t| lock_types.contains(&t)));
        if let Some(class) = outer.and_then(named) {
            return Some(class);
        }
        if let Some(class) = self.steps.last().and_then(named) {
            return Some(class);
        }
        match self.root {
            Root::Static(def) => Some(Class::Static(
                def,
                self.steps.iter().map(|s| s.field).collect(),
            )),
            Root::Param(_) | Root::Local(_) | Root::Pointee(_) | Root::Opaque => None,
        }
    }

    /// Both places name the same lock, for sure.
    pub(super) fn same_lock(&self, other: &Self) -> bool {
        self.exact
            && other.exact
            && self.root != Root::Opaque
            && self.root == other.root
            && self.steps == other.steps
    }

    /// The place in the caller's terms, given what the caller passes for
    /// each parameter. A callee's own locals mean nothing to the caller.
    pub(super) fn in_caller(&self, args: &[LockPlace]) -> Self {
        match &self.root {
            Root::Param(i) => match args.get(*i) {
                Some(base) => {
                    let mut steps = base.steps.clone();
                    steps.extend_from_slice(&self.steps);
                    let fits = steps.len() <= MAX_STEPS;
                    steps.truncate(MAX_STEPS);
                    Self {
                        root: base.root.clone(),
                        steps,
                        exact: base.exact && self.exact && fits,
                    }
                }
                None => Self {
                    root: Root::Opaque,
                    steps: self.steps.clone(),
                    exact: false,
                },
            },
            Root::Static(_) => self.clone(),
            Root::Local(_) | Root::Pointee(_) | Root::Opaque => Self {
                root: Root::Opaque,
                steps: self.steps.clone(),
                exact: false,
            },
        }
    }

    /// `self.state.inner` for a place found in `body`.
    pub(super) fn render(&self, tcx: TyCtxt<'_>, body: &Body<'_>) -> String {
        let mut out = match self.root {
            Root::Param(i) => local_name(body, Local::from_usize(i + 1))
                .map_or_else(|| format!("argument {}", i + 1), |n| n.to_string()),
            Root::Static(def) => tcx.item_name(def).to_string(),
            Root::Local(local) | Root::Pointee(local) => {
                local_name(body, local).map_or_else(|| "a local".to_string(), |n| n.to_string())
            }
            Root::Opaque => "a value".to_string(),
        };
        for step in &self.steps {
            out.push('.');
            out.push_str(&field_name(tcx, step));
        }
        out
    }
}

fn field_name(tcx: TyCtxt<'_>, step: &Step) -> String {
    match step.adt {
        Some(adt) => tcx.adt_def(adt).variant(step.variant).fields[step.field]
            .name
            .to_string(),
        None => step.field.as_usize().to_string(),
    }
}

/// `Type.field`, or the static's name and fields.
pub(super) fn class_name(tcx: TyCtxt<'_>, class: &Class) -> String {
    match class {
        Class::Field(adt, variant, field) => {
            let step = Step {
                adt: Some(*adt),
                variant: *variant,
                field: *field,
                ty: None,
            };
            format!("{}.{}", tcx.item_name(*adt), field_name(tcx, &step))
        }
        Class::Static(def, fields) => {
            let mut out = tcx.item_name(*def).to_string();
            for field in fields {
                out.push('.');
                out.push_str(&field.as_usize().to_string());
            }
            out
        }
    }
}

/// How a local gets its value.
#[derive(Clone, Copy)]
enum Def<'a, 'tcx> {
    Unset,
    Assign(&'a Rvalue<'tcx>),
    Call(&'a Operand<'tcx>, &'a [rustc_span::Spanned<Operand<'tcx>>]),
    Many,
}

/// Traces pointers in one body back to the place they point to.
pub(super) struct Tracer<'a, 'tcx> {
    tcx: TyCtxt<'tcx>,
    body: &'a Body<'tcx>,
    defs: IndexVec<Local, Def<'a, 'tcx>>,
    env: TypingEnv<'tcx>,
    /// For local fns that return a pointer, the place it points to.
    accessors: &'a FxHashMap<LocalDefId, LockPlace>,
}

impl<'a, 'tcx> Tracer<'a, 'tcx> {
    pub(super) fn new(
        tcx: TyCtxt<'tcx>,
        body: &'a Body<'tcx>,
        env: TypingEnv<'tcx>,
        accessors: &'a FxHashMap<LocalDefId, LockPlace>,
    ) -> Self {
        let mut defs = IndexVec::from_elem(Def::Unset, &body.local_decls);
        let mut set = |local: Local, def: Def<'a, 'tcx>| {
            defs[local] = match defs[local] {
                Def::Unset => def,
                Def::Assign(_) | Def::Call(..) | Def::Many => Def::Many,
            };
        };
        for data in body.basic_blocks.iter() {
            for statement in &data.statements {
                if let StatementKind::Assign(assign) = &statement.kind {
                    let (place, rvalue) = &**assign;
                    match place.as_local() {
                        Some(local) => set(local, Def::Assign(rvalue)),
                        None if !place.is_indirect() => set(place.local, Def::Many),
                        None => {}
                    }
                }
            }
            if let Some(terminator) = &data.terminator
                && let TerminatorKind::Call {
                    func,
                    args,
                    destination,
                    ..
                } = &terminator.kind
            {
                match destination.as_local() {
                    Some(local) => set(local, Def::Call(func, args)),
                    None => set(destination.local, Def::Many),
                }
            }
        }
        // A parameter that is assigned again in the body is not the value
        // the caller passed.
        for arg in body.args_iter() {
            if !matches!(defs[arg], Def::Unset) {
                defs[arg] = Def::Many;
            }
        }
        Self {
            tcx,
            body,
            defs,
            env,
            accessors,
        }
    }

    /// The place the pointer `op` points to.
    pub(super) fn pointee(&self, op: &Operand<'tcx>) -> LockPlace {
        self.operand(op, 0)
    }

    /// The place itself: its address, as if it were borrowed.
    pub(super) fn place(&self, place: Place<'tcx>) -> LockPlace {
        self.address(place, 0)
    }

    fn operand(&self, op: &Operand<'tcx>, depth: usize) -> LockPlace {
        match op {
            Operand::Copy(place) | Operand::Move(place) => match place.as_local() {
                Some(local) => self.local(local, depth),
                None => LockPlace::opaque(),
            },
            Operand::Constant(constant) => match constant.check_static_ptr(self.tcx) {
                Some(def) => LockPlace::rooted(Root::Static(def), true),
                None => LockPlace::opaque(),
            },
            _ => LockPlace::opaque(),
        }
    }

    /// The place the pointer held in `local` points to.
    fn local(&self, local: Local, depth: usize) -> LockPlace {
        const MAX_DEPTH: usize = 16;
        if depth > MAX_DEPTH {
            return LockPlace::opaque();
        }
        let is_arg = (1..=self.body.arg_count).contains(&local.as_usize());
        let found = self.traced(local, is_arg, depth);
        let single = matches!(self.defs[local], Def::Assign(_) | Def::Call(..));
        if found.root == Root::Opaque && found.steps.is_empty() && single && !is_arg {
            return LockPlace::rooted(Root::Pointee(local), true);
        }
        found
    }

    fn traced(&self, local: Local, is_arg: bool, depth: usize) -> LockPlace {
        match self.defs[local] {
            Def::Unset if is_arg => LockPlace::rooted(Root::Param(local.as_usize() - 1), true),
            Def::Assign(rvalue) => match rvalue {
                Rvalue::Ref(_, _, place) | Rvalue::RawPtr(_, place) => {
                    self.address(*place, depth + 1)
                }
                Rvalue::Use(op, _) | Rvalue::Cast(_, op, _) => self.operand(op, depth + 1),
                Rvalue::ThreadLocalRef(def) => LockPlace::rooted(Root::Static(*def), true),
                _ => LockPlace::opaque(),
            },
            Def::Call(func, args) if self.passes_pointer_through(func) => match args.first() {
                Some(arg) => self.operand(&arg.node, depth + 1),
                None => LockPlace::opaque(),
            },
            Def::Call(func, args) if let Some(place) = self.accessor(func) => {
                let args: Vec<LockPlace> = args
                    .iter()
                    .map(|a| self.operand(&a.node, depth + 1))
                    .collect();
                place.in_caller(&args)
            }
            Def::Unset | Def::Call(..) | Def::Many => LockPlace::opaque(),
        }
    }

    /// The place `place` names, as a root and the fields after it.
    fn address(&self, place: Place<'tcx>, depth: usize) -> LockPlace {
        let projection = place.projection;
        let (mut out, skip) = match projection.first() {
            Some(ProjectionElem::Deref) => (self.local(place.local, depth), 1),
            _ => {
                let single = !matches!(self.defs[place.local], Def::Many);
                (LockPlace::rooted(Root::Local(place.local), single), 0)
            }
        };
        for (i, elem) in projection.iter().enumerate().skip(skip) {
            match elem {
                ProjectionElem::Field(field, _) => {
                    let parent = PlaceRef {
                        local: place.local,
                        projection: &projection[..i],
                    }
                    .ty(&self.body.local_decls, self.tcx);
                    let adt = parent.ty.ty_adt_def().map(|adt| adt.did());
                    let ty = PlaceRef {
                        local: place.local,
                        projection: &projection[..=i],
                    }
                    .ty(&self.body.local_decls, self.tcx)
                    .ty
                    .ty_adt_def()
                    .map(|adt| adt.did());
                    if out.steps.len() == MAX_STEPS {
                        out.exact = false;
                        continue;
                    }
                    out.steps.push(Step {
                        adt,
                        variant: parent.variant_index.unwrap_or(FIRST_VARIANT),
                        field,
                        ty,
                    });
                }
                ProjectionElem::Index(_)
                | ProjectionElem::ConstantIndex { .. }
                | ProjectionElem::Subslice { .. } => out.exact = false,
                _ => {}
            }
        }
        out
    }

    /// The place a local fn's returned pointer points to, if known.
    fn accessor(&self, func: &Operand<'tcx>) -> Option<&'a LockPlace> {
        let (def, args) = func.const_fn_def()?;
        let item = resolve_item(self.tcx, self.env, def, args).unwrap_or(def);
        self.accessors.get(&item.as_local()?)
    }

    /// The place the fn's return value points to, when it returns a
    /// pointer to a parameter's field or a static.
    pub(super) fn returned(&self) -> Option<LockPlace> {
        let ty = self.body.local_decls[RETURN_PLACE].ty;
        if !matches!(ty.kind(), ty::Ref(..) | ty::RawPtr(..)) {
            return None;
        }
        let place = self.local(RETURN_PLACE, 0);
        matches!(place.root, Root::Param(_) | Root::Static(_)).then_some(place)
    }

    /// A call that returns its first argument as another pointer to the
    /// same place: `Deref::deref`, `NonNull::as_ptr`, `as_ref` on a pointer.
    fn passes_pointer_through(&self, func: &Operand<'tcx>) -> bool {
        let Some((def, _)) = func.const_fn_def() else {
            return false;
        };
        let tcx = self.tcx;
        if let Some(t) = tcx.trait_of_assoc(def) {
            return tcx.is_lang_item(t, LangItem::Deref)
                || tcx.is_lang_item(t, LangItem::DerefMut)
                || tcx.is_diagnostic_item(sym::AsRef, t);
        }
        let krate = tcx.crate_name(def.krate);
        (krate == sym::core || krate == sym::alloc || krate == sym::std)
            && matches!(
                tcx.item_name(def).as_str(),
                "as_ref" | "as_mut" | "as_ptr" | "as_mut_ptr" | "cast" | "get_ref" | "get_mut"
            )
    }
}
