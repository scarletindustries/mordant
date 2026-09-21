//! Finds `pub` items that no crate in the workspace uses.

mod files;
#[allow(
    dead_code,
    reason = "cargo-mordant reads what this library only writes"
)]
mod records;

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::PathBuf;

use rustc_hir::def::{DefKind, Res};
use rustc_hir::def_id::{DefId, LocalDefId};
use rustc_hir::{
    Expr, ExprKind, HirId, ImplItem, ImplItemKind, Item, ItemKind, Node, Pat, PatExpr, PatExprKind,
    PatKind, Path, QPath, TraitItem, TraitItemKind,
};
use rustc_lint::{LateContext, LateLintPass, Level, LintContext};
use rustc_middle::lint::LintLevelSource;
use rustc_middle::middle::codegen_fn_attrs::CodegenFnAttrFlags;
use rustc_middle::ty::print::with_no_trimmed_paths;
use rustc_span::Span;
use rustc_structures::CrateType;

use crate::protocol::FACTS_ENV;
use records::{Def, Note, Unit};

rustc_lint::declare_lint! {
    /// Finds a `pub` item that nothing in the workspace uses: no crate
    /// compiled in this run names it, calls it, or imports it, its own
    /// crate included. rustc's `dead_code` never reports such an item,
    /// because `pub` makes it reachable from outside the crate whether or
    /// not anything outside exists.
    ///
    /// Items checked: functions, methods and constants of inherent impls,
    /// trait methods and constants, structs, enums, unions, type aliases,
    /// traits, constants and statics, when reachable from outside their
    /// crate. A use is any resolved path or method call in the compiled
    /// code of any member, generated and macro-expanded code included,
    /// except the item's mention of itself, the self type of its own
    /// `impl` blocks, and code from `#[derive]` on it.
    ///
    /// Not reported: items with `#[no_mangle]`, `#[export_name]` or
    /// `#[used]`, which other languages reach by symbol; language items;
    /// the entry point; items produced by macros; items in a file brought
    /// in with `include!`, which is generated code as a rule.
    ///
    /// Under `cargo mordant` every compilation of a member records the items
    /// it defines and the ones it uses under `target/mordant/unused_pub/`,
    /// and the findings are printed once cargo is done, from the records of
    /// the targets that run built, so a use counts whichever crate cargo
    /// compiled first. With `--all-targets` that includes tests, benches and
    /// examples, and an item only they use is used. A crate compiled without
    /// `cargo mordant` is judged alone. A use that only exists under a `cfg`,
    /// target or feature not compiled in this run is not seen.
    pub UNUSED_PUB,
    Warn,
    "a public item that no crate in the workspace uses"
}

/// How this compilation takes part, decided in `check_crate`.
#[derive(Default)]
enum Mode {
    /// Not under `cargo mordant`, or a build script, which nothing else can
    /// name: the crate's items are judged against its own uses.
    #[default]
    Alone,
    /// Under `cargo mordant`: write this unit's records into `dir`, which
    /// `cargo mordant` judges once the run is built.
    Record { dir: PathBuf, unit: Unit },
}

#[derive(Default)]
pub struct UnusedPub {
    mode: Mode,
    /// Built with `--test`: its uses count, but its items are the ones the
    /// crate's own build records, or only exist for the tests.
    is_test: bool,
    is_proc_macro: bool,
    /// This crate's reachable `pub` items, with their local id and name
    /// span for the case where this crate prints them itself.
    defs: Vec<(LocalDefId, Span, Def)>,
    local_refs: HashSet<LocalDefId>,
    /// Keys of workspace items other crates define and this crate uses.
    foreign_refs: BTreeSet<String>,
    /// Keys listed by the `.defs` files present when this crate started:
    /// every library it depends on has finished by then.
    known: HashSet<String>,
    /// The crates those keys belong to, to skip the rest cheaply.
    known_crates: HashSet<String>,
    keys: HashMap<DefId, Option<String>>,
}

// Declares no lint, so rustc runs it where `unused_pub` is allowed too:
// every compilation's uses count, whatever its levels.
rustc_lint::impl_lint_pass!(UnusedPub => []);

impl<'tcx> LateLintPass<'tcx> for UnusedPub {
    fn check_crate(&mut self, cx: &LateContext<'tcx>) {
        self.is_test = cx.tcx.sess.is_test_crate();
        self.is_proc_macro = cx.tcx.crate_types().contains(&CrateType::ProcMacro);
        self.mode = mode(cx);
        if let Mode::Record { dir, .. } = &self.mode {
            self.known = records::all_def_keys(dir);
            self.known_crates = self
                .known
                .iter()
                .filter_map(|k| k.split("::").next().map(str::to_string))
                .collect();
        }
    }

