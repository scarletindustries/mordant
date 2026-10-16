use crate::adt_facts::{field_ty, has_fixed_repr, has_positional_fields};
use crate::baseline::emit_with_note;
use crate::field_writes::{self, Build, Value};
use rustc_hir::def_id::DefId;
use rustc_lint::{LateContext, LateLintPass};
use rustc_middle::ty::TyCtxt;
use rustc_span::{Span, Symbol};

use crate::MordantConfig;

rustc_lint::declare_lint! {
    /// Flags a struct with `bool-cluster-min-bools` or more `bool` fields
    /// when the crate builds it in only some of their combinations. `n`
    /// bools allow `2^n` combinations. If the code makes fewer, an enum with
    /// one variant per combination it makes says which are valid.
    ///
    /// Fires only on proof. Every write to the bools must be a `true` or
    /// `false` literal in a struct literal: no assignment, compound
    /// assignment or `&mut` to one, no computed value, no value copied by
    /// `..base`. Another crate must not be able to build the struct or
    /// assign one of the bools. A derived `Default` is a build with every
    /// bool `false`. A derived `Clone` copies and is not a build. The crate
    /// must make at least two combinations, and fewer than all of them.
    ///
    /// Silent on any struct with an explicit `repr`. Its layout is fixed
    /// from outside Rust (a hardware register, a wire format), so all `2^n`
    /// combinations may be real.
    ///
    /// On by default. `bool-cluster-enabled` in `mordant.toml` is still
    /// accepted but no longer needed.
    pub BOOL_CLUSTER,
    Warn,
    "struct whose bool fields are only built in some of their combinations"
}

pub struct BoolCluster {
    pub config: &'static MordantConfig,
}

rustc_lint::impl_lint_pass!(BoolCluster => [BOOL_CLUSTER]);

/// One combination of the bools the crate makes, with the builds that make
/// it.
struct Combination<'a> {
    values: Vec<bool>,
    builds: Vec<&'a Build>,
}

impl<'tcx> LateLintPass<'tcx> for BoolCluster {
    fn check_crate_post(&mut self, cx: &LateContext<'tcx>) {
        let facts = field_writes::facts();
        for did in facts.variants(cx.tcx) {
            let adt = field_writes::adt_of(cx.tcx, did);
            if !adt.is_struct() || has_fixed_repr(adt) {
                continue;
            }
            let variant = adt.non_enum_variant();
            if has_positional_fields(variant) {
                continue;
            }
            let bools: Vec<Symbol> = variant
                .fields
                .iter()
                .filter(|f| field_ty(cx, f).is_bool())
                .map(|f| f.name)
                .collect();
            if bools.len() < self.config.bool_cluster_min_bools
                || !bools.iter().all(|f| facts.only_built_here(did, *f))
            {
                continue;
            }
            let Some(combos) = combinations(facts.builds(did), &bools) else {
                continue;
            };
            let all = u32::try_from(bools.len())
                .ok()
                .and_then(|n| 2usize.checked_pow(n));
            if combos.len() < 2 || all.is_some_and(|all| combos.len() >= all) {
                continue;
            }
            report(cx, did, &bools, &combos);
        }
    }
}

/// The combinations the builds make, in the order of their first build.
/// None when a build writes a bool with something other than a literal.
fn combinations<'a>(builds: &'a [Build], bools: &[Symbol]) -> Option<Vec<Combination<'a>>> {
    let mut combos: Vec<Combination<'a>> = Vec::new();
    for b in builds {
        let mut values = Vec::with_capacity(bools.len());
        for f in bools {
            match b.get(*f)?.value {
                Value::Bool(v) => values.push(v),
                _ => return None,
            }
        }
        match combos.iter_mut().find(|c| c.values == values) {
            Some(c) => c.builds.push(b),
            None => combos.push(Combination {
                values,
                builds: vec![b],
            }),
        }
    }
    for c in &mut combos {
        c.builds.sort_by_key(|b| b.span);
    }
    combos.sort_by_key(|c| c.builds[0].span);
    Some(combos)
}

