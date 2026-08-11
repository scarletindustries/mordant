use std::collections::HashSet;

use rustc_hir::def_id::LOCAL_CRATE;
use rustc_hir::{Block, BlockCheckMode, UnsafeSource};
use rustc_lint::{LateContext, LateLintPass};

use crate::baseline::emit;

rustc_session::declare_lint! {
    /// Flags a `// SAFETY:` comment whose backticked identifiers no longer
    /// exist: not in the enclosing function's source, and not the name of any
    /// definition in the crate. A safety justification that names a guard
    /// which a refactor has since removed is documentation asserting an
    /// invariant nothing provides.
    pub STALE_SAFETY_COMMENT,
    Warn,
    "SAFETY comment names an identifier that no longer exists"
}

#[derive(Default)]
pub struct StaleSafetyComment {
    /// Every definition name in the crate: items, fields, methods, variants.
    def_names: HashSet<String>,
}

rustc_session::impl_lint_pass!(StaleSafetyComment => [STALE_SAFETY_COMMENT]);

impl StaleSafetyComment {
    pub fn new() -> Self {
        Self::default()
    }
}

/// The `//` run directly above `span`, when it mentions SAFETY.
fn safety_comment_above(
    cx: &LateContext<'_>,
    span: rustc_span::Span,
) -> Option<(String, rustc_span::Span)> {
    let (text, cspan) = crate::claims::comment_above(cx, span)?;
    text.contains("SAFETY").then_some((text, cspan))
}

impl<'tcx> LateLintPass<'tcx> for StaleSafetyComment {
    fn check_crate(&mut self, cx: &LateContext<'tcx>) {
        for def_id in cx.tcx.hir_crate_items(()).definitions() {
            if let Some(name) = cx.tcx.opt_item_name(def_id.to_def_id()) {
                self.def_names.insert(name.to_string());
            }
            if let rustc_hir::def::DefKind::Struct
            | rustc_hir::def::DefKind::Enum
            | rustc_hir::def::DefKind::Union = cx.tcx.def_kind(def_id)
            {
                let adt = cx.tcx.adt_def(def_id);
                for v in adt.variants() {
                    self.def_names.insert(v.name.to_string());
                    for f in &v.fields {
                        self.def_names.insert(f.name.to_string());
                    }
                }
            }
        }
        let _ = cx.tcx.crate_name(LOCAL_CRATE);
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
        // The whole source file is the scope the comment's names live in: a
        // justification legitimately points at a sibling impl or a helper a
        // few items away, not only at the enclosing function. Comment text is
        // stripped first, so a comment cannot vouch for itself.
        let file_src = cx
            .tcx
            .sess
            .source_map()
            .span_to_lines(block.span)
            .ok()
            .and_then(|l| l.file.src.as_ref().map(|s| s.to_string()))
            .unwrap_or_default();
        let code_only: String = file_src
            .lines()
            .map(|l| l.split("//").next().unwrap_or(""))
            .collect::<Vec<_>>()
            .join("\n");
        for ident in crate::claims::backticked_idents(&comment) {
            let in_file = code_only
                .split(|c: char| !(c.is_alphanumeric() || c == '_'))
                .any(|w| w == ident);
            if !in_file && !self.def_names.contains(&ident) {
                emit(
                    cx,
                    STALE_SAFETY_COMMENT,
                    comment_span,
                    format!(
                        "this SAFETY comment names `{ident}`, which appears nowhere in this file's code or the crate's definitions"
                    ),
                    "the guard this justification described has moved or gone; update the comment or restore the guard",
                );
            }
        }
    }
}
