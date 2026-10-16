use std::collections::HashSet;

use crate::adt_facts::{field_ty, has_fixed_repr, is_option_ty, struct_field};
use crate::baseline::emit_with_note;
use crate::field_writes::{self, BlockPart, Value, WriteKind};
use rustc_hir::HirId;
use rustc_hir::def_id::DefId;
use rustc_lint::{LateContext, LateLintPass};
use rustc_middle::ty::TyCtxt;
use rustc_span::{Span, Symbol};

rustc_lint::declare_lint! {
    /// Flags a bool field that is only ever written next to a sibling
    /// `Option` field, `true` with `Some` and `false` with `None` (or always
    /// the reverse): every write to either, struct literal or field
    /// assignment, sits beside a write to the other in the same struct
    /// expression or straight-line block. The flag is that field's
    /// `is_some()` stored twice.
    ///
    /// Only fires on named fields nothing outside the crate can name, when
    /// every write to the pair is a literal `true`/`false` and
    /// `Some(..)`/`None` (or `Default::default()`, which is `false`/`None`)
    /// and both polarities occur. Each of these is unprovable and silences
    /// it: a lone write to either field, one in a match arm or closure
    /// without a block of its own, a block that writes both polarities, a
    /// computed value, a field copied by `..base`, a compound assignment, or
    /// a `&mut` to either field. A `&mut` includes `.take()`,
    /// `mem::replace`, `&mut s.opt` and a `ref mut` pattern binding. `as_mut`
    /// and `as_deref_mut` cannot change which variant the field holds, so
    /// they do not count. Tuple structs are built through their constructor
    /// fn and are not followed. A derived `Clone` copies the pair as it
    /// stands and is not a write of its own.
    pub BOOL_BESIDE_OPTION,
    Warn,
    "bool field that repeats whether a sibling Option field is Some"
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Bool,
    Opt,
}

/// Where a write happens: the struct literal, or for an assignment the
/// part of a block it runs in. So `a.opt = ..` and `b.flag = ..` in one
/// block, or two writes a `?` can separate, are not beside each other.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Site {
    Literal(HirId),
    Block(BlockPart),
}

struct FieldWrites {
    kind: Kind,
    /// Some write to the field is not a literal of its kind, one site writes
    /// both polarities, or the field is mutably borrowed: its value is no
    /// longer a function of the sites.
    unprovable: bool,
    /// (site, set): where the field is written, and whether it is written
    /// `true` / `Some(..)` there.
    sites: HashSet<(Site, bool)>,
    /// The first write recorded, for the note.
    first: Option<Span>,
}

impl FieldWrites {
    fn record(&mut self, site: Site, at: Span, set: Option<bool>) {
        match set {
            // A site that writes both polarities no longer says which of
            // its sibling writes each one sits beside.
            Some(set) if self.sites.contains(&(site, !set)) => self.unprovable = true,
            Some(set) => {
                self.sites.insert((site, set));
                // A write the user spelled, over one a derive expanded to.
                if self
                    .first
                    .is_none_or(|f| f.from_expansion() && !at.from_expansion())
                {
                    self.first = Some(at);
                }
            }
            None => self.unprovable = true,
        }
    }
}

#[derive(Default)]
pub struct BoolBesideOption;

rustc_lint::impl_lint_pass!(BoolBesideOption => [BOOL_BESIDE_OPTION]);

/// Whether a value writes `true`/`Some(..)` (Some(true)) or `false`/`None`
/// (Some(false)). None for anything computed.
fn polarity(tcx: TyCtxt<'_>, value: &Value) -> Option<bool> {
    match value {
        Value::Bool(b) => Some(*b),
        v if v.is_some(tcx) => Some(true),
        v if v.is_none(tcx) => Some(false),
        _ => None,
    }
}

