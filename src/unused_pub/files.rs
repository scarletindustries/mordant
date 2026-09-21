//! The two things about an item only the compiler can say, for the records
//! in [`super::records`].

use rustc_hir::def_id::DefId;
use rustc_lint::LateContext;
use rustc_middle::ty::TyCtxt;
use rustc_span::{FileName, Span};

/// Crate name plus definition path: `bun_core::fmt::raw`,
/// `bun_css::{impl#3}::eql`. The same string whichever crate computes it.
/// An item of a binary carries `[bin]` after the crate name: a package's
/// library and binary are often both the crate `tool`, and nothing outside
/// the binary can name its items.
pub fn key(tcx: TyCtxt<'_>, def_id: DefId) -> String {
    let bin = if def_id.is_local() && std::env::var_os("CARGO_BIN_NAME").is_some() {
        "[bin]"
    } else {
        ""
    };
    format!(
        "{}{bin}{}",
        tcx.crate_name(def_id.krate),
        tcx.def_path(def_id).to_string_no_crate_verbose()
    )
}

/// Where `span` starts and ends in its file, if that is a real file.
pub fn locate(cx: &LateContext<'_>, span: Span) -> Option<(String, u32, u32)> {
    let sm = cx.tcx.sess.source_map();
    let file = sm.lookup_source_file(span.lo());
    let FileName::Real(real) = &file.name else {
        return None;
    };
    let path = real.local_path()?.to_string_lossy().into_owned();
    Some((
        path,
        (span.lo() - file.start_pos).0,
        (span.hi() - file.start_pos).0,
    ))
}
