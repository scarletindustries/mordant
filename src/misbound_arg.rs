use rustc_hir::def::{DefKind, Res};
use rustc_hir::{Expr, ExprKind, QPath, UnOp};
use rustc_lint::{LateContext, LateLintPass};
use rustc_middle::ty::Ty;
use rustc_span::Symbol;

use crate::baseline::emit;
use crate::hir_shapes::{Callee, callee_of};

rustc_session::declare_lint! {
    /// Flags a call argument whose own name is the name of a *different*
    /// parameter of the callee than the one it is bound to, when both
    /// parameters have the same type: `resize(height, width)` against
    /// `fn resize(width: u32, height: u32)`, or `spawn(opts.inherit_stderr,
    /// opts.inherit_stdout)`. The two values are told apart by position and
    /// by nothing else, so the transposition type-checks; distinct types
    /// per parameter (a newtype per quantity, an enum per flag) reject it.
    ///
    /// The argument's name is a local or parameter it is spelled as, or the
    /// last field of a field access. Silent when the names agree after
    /// dropping a leading `is_`/`has_`/`_` and a trailing `_`, when the
    /// parameter the name points at already receives an argument of that
    /// name, when the bound parameter's name contains the argument's as a
    /// word (`from_index` receiving `index`), for one-character names,
    /// method receivers, and calls through closures or fn pointers.
    pub MISBOUND_ARG,
    Warn,
    "argument named as a different same-typed parameter of the callee"
}

rustc_session::declare_lint_pass!(MisboundArg => [MISBOUND_ARG]);

/// The name an argument is spelled with: a local `x`, or the last field of
/// `x.a.b`, through `&`, `*` and casts. Anything computed has no name to
/// cross.
fn arg_name(mut e: &Expr<'_>) -> Option<Symbol> {
    loop {
        match &e.kind {
            ExprKind::Cast(inner, _)
            | ExprKind::AddrOf(_, _, inner)
            | ExprKind::Unary(UnOp::Deref, inner)
            | ExprKind::DropTemps(inner) => e = inner,
            ExprKind::Field(_, ident) => return Some(ident.name),
            ExprKind::Path(QPath::Resolved(None, p))
                if p.segments.len() == 1 && matches!(p.res, Res::Local(_)) =>
            {
                return Some(p.segments[0].ident.name);
            }
            _ => return None,
        }
    }
}

/// `is_open_` and `open` name the same thing for this lint's purposes.
fn normalized(name: &str) -> &str {
    let name = name.trim_start_matches('_').trim_end_matches('_');
    name.strip_prefix("is_")
        .or_else(|| name.strip_prefix("has_"))
        .unwrap_or(name)
}

/// `from_file_index` qualifies `file_index` rather than naming another slot.
fn qualifies(param: &str, arg: &str) -> bool {
    param.len() > arg.len()
        && ((param.ends_with(arg) && param[..param.len() - arg.len()].ends_with('_'))
            || (param.starts_with(arg) && param[arg.len()..].starts_with('_')))
}

struct Crossing<'tcx> {
    /// Index into the call's argument list.
    arg: usize,
    name: Symbol,
    bound_to: Symbol,
    /// Signature index of the parameter the name points at.
    names_param: usize,
    ty: Ty<'tcx>,
}

impl<'tcx> LateLintPass<'tcx> for MisboundArg {
    fn check_expr(&mut self, cx: &LateContext<'tcx>, expr: &'tcx Expr<'tcx>) {
        if expr.span.from_expansion() {
            return;
        }
        let Some(callee) = callee_of(cx, expr) else {
            return;
        };
        let def = callee.def();
        if !matches!(cx.tcx.def_kind(def), DefKind::Fn | DefKind::AssocFn) {
            return;
        }
        // A method call's receiver is bound to `self`; explicit args start
        // one signature slot later.
        let (args, offset) = match callee {
            Callee::Path { args, .. } => (args, 0),
            Callee::Method { args, .. } => (args, 1),
        };
        let idents = cx.tcx.fn_arg_idents(def);
        let sig = cx.tcx.erase_and_anonymize_regions(
            cx.tcx.instantiate_bound_regions_with_erased(
                cx.tcx
                    .fn_sig(def)
                    .instantiate_identity()
                    .skip_normalization(),
            ),
        );
        let inputs = sig.inputs();
        if idents.len() != inputs.len() || args.len() + offset > inputs.len() {
            return;
        }
        let param_name = |i: usize| idents[i].map(|id| id.name);
        // The name each signature slot actually receives at this call.
        let received = |slot: usize| {
            slot.checked_sub(offset)
                .and_then(|i| args.get(i))
                .and_then(|a| arg_name(a))
        };
        let mut crossings: Vec<Crossing<'tcx>> = Vec::new();
        for (i, arg) in args.iter().enumerate() {
            if arg.span.from_expansion() {
                continue;
            }
            let slot = i + offset;
            let (Some(name), Some(bound_to)) = (arg_name(arg), param_name(slot)) else {
                continue;
            };
            let an = normalized(name.as_str());
            let bn = normalized(bound_to.as_str());
            if an.len() < 2 || bn.len() < 2 || an == bn || qualifies(bn, an) {
                continue;
            }
            let Some(other) = (0..inputs.len()).find(|&q| {
                q != slot
                    && inputs[q] == inputs[slot]
                    && param_name(q).is_some_and(|p| normalized(p.as_str()) == an)
            }) else {
                continue;
            };
            // `f(name, name)`: the namesake parameter already gets its name,
            // so nothing is transposed, one value fills two roles.
            if received(other).is_some_and(|r| normalized(r.as_str()) == an) {
                continue;
            }
            crossings.push(Crossing {
                arg: i,
                name,
                bound_to,
                names_param: other,
                ty: inputs[slot],
            });
        }
        let fn_name = cx.tcx.item_name(def);
        let mut reported = vec![false; crossings.len()];
        for (k, c) in crossings.iter().enumerate() {
            if reported[k] {
                continue;
            }
            reported[k] = true;
            let partner = (k + 1..crossings.len()).find(|&j| {
                let d = &crossings[j];
                d.arg + offset == c.names_param && c.arg + offset == d.names_param
            });
            if let Some(j) = partner {
                reported[j] = true;
                let d = &crossings[j];
                emit(
                    cx,
                    MISBOUND_ARG,
                    expr.span,
                    format!(
                        "arguments `{}` and `{}` are bound to `{fn_name}`'s parameters `{}` and `{}`; \
                         all are `{}`, so the transposition type-checks",
                        c.name, d.name, c.bound_to, d.bound_to, c.ty,
                    ),
                    "give the two parameters distinct types (a newtype per quantity, an enum per flag) so a swap is a type error",
                );
            } else {
                emit(
                    cx,
                    MISBOUND_ARG,
                    args[c.arg].span,
                    format!(
                        "argument `{}` is bound to `{fn_name}`'s parameter `{}`, but `{fn_name}` also takes \
                         a parameter `{}` of the same type `{}`",
                        c.name,
                        c.bound_to,
                        param_name(c.names_param).unwrap_or(c.name),
                        c.ty,
                    ),
                    "give the two parameters distinct types (a newtype per quantity, an enum per flag) so a swap is a type error",
                );
            }
        }
    }
}
