//! Which pointer parameters a fn frees, read from MIR and followed into the
//! fns it calls. A lint asks [`Frees::events`] for one body, or
//! [`Frees::summary`] for one resolved callee. Nothing here decides what to
//! report.
//!
//! A value "comes from" a parameter when it is that parameter, a copy, cast,
//! reborrow or raw pointer of it, a `core::ptr` helper applied to it
//! (`from_mut`, `cast`, `NonNull::from`), or a `Box` built from it. Field
//! projections end the chain: freeing a field is not freeing the parameter.
//!
//! A parameter is freed when a `Box` that comes from it is dropped, or when
//! it reaches `dealloc` or C `free`, or a callee that frees that parameter.
//! A callee is followed when its MIR is available: any local fn, and generic
//! or inline fns of other crates in a full build. A check build has no MIR
//! for other crates. There, a callee counts only when the configuration
//! lists it, or when its signature proves it returns a `Box` of its pointer
//! argument (`fn take<T>(p: *mut T) -> Box<T>` with no bounds on `T`).

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use rustc_hir::def::DefKind;
use rustc_hir::def_id::DefId;
use rustc_index::IndexVec;
use rustc_middle::mir::{
    BasicBlock, Body, Local, Operand, Place, ProjectionElem, Rvalue, START_BLOCK, StatementKind,
    TerminatorKind,
};
use rustc_middle::ty::{self, Instance, Ty, TyCtxt, TypingEnv};
use rustc_span::{Span, sym};

use crate::hir_shapes::strip_generic_segments;
use crate::mir_flow::{FlowGraph, mir_for};

/// How sure a free is.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) enum Reach {
    /// Some returning paths free the value and some do not, as with a
    /// reference count that frees only on its last release.
    Sometimes,
    /// Every returning path frees the value.
    Always,
}

/// One place where a body frees a value that comes from parameter `param`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct FreeEvent {
    pub(crate) param: usize,
    pub(crate) reach: Reach,
    /// The call or drop in this body.
    pub(crate) span: Span,
    /// The free itself: the deepest local-crate site on the path, or `span`.
    pub(crate) site: Span,
    /// The callee the free goes through, if it is a call.
    pub(crate) callee: Option<DefId>,
}

/// What a fn does to each of its parameters.
#[derive(Default, Debug)]
pub(crate) struct Summary {
    /// Indexed by parameter: how sure the free is, and where it happens.
    pub(crate) frees: Vec<Option<(Reach, Span)>>,
    /// The fn returns a `Box` that comes from this parameter.
    pub(crate) returns_box_of: Option<usize>,
}

/// Calls deeper than this are not followed.
const MAX_DEPTH: usize = 8;

/// Memoised summaries for one crate's lint run.
pub(crate) struct Frees<'tcx> {
    tcx: TyCtxt<'tcx>,
    memo: HashMap<Instance<'tcx>, Rc<Summary>>,
    active: HashSet<Instance<'tcx>>,
    /// Fns that free their first pointer argument on every path.
    always: &'static [String],
    /// Fns that free it on some paths, such as a reference count release.
    sometimes: &'static [String],
}

impl<'tcx> Frees<'tcx> {
    pub(crate) fn new(
        tcx: TyCtxt<'tcx>,
        always: &'static [String],
        sometimes: &'static [String],
    ) -> Self {
        Frees {
            tcx,
            memo: HashMap::new(),
            active: HashSet::new(),
            always,
            sometimes,
        }
    }

    /// The summary the configuration or the signature gives `def`.
    fn declared(&self, def: DefId) -> Option<Summary> {
        let tcx = self.tcx;
        if !matches!(tcx.def_kind(def), DefKind::Fn | DefKind::AssocFn) {
            return None;
        }
        let sig = tcx
            .fn_sig(def)
            .instantiate_identity()
            .skip_normalization()
            .skip_binder();
        let inputs = sig.inputs();
        let path = strip_generic_segments(&tcx.def_path_str(def));
        let listed = |list: &[String]| {
            list.iter()
                .any(|e| path == *e || path.ends_with(&format!("::{e}")))
        };
        let reach = if listed(self.always) {
            Reach::Always
        } else if listed(self.sometimes) {
            Reach::Sometimes
        } else {
            return Some(Summary {
                frees: vec![None; inputs.len()],
                returns_box_of: returns_box_of_pointer(tcx, def, inputs, sig.output()),
            })
            .filter(|s| s.returns_box_of.is_some());
        };
        let first = inputs.iter().position(|t| holds_pointer(tcx, *t))?;
        let mut frees = vec![None; inputs.len()];
        frees[first] = Some((reach, rustc_span::DUMMY_SP));
        Some(Summary {
            frees,
            returns_box_of: None,
        })
    }