    fn check_item(&mut self, cx: &LateContext<'tcx>, item: &'tcx Item<'tcx>) {
        let checked = matches!(
            item.kind,
            ItemKind::Fn { .. }
                | ItemKind::Struct(..)
                | ItemKind::Enum(..)
                | ItemKind::Union(..)
                | ItemKind::Const(..)
                | ItemKind::Static(..)
                | ItemKind::TyAlias(..)
                | ItemKind::Trait { .. }
        );
        if checked && let Some(ident) = item.kind.ident() {
            self.record_def(cx, item.owner_id.def_id, item.span, ident.span);
        }
    }

    fn check_impl_item(&mut self, cx: &LateContext<'tcx>, item: &'tcx ImplItem<'tcx>) {
        if matches!(item.kind, ImplItemKind::Fn(..) | ImplItemKind::Const(..))
            && let Node::Item(parent) = cx.tcx.parent_hir_node(item.hir_id())
            && let ItemKind::Impl(imp) = parent.kind
            && imp.of_trait.is_none()
        {
            self.record_def(cx, item.owner_id.def_id, item.span, item.ident.span);
        }
    }

    fn check_trait_item(&mut self, cx: &LateContext<'tcx>, item: &'tcx TraitItem<'tcx>) {
        if matches!(item.kind, TraitItemKind::Fn(..) | TraitItemKind::Const(..)) {
            self.record_def(cx, item.owner_id.def_id, item.span, item.ident.span);
        }
    }

    fn check_path(&mut self, cx: &LateContext<'tcx>, path: &Path<'tcx>, hir_id: HirId) {
        let Res::Def(_, def_id) = path.res else {
            return;
        };
        // `impl Foo { .. }` and `impl Trait for Foo` do not use `Foo`. They do
        // use an alias written there: the impl is of the type it names.
        if !matches!(path.res, Res::Def(DefKind::TyAlias, _))
            && let Node::Item(item) = cx.tcx.parent_hir_node(hir_id)
            && let ItemKind::Impl(imp) = item.kind
            && imp.self_ty.hir_id == hir_id
        {
            return;
        }
        let def_id = owning_item(cx, def_id);
        if path.span.in_derive_expansion() && enclosing_impl_self_adt(cx, hir_id) == Some(def_id) {
            return;
        }
        self.record_ref(cx, def_id, hir_id);
    }

    fn check_expr(&mut self, cx: &LateContext<'tcx>, expr: &'tcx Expr<'tcx>) {
        let def_id = match expr.kind {
            ExprKind::MethodCall(..) => cx.typeck_results().type_dependent_def_id(expr.hir_id),
            // `check_path` sees `QPath::Resolved`; the rest need type checking.
            ExprKind::Path(ref qpath @ QPath::TypeRelative(..))
            | ExprKind::Struct(&ref qpath @ QPath::TypeRelative(..), ..) => {
                cx.qpath_res(qpath, expr.hir_id).opt_def_id()
            }
            _ => None,
        };
        if let Some(def_id) = def_id {
            self.record_ref(cx, owning_item(cx, def_id), expr.hir_id);
        }
    }

    fn check_pat(&mut self, cx: &LateContext<'tcx>, pat: &'tcx Pat<'tcx>) {
        let (qpath, hir_id) = match pat.kind {
            PatKind::Struct(ref qpath @ QPath::TypeRelative(..), ..)
            | PatKind::TupleStruct(ref qpath @ QPath::TypeRelative(..), ..) => (qpath, pat.hir_id),
            PatKind::Expr(PatExpr {
                hir_id,
                kind: PatExprKind::Path(qpath @ QPath::TypeRelative(..)),
                ..
            }) => (qpath, *hir_id),
            _ => return,
        };
        if let Some(def_id) = cx.qpath_res(qpath, hir_id).opt_def_id() {
            self.record_ref(cx, owning_item(cx, def_id), hir_id);
        }
    }

    fn check_crate_post(&mut self, cx: &LateContext<'tcx>) {
        let defs = std::mem::take(&mut self.defs);
        match &self.mode {
            Mode::Alone => {
                if self.is_test {
                    return;
                }
                let unused: Vec<&(LocalDefId, Span, Def)> = defs
                    .iter()
                    .filter(|(id, ..)| !self.local_refs.contains(id))
                    .collect();
                let keys: HashSet<&str> = unused.iter().map(|(.., d)| d.key.as_str()).collect();
                for (_, span, def) in unused
                    .iter()
                    .filter(|(.., d)| !keys.contains(d.parent.as_str()))
                {
                    report(cx, *span, def);
                }
            }
            Mode::Record { dir, unit } => {
                let mut refs = std::mem::take(&mut self.foreign_refs);
                // By key, so a test build's use of the crate's own items
                // counts for the build that records them.
                refs.extend(
                    self.local_refs
                        .iter()
                        .filter(|id| cx.effective_visibilities.is_reachable(**id))
                        .map(|id| files::key(cx.tcx, id.to_def_id())),
                );
                records::write_refs(&unit.refs(dir), &refs);
                if !self.is_test && !self.is_proc_macro {
                    let section = crate::baseline::section_name(cx);
                    records::write_defs(&unit.defs(dir), &section, defs.iter().map(|(.., d)| d));
                }
            }
        }
    }
}