fn report(cx: &LateContext<'_>, did: DefId, bools: &[Symbol], combos: &[Combination<'_>]) {
    let tcx = cx.tcx;
    let path = tcx.def_path_str(did);
    let names: Vec<String> = bools.iter().map(|s| format!("`{s}`")).collect();
    let variants = variant_names(tcx, bools, combos);
    let listed: Vec<String> = combos
        .iter()
        .zip(&variants)
        .map(|(c, name)| {
            let from = fn_name(tcx, c.builds[0].func);
            format!("`{name}` ({}, from `{from}`)", set_fields(bools, &c.values))
        })
        .collect();
    let shown: Span = combos
        .iter()
        .flat_map(|c| &c.builds)
        .find(|b| !b.derived)
        .unwrap_or(&combos[0].builds[0])
        .span;
    emit_with_note(
        cx,
        BOOL_CLUSTER,
        tcx.def_span(did),
        format!(
            "`{path}` has {} bool fields ({}), and the crate only makes {} of their {} combinations",
            bools.len(),
            names.join(", "),
            combos.len(),
            states(bools.len()),
        ),
        shown,
        "one of the struct literals",
        format!(
            "replace the bools with an enum of {} variants, one per combination: {}",
            combos.len(),
            listed.join(", "),
        ),
    );
}

/// The bools a combination sets to `true`, as a phrase.
fn set_fields(bools: &[Symbol], values: &[bool]) -> String {
    let set: Vec<String> = bools
        .iter()
        .zip(values)
        .filter(|(_, v)| **v)
        .map(|(f, _)| format!("`{f}`"))
        .collect();
    match set.len() {
        0 => "none set".to_owned(),
        _ => format!("{} set", set.join(" and ")),
    }
}

/// A variant name per combination: the fn that first builds it, in camel
/// case. When two combinations would get the same name, both are named
/// after the bools they set instead.
fn variant_names(tcx: TyCtxt<'_>, bools: &[Symbol], combos: &[Combination<'_>]) -> Vec<String> {
    let by_fn: Vec<String> = combos
        .iter()
        .map(|c| camel(&fn_name(tcx, c.builds[0].func)))
        .collect();
    combos
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let unique =
                !by_fn[i].is_empty() && by_fn.iter().filter(|n| **n == by_fn[i]).count() == 1;
            if unique {
                return by_fn[i].clone();
            }
            let set: Vec<&str> = bools
                .iter()
                .zip(&c.values)
                .filter(|(_, v)| **v)
                .map(|(f, _)| f.as_str())
                .collect();
            match set.is_empty() {
                true => "NoneSet".to_owned(),
                false => camel(&set.join("_")),
            }
        })
        .collect()
}

/// The name of the fn, const or static a build is in. Empty when it has
/// none.
fn fn_name(tcx: TyCtxt<'_>, func: DefId) -> String {
    tcx.opt_item_name(func)
        .map(|s| s.to_string())
        .unwrap_or_default()
}

/// `snake_case` or `SCREAMING_CASE` as `CamelCase`.
fn camel(s: &str) -> String {
    s.split('_')
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut cs = w.chars();
            let first = cs.next().map(|c| c.to_ascii_uppercase());
            first
                .into_iter()
                .chain(cs.map(|c| c.to_ascii_lowercase()))
                .collect::<String>()
        })
        .collect()
}

fn states(n: usize) -> String {
    match u32::try_from(n).ok().and_then(|n| 2u64.checked_pow(n)) {
        Some(n) => n.to_string(),
        None => format!("2^{n}"),
    }
}

#[cfg(test)]
mod tests {
    use super::{camel, states};

    #[test]
    fn states_is_two_to_the_n() {
        assert_eq!(states(3), "8");
        assert_eq!(states(4), "16");
        assert_eq!(states(5), "32");
    }

    /// The count cannot be raised to a power of two in `u64`, so the message
    /// must say so rather than print a wrong number.
    #[test]
    fn states_does_not_overflow() {
        assert_eq!(states(64), "2^64");
        assert_eq!(states(usize::MAX), format!("2^{}", usize::MAX));
    }

    #[test]
    fn camel_joins_words() {
        assert_eq!(camel("new"), "New");
        assert_eq!(camel("refuse_big"), "RefuseBig");
        assert_eq!(camel("DEFAULT_SPEC"), "DefaultSpec");
        assert_eq!(camel("_x__y"), "XY");
    }
}
