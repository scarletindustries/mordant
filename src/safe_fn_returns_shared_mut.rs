use rustc_abi::ExternAbi;
use rustc_hir::def_id::LocalDefId;
use rustc_hir::intravisit::FnKind;
use rustc_hir::{Body, FnDecl, ImplItemImplKind, ItemKind, Node};
use rustc_lint::{LateContext, LateLintPass, LintContext};
use rustc_middle::mir;
use rustc_middle::ty::{self, Ty, TyCtxt};
use rustc_span::Span;

use crate::baseline::emit_with_note;
use crate::mir_flow::{local_name, mir_for};
use crate::pointer_origin::{Origins, Root, Source};

rustc_lint::declare_lint! {
    /// Flags a safe fn that returns `&mut T` built from a static, a
    /// thread-local, or memory an argument only points to, when no input
    /// carries the lifetime of the result. Two calls then give two live
    /// `&mut` to the same memory, which is undefined behaviour, and nothing
    /// in the signature stops a caller from making them. The fix is an
    /// `unsafe fn` whose `# Safety` section states the rule, scoped access
    /// through a closure, or a raw pointer return.
    pub SAFE_FN_RETURNS_SHARED_MUT,
    Warn,
    "safe fn returns a `&mut` that a second call can return again"
}

#[derive(Default)]
pub struct SafeFnReturnsSharedMut;

rustc_lint::impl_lint_pass!(SafeFnReturnsSharedMut => [SAFE_FN_RETURNS_SHARED_MUT]);

/// The returned `&mut`, when its lifetime is `'static` or appears in no
/// input, so no borrow of an input holds the result to one at a time.
fn unbound_mut_return<'tcx>(tcx: TyCtxt<'tcx>, def: LocalDefId) -> Option<Ty<'tcx>> {
    let sig = tcx
        .fn_sig(def)
        .instantiate_identity()
        .skip_normalization()
        .skip_binder();
    let ret = sig.output();
    let ty::Ref(region, _, ty::Mutability::Mut) = *ret.kind() else {
        return None;
    };
    let in_inputs = sig.inputs().iter().any(|input| {
        input
            .walk()
            .any(|arg| arg.as_region().is_some_and(|r| r == region))
    });
    (!in_inputs).then_some(ret)
}

/// A fn whose signature a trait sets: the fix belongs on the trait.
fn signature_from_trait(cx: &LateContext<'_>, def: LocalDefId) -> bool {
    let node = cx.tcx.hir_node_by_def_id(def);
    if let Node::ImplItem(item) = node {
        return matches!(item.impl_kind, ImplItemImplKind::Trait { .. });
    }
    let parent = cx.tcx.parent_hir_node(cx.tcx.local_def_id_to_hir_id(def));
    matches!(parent, Node::Item(item) if matches!(item.kind, ItemKind::Trait { .. }))
}

/// The source to name, when one proves the claim: a static or thread-local
/// first, then an argument a caller can pass twice.
fn chosen_source<'tcx>(
    cx: &LateContext<'tcx>,
    body: &mir::Body<'tcx>,
    sources: &[Source],
) -> Option<Source> {
    let global = sources
        .iter()
        .find(|s| matches!(s.root, Root::Static(_) | Root::ThreadLocal));
    let copied_arg = || {
        sources.iter().find(|s| match s.root {
            Root::Arg(local) => clippy_utils::ty::is_copy(cx, body.local_decls[local].ty),
            _ => false,
        })
    };
    global.or_else(copied_arg).copied()
}

fn report(
    cx: &LateContext<'_>,
    def: LocalDefId,
    ret: String,
    body: &mir::Body<'_>,
    source: Source,
) {
    let name = cx.tcx.item_name(def.to_def_id());
    let unsafe_fn = format!(
        "make `{name}` an `unsafe fn` with a `# Safety` section that says callers must not hold two results at once"
    );
    let help = match source.root {
        Root::Arg(_) => format!("{unsafe_fn}. Or return a raw pointer"),
        Root::Static(_) | Root::ThreadLocal => format!(
            "{unsafe_fn}. Or pass the `&mut` to a closure so it lives only for that call, or return a raw pointer"
        ),
    };
    let (from, twice, note) = match source.root {
        Root::Static(s) => {
            let s = cx.tcx.item_name(s);
            (
                format!("the static `{s}`"),
                "two calls".to_string(),
                format!("the reference comes from `{s}` here"),
            )
        }
        Root::ThreadLocal => (
            "a thread-local".to_string(),
            "two calls on one thread".to_string(),
            "the reference comes from the thread-local here".to_string(),
        ),
        Root::Arg(local) => {
            let arg =
                local_name(body, local).map_or("the argument".to_string(), |n| format!("`{n}`"));
            (
                format!("what {arg} points to"),
                format!("two calls with the same {arg}"),
                format!(
                    "the reference comes from {arg}, and the result's lifetime is not tied to it"
                ),
            )
        }
    };
    emit_with_note(
        cx,
        SAFE_FN_RETURNS_SHARED_MUT,
        cx.tcx.def_span(def),
        format!(
            "safe fn `{name}` returns `{ret}` made from {from}, so {twice} give two live `&mut` to the same memory"
        ),
        source.span,
        note,
        help,
    );
}

impl<'tcx> LateLintPass<'tcx> for SafeFnReturnsSharedMut {
    fn check_fn(
        &mut self,
        cx: &LateContext<'tcx>,
        kind: FnKind<'tcx>,
        _decl: &'tcx FnDecl<'tcx>,
        _body: &'tcx Body<'tcx>,
        span: Span,
        def: LocalDefId,
    ) {
        let Some(header) = kind.header() else {
            return;
        };
        if header.is_unsafe()
            || header.abi != ExternAbi::Rust
            || span.in_external_macro(cx.sess().source_map())
            || clippy_utils::in_automatically_derived(cx.tcx, cx.tcx.local_def_id_to_hir_id(def))
            || signature_from_trait(cx, def)
        {
            return;
        }
        let Some(ret) = unbound_mut_return(cx.tcx, def) else {
            return;
        };
        let Some(body) = mir_for(cx.tcx, def) else {
            return;
        };
        let sources = Origins::new(cx.tcx).of_return(&body);
        if let Some(source) = chosen_source(cx, &body, &sources) {
            report(cx, def, ret.to_string(), &body, source);
        }
    }
}
