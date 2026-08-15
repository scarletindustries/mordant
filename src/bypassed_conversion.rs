use crate::adt_facts::in_own_code_of;
use crate::baseline::{emit, emit_with_note};
use crate::hir_shapes::callee_of;
use rustc_hir::def_id::DefId;
use rustc_hir::{Expr, ExprKind};
use rustc_lint::{LateContext, LateLintPass};
use rustc_middle::ty::{self, Ty, TypeVisitableExt};
use rustc_span::{Symbol, sym};

rustc_session::declare_lint! {
    /// Flags a value of a struct, enum or union produced by reinterpreting
    /// the bits of some other type -- `mem::transmute` /
    /// `mem::transmute_copy`, or a pointer cast (`p as *const T`,
    /// `p.cast::<T>()`) between different pointee types -- when a conversion
    /// from that same source type to that same target type already exists:
    /// an `impl From<A> for B`, an `impl TryFrom<A> for B`, or a safe
    /// receiver-less associated function of `B` taking one `A` and returning
    /// `B`, `Option<B>` or `Result<B, _>`. That function is where the crate
    /// decided which `A` values are a `B` and how; the reinterpreting site
    /// takes any bit pattern, so whatever the conversion rejects or remaps
    /// arrives as a `B` anyway, and for an enum an unlisted discriminant is
    /// undefined behaviour on the spot.
    ///
    /// For a transmute of a value, every integer type counts as the same
    /// source: `transmute::<u16, E>(n as u16)` had to pick the repr width,
    /// and `E::from_raw(n: u32)` is still the conversion it skipped. For a
    /// pointer cast the pointee must be exactly the conversion's input type,
    /// so a byte buffer viewed as a header is not matched against
    /// `Header::new(u32)`.
    ///
    /// Silent on: anything in the target type's own module or in any impl of
    /// it, trait impls included, since that code is the conversion or sits
    /// beside it; a transmute that only changes lifetimes or is otherwise
    /// between the same type; a target that is not an ADT (fn pointers,
    /// integers, type parameters); a pointer cast into a type with interior
    /// mutability, which views the pointee in place (`usize` as
    /// `AtomicUsize`) where a by-value `From` would make a new cell; a type
    /// nothing converts into, which has no check to bypass; `unsafe fn`
    /// constructors, which promise no check; and conversions whose input is
    /// the target type itself or generic.
    pub BYPASSED_CONVERSION,
    Warn,
    "bits reinterpreted as a type that has a conversion from the same source, which the site skips"
}

/// One way the target type's crate (or a trait impl anywhere) turns `from`
/// into the target.
struct Conversion<'tcx> {
    from: Ty<'tcx>,
    def: DefId,
    /// `Level::try_from`, `Code::from_raw`: how the message names it.
    name: String,
    /// Returns `Option`/`Result`: it can refuse a value, not just remap one.
    fallible: bool,
}

/// How the site got from one type to the other; decides whether integer
/// widths are interchangeable when matching a conversion's input.
#[derive(Clone, Copy, PartialEq)]
enum Shape {
    Value,
    Pointer,
}

rustc_session::declare_lint_pass!(BypassedConversion => [BYPASSED_CONVERSION]);

const TRANSMUTE: &str = "`mem::transmute`";
const TRANSMUTE_COPY: &str = "`mem::transmute_copy`";
const POINTER_CAST: &str = "a pointer cast";

