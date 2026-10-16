//! `lock_order` and `lock_taken_twice`. Each body is read for the locks it
//! takes and the blocks that run while each is held. A summary per fn says
//! which locks a call to it may take. A lock taken while another is held is
//! an edge between the two. A cycle of edges can deadlock.

mod body;
mod place;

use rustc_data_structures::fx::{FxHashMap, FxHashSet, FxIndexMap};
use rustc_hir::def_id::{DefId, LocalDefId};
use rustc_lint::{LateContext, LateLintPass};
use rustc_middle::mir::BasicBlock;
use rustc_middle::ty::TyCtxt;
use rustc_span::Span;

use self::body::{Config, Facts, Kinds, Prims, Taken, scan, skipped};
use self::place::{Class, LockPlace, Root, class_name};
use crate::MordantConfig;
use crate::baseline::emit_hir_then;
use crate::call_graph::{CallGraph, Edge, EdgeKind};
use crate::mir_flow::mir_for;

rustc_lint::declare_lint! {
    /// Flags locks the crate takes in a cycle: one place locks `b` while `a`
    /// is held, and another locks `a` while `b` is held, directly or through
    /// a chain of calls. Two threads, one on each path, can each hold one
    /// lock and wait for the other forever.
    ///
    /// A lock is a call that returns a guard (std, `parking_lot` and
    /// `lock_api` guards, a type marked `#[clippy::has_significant_drop]`,
    /// or one named in `lock-guard-types`), or a call to a `lock-methods`
    /// entry. It is held until the guard is dropped or the matching
    /// `unlock-methods` call. A lock is named by the struct field it lives
    /// in, so `self.a` locked in two methods of one struct is one lock.
    /// Calls are followed when they resolve to one body in this crate.
    pub LOCK_ORDER,
    Warn,
    "locks taken in a cycle of orders"
}

rustc_lint::declare_lint! {
    /// Flags a lock taken again while the same thread holds it, directly or
    /// in a function it calls. A `Mutex` is not reentrant, so the thread
    /// waits for itself forever. Only the same place counts: the same field
    /// of the same parameter or static, never two values of one type. Two
    /// read locks are not flagged, and nor are reentrant locks.
    pub LOCK_TAKEN_TWICE,
    Warn,
    "a lock taken again while the same thread holds it"
}

pub struct LockOrder {
    config: Config,
}

impl LockOrder {
    pub fn new(config: &MordantConfig) -> Self {
        Self {
            config: Config {
                guard_types: config.lock_guard_types.clone(),
                lock_methods: config.lock_methods.clone(),
                unlock_methods: config.unlock_methods.clone(),
                reentrant_types: config.reentrant_lock_types.clone(),
            },
        }
    }
}

rustc_lint::impl_lint_pass!(LockOrder => [LOCK_ORDER, LOCK_TAKEN_TWICE]);

/// A lock a call to some fn may take, in that fn's terms.
#[derive(Clone)]
struct Reach {
    taken: Taken,
    class: Class,
    /// Where the lock is finally taken.
    site: Span,
    /// The calls from the fn down to `site`.
    chain: Chain,
}

/// Why there is an edge from one lock to another.
#[derive(Clone)]
struct Witness {
    def: LocalDefId,
    /// Where the held lock was taken.
    held: Span,
    /// The acquisition or the call that takes the second lock.
    at: Span,
    chain: Chain,
}

/// The calls from a fn down to where a lock is taken.
type Chain = Vec<(DefId, Span)>;

/// A lock taken again while held.
struct Twice {
    def: LocalDefId,
    at: Span,
    first: Span,
    name: String,
    chain: Chain,
    site: Span,
}

/// A lock a terminator takes: where, through which calls, and where the
/// last of them takes it.
struct Second {
    taken: Taken,
    at: Span,
    chain: Chain,
    site: Span,
}

/// The longest call chain a summary keeps.
const MAX_CHAIN: usize = 6;
/// The most locks one summary keeps.
const MAX_REACH: usize = 64;

