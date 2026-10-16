//! Facts about integer types and integer constants that several lints read
//! the same way: the width and sign of an integer type, whether a conversion
//! between two of them can lose a value, and the value of a constant
//! expression that `ConstEvalCtxt` alone cannot fold.

use clippy_utils::consts::{ConstEvalCtxt, Constant};
use rustc_hir::{BinOpKind, Expr, ExprKind, UnOp};
use rustc_lint::LateContext;
use rustc_middle::ty::{self, Ty};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct IntLayout {
    pub(crate) bits: u64,
    pub(crate) signed: bool,
}

impl IntLayout {
    /// The largest value the type holds.
    pub(crate) fn max(self) -> u128 {
        let value_bits = self.bits - u64::from(self.signed);
        if value_bits >= 128 {
            u128::MAX
        } else {
            (1u128 << value_bits) - 1
        }
    }

    /// All bits of the type set, as an unsigned value.
    fn mask(self) -> u128 {
        if self.bits >= 128 {
            u128::MAX
        } else {
            (1u128 << self.bits) - 1
        }
    }
}

/// `usize`/`isize` take the target's pointer width: whether `u64 as usize`
/// narrows is a question about the target being linted, not about all of
/// them.
pub(crate) fn int_layout(cx: &LateContext<'_>, ty: Ty<'_>) -> Option<IntLayout> {
    let ptr_bits = || cx.tcx.data_layout.pointer_size().bits();
    match ty.kind() {
        ty::Int(i) => Some(IntLayout {
            bits: i.bit_width().unwrap_or_else(ptr_bits),
            signed: true,
        }),
        ty::Uint(u) => Some(IntLayout {
            bits: u.bit_width().unwrap_or_else(ptr_bits),
            signed: false,
        }),
        _ => None,
    }
}

/// Some value of `src` has no representation in `dst`.
pub(crate) fn lossy(src: IntLayout, dst: IntLayout) -> bool {
    let preserving = (src.signed == dst.signed && dst.bits >= src.bits)
        || (!src.signed && dst.signed && dst.bits > src.bits);
    !preserving
}

/// `e` with integer casts that keep every value peeled off, so
/// `(x as u64) <= LIMIT` reads `x`.
pub(crate) fn peel_widening<'tcx>(
    cx: &LateContext<'tcx>,
    mut e: &'tcx Expr<'tcx>,
) -> &'tcx Expr<'tcx> {
    let typeck = cx.typeck_results();
    loop {
        match e.kind {
            ExprKind::DropTemps(inner) => e = inner,
            ExprKind::Cast(inner, _) => {
                let from = int_layout(cx, typeck.expr_ty(inner));
                let to = int_layout(cx, typeck.expr_ty(e));
                match (from, to) {
                    (Some(from), Some(to)) if !lossy(from, to) => e = inner,
                    _ => return e,
                }
            }
            _ => return e,
        }
    }
}

/// The value of a non-negative integer constant expression: a literal, a
/// named constant, or casts, shifts, masks and arithmetic over those.
/// `ConstEvalCtxt` alone stops at `as`. `None` for a negative value or for
/// anything that is not fixed at compile time.
pub(crate) fn const_value<'tcx>(cx: &LateContext<'tcx>, e: &'tcx Expr<'tcx>) -> Option<u128> {
    let layout = int_layout(cx, cx.typeck_results().expr_ty(e))?;
    let value = match e.kind {
        ExprKind::DropTemps(inner) | ExprKind::Cast(inner, _) => const_value(cx, inner)?,
        ExprKind::Unary(UnOp::Not, inner) => !const_value(cx, inner)?,
        ExprKind::Binary(op, l, r) => {
            let (a, b) = (const_value(cx, l)?, const_value(cx, r)?);
            match op.node {
                BinOpKind::Add => a.checked_add(b)?,
                BinOpKind::Sub => a.checked_sub(b)?,
                BinOpKind::Mul => a.checked_mul(b)?,
                BinOpKind::Div => a.checked_div(b)?,
                BinOpKind::Shl => a.checked_shl(u32::try_from(b).ok()?)?,
                BinOpKind::Shr => a.checked_shr(u32::try_from(b).ok()?)?,
                BinOpKind::BitAnd => a & b,
                BinOpKind::BitOr => a | b,
                BinOpKind::BitXor => a ^ b,
                _ => return None,
            }
        }
        _ => match ConstEvalCtxt::new(cx).eval(e)? {
            Constant::Int(v) => v,
            _ => return None,
        },
    };
    // Wrap to the expression's own type, as the compiler would.
    let value = value & layout.mask();
    (value <= layout.max()).then_some(value)
}
