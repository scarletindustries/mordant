use rustc_hir::HirId;
use rustc_hir::def_id::LocalDefId;
use rustc_lint::{LateContext, LateLintPass};
use rustc_middle::ty;
use rustc_span::Span;

use crate::baseline::emit_hir_then;
use crate::frees::{FreeEvent, Frees, Reach};

rustc_lint::declare_lint! {
    /// Flags a fn that frees the value behind one of its reference
    /// parameters, often `&mut self`. A reference parameter must stay valid
    /// until the fn returns, so freeing what it points to is undefined
    /// behaviour, even when the code does not use it again.
    ///
    /// The free is found in MIR. A raw pointer made from the reference reaches
    /// `Box::from_raw` and the `Box` is dropped, or it reaches `dealloc`, or a
    /// fn that frees its pointer parameter. Such fns are followed through
    /// their bodies, in this crate and in generic code of other crates. A
    /// free that happens only on some paths, as with a reference count, gets
    /// a weaker message.
    ///
    /// The fix is to take a raw pointer (`this: *mut Self`) in place of the
    /// reference, so no reference is live when the memory goes away.
    pub FREED_WHILE_BORROWED,
    Warn,
    "fn frees the value behind one of its own reference parameters"
}

pub struct FreedWhileBorrowed {
    pub config: &'static crate::MordantConfig,
}

rustc_lint::impl_lint_pass!(FreedWhileBorrowed => [FREED_WHILE_BORROWED]);

/// One reference parameter and the first free of the value behind it.
struct Finding {
    fn_id: HirId,
    param: Span,
    name: String,
    event: FreeEvent,
}

impl<'tcx> LateLintPass<'tcx> for FreedWhileBorrowed {
    fn check_crate_post(&mut self, cx: &LateContext<'tcx>) {
        let tcx = cx.tcx;
        // One set of summaries for the crate, so each callee is read once.
        let mut frees = Frees::new(
            tcx,
            &self.config.freed_while_borrowed_frees,
            &self.config.freed_while_borrowed_releases,
        );
        let mut findings = Vec::new();
        for def in tcx.hir_body_owners() {
            if tcx.def_kind(def).is_fn_like() {
                findings.extend(check_fn(cx, &mut frees, def));
            }
        }
        findings.sort_by_key(|f| f.event.span.lo());
        for f in findings {
            report(cx, &f);
        }
    }
}

fn check_fn(cx: &LateContext<'_>, frees: &mut Frees<'_>, def: LocalDefId) -> Vec<Finding> {
    let tcx = cx.tcx;
    let fn_id = tcx.local_def_id_to_hir_id(def);
    if tcx.def_span(def).in_external_macro(tcx.sess.source_map())
        || clippy_utils::in_automatically_derived(tcx, fn_id)
    {
        return Vec::new();
    }
    let params = reference_params(cx, def);
    if params.iter().all(Option::is_none) {
        return Vec::new();
    }
    // A closure's MIR takes its captures as the first argument.
    let skip = usize::from(tcx.is_closure_like(def.to_def_id()));
    let mut events = frees.events(def);
    // A sure free first, then source order.
    events.sort_by_key(|e| (std::cmp::Reverse(e.reach), e.span.lo()));
    let mut seen = Vec::new();
    let mut out = Vec::new();
    for event in events {
        let Some(k) = event.param.checked_sub(skip) else {
            continue;
        };
        let Some(Some((param, name))) = params.get(k) else {
            continue;
        };
        if seen.contains(&k) || event.span.from_expansion() {
            continue;
        }
        seen.push(k);
        out.push(Finding {
            fn_id,
            param: *param,
            name: name.clone(),
            event,
        });
    }
    out
}

/// For each HIR parameter, its span and name when its type is a reference.
fn reference_params(cx: &LateContext<'_>, def: LocalDefId) -> Vec<Option<(Span, String)>> {
    let tcx = cx.tcx;
    let body = tcx.hir_body_owned_by(def);
    let typeck = tcx.typeck(def);
    body.params
        .iter()
        .map(|p| {
            let is_ref = matches!(typeck.node_type(p.hir_id).kind(), ty::Ref(..));
            let name = p.pat.simple_ident()?.to_string();
            is_ref.then_some((p.span, name))
        })
        .collect()
}

fn report(cx: &LateContext<'_>, f: &Finding) {
    let (e, name) = (&f.event, &f.name);
    let what = match e.callee {
        Some(def) => format!("this call to `{}`", cx.tcx.item_name(def)),
        None => "this drop".to_string(),
    };
    let verb = match e.reach {
        Reach::Always => "frees",
        Reach::Sometimes => "can free",
    };
    let msg = format!("{what} {verb} the value behind `{name}` while `{name}` is still borrowed");
    let site_note = match e.reach {
        Reach::Always => "the memory is freed here",
        Reach::Sometimes => {
            "it is freed here on some paths only, so the code relies on another owner keeping it alive"
        }
    };
    let help = if name == "self" {
        "take `this: *mut Self` in place of the `self` reference, so no reference is live when the memory is freed".to_string()
    } else {
        format!(
            "take a raw pointer for `{name}` in place of the reference, so no reference is live when the memory is freed"
        )
    };
    let (site, span, param) = (e.site, e.span, f.param);
    let reach = e.reach;
    emit_hir_then(cx, FREED_WHILE_BORROWED, f.fn_id, span, msg, |diag| {
        diag.span_note(
            param,
            format!(
                "`{name}` is a reference parameter, so it must stay valid until the call returns"
            ),
        );
        if site != span {
            diag.span_note(site, site_note);
        } else if reach == Reach::Sometimes {
            diag.note(
                "it frees on some paths only, so the code relies on another owner keeping it alive",
            );
        }
        diag.help(help);
    });
}