impl<'tcx> LateLintPass<'tcx> for LockOrder {
    fn check_crate_post(&mut self, cx: &LateContext<'tcx>) {
        let tcx = cx.tcx;
        let graph = CallGraph::get(tcx);
        let kinds = Kinds::new(tcx, &self.config);
        let facts = read_bodies(&kinds, graph);
        if facts.values().all(|f| f.acqs.is_empty()) {
            return;
        }
        let lock_types: FxHashSet<DefId> = facts
            .values()
            .flat_map(|f| f.acqs.iter().filter_map(|a| a.lock_ty))
            .collect();
        let reach = summarize(graph, &facts, &lock_types);
        let mut edges: FxIndexMap<(Class, Class), Witness> = FxIndexMap::default();
        let mut twice: Vec<Twice> = Vec::new();
        let mut seen_twice: FxHashSet<Span> = FxHashSet::default();
        for (&def, fact) in &facts {
            let by_block = edges_by_block(graph, def);
            for held in &fact.acqs {
                let held_class = held.taken.place.class(&lock_types);
                for b in held.held.iter() {
                    for Second {
                        taken,
                        at,
                        chain,
                        site,
                    } in taken_at(fact, &by_block, &facts, &reach, b)
                    {
                        if is_twice(&held.taken, &taken) {
                            if seen_twice.insert(at) {
                                let name = describe(tcx, def, &held.taken.place);
                                twice.push(Twice {
                                    def,
                                    at,
                                    first: held.span,
                                    name,
                                    chain,
                                    site,
                                });
                            }
                            continue;
                        }
                        let (Some(from), Some(to)) = (&held_class, taken.place.class(&lock_types))
                        else {
                            continue;
                        };
                        if *from != to {
                            edges.entry((from.clone(), to)).or_insert(Witness {
                                def,
                                held: held.span,
                                at,
                                chain,
                            });
                        }
                    }
                }
            }
        }
        report_twice(cx, twice);
        report_cycles(cx, &edges);
    }
}

/// Every body's facts. A body is read again when a local fn it calls turns
/// out to return a guard, leave a lock held, or unlock one.
fn read_bodies(kinds: &Kinds<'_, '_>, graph: &CallGraph) -> FxIndexMap<LocalDefId, Facts> {
    let tcx = kinds.tcx;
    let bodies: Vec<LocalDefId> = graph.bodies().filter(|&d| !skipped(tcx, d)).collect();
    let callers = graph.callers();
    let accessors = accessors(tcx, &bodies);
    let mut prims: FxHashMap<LocalDefId, Prims> = FxHashMap::default();
    let mut facts: FxIndexMap<LocalDefId, Facts> = FxIndexMap::default();
    let mut todo: Vec<LocalDefId> = bodies.clone();
    for _round in 0..4 {
        let mut changed: Vec<LocalDefId> = Vec::new();
        for &def in &todo {
            let Some(body) = mir_for(tcx, def) else {
                continue;
            };
            let fact = scan(kinds, graph, def, &body, &prims, &accessors);
            let old = prims.get(&def).cloned().unwrap_or_default();
            if fact.prims != old {
                changed.push(def);
                if fact.prims == Prims::default() {
                    prims.remove(&def);
                } else {
                    prims.insert(def, fact.prims.clone());
                }
            }
            facts.insert(def, fact);
        }
        let mut next: FxHashSet<LocalDefId> = FxHashSet::default();
        for def in changed {
            for call in callers.get(&def).into_iter().flatten() {
                next.insert(call.caller);
            }
        }
        if next.is_empty() {
            break;
        }
        todo = bodies
            .iter()
            .copied()
            .filter(|d| next.contains(d))
            .collect();
    }
    facts.sort_by_cached_key(|def, _| bodies.iter().position(|d| d == def));
    facts
}

/// For each local fn that returns a pointer to a parameter's field or a
/// static, the place it points to. Two rounds see an accessor that calls
/// another.
fn accessors(tcx: TyCtxt<'_>, bodies: &[LocalDefId]) -> FxHashMap<LocalDefId, LockPlace> {
    let mut found: FxHashMap<LocalDefId, LockPlace> = FxHashMap::default();
    for _round in 0..2 {
        let mut next = found.clone();
        for &def in bodies {
            let Some(body) = mir_for(tcx, def) else {
                continue;
            };
            let env = rustc_middle::ty::TypingEnv::post_analysis(tcx, def);
            if let Some(place) = place::Tracer::new(tcx, &body, env, &found).returned() {
                next.insert(def, place);
            }
        }
        found = next;
    }
    found
}

