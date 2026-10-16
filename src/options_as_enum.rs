use crate::adt_facts::{field_ty, is_option_ty};
use crate::baseline::emit_with_note;
use crate::field_writes;
use rustc_hir::def_id::DefId;
use rustc_lint::{LateContext, LateLintPass};
use rustc_middle::ty;
use rustc_span::{Span, Symbol};

use crate::MordantConfig;

rustc_lint::declare_lint! {
    /// Flags a struct whose `Option` fields are alternatives: none of the
    /// places it is built sets more than one of them to `Some`, but the
    /// struct allows both at once. One enum field with a variant per case
    /// says what the code means and cannot hold two at once.
    ///
    /// Only fires on structs private to the crate, when every build writes
    /// each `Option` field as `Some(..)` or `None` and nothing writes one
    /// later. A later write is an assignment, a compound assignment or a
    /// `&mut` to the field, such as `.insert(..)`, direct or through a
    /// `Box`, guard or other `Deref` container. A derived `Default` sets
    /// every field to `None`, so it passes and is not counted.
    pub OPTIONS_AS_ENUM,
    Warn,
    "struct whose Option fields are never populated together"
}

pub struct OptionsAsEnum {
    min_fields: usize,
}

rustc_lint::impl_lint_pass!(OptionsAsEnum => [OPTIONS_AS_ENUM]);

impl OptionsAsEnum {
    pub fn new(config: &MordantConfig) -> Self {
        Self {
            min_fields: config.options_as_enum_min_fields,
        }
    }
}

fn option_fields(cx: &LateContext<'_>, adt: ty::AdtDef<'_>) -> Vec<Symbol> {
    adt.non_enum_variant()
        .fields
        .iter()
        .filter(|f| is_option_ty(cx, field_ty(cx, f)))
        .map(|f| f.name)
        .collect()
}

/// Per build of `did` the user wrote, where it is and which option fields
/// it sets to `Some`. A derived build only has to pass the check. None when
/// a build writes an option field with a value that is not `Some(..)` or
/// `None`.
fn sites(cx: &LateContext<'_>, did: DefId, opts: &[Symbol]) -> Option<Vec<(Span, Vec<Symbol>)>> {
    let mut sites = Vec::new();
    for build in field_writes::facts().builds(did) {
        let mut somes = Vec::new();
        for f in opts {
            let value = &build.get(*f)?.value;
            if value.is_some(cx.tcx) {
                somes.push(*f);
            } else if !value.is_none(cx.tcx) {
                return None;
            }
        }
        if build.derived && somes.len() < 2 {
            continue;
        }
        sites.push((build.span, somes));
    }
    Some(sites)
}

impl<'tcx> LateLintPass<'tcx> for OptionsAsEnum {
    fn check_crate_post(&mut self, cx: &LateContext<'tcx>) {
        let facts = field_writes::facts();
        for did in facts.variants(cx.tcx) {
            if !facts.access(did).is_some_and(|r| r.private) {
                continue;
            }
            let opts = option_fields(cx, cx.tcx.adt_def(did));
            if opts.len() < self.min_fields {
                continue;
            }
            // A write after the build re-opens every combination. The
            // builds alone no longer prove anything.
            if opts.iter().any(|f| !facts.writes(did, *f).is_empty()) {
                continue;
            }
            let Some(sites) = sites(cx, did, &opts) else {
                continue;
            };
            if sites.len() < 2 || sites.iter().any(|(_, s)| s.len() > 1) {
                continue;
            }
            // At least two distinct fields must actually appear as Some.
            // Otherwise this is one live field and dead siblings, not a state.
            let mut seen: Vec<Symbol> = Vec::new();
            for (_, site) in &sites {
                for f in site {
                    if !seen.contains(f) {
                        seen.push(*f);
                    }
                }
            }
            if seen.len() < 2 {
                continue;
            }
            let names: Vec<String> = seen.iter().map(|s| format!("`{s}`")).collect();
            let names = names.join(", ");
            emit_with_note(
                cx,
                OPTIONS_AS_ENUM,
                cx.tcx.def_span(did),
                format!(
                    "`{}` is built in {} places and none sets more than one of {names} to `Some`. The fields are alternatives, but the struct allows both at once",
                    cx.tcx.def_path_str(did),
                    sites.len(),
                ),
                sites[0].0,
                "one of the constructions",
                format!("replace {names} with one enum field, one variant per case"),
            );
        }
    }
}