    /// Every free in the body of local fn `def`, with its own generics.
    pub(crate) fn events(&mut self, def: rustc_hir::def_id::LocalDefId) -> Vec<FreeEvent> {
        let tcx = self.tcx;
        let instance = Instance::new_raw(
            def.to_def_id(),
            ty::GenericArgs::identity_for_item(tcx, def.to_def_id()),
        );
        let env = TypingEnv::post_analysis(tcx, def.to_def_id());
        let Some(body) = mir_for(tcx, def) else {
            return Vec::new();
        };
        self.active.insert(instance);
        let (events, _) = self.scan(&body, instance, env, 0);
        self.active.remove(&instance);
        events
    }

    /// What `instance` frees, or `None` when its MIR is not available.
    pub(crate) fn summary(
        &mut self,
        instance: Instance<'tcx>,
        env: TypingEnv<'tcx>,
        depth: usize,
    ) -> Option<Rc<Summary>> {
        if let Some(s) = self.memo.get(&instance) {
            return Some(s.clone());
        }
        if depth > MAX_DEPTH || !self.active.insert(instance) {
            return None;
        }
        let summary = self.compute(instance, env, depth);
        self.active.remove(&instance);
        let summary = Rc::new(summary?);
        self.memo.insert(instance, summary.clone());
        Some(summary)
    }

    fn compute(
        &mut self,
        instance: Instance<'tcx>,
        env: TypingEnv<'tcx>,
        depth: usize,
    ) -> Option<Summary> {
        let tcx = self.tcx;
        let ty::InstanceKind::Item(def) = instance.def else {
            return None;
        };
        let declared = self.declared(def);
        if declared
            .as_ref()
            .is_some_and(|s| s.frees.iter().any(Option::is_some))
        {
            return declared;
        }
        let (events, returns_box_of, args) = if let Some(local) = def.as_local() {
            let body = mir_for(tcx, local)?;
            let (events, ret) = self.scan(&body, instance, env, depth);
            (events, ret, body.arg_count)
        } else {
            if !tcx.is_mir_available(def) {
                return declared;
            }
            let body = tcx.optimized_mir(def);
            let (events, ret) = self.scan(body, instance, env, depth);
            (events, ret, body.arg_count)
        };
        let mut frees: Vec<Option<(Reach, Span)>> = vec![None; args];
        for e in events {
            let slot = &mut frees[e.param];
            if slot.is_none_or(|(r, _)| r < e.reach) {
                *slot = Some((e.reach, e.site));
            }
        }
        Some(Summary {
            frees,
            returns_box_of,
        })
    }