/// For each fn, the locks a call to it may take, in its own terms.
fn summarize(
    graph: &CallGraph,
    facts: &FxIndexMap<LocalDefId, Facts>,
    lock_types: &FxHashSet<DefId>,
) -> FxHashMap<LocalDefId, Vec<Reach>> {
    let mut reach: FxHashMap<LocalDefId, Vec<Reach>> = FxHashMap::default();
    for (&def, fact) in facts {
        let mut own: Vec<Reach> = Vec::new();
        for acq in fact.acqs.iter().filter(|a| a.taken.mode.blocking) {
            let taken = for_callers(&acq.taken);
            if let Some(class) = acq.taken.place.class(lock_types) {
                add_reach(
                    &mut own,
                    Reach {
                        taken,
                        class,
                        site: acq.span,
                        chain: Vec::new(),
                    },
                );
            }
        }
        if !own.is_empty() {
            reach.insert(def, own);
        }
    }
    for _round in 0..12 {
        let mut changed = false;
        for (&def, fact) in facts {
            let mut add: Vec<Reach> = Vec::new();
            for edge in graph.edges(def) {
                let Some(callee) = walked_callee(edge, facts) else {
                    continue;
                };
                let (Some(from), Some(args)) = (reach.get(&callee), fact.args.get(&edge.block))
                else {
                    continue;
                };
                for r in from {
                    if r.chain.len() >= MAX_CHAIN {
                        continue;
                    }
                    let place = r.taken.place.in_caller(args);
                    let mut chain = vec![(callee.to_def_id(), edge.span)];
                    chain.extend(r.chain.iter().copied());
                    add.push(Reach {
                        taken: for_callers(&Taken {
                            place,
                            mode: r.taken.mode,
                        }),
                        class: r.class.clone(),
                        site: r.site,
                        chain,
                    });
                }
            }
            let entry = reach.entry(def).or_default();
            for r in add {
                changed |= add_reach(entry, r);
            }
        }
        if !changed {
            break;
        }
    }
    reach
}

/// The local fn a call or drop runs, when the lock walk goes into it.
fn walked_callee(edge: &Edge, facts: &FxIndexMap<LocalDefId, Facts>) -> Option<LocalDefId> {
    match edge.kind {
        EdgeKind::Call | EdgeKind::Drop => {
            let callee = edge.local_callee()?;
            let fact = facts.get(&callee)?;
            (!fact.primitive).then_some(callee)
        }
        // A drop that may not run says nothing certain about locks.
        EdgeKind::MaybeDrop | EdgeKind::Panic | EdgeKind::Closure => None,
    }
}

fn add_reach(into: &mut Vec<Reach>, r: Reach) -> bool {
    if into.len() >= MAX_REACH
        || into
            .iter()
            .any(|o| o.class == r.class && o.taken.place == r.taken.place)
    {
        return false;
    }
    into.push(r);
    true
}

/// `taken` as a caller can use it: this body's locals mean nothing there.
fn for_callers(taken: &Taken) -> Taken {
    match taken.place.root {
        Root::Param(_) | Root::Static(_) => taken.clone(),
        Root::Local(_) | Root::Pointee(_) | Root::Opaque => Taken {
            place: LockPlace {
                root: Root::Opaque,
                steps: taken.place.steps.clone(),
                exact: false,
            },
            mode: taken.mode,
        },
    }
}

/// The local edges of `def` that the lock walk follows, by block.
fn edges_by_block(graph: &CallGraph, def: LocalDefId) -> FxHashMap<BasicBlock, Vec<Edge>> {
    let mut out: FxHashMap<BasicBlock, Vec<Edge>> = FxHashMap::default();
    for edge in graph.edges(def) {
        out.entry(edge.block).or_default().push(*edge);
    }
    out
}

/// The locks taken by the terminator of block `b`: an acquisition there,
/// or the locks a called fn may take, in the caller's terms.
fn taken_at(
    fact: &Facts,
    by_block: &FxHashMap<BasicBlock, Vec<Edge>>,
    facts: &FxIndexMap<LocalDefId, Facts>,
    reach: &FxHashMap<LocalDefId, Vec<Reach>>,
    b: BasicBlock,
) -> Vec<Second> {
    let mut out = Vec::new();
    for acq in fact
        .acqs
        .iter()
        .filter(|a| a.block == b && a.taken.mode.blocking)
    {
        out.push(Second {
            taken: acq.taken.clone(),
            at: acq.span,
            chain: Vec::new(),
            site: acq.span,
        });
    }
    let Some(args) = fact.args.get(&b) else {
        return out;
    };
    for edge in by_block.get(&b).into_iter().flatten() {
        let Some(callee) = walked_callee(edge, facts) else {
            continue;
        };
        for r in reach.get(&callee).into_iter().flatten() {
            let place = r.taken.place.in_caller(args);
            let mut chain = vec![(callee.to_def_id(), edge.span)];
            chain.extend(r.chain.iter().copied());
            out.push(Second {
                taken: Taken {
                    place,
                    mode: r.taken.mode,
                },
                at: edge.span,
                chain,
                site: r.site,
            });
        }
    }
    out
}