/// How this compilation takes part: `cargo mordant` sets the environment
/// for the run's compilations and, differently, for its last one.
fn mode(cx: &LateContext<'_>) -> Mode {
    let Some(dir) = std::env::var_os(FACTS_ENV).map(PathBuf::from) else {
        return Mode::Alone;
    };
    let name = cx.tcx.crate_name(rustc_hir::def_id::LOCAL_CRATE);
    let src = cx
        .tcx
        .sess
        .local_crate_source_file()
        .and_then(|f| f.local_path().map(std::path::Path::to_path_buf));
    match src {
        Some(src) if name.as_str() != "build_script_build" => Mode::Record {
            dir,
            unit: Unit {
                src,
                test: cx.tcx.sess.is_test_crate(),
            },
        },
        _ => Mode::Alone,
    }
}

impl UnusedPub {
    fn record_def(
        &mut self,
        cx: &LateContext<'_>,
        def_id: LocalDefId,
        item_span: Span,
        name: Span,
    ) {
        let hir_id = cx.tcx.local_def_id_to_hir_id(def_id);
        if self.is_test
            || item_span.from_expansion()
            || !cx.effective_visibilities.is_reachable(def_id)
            || exempt(cx, def_id)
            || included(cx, hir_id, item_span)
        {
            return;
        }
        let spec = cx.tcx.lint_level_spec_at_node(UNUSED_PUB, hir_id);
        if spec.is_allow() || spec.is_expect() {
            if let Some(expectation) = spec.lint_id() {
                cx.fulfill_expectation(expectation);
            }
            return;
        }
        let Some((file, lo, hi)) = files::locate(cx, name) else {
            return;
        };
        let level = spec.level().as_str().to_string();
        let notes = level_source(cx, spec.level(), spec.src);
        let did = def_id.to_def_id();
        let crate_name = cx.tcx.crate_name(rustc_hir::def_id::LOCAL_CRATE);
        let mut path = with_no_trimmed_paths!(cx.tcx.def_path_str(did));
        if !path.starts_with(&format!("{crate_name}::")) {
            path = format!("{crate_name}::{path}");
        }
        let def = Def {
            key: files::key(cx.tcx, did),
            file,
            lo,
            hi,
            descr: cx.tcx.def_descr(did).to_string(),
            path,
            parent: parent_key(cx, did).unwrap_or_default(),
            level,
            notes,
        };
        self.defs.push((def_id, name, def));
    }

    fn record_ref(&mut self, cx: &LateContext<'_>, def_id: DefId, from: HirId) {
        // An item naming itself (recursion, its own signature) is no use.
        if from.owner.to_def_id() == def_id {
            return;
        }
        if let Some(local) = def_id.as_local() {
            self.local_refs.insert(local);
            return;
        }
        if !matches!(self.mode, Mode::Record { .. })
            || !self
                .known_crates
                .contains(cx.tcx.crate_name(def_id.krate).as_str())
        {
            return;
        }
        let known = &self.known;
        let key = self
            .keys
            .entry(def_id)
            .or_insert_with(|| Some(files::key(cx.tcx, def_id)).filter(|k| known.contains(k)));
        if let Some(key) = key {
            self.foreign_refs.insert(key.clone());
        }
    }
}

/// The item a use counts for: a constructor or variant counts as its
/// struct or enum.
fn owning_item(cx: &LateContext<'_>, def_id: DefId) -> DefId {
    match cx.tcx.def_kind(def_id) {
        DefKind::Ctor(..) => owning_item(cx, cx.tcx.parent(def_id)),
        DefKind::Variant => cx.tcx.parent(def_id),
        _ => def_id,
    }
}