    /// The frees in `body` and the parameter a returned `Box` comes from.
    fn scan(
        &mut self,
        body: &Body<'tcx>,
        instance: Instance<'tcx>,
        env: TypingEnv<'tcx>,
        depth: usize,
    ) -> (Vec<FreeEvent>, Option<usize>) {
        let tcx = self.tcx;
        let ty_of = |t: Ty<'tcx>| {
            instance
                .try_instantiate_mir_and_normalize_erasing_regions(
                    tcx,
                    env,
                    ty::EarlyBinder::bind(tcx, t),
                )
                .ok()
        };
        let mut from: IndexVec<Local, Option<usize>> =
            IndexVec::from_elem_n(None, body.local_decls.len());
        for (i, arg) in body.args_iter().enumerate() {
            from[arg] = Some(i);
        }
        let flow = FlowGraph::new(body);
        let on_every_path =
            |bb: BasicBlock| bb == START_BLOCK || flow.ipdom_chain(START_BLOCK).any(|b| b == bb);
        // Resolved callees per block, filled once.
        let mut callees: HashMap<BasicBlock, Instance<'tcx>> = HashMap::new();
        for (bb, data) in body.basic_blocks.iter_enumerated() {
            if let TerminatorKind::Call { func, .. } = &data.terminator().kind
                && let Some(callee) = resolve(tcx, instance, env, func)
            {
                callees.insert(bb, callee);
            }
        }
        // A callee is read only once a pointer from a parameter reaches it.
        let mut summaries: HashMap<BasicBlock, Rc<Summary>> = HashMap::new();
        // Spread "comes from" to a fixed point; each local takes one origin.
        let mut changed = true;
        while changed {
            changed = false;
            let mut set =
                |from: &mut IndexVec<Local, Option<usize>>, l: Local, o: Option<usize>| {
                    if from[l].is_none() && o.is_some() {
                        from[l] = o;
                        changed = true;
                    }
                };
            for (bb, data) in body.basic_blocks.iter_enumerated() {
                for stmt in &data.statements {
                    let StatementKind::Assign(assign) = &stmt.kind else {
                        continue;
                    };
                    let (dest, rv) = &**assign;
                    let Some(dest) = dest.as_local() else {
                        continue;
                    };
                    let o = match rv {
                        Rvalue::Use(op, ..) | Rvalue::Cast(_, op, _) => operand_from(&from, op),
                        Rvalue::Ref(_, _, p) | Rvalue::RawPtr(_, p) => place_from(&from, *p),
                        Rvalue::Aggregate(_, ops) => {
                            ops.iter().find_map(|op| operand_from(&from, op))
                        }
                        _ => None,
                    };
                    set(&mut from, dest, o);
                }
                let TerminatorKind::Call {
                    args, destination, ..
                } = &data.terminator().kind
                else {
                    continue;
                };
                let Some(&callee) = callees.get(&bb) else {
                    continue;
                };
                let def = callee.def_id();
                let passthrough = is_box_from_raw(tcx, def) || is_ptr_helper(tcx, def);
                let reached = args.iter().any(|a| {
                    operand_from(&from, &a.node).is_some()
                        && ty_of(a.node.ty(body, tcx)).is_some_and(|t| holds_pointer(tcx, t))
                });
                if !passthrough
                    && reached
                    && !is_raw_free_def(tcx, def)
                    && !summaries.contains_key(&bb)
                    && let Some(s) = self.summary(callee, env, depth + 1)
                {
                    summaries.insert(bb, s);
                }
                let Some(dest) = destination.as_local() else {
                    continue;
                };
                let o = if passthrough {
                    args.first().and_then(|a| operand_from(&from, &a.node))
                } else {
                    summaries
                        .get(&bb)
                        .and_then(|s| s.returns_box_of)
                        .and_then(|i| args.get(i))
                        .and_then(|a| operand_from(&from, &a.node))
                };
                set(&mut from, dest, o);
            }
        }
        let mut events = Vec::new();
        let sm = tcx.sess.source_map();
        for (bb, data) in body.basic_blocks.iter_enumerated() {
            if data.is_cleanup || !flow.contains(bb) {
                continue;
            }
            let term = data.terminator();
            let span = term.source_info.span;
            // In the fn being checked, a free on any path is a finding. In a callee,
            // a free on only some paths makes the callee a "sometimes" free.
            let path = if depth == 0 || on_every_path(bb) {
                Reach::Always
            } else {
                Reach::Sometimes
            };
            match &term.kind {
                TerminatorKind::Drop { place, .. } => {
                    if let Some(l) = place.as_local()
                        && let Some(param) = from[l]
                        && ty_of(body.local_decls[l].ty).is_some_and(|t| t.is_box())
                    {
                        events.push(FreeEvent {
                            param,
                            reach: path,
                            span,
                            site: span,
                            callee: None,
                        });
                    }
                }
                TerminatorKind::Call { func, args, .. } => {
                    let freed_args: Vec<(usize, Reach, Span)> = if is_raw_free(tcx, func) {
                        vec![(0, Reach::Always, span)]
                    } else if let Some(s) = summaries.get(&bb) {
                        s.frees
                            .iter()
                            .enumerate()
                            .filter_map(|(j, f)| f.map(|(r, site)| (j, r, site)))
                            .filter(|(j, ..)| {
                                args.get(*j).is_some_and(|a| {
                                    ty_of(a.node.ty(body, tcx))
                                        .is_some_and(|t| holds_pointer(tcx, t))
                                })
                            })
                            .map(|(j, r, site)| {
                                let local = !sm.is_imported(site) && !site.is_dummy();
                                (j, r, if local { site } else { span })
                            })
                            .collect()
                    } else {
                        continue;
                    };
                    for (j, r, site) in freed_args {
                        let Some(param) = args.get(j).and_then(|a| operand_from(&from, &a.node))
                        else {
                            continue;
                        };
                        events.push(FreeEvent {
                            param,
                            reach: r.min(path),
                            span,
                            site,
                            callee: callees.get(&bb).map(|c| c.def_id()),
                        });
                    }
                }
                _ => {}
            }
        }
        let ret = Place::return_place().local;
        let returns_box_of =
            from[ret].filter(|_| ty_of(body.local_decls[ret].ty).is_some_and(|t| t.is_box()));
        (events, returns_box_of)
    }
}

/// The parameter that `&*x` or `&raw *x` comes from: the address of `*x` is
/// the value of `x`. Any other place has an address of its own.
fn place_from(from: &IndexVec<Local, Option<usize>>, place: Place<'_>) -> Option<usize> {
    match place.projection.as_slice() {
        [ProjectionElem::Deref] => from[place.local],
        _ => None,
    }
}