/// The second lock is surely the held one, and the two wait on each other.
fn is_twice(held: &Taken, taken: &Taken) -> bool {
    let both_read = held.mode.shared && taken.mode.shared;
    let reentrant = held.mode.reentrant || taken.mode.reentrant;
    held.place.same_lock(&taken.place) && !both_read && !reentrant && taken.mode.blocking
}

fn describe(tcx: TyCtxt<'_>, def: LocalDefId, place: &LockPlace) -> String {
    match mir_for(tcx, def) {
        Some(body) => place.render(tcx, &body),
        None => "the lock".to_string(),
    }
}

/// `through `a` -> `b``, naming the calls that lead to the lock.
fn through(tcx: TyCtxt<'_>, chain: &[(DefId, Span)]) -> String {
    if chain.is_empty() {
        return String::new();
    }
    let names: Vec<String> = chain
        .iter()
        .map(|(def, _)| format!("`{}`", tcx.def_path_str(*def)))
        .collect();
    format!(", through {}", names.join(" -> "))
}

fn report_twice(cx: &LateContext<'_>, mut twice: Vec<Twice>) {
    let tcx = cx.tcx;
    twice.sort_by_key(|t| t.at.lo());
    for Twice {
        def,
        at,
        first,
        name,
        chain,
        site,
    } in twice
    {
        let msg = if chain.is_empty() {
            format!(
                "`{name}` is locked here while this function already holds it. The lock is not \
                 reentrant, so the thread waits for itself forever"
            )
        } else {
            format!(
                "`{name}` is locked again here{}, while this function already holds it. The lock is \
                 not reentrant, so the thread waits for itself forever",
                through(tcx, &chain),
            )
        };
        let hir_id = tcx.local_def_id_to_hir_id(def);
        emit_hir_then(cx, LOCK_TAKEN_TWICE, hir_id, at, msg, |diag| {
            diag.span_note(first, format!("`{name}` is first locked here"));
            if site != at {
                diag.span_note(
                    site,
                    "the call path ends here, where the lock is taken again",
                );
            }
            diag.help("release the first guard before this point, or pass the guard down instead of locking again");
        });
    }
}

/// One finding per group of locks that reach each other, with the shortest
/// cycle through the first lock by name.
fn report_cycles(cx: &LateContext<'_>, edges: &FxIndexMap<(Class, Class), Witness>) {
    let tcx = cx.tcx;
    let mut names: FxHashMap<Class, String> = FxHashMap::default();
    for (a, b) in edges.keys() {
        for class in [a, b] {
            names
                .entry(class.clone())
                .or_insert_with(|| class_name(tcx, class));
        }
    }
    let mut nodes: Vec<Class> = names.keys().cloned().collect();
    nodes.sort_by_cached_key(|c| (names[c].clone(), format!("{c:?}")));
    let mut succ: FxHashMap<Class, Vec<Class>> = FxHashMap::default();
    for (a, b) in edges.keys() {
        succ.entry(a.clone()).or_default().push(b.clone());
    }
    for list in succ.values_mut() {
        list.sort_by_cached_key(|c| (names[c].clone(), format!("{c:?}")));
    }
    let mut findings: Vec<(Span, Vec<(Class, Class)>)> = Vec::new();
    for group in groups(&nodes, &succ) {
        if group.len() < 2 {
            continue;
        }
        let Some(cycle) = shortest_cycle(&group[0], &group, &succ) else {
            continue;
        };
        let at = edges[&cycle[0]].at;
        findings.push((at, cycle));
    }
    findings.sort_by_key(|(at, _)| at.lo());
    for (at, cycle) in findings {
        let locks: Vec<String> = cycle
            .iter()
            .map(|(a, _)| format!("`{}`", names[a]))
            .collect();
        let first = &edges[&cycle[0]];
        let lead = format!(
            "`{}` is locked here while `{}` is held{}. ",
            names[&cycle[0].1],
            names[&cycle[0].0],
            through(tcx, &first.chain),
        );
        let msg = if locks.len() == 2 {
            format!(
                "{lead}Another place takes them in the other order. Two threads, one on each \
                 path, can each hold one lock and wait for the other forever",
            )
        } else {
            format!(
                "{lead}{} are locked in a cycle, each while the one before it is held. Threads \
                 on these paths can wait for each other forever",
                crate::baseline::join(&locks, "and"),
            )
        };
        let hir_id = tcx.local_def_id_to_hir_id(first.def);
        emit_hir_then(cx, LOCK_ORDER, hir_id, at, msg, |diag| {
            diag.span_note(
                first.held,
                format!(
                    "`{}` is locked here and still held where the warning points",
                    names[&cycle[0].0]
                ),
            );
            for (a, b) in &cycle[1..] {
                let w = &edges[&(a.clone(), b.clone())];
                let line = tcx.sess.source_map().lookup_char_pos(w.held.lo()).line;
                diag.span_note(
                    w.at,
                    format!(
                        "`{}` is locked here while `{}`, locked on line {line}, is held{}",
                        names[b],
                        names[a],
                        through(tcx, &w.chain),
                    ),
                );
            }
            diag.help("pick one order for these locks and take them in that order everywhere");
        });
    }
}

