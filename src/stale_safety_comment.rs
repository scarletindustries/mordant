use rustc_hir::{Block, BlockCheckMode, UnsafeSource};
use rustc_lint::{LateContext, LateLintPass};

use crate::baseline::emit;
use crate::claims::{self, DefNames};

rustc_session::declare_lint! {
    /// Flags a `// SAFETY:` comment whose backticked identifiers no longer
    /// exist: not in the file's code, and not the name of any definition in
    /// this crate or a crate it links. A safety justification that names a
    /// guard which a refactor has since removed is documentation asserting an
    /// invariant nothing provides.
    ///
    /// Runs only with `stale-safety-comment-enabled = true` in `dylint.toml`.
    /// The crate cannot see names defined in C++, in scripts, or in crates
    /// that depend on it, and once a codebase's genuinely stale comments have
    /// been fixed those are most of what remains, so it is a survey to run
    /// once, not a gate to keep on.
    pub STALE_SAFETY_COMMENT,
    Warn,
    "SAFETY comment names an identifier that no longer exists"
}

#[derive(Default)]
pub struct StaleSafetyComment {
    defs: Option<DefNames>,
}

rustc_session::impl_lint_pass!(StaleSafetyComment => [STALE_SAFETY_COMMENT]);

/// The `//` run directly above `span`, when it mentions SAFETY.
fn safety_comment_above(
    cx: &LateContext<'_>,
    span: rustc_span::Span,
) -> Option<(String, rustc_span::Span)> {
    let (text, cspan) = claims::comment_above(cx, span)?;
    text.contains("SAFETY").then_some((text, cspan))
}

impl<'tcx> LateLintPass<'tcx> for StaleSafetyComment {
    fn check_crate(&mut self, cx: &LateContext<'tcx>) {
        self.defs = Some(DefNames::collect(cx));
    }

    fn check_block(&mut self, cx: &LateContext<'tcx>, block: &'tcx Block<'tcx>) {
        if !matches!(
            block.rules,
            BlockCheckMode::UnsafeBlock(UnsafeSource::UserProvided)
        ) || block.span.from_expansion()
        {
            return;
        }
        let Some((comment, comment_span)) = safety_comment_above(cx, block.span) else {
            return;
        };
        let Some(defs) = &self.defs else { return };
        // The whole source file is the scope the comment's names live in: a
        // justification legitimately points at a sibling impl or a helper a
        // few items away, not only at the enclosing function. Comment text is
        // stripped first, so a comment cannot vouch for itself.
        let code = claims::file_code_only(cx, block.span);
        for ident in claims::backticked_idents(&comment) {
            if !claims::word_in(&code, &ident) && !defs.contains(cx, &ident) {
                emit(
                    cx,
                    STALE_SAFETY_COMMENT,
                    comment_span,
                    format!(
                        "this SAFETY comment names `{ident}`, which appears nowhere in this file's code, this crate, or any crate it links"
                    ),
                    "the guard this justification described has moved or gone; update the comment or restore the guard",
                );
            }
        }
    }
}
