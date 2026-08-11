//! Shared machinery for lints that treat prose as checkable claims: comment
//! harvesting, backticked-name extraction, and the two scopes a name can
//! legitimately live in (the file's code, the crate's definitions).

use std::collections::HashMap;

use rustc_hir::def_id::LocalDefId;
use rustc_lint::LateContext;
use rustc_span::{BytePos, Span};

pub(crate) const IDENT_STOPLIST: &[&str] = &[
    "self", "Self", "mut", "true", "false", "None", "Some", "Ok", "Err", "Vec", "Box", "drop",
    "u8", "u16", "u32", "u64", "u128", "usize", "i8", "i16", "i32", "i64", "i128", "isize", "f32",
    "f64", "bool", "str", "String", "unsafe", "SAFETY",
];

/// Backticked mentions in text, reduced to their final identifier:
/// `` `self.frames` `` and `` `VM::wake()` `` both yield their last segment,
/// and anything that is not identifier-shaped after that reduction is skipped.
pub(crate) fn backticked_idents(text: &str) -> Vec<String> {
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
/// line, joined, with the span covering those lines.
pub(crate) fn comment_above(cx: &LateContext<'_>, span: Span) -> Option<(String, Span)> {
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
    if collected.is_empty() {
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

/// The source of `span`'s whole file with every `//` comment stripped, so a
/// comment can never vouch for its own claims.
pub(crate) fn file_code_only(cx: &LateContext<'_>, span: Span) -> String {
    let file_src = cx
        .tcx
        .sess
        .source_map()
        .span_to_lines(span)
        .ok()
        .and_then(|l| l.file.src.as_ref().map(|s| s.to_string()))
        .unwrap_or_default();
    file_src
        .lines()
        .map(|l| l.split("//").next().unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n")
}

pub(crate) fn word_in(haystack: &str, ident: &str) -> bool {
    haystack
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .any(|w| w == ident)
}

/// Every definition name in the crate, mapped to one owning def. Names that
/// several defs share map to `None`, so a consumer needing THE definition
/// (fingerprinting, spans) skips ambiguous names instead of guessing.
pub(crate) fn crate_def_index(cx: &LateContext<'_>) -> HashMap<String, Option<LocalDefId>> {
    let mut index: HashMap<String, Option<LocalDefId>> = HashMap::new();
    let mut insert = |name: String, def: Option<LocalDefId>| match index.entry(name) {
        std::collections::hash_map::Entry::Vacant(e) => {
            e.insert(def);
        }
        std::collections::hash_map::Entry::Occupied(mut e) => {
            e.insert(None);
        }
    };
    for def_id in cx.tcx.hir_crate_items(()).definitions() {
        if let Some(name) = cx.tcx.opt_item_name(def_id.to_def_id()) {
            insert(name.to_string(), Some(def_id));
        }
        if let rustc_hir::def::DefKind::Struct
        | rustc_hir::def::DefKind::Enum
        | rustc_hir::def::DefKind::Union = cx.tcx.def_kind(def_id)
        {
            let adt = cx.tcx.adt_def(def_id);
            for v in adt.variants() {
                insert(v.name.to_string(), None);
                for f in &v.fields {
                    insert(f.name.to_string(), None);
                }
            }
        }
    }
    index
}