/// The groups of locks in which each lock can reach every other one, each
/// group in `nodes` order, and the groups in the order of their first lock.
fn groups(nodes: &[Class], succ: &FxHashMap<Class, Vec<Class>>) -> Vec<Vec<Class>> {
    struct State<'a> {
        succ: &'a FxHashMap<Class, Vec<Class>>,
        index: FxHashMap<Class, usize>,
        low: FxHashMap<Class, usize>,
        stack: Vec<Class>,
        on_stack: FxHashSet<Class>,
        out: Vec<Vec<Class>>,
    }
    fn visit(s: &mut State<'_>, v: &Class) {
        let i = s.index.len();
        s.index.insert(v.clone(), i);
        s.low.insert(v.clone(), i);
        s.stack.push(v.clone());
        s.on_stack.insert(v.clone());
        for w in s.succ.get(v).into_iter().flatten() {
            if !s.index.contains_key(w) {
                visit(s, w);
                let low = s.low[v].min(s.low[w]);
                s.low.insert(v.clone(), low);
            } else if s.on_stack.contains(w) {
                let low = s.low[v].min(s.index[w]);
                s.low.insert(v.clone(), low);
            }
        }
        if s.low[v] == s.index[v] {
            let mut group = Vec::new();
            while let Some(w) = s.stack.pop() {
                s.on_stack.remove(&w);
                let done = w == *v;
                group.push(w);
                if done {
                    break;
                }
            }
            s.out.push(group);
        }
    }
    let mut s = State {
        succ,
        index: FxHashMap::default(),
        low: FxHashMap::default(),
        stack: Vec::new(),
        on_stack: FxHashSet::default(),
        out: Vec::new(),
    };
    for v in nodes {
        if !s.index.contains_key(v) {
            visit(&mut s, v);
        }
    }
    let rank: FxHashMap<&Class, usize> = nodes.iter().enumerate().map(|(i, c)| (c, i)).collect();
    for group in &mut s.out {
        group.sort_by_key(|c| rank[c]);
    }
    s.out.sort_by_key(|g| rank[&g[0]]);
    s.out
}

/// The shortest cycle from `start` back to it inside `group`, as edges.
fn shortest_cycle(
    start: &Class,
    group: &[Class],
    succ: &FxHashMap<Class, Vec<Class>>,
) -> Option<Vec<(Class, Class)>> {
    let inside: FxHashSet<&Class> = group.iter().collect();
    let mut parent: FxHashMap<Class, Class> = FxHashMap::default();
    let mut queue = std::collections::VecDeque::from([start.clone()]);
    while let Some(cur) = queue.pop_front() {
        for next in succ.get(&cur).into_iter().flatten() {
            if !inside.contains(next) {
                continue;
            }
            if next == start {
                let mut cycle = vec![(cur.clone(), start.clone())];
                let mut at = cur.clone();
                while at != *start {
                    let prev = parent[&at].clone();
                    cycle.push((prev.clone(), at));
                    at = prev;
                }
                cycle.reverse();
                return Some(cycle);
            }
            if !parent.contains_key(next) && next != start {
                parent.insert(next.clone(), cur.clone());
                queue.push_back(next.clone());
            }
        }
    }
    None
}
