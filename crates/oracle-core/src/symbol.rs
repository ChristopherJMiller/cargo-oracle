//! The unit of attribution: a *symbol*, keyed by its definition site.
//!
//! # Why the key is a span and not a name
//!
//! Every downstream tool we join against reports `file:line:col`, and none of
//! them agree on names:
//!
//! - **llvm-cov** reports *mangled, monomorphized* symbols. One generic
//!   `fn parse<T>` appears once per instantiation (`parse::<u32>`,
//!   `parse::<String>`, ...), closures appear as `parse::{{closure}}`, and an
//!   `async fn` appears as the state machine the compiler generated for it.
//! - **cargo-mutants** reports the source path and the unmangled function name.
//! - **`syn`** — our inventory — sees the written source and nothing else.
//!
//! Reconciling mangled names across those three is a swamp. Definition spans
//! are not: every tool emits one, and *containment* does the join for free.
//! N monomorphized coverage entries all land inside the one source span that
//! defined them, so instantiations collapse without any demangling. Code with
//! no source span — derive output, macro expansions — falls outside every
//! inventory span and filters itself out, which is the behaviour we want.

use serde::{Deserialize, Serialize};
use std::fmt;

/// A symbol's identity: where it was *defined*.
///
/// Lines are 1-based. Columns are 1-based, matching `cargo-mutants` output
/// (`proc-macro2` hands us 0-based columns; [`SymbolId::new`] adjusts).
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SymbolId {
    /// Path relative to the workspace root, so IDs are stable across machines.
    pub file: String,
    /// 1-based line of the symbol's *name* in its definition.
    pub line: u32,
    /// 1-based column, matching `cargo-mutants` output.
    pub col: u32,
}

impl SymbolId {
    /// Build an ID from a `proc-macro2` position.
    ///
    /// `proc-macro2` reports 0-based columns while `cargo-mutants` reports 1-based
    /// ones; this adjusts so the two join without a fudge factor at the call site.
    ///
    /// ```
    /// use oracle_core::symbol::SymbolId;
    ///
    /// let id = SymbolId::new("src/config.rs", 38, 4);
    /// assert_eq!(id.col, 5);
    /// assert_eq!(id.to_string(), "src/config.rs:38:5");
    /// ```
    pub fn new(file: impl Into<String>, line: usize, zero_based_col: usize) -> Self {
        Self {
            file: file.into(),
            line: line as u32,
            col: zero_based_col as u32 + 1,
        }
    }
}

impl fmt::Display for SymbolId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}:{}", self.file, self.line, self.col)
    }
}

/// A half-open line range covering an item's full definition, used to decide
/// which coverage regions and which mutants belong to this symbol.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineSpan {
    /// First line of the definition, at the `fn` keyword.
    pub start: u32,
    /// Last line, at the closing brace.
    pub end: u32,
}

impl LineSpan {
    /// Whether `line` falls inside this span.
    ///
    /// This is the join primitive: every tool cargo-oracle reads reports
    /// `file:line`, and containment places each report on a symbol without ever
    /// comparing names.
    ///
    /// ```
    /// use oracle_core::symbol::LineSpan;
    ///
    /// let span = LineSpan { start: 38, end: 42 };
    /// assert!(span.contains(38), "inclusive at the start");
    /// assert!(span.contains(42), "inclusive at the end");
    /// assert!(!span.contains(37));
    /// ```
    pub fn contains(&self, line: u32) -> bool {
        line >= self.start && line <= self.end
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
/// What kind of item a symbol is.
pub enum SymbolKind {
    /// A free function at module scope.
    Free,
    /// A method in an inherent `impl Type { .. }` block.
    Inherent,
    /// A method in an `impl Trait for Type { .. }` block.
    TraitImpl,
    /// A *default body* in a `trait T { fn f() { .. } }` declaration. Monomorphized
    /// into every implementor, so one source symbol, many runtime instantiations.
    TraitDefault,
}

/// How the callee can be observed, derived from the signature.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SelfKind {
    /// Free function, or an associated function with no receiver.
    None,
    /// `self`
    Value,
    /// `&self`
    Ref,
    /// `&mut self`
    RefMut,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
/// The shape of a function's return type, as far as an oracle is concerned.
pub enum ReturnShape {
    /// `-> ()` or no return type.
    Unit,
    /// `-> !`
    Never,
    /// `-> Result<_, _>`
    ResultLike,
    /// `-> Option<_>`
    OptionLike,
    /// `-> impl Trait` — frequently *unmutatable*, since `Default` rarely applies.
    ImplTrait,
    /// Any other concrete return type.
    Value,
}

impl ReturnShape {
    /// Whether a test can observe this call by looking at what it returned.
    pub fn is_observable(self) -> bool {
        !matches!(self, ReturnShape::Unit | ReturnShape::Never)
    }
}

/// What a test *must* do to have any chance of detecting a fault here.
///
/// This is the check that Rust makes unusually tractable: `&mut` and ownership
/// put effects in the signature, so the shape of a required oracle is readable
/// from the type alone. A test that only inspects a return value cannot detect
/// a fault in a `fn(&mut self)` no matter how many assertions it contains.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RequiredOracle {
    /// Observe the returned value.
    ReturnValue,
    /// Observe the receiver (or a `&mut` argument) *after* the call.
    PostState,
    /// Both of the above; either alone leaves a blind spot.
    Both,
    /// Neither: no receiver mutated, nothing returned. Any effect escapes
    /// through I/O, globals, or interior mutability, so no signature-derived
    /// oracle exists and the test must reach for a side channel.
    SideChannel,
}

impl RequiredOracle {
    fn derive(self_kind: SelfKind, ret: ReturnShape, has_mut_param: bool) -> Self {
        let mutates = matches!(self_kind, SelfKind::RefMut) || has_mut_param;
        match (mutates, ret.is_observable()) {
            (true, true) => RequiredOracle::Both,
            (true, false) => RequiredOracle::PostState,
            (false, true) => RequiredOracle::ReturnValue,
            (false, false) => RequiredOracle::SideChannel,
        }
    }
}

/// Whether this symbol carries enough behaviour to be worth scoring.
///
/// Mirrors the "low-signal class" exclusions that production mutation gates
/// apply to DTOs and accessors: mutating `fn name(&self) -> &str { &self.name }`
/// produces a mutant that is technically uncaught and entirely unactionable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Triviality {
    /// Real logic. Score it.
    Normal,
    /// Body is a bare field access, a literal, or a passthrough delegation.
    Accessor,
    /// Body is empty or `()`. Nothing to destroy.
    Empty,
}