/// `mem::transmute` / `mem::transmute_copy`, named the way the message shows
/// them. `transmute_copy` has no diagnostic item, hence the path test.
fn transmuter(cx: &LateContext<'_>, def: DefId) -> Option<&'static str> {
    if cx.tcx.is_diagnostic_item(sym::transmute, def) {
        return Some(TRANSMUTE);
    }
    (cx.tcx.crate_name(def.krate) == sym::core
        && cx.tcx.item_name(def).as_str() == "transmute_copy"
        && cx
            .tcx
            .opt_parent(def)
            .is_some_and(|m| cx.tcx.opt_item_name(m) == Some(sym::mem)))
    .then_some(TRANSMUTE_COPY)
}

/// Strips one reference or raw pointer layer off both types at once, as
/// long as both have one: `&A -> &B` and `*const A -> *mut B` compare their
/// pointees, `usize -> *const B` compares nothing.
fn peel_pointer_pair<'tcx>(mut a: Ty<'tcx>, mut b: Ty<'tcx>) -> (Ty<'tcx>, Ty<'tcx>, Shape) {
    let mut shape = Shape::Value;
    loop {
        let pointee = |t: Ty<'tcx>| match *t.kind() {
            ty::Ref(_, inner, _) | ty::RawPtr(inner, _) => Some(inner),
            _ => None,
        };
        match (pointee(a), pointee(b)) {
            (Some(pa), Some(pb)) => {
                a = pa;
                b = pb;
                shape = Shape::Pointer;
            }
            _ => return (a, b, shape),
        }
    }
}

/// Reports `expr`, which turns a `from` into a `to` by reinterpretation
/// (`how` names the means), when a conversion between the same pair exists
/// and the site is not the target type's own code.
fn check_reinterpretation<'tcx>(
    cx: &LateContext<'tcx>,
    expr: &'tcx Expr<'tcx>,
    from: Ty<'tcx>,
    to: Ty<'tcx>,
    how: &'static str,
) {
    if expr.span.in_external_macro(cx.tcx.sess.source_map()) {
        return;
    }
    let tcx = cx.tcx;
    let (from, to, shape) = peel_pointer_pair(
        tcx.erase_and_anonymize_regions(from),
        tcx.erase_and_anonymize_regions(to),
    );
    if from == to {
        return;
    }
    let ty::Adt(adt, _) = *to.kind() else {
        return;
    };
    // A pointer cast to a type with interior mutability views the pointee
    // in place (a `usize` as an `AtomicUsize`); a by-value conversion into
    // such a type makes a new cell and is no substitute for the view.
    if from.ty_adt_def().is_some_and(|a| a.did() == adt.did())
        || (shape == Shape::Pointer && !to.is_freeze(tcx, cx.typing_env()))
        || in_own_code_of(cx, expr.hir_id, adt.did())
    {
        return;
    }
    let conversions = collect_conversions(cx, adt);
    let same_integer =
        |c: &&Conversion<'tcx>| shape == Shape::Value && c.from.is_integral() && from.is_integral();
    let Some(conv) = conversions
        .iter()
        .find(|c| c.from == from)
        .or_else(|| conversions.iter().find(same_integer))
    else {
        return;
    };
    let consequence = if conv.fallible {
        format!(
            "`{}` converts `{}` to `{to}` and can refuse a value; this site accepts any bit pattern",
            conv.name, conv.from
        )
    } else {
        format!(
            "`{}` is how `{}` becomes `{to}`; this site goes around it",
            conv.name, conv.from
        )
    };
    let msg = format!("`{from}` is reinterpreted as `{to}` by {how} here, but {consequence}");
    let help = "convert through that function, or put an unchecked constructor beside it so both sit with the layout they depend on";
    if conv.def.is_local() {
        emit_with_note(
            cx,
            BYPASSED_CONVERSION,
            expr.span,
            msg,
            tcx.def_span(conv.def),
            "the conversion this site skips",
            help,
        );
    } else {
        emit(cx, BYPASSED_CONVERSION, expr.span, msg, help);
    }
}