/// The writes to every bool and `Option` field of `did`, a local struct
/// with named fields and no `repr`, that nothing outside the crate can
/// write.
fn field_writes_of(cx: &LateContext<'_>, did: DefId) -> Vec<(Symbol, FieldWrites)> {
    let facts = field_writes::facts();
    let adt = field_writes::adt_of(cx.tcx, did);
    if !adt.is_struct() || has_fixed_repr(adt) || adt.non_enum_variant().ctor.is_some() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for f in &adt.non_enum_variant().fields {
        if facts.others_write(did, f.name) {
            continue;
        }
        let fty = field_ty(cx, f);
        let kind = if fty.is_bool() {
            Kind::Bool
        } else if is_option_ty(cx, fty) {
            Kind::Opt
        } else {
            continue;
        };
        let mut fw = FieldWrites {
            kind,
            unprovable: false,
            sites: HashSet::new(),
            first: None,
        };
        for build in facts.builds(did) {
            let Some(init) = build.get(f.name) else {
                continue;
            };
            let site = Site::Literal(build.hir_id);
            fw.record(site, init.span, polarity(cx.tcx, &init.value));
        }
        for w in facts.writes(did, f.name) {
            match (w.kind, w.block_part) {
                (WriteKind::Assign, Some(part)) => {
                    fw.record(Site::Block(part), w.span, polarity(cx.tcx, &w.value));
                }
                _ => fw.unprovable = true,
            }
        }
        if !fw.unprovable && !fw.sites.is_empty() {
            out.push((f.name, fw));
        }
    }
    out.sort_by_key(|(name, _)| name.as_str().to_owned());
    out
}

impl<'tcx> LateLintPass<'tcx> for BoolBesideOption {
    fn check_crate_post(&mut self, cx: &LateContext<'tcx>) {
        let mut findings: Vec<(Span, String, Span, String)> = Vec::new();
        for did in field_writes::facts().variants(cx.tcx) {
            let fields = field_writes_of(cx, did);
            if fields.is_empty() {
                continue;
            }
            let adt = cx.tcx.adt_def(did);
            for (flag, flag_facts) in fields.iter().filter(|(_, f)| f.kind == Kind::Bool) {
                // A flag only ever `false` (or only ever `true`) is a constant,
                // not a copy of anything.
                if !flag_facts.sites.iter().any(|&(_, set)| set)
                    || !flag_facts.sites.iter().any(|&(_, set)| !set)
                {
                    continue;
                }
                let Some(first) = flag_facts.first else {
                    continue;
                };
                for (opt, opt_facts) in fields.iter().filter(|(_, f)| f.kind == Kind::Opt) {
                    let inverted: HashSet<(Site, bool)> =
                        opt_facts.sites.iter().map(|&(s, set)| (s, !set)).collect();
                    let reading = if flag_facts.sites == opt_facts.sites {
                        "is_some"
                    } else if flag_facts.sites == inverted {
                        "is_none"
                    } else {
                        continue;
                    };
                    let (with_true, with_false) = match reading {
                        "is_some" => ("Some", "None"),
                        _ => ("None", "Some"),
                    };
                    let Some(field) = struct_field(adt, *flag) else {
                        continue;
                    };
                    findings.push((
                        cx.tcx.def_span(field.did),
                        format!(
                            "`{}.{flag}` is only ever written next to `{opt}`, `true` with `{with_true}` and `false` with `{with_false}`, in all {} places. It is `{opt}.{reading}()` stored twice",
                            cx.tcx.def_path_str(did),
                            flag_facts.sites.len(),
                        ),
                        first,
                        format!(
                            "delete `{flag}` and call `{opt}.{reading}()` where it was read"
                        ),
                    ));
                    break;
                }
            }
        }
        findings.sort_by_key(|(span, ..)| span.lo());
        for (span, msg, first, help) in findings {
            emit_with_note(
                cx,
                BOOL_BESIDE_OPTION,
                span,
                msg,
                first,
                "one of the paired writes",
                help,
            );
        }
    }
}