/// Where `unused_pub`'s level at an item comes from, in the words rustc puts
/// under a lint (`explain_lint_level_source`), for `cargo mordant` to print
/// under the finding.
fn level_source(cx: &LateContext<'_>, level: Level, src: LintLevelSource) -> Vec<Note> {
    let name = UNUSED_PUB.name_lower();
    let note = |message: String| Note {
        help: false,
        message,
        at: None,
    };
    match src {
        LintLevelSource::Default => {
            vec![note(format!(
                "`#[{}({name})]` on by default",
                level.as_str()
            ))]
        }
        LintLevelSource::CommandLine(flag_value, set) => {
            let flag = set.to_cmd_flag();
            let hyphenated = name.replace('_', "-");
            if flag_value.as_str() == name {
                return vec![note(format!(
                    "requested on the command line with `{flag} {hyphenated}`"
                ))];
            }
            let group = flag_value.as_str().replace('_', "-");
            vec![
                note(format!("`{flag} {hyphenated}` implied by `{flag} {group}`")),
                Note {
                    help: true,
                    message: format!("to override `{flag} {group}` add `#[allow({name})]`"),
                    at: None,
                },
            ]
        }
        LintLevelSource::Node {
            name: attr, span, ..
        } => {
            let mut notes = vec![Note {
                help: false,
                message: "the lint level is defined here".to_string(),
                at: files::locate(cx, span),
            }];
            if attr.as_str() != name {
                let level = level.as_str();
                notes.push(note(format!(
                    "`#[{level}({name})]` implied by `#[{level}({attr})]`"
                )));
            }
            notes
        }
    }
}

/// The trait or, for an inherent impl, the type an associated item
/// belongs to.
fn parent_key(cx: &LateContext<'_>, did: DefId) -> Option<String> {
    if !matches!(
        cx.tcx.def_kind(did),
        DefKind::AssocFn | DefKind::AssocConst { .. }
    ) {
        return None;
    }
    let parent = cx.tcx.parent(did);
    let owner = match cx.tcx.def_kind(parent) {
        DefKind::Trait => parent,
        DefKind::Impl { .. } => cx
            .tcx
            .type_of(parent)
            .instantiate_identity()
            .skip_normalization()
            .ty_adt_def()?
            .did(),
        _ => return None,
    };
    Some(files::key(cx.tcx, owner))
}

/// The ADT of the `impl` block `hir_id` is written in, if any.
fn enclosing_impl_self_adt(cx: &LateContext<'_>, hir_id: HirId) -> Option<DefId> {
    let owner = hir_id.owner.to_def_id();
    let impl_id = match cx.tcx.def_kind(owner) {
        DefKind::Impl { .. } => owner,
        DefKind::AssocFn | DefKind::AssocConst { .. } | DefKind::AssocTy => cx.tcx.parent(owner),
        _ => return None,
    };
    if !matches!(cx.tcx.def_kind(impl_id), DefKind::Impl { .. }) {
        return None;
    }
    cx.tcx
        .type_of(impl_id)
        .instantiate_identity()
        .skip_normalization()
        .ty_adt_def()
        .map(|adt| adt.did())
}

/// In a different file from the module body around it: brought in by
/// `include!`, which is how generated code enters a crate, and an unused
/// item there is the generator's to drop. An out-of-line `mod m;` also
/// changes file, but then the module body and its items agree; only
/// `include!` puts an item in a file its module body is not in, at some
/// level between the item and the crate root.
fn included(cx: &LateContext<'_>, hir_id: HirId, item_span: Span) -> bool {
    let sm = cx.tcx.sess.source_map();
    let file_of = |span: Span| sm.lookup_source_file(span.lo()).start_pos;
    let mut file = file_of(item_span);
    let mut module = cx.tcx.parent_module(hir_id);
    loop {
        match cx.tcx.hir_node_by_def_id(module.to_local_def_id()) {
            Node::Crate(body) => return file != file_of(body.spans.inner_span),
            Node::Item(item) if let ItemKind::Mod(_, body) = item.kind => {
                if file != file_of(body.spans.inner_span) {
                    return true;
                }
                file = file_of(item.span);
                module = cx.tcx.parent_module(item.hir_id());
            }
            _ => return false,
        }
    }
}

/// Reached by symbol name from outside Rust, required by the language, or
/// the program's entry point.
fn exempt(cx: &LateContext<'_>, def_id: LocalDefId) -> bool {
    let did = def_id.to_def_id();
    let by_symbol = matches!(
        cx.tcx.def_kind(did),
        DefKind::Fn | DefKind::AssocFn | DefKind::Static { .. }
    ) && {
        let attrs = cx.tcx.codegen_fn_attrs(did);
        attrs.contains_extern_indicator()
            || attrs
                .flags
                .intersects(CodegenFnAttrFlags::USED_COMPILER | CodegenFnAttrFlags::USED_LINKER)
    };
    by_symbol
        || cx.tcx.lang_items().from_def_id(did).is_some()
        || cx.tcx.entry_fn(()).is_some_and(|(entry, _)| entry == did)
}

fn report(cx: &LateContext<'_>, span: Span, def: &Def) {
    crate::baseline::emit(
        cx,
        UNUSED_PUB,
        span,
        format!(
            "{} `{}` is public, but nothing in the workspace uses it",
            def.descr, def.path
        ),
        "remove it; if code under a `cfg`, target or feature not compiled here uses it, \
         gate the item the same way",
    );
}