/// Every `From`/`TryFrom` impl for the ADT and every safe receiver-less
/// inherent fn of it that takes one value and returns the ADT, bare or in
/// `Option`/`Result`. Inputs that are the ADT itself or mention a type
/// parameter convert nothing a reinterpreting site could have held.
fn collect_conversions<'tcx>(
    cx: &LateContext<'tcx>,
    adt: ty::AdtDef<'tcx>,
) -> Vec<Conversion<'tcx>> {
    let tcx = cx.tcx;
    let target = adt.did();
    let self_ty = tcx
        .type_of(target)
        .instantiate_identity()
        .skip_normalization();
    let is_target = |t: Ty<'tcx>| {
        t.peel_refs()
            .ty_adt_def()
            .is_some_and(|a| a.did() == target)
    };
    let usable_input = |t: Ty<'tcx>| !is_target(t) && !t.has_param();
    let name_of = |f: Symbol| format!("{}::{f}", tcx.item_name(target));
    let mut out = Vec::new();
    for (trait_sym, method, fallible) in
        [(sym::From, "from", false), (sym::TryFrom, "try_from", true)]
    {
        let Some(trait_did) = tcx.get_diagnostic_item(trait_sym) else {
            continue;
        };
        for imp in tcx.non_blanket_impls_for_ty(trait_did, self_ty) {
            let args = tcx
                .impl_trait_ref(imp)
                .instantiate_identity()
                .skip_normalization()
                .args;
            if !is_target(args.type_at(0)) {
                continue;
            }
            let from = tcx.erase_and_anonymize_regions(args.type_at(1));
            if !usable_input(from) {
                continue;
            }
            let def = tcx
                .associated_items(imp)
                .in_definition_order()
                .find(|i| i.is_fn())
                .map_or(imp, |i| i.def_id);
            out.push(Conversion {
                from,
                def,
                name: name_of(Symbol::intern(method)),
                fallible,
            });
        }
    }
    for &imp in tcx.inherent_impls(target) {
        for item in tcx.associated_items(imp).in_definition_order() {
            if !item.is_fn() || item.is_method() {
                continue;
            }
            let sig = tcx
                .fn_sig(item.def_id)
                .instantiate_identity()
                .skip_normalization()
                .skip_binder();
            if sig.safety().is_unsafe() {
                continue;
            }
            let [input] = sig.inputs() else {
                continue;
            };
            let from = tcx.erase_and_anonymize_regions(*input);
            if !usable_input(from) {
                continue;
            }
            let output = sig.output();
            let fallible = match *output.kind() {
                ty::Adt(o, _) if o.did() == target => false,
                ty::Adt(o, args)
                    if (tcx.is_diagnostic_item(sym::Option, o.did())
                        || tcx.is_diagnostic_item(sym::Result, o.did()))
                        && matches!(*args.type_at(0).kind(), ty::Adt(inner, _) if inner.did() == target) =>
                {
                    true
                }
                _ => continue,
            };
            out.push(Conversion {
                from,
                def: item.def_id,
                name: name_of(item.name()),
                fallible,
            });
        }
    }
    out
}

impl<'tcx> LateLintPass<'tcx> for BypassedConversion {
    fn check_expr(&mut self, cx: &LateContext<'tcx>, expr: &'tcx Expr<'tcx>) {
        let results = cx.typeck_results();
        match expr.kind {
            ExprKind::Call(_, [arg]) => {
                let Some(def) = callee_of(cx, expr).map(|c| c.def()) else {
                    return;
                };
                let Some(how) = transmuter(cx, def) else {
                    return;
                };
                // `transmute_copy(&src)` reads through the reference.
                let from = match (how, results.expr_ty(arg).kind()) {
                    (TRANSMUTE_COPY, &ty::Ref(_, inner, _)) => inner,
                    _ => results.expr_ty(arg),
                };
                check_reinterpretation(cx, expr, from, results.expr_ty(expr), how);
            }
            ExprKind::Cast(inner, _) if results.expr_ty(expr).is_raw_ptr() => {
                check_reinterpretation(
                    cx,
                    expr,
                    results.expr_ty(inner),
                    results.expr_ty(expr),
                    POINTER_CAST,
                );
            }
            ExprKind::MethodCall(_, recv, [], _) => {
                let Some(def) = results.type_dependent_def_id(expr.hir_id) else {
                    return;
                };
                if !(cx.tcx.is_diagnostic_item(sym::ptr_cast, def)
                    || cx.tcx.is_diagnostic_item(sym::const_ptr_cast, def))
                {
                    return;
                }
                check_reinterpretation(
                    cx,
                    expr,
                    results.expr_ty(recv),
                    results.expr_ty(expr),
                    POINTER_CAST,
                );
            }
            _ => {}
        }
    }
}
