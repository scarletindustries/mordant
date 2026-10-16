//! Every value the crate writes into the fields of its own structs and enum
//! variants, gathered in one pass before the lints that ask about it run.
//!
//! A write is either a field of a struct literal (a build) or a write to the
//! field of a value that already exists: `x.f = ..`, `x.f += ..`, `&mut x.f`,
//! or a `ref mut` binding of the field. Each one gets an abstract [`Value`].
//! One set of rules applies to every lint that reads these facts:
//!
//! - A derived `Clone` copies a value and is not recorded.
//! - A derived `Default` is a build that writes each field's default.
//! - A field a `..base` literal copies from `base` is [`Value::Unknown`],
//!   unless `base` is the derived `Default` of the same type.
//! - Code from macros is recorded like any other code.
//!
//! The facts only answer. Each lint still decides which types it looks at.

mod collect;
mod value;

use std::sync::OnceLock;

use rustc_data_structures::fx::FxHashMap;
use rustc_hir::HirId;
use rustc_hir::def::DefKind;
use rustc_hir::def_id::DefId;
use rustc_middle::ty::{AdtDef, TyCtxt};
use rustc_span::{Span, Symbol};

pub use collect::FieldWritesPass;
pub(crate) use value::{Value, is_placeholder};

/// One struct literal of a local struct or enum variant.
pub(crate) struct Build {
    pub span: Span,
    pub hir_id: HirId,
    /// The fn the literal is in, with closures folded into the fn that wrote
    /// them.
    pub func: DefId,
    /// Built by a derived impl, such as the derived `Default`.
    pub derived: bool,
    /// Every field of the variant, in declaration order.
    pub fields: Vec<Init>,
}

impl Build {
    pub fn get(&self, field: Symbol) -> Option<&Init> {
        self.fields.iter().find(|i| i.name == field)
    }
}

/// What one build writes into one field.
pub(crate) struct Init {
    pub name: Symbol,
    pub value: Value,
    /// A value that stands for "nothing here": what `Default` gives, or a
    /// null pointer.
    pub placeholder: bool,
    /// Where the value is written. The literal itself for a field `..` fills.
    pub span: Span,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum WriteKind {
    /// `x.f = v`, or `x.f += v` and the other compound assignments, whose
    /// value is unknown.
    Assign,
    /// `&mut x.f`, a method that takes `&mut self` on it, or a `ref mut`
    /// binding of it. What it leaves behind is unknown.
    MutBorrow,
}

/// One write to a field of a value that already exists.
pub(crate) struct Write {
    pub kind: WriteKind,
    /// Known only for an `Assign`.
    pub value: Value,
    pub placeholder: bool,
    pub span: Span,
    /// The assignment, borrow or pattern.
    pub hir_id: HirId,
    /// The body the write is in. A closure is a body of its own.
    pub body: DefId,
    /// The innermost block around the write.
    pub block: Option<HirId>,
    /// For an `Assign`, the part of a block it runs in.
    pub block_part: Option<BlockPart>,
}

/// The part of a block an assignment runs in. Two assignments in the same
/// part always run together: no statement between them can leave the block.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) struct BlockPart {
    pub block: HirId,
    /// How many statements before the assignment can leave the block early.
    pub exits_before: usize,
    /// The place the field is read off, as a hash: `a.f` and `b.f` differ.
    pub place: u64,
}

/// Who besides this crate can write a type's fields.
#[derive(Clone, Copy)]
pub(crate) struct Access {
    /// The type is a struct that nothing outside the crate can name.
    pub private: bool,
    /// Another crate can build the variant with a literal.
    pub others_build: bool,
}

#[derive(Default)]
pub(crate) struct FieldWrites {
    builds: FxHashMap<DefId, Vec<Build>>,
    writes: FxHashMap<(DefId, Symbol), Vec<Write>>,
    access: FxHashMap<DefId, Access>,
    /// Fields that code in another crate can assign.
    exported: FxHashMap<DefId, Vec<Symbol>>,
}

/// The lints that read these facts. The pass runs when one of them is on.
pub const READERS: &[&str] = &[
    "options_as_enum",
    "parallel_bools",
    "bool_cluster",
    "derived_field",
    "bool_beside_option",
    "field_valid_only_when",
];

static FACTS: OnceLock<FieldWrites> = OnceLock::new();

/// The facts for the crate being linted. Empty until `FieldWritesPass` has
/// seen the whole crate, so read them only from `check_crate_post`.
pub(crate) fn facts() -> &'static FieldWrites {
    static EMPTY: OnceLock<FieldWrites> = OnceLock::new();
    FACTS
        .get()
        .unwrap_or_else(|| EMPTY.get_or_init(FieldWrites::default))
}

/// The struct or enum `variant` belongs to. A struct is its own variant.
pub(crate) fn adt_of(tcx: TyCtxt<'_>, variant: DefId) -> AdtDef<'_> {
    match tcx.def_kind(variant) {
        DefKind::Variant => tcx.adt_def(tcx.parent(variant)),
        _ => tcx.adt_def(variant),
    }
}

impl FieldWrites {
    /// Every variant (a struct is its own variant) with a build or a write,
    /// in definition order.
    pub fn variants(&self, tcx: TyCtxt<'_>) -> Vec<DefId> {
        let mut out: Vec<DefId> = self.access.keys().copied().collect();
        out.sort_by_key(|d| tcx.def_span(*d));
        out
    }

    /// The struct literals of `variant`. A derived `Clone` is not among them.
    pub fn builds(&self, variant: DefId) -> &[Build] {
        self.builds.get(&variant).map_or(&[], Vec::as_slice)
    }

    /// The writes to `field` of `variant` after it was built.
    pub fn writes(&self, variant: DefId, field: Symbol) -> &[Write] {
        self.writes
            .get(&(variant, field))
            .map_or(&[], Vec::as_slice)
    }

    pub fn access(&self, variant: DefId) -> Option<Access> {
        self.access.get(&variant).copied()
    }

    /// Whether code in another crate can assign `field` of `variant`.
    pub fn others_write(&self, variant: DefId, field: Symbol) -> bool {
        self.exported
            .get(&variant)
            .is_some_and(|fs| fs.contains(&field))
    }

    /// Whether the builds of this crate are the only writes to `field` of
    /// `variant`: no other crate can build the variant or assign the field,
    /// and nothing writes it after a build.
    pub fn only_built_here(&self, variant: DefId, field: Symbol) -> bool {
        self.access(variant).is_some_and(|r| !r.others_build)
            && !self.others_write(variant, field)
            && self.writes(variant, field).is_empty()
    }
}
