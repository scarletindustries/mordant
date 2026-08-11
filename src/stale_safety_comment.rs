use std::collections::HashSet;

use rustc_hir::def_id::LOCAL_CRATE;
use rustc_hir::{Block, BlockCheckMode, UnsafeSource};
use rustc_lint::{LateContext, LateLintPass};
use rustc_span::{BytePos, Span};

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

const IDENT_STOPLIST: &[&str] = &[
    "self", "Self", "mut", "true", "false", "None", "Some", "Ok", "Err", "Vec", "Box", "drop",
    "u8", "u16", "u32", "u64", "u128", "usize", "i8", "i16", "i32", "i64", "i128", "isize", "f32",
    "f64", "bool", "str", "String", "unsafe", "SAFETY",
];

/// Backticked mentions in comment text, reduced to their final identifier:
/// `` `self.frames` `` and `` `VM::wake` `` both yield their last segment, and
/// anything that is not identifier-shaped after that reduction is skipped.
fn backticked_idents(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find('`') {
        let after = &rest[start + 1..];
        let Some(end) = after.find('`') else { break };
        let token = &after[..end];
        rest = &after[end + 1..];
        let last = token
            .trim_end_matches("()")
            .rsplit(&[':', '.'][..])
            .next()
            .unwrap_or(token);
        let is_ident = !last.is_empty()
            && last
                .chars()
                .next()
                .is_some_and(|c| c.is_alphabetic() || c == '_')
            && last.chars().all(|c| c.is_alphanumeric() || c == '_');
        if is_ident && !IDENT_STOPLIST.contains(&last) {
            out.push(last.to_string());
        }
    }
    out
}

/// The contiguous run of `//` comment lines directly above `span`'s first
/// line, joined, if any of them mentions SAFETY.
fn safety_comment_above(cx: &LateContext<'_>, span: Span) -> Option<(String, Span)> {
    let sm = cx.tcx.sess.source_map();
    let lines = sm.span_to_lines(span).ok()?;
    let file = lines.file;
    let first = lines.lines.first()?.line_index;
    let mut collected: Vec<String> = Vec::new();
    let mut top = first;
    for i in (0..first).rev() {
        let line = file.get_line(i)?;
        let trimmed = line.trim_start();
        if trimmed.starts_with("//") {
            collected.push(trimmed.trim_start_matches('/').to_string());
            top = i;
        } else {
            break;
        }
    }
    if collected.is_empty() || !collected.iter().any(|l| l.contains("SAFETY")) {
        return None;
    }
    collected.reverse();
    let lo = file.line_bounds(top).start;
    let hi = file.line_bounds(first.saturating_sub(1)).end;
    Some((
        collected.join("\n"),
        Span::with_root_ctxt(BytePos(lo.0), BytePos(hi.0)),
    ))
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
        for ident in backticked_idents(&comment) {
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