/// The parameter a copy or move of a whole local comes from. Reading through
/// a deref reads the value behind it, which is some other pointer.
fn operand_from(from: &IndexVec<Local, Option<usize>>, op: &Operand<'_>) -> Option<usize> {
    op.place().and_then(|p| p.as_local()).and_then(|l| from[l])
}

/// A raw pointer, `NonNull` or `Box`: a value a free can go through. A
/// reference cannot: the callee that frees it is reported on its own.
fn holds_pointer(tcx: TyCtxt<'_>, t: Ty<'_>) -> bool {
    match t.kind() {
        ty::RawPtr(..) => true,
        ty::Adt(adt, _) => {
            t.is_box()
                || tcx
                    .get_diagnostic_name(adt.did())
                    .is_some_and(|n| n.as_str() == "NonNull")
        }
        _ => false,
    }
}

/// The pointer argument a `Box<T>` result must come from, when `T` is a type
/// parameter with no bounds and that argument is the only input naming it.
/// With nothing known about `T`, the fn cannot make a `T` of its own.
fn returns_box_of_pointer<'tcx>(
    tcx: TyCtxt<'tcx>,
    def: DefId,
    inputs: &[Ty<'tcx>],
    output: Ty<'tcx>,
) -> Option<usize> {
    let param = output.boxed_ty()?;
    if !matches!(param.kind(), ty::Param(_)) {
        return None;
    }
    let bounded = tcx.param_env(def).caller_bounds().any(|clause| {
        clause.as_trait_clause().is_some_and(|t| {
            t.self_ty().skip_binder() == param && !tcx.is_sizedness_trait(t.def_id())
        })
    });
    let mut naming = inputs
        .iter()
        .enumerate()
        .filter(|(_, t)| t.walk().any(|a| a.as_type() == Some(param)));
    let (i, t) = naming.next()?;
    let points_at = match t.kind() {
        ty::RawPtr(inner, _) => *inner == param,
        ty::Adt(_, a) => holds_pointer(tcx, *t) && a.type_at(0) == param,
        _ => false,
    };
    (!bounded && naming.next().is_none() && points_at).then_some(i)
}

/// The callee of `func`, resolved with `instance`'s generic arguments.
fn resolve<'tcx>(
    tcx: TyCtxt<'tcx>,
    instance: Instance<'tcx>,
    env: TypingEnv<'tcx>,
    func: &Operand<'tcx>,
) -> Option<Instance<'tcx>> {
    let (def, args) = func.const_fn_def()?;
    let args = instance
        .try_instantiate_mir_and_normalize_erasing_regions(
            tcx,
            env,
            ty::EarlyBinder::bind(tcx, args),
        )
        .ok()?;
    Instance::try_resolve(tcx, env, def, args).ok()?
}

/// `Box::from_raw` and its `NonNull` and allocator forms.
fn is_box_from_raw(tcx: TyCtxt<'_>, def: DefId) -> bool {
    tcx.crate_name(def.krate) == sym::alloc
        && matches!(
            tcx.item_name(def).as_str(),
            "from_raw" | "from_raw_in" | "from_non_null" | "from_non_null_in"
        )
        && tcx.impl_of_assoc(def).is_some_and(|i| {
            tcx.type_of(i)
                .instantiate_identity()
                .skip_normalization()
                .is_box()
        })
}

/// A `core::ptr` fn that returns its first argument as another pointer:
/// `from_mut`, `cast`, `cast_mut`, `NonNull::as_ptr`, `NonNull::from`.
fn is_ptr_helper(tcx: TyCtxt<'_>, def: DefId) -> bool {
    if tcx.crate_name(def.krate) != sym::core
        || !matches!(tcx.def_kind(def), DefKind::Fn | DefKind::AssocFn)
    {
        return false;
    }
    let path = tcx.def_path(def);
    let in_ptr = path
        .data
        .first()
        .is_some_and(|d| d.data.get_opt_name() == Some(sym::ptr));
    let returns_pointer = holds_pointer(
        tcx,
        tcx.fn_sig(def)
            .instantiate_identity()
            .skip_binder()
            .output(),
    );
    in_ptr && returns_pointer
}

/// `dealloc(p, layout)` from `alloc`, or a C `free(p)` / `mi_free(p)`.
fn is_raw_free(tcx: TyCtxt<'_>, func: &Operand<'_>) -> bool {
    func.const_fn_def()
        .is_some_and(|(def, _)| is_raw_free_def(tcx, def))
}

fn is_raw_free_def(tcx: TyCtxt<'_>, def: DefId) -> bool {
    let name = tcx.item_name(def);
    if tcx.is_foreign_item(def) {
        return matches!(name.as_str(), "free" | "mi_free");
    }
    tcx.crate_name(def.krate) == sym::alloc && name.as_str() == "dealloc"
}