impl Triviality {
    /// Whether this symbol carries enough behaviour to be worth scoring.
    ///
    /// Accessors and empty bodies are excluded: a mutant of
    /// `fn name(&self) -> &str { &self.name }` is uncaught and unactionable.
    pub fn is_scorable(self) -> bool {
        matches!(self, Triviality::Normal)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
/// How widely a symbol is reachable, which bounds who can claim it.
pub enum Visibility {
    /// `pub`.
    Public,
    /// `pub(crate)`.
    Crate,
    /// Private, or restricted to some module.
    Private,
}

/// One testable item in the crate under audit.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Symbol {
    /// Where the symbol is defined; its identity.
    pub id: SymbolId,
    /// Display path, e.g. `mycrate::parser::Config::validate` or
    /// `mycrate::parser::<Config as Display>::fmt`.
    pub path: String,
    /// Bare name, for name-similarity claim matching.
    pub name: String,
    /// Free function, inherent method, trait impl, or trait default body.
    pub kind: SymbolKind,
    /// Full extent of the definition, used for containment joins.
    pub span: LineSpan,
    /// Visibility as written on the item.
    pub visibility: Visibility,
    /// The receiver, if any.
    pub self_kind: SelfKind,
    /// The return type's shape.
    pub returns: ReturnShape,
    /// Whether any argument is a `&mut` reference, which also implies post-state.
    pub has_mut_param: bool,
    /// What a test must observe to detect a fault here. Derived, not declared.
    pub required_oracle: RequiredOracle,
    /// Whether there is enough behaviour here to score.
    pub triviality: Triviality,
    /// `async fn`.
    pub is_async: bool,
    /// `const fn`. Cannot host a runtime mutation switch.
    pub is_const: bool,
    /// `unsafe fn`.
    pub is_unsafe: bool,
    /// Returns `Result<(), _>` or `Option<()>`: the success case carries no
    /// payload, so `is_ok()`/`is_some()` on it observes everything there is.
    pub returns_unit_ok: bool,
    /// Trait being implemented, when `kind` is `TraitImpl`.
    pub trait_name: Option<String>,
    /// Number of doctests attached to this item's documentation.
    pub doctests: usize,
}

impl Symbol {
    /// Field-by-field constructor. The arity is the record's, not a design choice;
    /// `required_oracle` is the one derived field, so it is not accepted here.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: SymbolId,
        path: String,
        name: String,
        kind: SymbolKind,
        span: LineSpan,
        visibility: Visibility,
        self_kind: SelfKind,
        returns: ReturnShape,
        has_mut_param: bool,
        triviality: Triviality,
    ) -> Self {
        Self {
            id,
            path,
            name,
            kind,
            span,
            visibility,
            self_kind,
            returns,
            has_mut_param,
            required_oracle: RequiredOracle::derive(self_kind, returns, has_mut_param),
            triviality,
            is_async: false,
            is_const: false,
            is_unsafe: false,
            returns_unit_ok: false,
            trait_name: None,
            doctests: 0,
        }
    }

    /// `const fn` cannot host a runtime mutation switch, and `impl Trait`
    /// returns usually have no `Default`. Both show up as *unviable* mutants
    /// rather than gaps — see the `type-enforced` state in the report.
    pub fn likely_unviable_to_mutate(&self) -> bool {
        self.is_const || matches!(self.returns, ReturnShape::ImplTrait | ReturnShape::Never)
    }
}
