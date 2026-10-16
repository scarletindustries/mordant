use std::collections::HashSet;

use crate::adt_facts::field_ty;
use crate::baseline::{emit_with_note, join};
use crate::field_writes::{self, WriteKind};
use rustc_hir::HirId;
use rustc_hir::def_id::DefId;
use rustc_lint::{LateContext, LateLintPass};
use rustc_span::{Span, Symbol};

rustc_lint::declare_lint! {
    /// Flags bool fields that are always assigned together: every write to
    /// one sits in the same block as a write to the other, across at least two
    /// functions. Together they hold one state, and the struct lets them
    /// drift apart. One enum field naming those states cannot.
    ///
    /// Only fires on structs private to the crate. One lone write to either
    /// field anywhere disproves the pairing and silences the lint. A write is
    /// `s.f = ..` or `s.f |= ..`, direct or through a `Box`, guard or other
    /// `Deref` container. Any `&mut` to a field, a `ref mut` binding
    /// included, counts as a lone write.
    pub PARALLEL_BOOLS,
    Warn,
    "bool fields only ever assigned together"
}

#[derive(Default)]
pub struct ParallelBools;

rustc_lint::impl_lint_pass!(ParallelBools => [PARALLEL_BOOLS]);

/// The (body, block) pairs a field is written in. A write with no block
/// around it is a site of its own.
type Sites = HashSet<(DefId, HirId)>;

impl<'tcx> LateLintPass<'tcx> for ParallelBools {
    fn check_crate_post(&mut self, cx: &LateContext<'tcx>) {
        let facts = field_writes::facts();
        for did in facts.variants(cx.tcx) {
            if !facts.access(did).is_some_and(|r| r.private) {
                continue;
            }
            let adt = cx.tcx.adt_def(did);
            let bools: Vec<Symbol> = adt
                .non_enum_variant()
                .fields
                .iter()
                .filter(|f| field_ty(cx, f).is_bool())
                .map(|f| f.name)
                .collect();
            if bools.len() < 2 {
                continue;
            }
            let mut fields: Vec<(Symbol, Sites, Span)> = Vec::new();
            for f in bools {
                let writes = facts.writes(did, f);
                // A `&mut` can change the field alone, so it is never part
                // of a pair, even when a pattern takes every field at once.
                if writes
                    .iter()
                    .any(|w| matches!(w.kind, WriteKind::MutBorrow))
                {
                    continue;
                }
                let Some(first) = writes.iter().map(|w| w.span).min() else {
                    continue;
                };
                let sites = writes
                    .iter()
                    .map(|w| (w.body, w.block.unwrap_or(w.hir_id)))
                    .collect();
                fields.push((f, sites, first));
            }
            fields.sort_by_key(|(f, ..)| f.as_str().to_owned());
            // Partition by identical site sets.
            let mut groups: Vec<(&Sites, Vec<Symbol>, Span)> = Vec::new();
            for (field, sites, first) in &fields {
                match groups.iter_mut().find(|(s, ..)| *s == sites) {
                    Some((_, members, _)) => members.push(*field),
                    None => groups.push((sites, vec![*field], *first)),
                }
            }
            for (sites, members, first) in groups {
                let bodies: HashSet<DefId> = sites.iter().map(|(b, _)| *b).collect();
                if members.len() < 2 || sites.len() < 2 || bodies.len() < 2 {
                    continue;
                }
                let names: Vec<String> = members.iter().map(|s| format!("`{s}`")).collect();
                let anded = join(&names, "and");
                let names = names.join(", ");
                emit_with_note(
                    cx,
                    PARALLEL_BOOLS,
                    cx.tcx.def_span(did),
                    format!(
                        "{anded} of `{}` are always assigned together ({} blocks in {} functions). Together they hold one state, and the struct lets them drift apart",
                        cx.tcx.def_path_str(did),
                        sites.len(),
                        bodies.len(),
                    ),
                    first,
                    "one of the blocks that sets both",
                    format!(
                        "replace {names} with one enum field whose variants are the states those blocks set"
                    ),
                );
            }
        }
    }
}
