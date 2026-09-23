//! Single-compile mutation: rewrite once, build once, run many.
//!
//! The premise of slice v3 is that cargo-mutants must recompile for every
//! mutant, and in Rust that build dominates — a mutant costs seconds of
//! compiler time for milliseconds of test time. This module takes the other
//! side of the trade: compile *every* mutation in at once behind a runtime
//! switch, then select one per test run with an environment variable.
//!
//! ```text
//! cargo-mutants:  N × (rewrite + build + test)
//! fastmutate:     1 × (rewrite + build) + N × test
//! ```
//!
//! # Textual insertion, not reprinting
//!
//! The rewrite inserts a guard immediately after each function body's opening
//! brace, by byte offset, leaving every other character of the file untouched.
//! Reprinting the parsed AST would be easier but renumbers every line, and line
//! numbers are this project's join key — see [`crate::symbol`]. Keeping the
//! file byte-identical apart from the insertions means a `SymbolId` computed
//! against the original tree still addresses the same code in the rewritten
//! one.
//!
//! # Status
//!
//! Experimental, and narrower than [`crate::mutation`]: it generates one mutant
//! per function (body replacement) where cargo-mutants also mutates operators,
//! and it skips constructs listed in [`SkipReason`]. Use it to iterate; use
//! `cargo oracle verify` for the authoritative answer.

use crate::inventory::Inventory;
use crate::symbol::{ReturnShape, Symbol, SymbolId};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

/// Why a symbol cannot carry a runtime mutation switch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SkipReason {
    /// `const fn` cannot call the switch at compile time.
    ConstFn,
    /// `-> impl Trait` cannot be named as a generic argument, so the type
    /// cannot be probed for a `Default`.
    ImplTraitReturn,
    /// `-> !` never returns, so there is no value to substitute.
    NeverReturns,
    /// An accessor or empty body: nothing worth destroying.
    NotScorable,
}

impl SkipReason {
    /// Human-readable explanation for reports.
    pub fn label(self) -> &'static str {
        match self {
            SkipReason::ConstFn => "const fn cannot call the switch",
            SkipReason::ImplTraitReturn => "`impl Trait` cannot be probed for Default",
            SkipReason::NeverReturns => "diverging function has no value to return",
            SkipReason::NotScorable => "accessor or empty body",
        }
    }
}

/// One function that will carry a switch, and the id selecting it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FastMutant {
    /// Value of `ORACLE_MUTANT` that activates this mutation.
    pub id: u32,
    /// The symbol whose body is replaced, addressed in the *original* tree.
    pub symbol: SymbolId,
    /// Display path, for reports.
    pub path: String,
    /// The replacement, as source, e.g. `Ok(Default::default())`.
    pub replacement: String,
}

/// Every mutation a rewrite will install, plus what it declined to touch.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Plan {
    /// Mutations to install, in id order.
    pub mutants: Vec<FastMutant>,
    /// Symbols deliberately left alone, and why.
    pub skipped: Vec<(String, SkipReason)>,
}

impl Plan {
    /// Decide which symbols can carry a switch.
    pub fn build(inv: &Inventory) -> Self {
        let mut plan = Plan::default();

        for symbol in &inv.symbols {
            if !symbol.triviality.is_scorable() {
                plan.skipped
                    .push((symbol.path.clone(), SkipReason::NotScorable));
                continue;
            }
            if let Some(reason) = skip_reason(symbol) {
                plan.skipped.push((symbol.path.clone(), reason));
                continue;
            }

            let id = plan.mutants.len() as u32;
            plan.mutants.push(FastMutant {
                id,
                symbol: symbol.id.clone(),
                path: symbol.path.clone(),
                replacement: describe_replacement(symbol),
            });
        }

        plan
    }

    /// Look up which symbol an id belongs to.
    pub fn symbol_for(&self, id: u32) -> Option<&FastMutant> {
        self.mutants.iter().find(|m| m.id == id)
    }
}

fn skip_reason(symbol: &Symbol) -> Option<SkipReason> {
    if symbol.is_const {
        return Some(SkipReason::ConstFn);
    }
    match symbol.returns {
        ReturnShape::ImplTrait => Some(SkipReason::ImplTraitReturn),
        ReturnShape::Never => Some(SkipReason::NeverReturns),
        _ => None,
    }
}

fn describe_replacement(symbol: &Symbol) -> String {
    match symbol.returns {
        ReturnShape::Unit => "()".into(),
        ReturnShape::ResultLike => "Ok(Default::default())".into(),
        _ => "Default::default()".into(),
    }
}

/// Map 1-based line and 0-based column to a byte offset in `text`.
///
/// ```
/// use oracle_core::fastmutate::byte_offset;
///
/// let text = "fn a() {}\nfn b() {}\n";
/// assert_eq!(byte_offset(text, 1, 0), Some(0));
/// assert_eq!(byte_offset(text, 2, 0), Some(10));
/// assert_eq!(byte_offset(text, 2, 3), Some(13));
/// assert_eq!(byte_offset(text, 9, 0), None, "past the end");
/// ```
pub fn byte_offset(text: &str, line: u32, col: u32) -> Option<usize> {
    let mut offset = 0usize;
    for (n, l) in text.split_inclusive('\n').enumerate() {
        if n as u32 + 1 == line {
            // Columns are counted in characters, not bytes.
            let within: usize = l.chars().take(col as usize).map(char::len_utf8).sum();
            return Some(offset + within);
        }
        offset += l.len();
    }
    None
}

/// The guard inserted at the top of a function body.
///
/// `active` is checked first so a baseline run — no environment variable set —
/// pays one integer comparison and skips the probe entirely.
///
/// ```
/// use oracle_core::fastmutate::guard;
/// use oracle_core::symbol::ReturnShape;
///
/// // A fallible function probes its *success* type: `Result` has no `Default`.
/// let g = guard(7, ReturnShape::ResultLike, Some("Config"));
/// assert!(g.contains("active(7u32)"));
/// assert!(g.contains("Probe::<Config>"));
/// assert!(g.contains("return Ok(__oracle_v)"));
///
/// // Anything else probes the return type directly.
/// let g = guard(3, ReturnShape::Value, Some("u16"));
/// assert!(g.contains("return __oracle_v"));
/// ```
pub fn guard(id: u32, returns: ReturnShape, inner_type: Option<&str>) -> String {
    let (probe_ty, wrap) = match returns {
        ReturnShape::Unit => ("()".to_string(), "__oracle_v"),
        // `Result<T, E>` has no `Default`; probe `T` and wrap.
        ReturnShape::ResultLike => (inner_type.unwrap_or("_").to_string(), "Ok(__oracle_v)"),
        _ => (inner_type.unwrap_or("_").to_string(), "__oracle_v"),
    };

    format!(
        " if ::oracle_switch::active({id}u32) {{ \
         #[allow(unused_imports)] use ::oracle_switch::{{ViaDefault as _, ViaFallback as _}}; \
         match (&&::oracle_switch::Probe::<{probe_ty}>::new()).oracle_default() {{ \
         Some(__oracle_v) => {{ ::oracle_switch::note(::oracle_switch::Note::Applied); \
         return {wrap}; }} \
         None => ::oracle_switch::note(::oracle_switch::Note::Inert), }} }}"
    )
}

/// Copy a crate tree and install the switches described by `plan`.
///
/// Only files containing mutants are rewritten; everything else is copied
/// byte-for-byte. `target/` is skipped.
pub fn rewrite_tree(src_root: &Path, dst_root: &Path, inv: &Inventory, plan: &Plan) -> Result<()> {
    // Group insertions by file so each is read and written once.
    let mut by_file: BTreeMap<&str, Vec<&FastMutant>> = BTreeMap::new();
    for mutant in &plan.mutants {
        by_file
            .entry(mutant.symbol.file.as_str())
            .or_default()
            .push(mutant);
    }

    if dst_root.exists() {
        std::fs::remove_dir_all(dst_root)
            .with_context(|| format!("clearing {}", dst_root.display()))?;
    }
    std::fs::create_dir_all(dst_root)?;

    for entry in walkdir::WalkDir::new(src_root)
        .into_iter()
        .filter_entry(|e| {
            e.file_name() != "target" && !e.file_name().to_string_lossy().starts_with('.')
        })
        .filter_map(|e| e.ok())
    {
        let path = entry.path();
        let Ok(rel) = path.strip_prefix(src_root) else {
            continue;
        };
        let dst = dst_root.join(rel);

        if entry.file_type().is_dir() {
            std::fs::create_dir_all(&dst)?;
            continue;
        }

        let rel_str = rel.display().to_string();
        match by_file.get(rel_str.as_str()) {
            Some(mutants) => {
                let text = std::fs::read_to_string(path)
                    .with_context(|| format!("reading {}", path.display()))?;
                let rewritten = insert_guards(&text, inv, mutants)?;
                std::fs::write(&dst, rewritten)?;
            }
            None => {
                std::fs::copy(path, &dst).with_context(|| format!("copying {}", path.display()))?;
            }
        }
    }

    Ok(())
}

/// Insert each mutant's guard just after its function body's opening brace.
///
/// Insertions are applied from the end of the file backwards so earlier byte
/// offsets stay valid.
fn insert_guards(text: &str, inv: &Inventory, mutants: &[&FastMutant]) -> Result<String> {
    let ast = syn::parse_file(text).map_err(|e| anyhow::anyhow!("parse error: {e}"))?;

    // Find each target's opening brace by walking the AST for a matching span.
    let mut edits: Vec<(usize, String)> = Vec::new();
    for mutant in mutants {
        let Some(symbol) = inv.symbol(&mutant.symbol) else {
            continue;
        };
        let Some((line, col, inner)) = find_body_open(&ast, symbol) else {
            continue;
        };
        let Some(offset) = byte_offset(text, line, col) else {
            continue;
        };
        // Insert *after* the brace itself.
        edits.push((
            offset + 1,
            guard(mutant.id, symbol.returns, inner.as_deref()),
        ));
    }

    edits.sort_by_key(|(offset, _)| std::cmp::Reverse(*offset));
    let mut out = text.to_string();
    for (offset, insertion) in edits {
        out.insert_str(offset, &insertion);
    }
    Ok(out)
}

/// Locate a symbol's body brace, and the type to probe.
///
/// Returns `(line, 0-based col, inner type)` where the inner type is the
/// success type for a `Result` and the whole return type otherwise.
fn find_body_open(ast: &syn::File, symbol: &Symbol) -> Option<(u32, u32, Option<String>)> {
    use quote::ToTokens;

    fn probe_type(sig: &syn::Signature) -> Option<String> {
        let syn::ReturnType::Type(_, ty) = &sig.output else {
            return Some("()".to_string());
        };
        // For `Result<T, E>`, probe `T`.
        if let syn::Type::Path(p) = &**ty {
            if let Some(last) = p.path.segments.last() {
                if last.ident == "Result" {
                    if let syn::PathArguments::AngleBracketed(args) = &last.arguments {
                        if let Some(syn::GenericArgument::Type(ok)) = args.args.first() {
                            return Some(ok.to_token_stream().to_string());
                        }
                    }
                    // `Result<T>` with a crate alias: the error is implied, so
                    // there is no success type we can name.
                    return None;
                }
            }
        }
        Some(ty.to_token_stream().to_string())
    }

    fn matches(sig: &syn::Signature, symbol: &Symbol) -> bool {
        sig.ident.span().start().line as u32 == symbol.id.line
    }

    fn open_of(block: &syn::Block) -> (u32, u32) {
        let open = block.brace_token.span.open().start();
        (open.line as u32, open.column as u32)
    }

    fn search(items: &[syn::Item], symbol: &Symbol) -> Option<(u32, u32, Option<String>)> {
        for item in items {
            match item {
                syn::Item::Fn(f) if matches(&f.sig, symbol) => {
                    let (l, c) = open_of(&f.block);
                    return Some((l, c, probe_type(&f.sig)));
                }
                syn::Item::Impl(i) => {
                    for sub in &i.items {
                        if let syn::ImplItem::Fn(f) = sub {
                            if matches(&f.sig, symbol) {
                                let (l, c) = open_of(&f.block);
                                return Some((l, c, probe_type(&f.sig)));
                            }
                        }
                    }
                }
                syn::Item::Trait(t) => {
                    for sub in &t.items {
                        if let syn::TraitItem::Fn(f) = sub {
                            if let Some(block) = &f.default {
                                if matches(&f.sig, symbol) {
                                    let (l, c) = open_of(block);
                                    return Some((l, c, probe_type(&f.sig)));
                                }
                            }
                        }
                    }
                }
                syn::Item::Mod(m) => {
                    if let Some((_, items)) = &m.content {
                        if let Some(found) = search(items, symbol) {
                            return Some(found);
                        }
                    }
                }
                _ => {}
            }
        }
        None
    }

    search(&ast.items, symbol)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_offset_counts_characters_not_bytes() {
        // A column is a character index; naive byte arithmetic would land
        // mid-codepoint and corrupt the file.
        let text = "fn é() { 1 }\n";
        assert_eq!(byte_offset(text, 1, 0), Some(0));
        assert_eq!(
            byte_offset(text, 1, 3),
            Some(3),
            "column 3 is where `é` starts"
        );
        assert_eq!(
            byte_offset(text, 1, 4),
            Some(5),
            "one column later is two bytes later: `é` is multi-byte"
        );
    }

    #[test]
    fn byte_offset_past_the_last_line_is_none() {
        assert_eq!(byte_offset("a\nb\n", 5, 0), None);
    }

    #[test]
    fn a_result_returning_guard_probes_the_success_type_and_wraps_it() {
        let g = guard(4, ReturnShape::ResultLike, Some("Config"));
        assert!(g.contains("Probe::<Config>"), "{g}");
        assert!(g.contains("return Ok(__oracle_v)"), "{g}");
    }

    #[test]
    fn a_unit_returning_guard_probes_the_unit_type() {
        let g = guard(0, ReturnShape::Unit, None);
        assert!(g.contains("Probe::<()>"), "{g}");
        assert!(g.contains("return __oracle_v"), "{g}");
    }

    #[test]
    fn the_guard_checks_the_switch_before_probing() {
        // The baseline run sets no variable, so it must pay one comparison
        // and skip the probe entirely.
        let g = guard(9, ReturnShape::Value, Some("u8"));
        let active = g.find("active(9u32)").expect("switch check present");
        let probe = g.find("Probe::<u8>").expect("probe present");
        assert!(active < probe, "the switch must be checked first: {g}");
    }

    #[test]
    fn const_fns_and_impl_trait_returns_are_skipped_with_a_reason() {
        assert_eq!(
            SkipReason::ConstFn.label(),
            "const fn cannot call the switch"
        );
        assert!(SkipReason::ImplTraitReturn.label().contains("impl Trait"));
    }
}

// ---------------------------------------------------------------------------
// Driving the rewritten tree
// ---------------------------------------------------------------------------

use std::process::Command;

/// What one mutant run established.
///
/// Four outcomes where cargo-mutants reports three, because the runtime note
/// separates a genuine survivor from a mutation that could not be made and
/// from one no test ever reached.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FastOutcome {
    /// A test failed. The symbol is verified against this mutation.
    Caught,
    /// The body was replaced, a test ran it, and everything still passed.
    Survived,
    /// The return type has no `Default`, so nothing was actually changed.
    /// The same finding cargo-mutants reports as unviable.
    Inert,
    /// No test called the function, so the mutation never had a chance to
    /// matter. cargo-mutants cannot distinguish this from a survivor.
    Unreached,
}

impl FastOutcome {
    /// Short label for reports.
    pub fn label(self) -> &'static str {
        match self {
            FastOutcome::Caught => "caught",
            FastOutcome::Survived => "SURVIVED",
            FastOutcome::Inert => "inert (no Default)",
            FastOutcome::Unreached => "unreached",
        }
    }
}

/// Add an `oracle-switch` path dependency to a manifest.
///
/// Inserts into an existing `[dependencies]` table when there is one, since a
/// second `[dependencies]` header is a TOML error, and appends the section
/// otherwise.
///
/// ```
/// use oracle_core::fastmutate::patch_manifest_text;
///
/// let existing = "[package]\nname = \"x\"\n\n[dependencies]\nserde = \"1\"\n";
/// let patched = patch_manifest_text(existing, "/tmp/switch");
/// assert_eq!(patched.matches("[dependencies]").count(), 1);
/// assert!(patched.contains("oracle-switch = { path = \"/tmp/switch\" }"));
///
/// let bare = "[package]\nname = \"x\"\n";
/// let patched = patch_manifest_text(bare, "/tmp/switch");
/// assert!(patched.contains("[dependencies]"));
/// ```
pub fn patch_manifest_text(text: &str, switch_path: &str) -> String {
    let dep = format!("oracle-switch = {{ path = \"{switch_path}\" }}");

    if let Some(idx) = text.find("\n[dependencies]\n") {
        let insert_at = idx + "\n[dependencies]\n".len();
        let mut out = String::with_capacity(text.len() + dep.len() + 1);
        out.push_str(&text[..insert_at]);
        out.push_str(&dep);
        out.push('\n');
        out.push_str(&text[insert_at..]);
        return out;
    }
    format!("{text}\n[dependencies]\n{dep}\n")
}

/// Patch a manifest file in place.
pub fn patch_manifest(manifest: &Path, switch_path: &Path) -> Result<()> {
    let text = std::fs::read_to_string(manifest)
        .with_context(|| format!("reading {}", manifest.display()))?;
    let patched = patch_manifest_text(&text, &switch_path.display().to_string());
    std::fs::write(manifest, patched).with_context(|| format!("writing {}", manifest.display()))?;
    Ok(())
}

/// Build the rewritten tree once, and confirm it passes with nothing active.
///
/// A red baseline makes every later result meaningless, so this is checked
/// before any mutant runs — the same guard cargo-mutants applies.
pub fn run_baseline(dir: &Path) -> Result<()> {
    let status = Command::new("cargo")
        .current_dir(dir)
        .args(["nextest", "run", "--no-fail-fast"])
        .status()
        .context("running the baseline suite in the rewritten tree")?;

    if !status.success() {
        anyhow::bail!(
            "the rewritten tree fails its own tests with no mutant active. \
             Either the rewrite broke something, or the suite was already red."
        );
    }
    Ok(())
}

/// Run the suite with one mutant active. No rebuild: this is the whole point.
pub fn run_mutant(dir: &Path, id: u32, log: &Path) -> Result<FastOutcome> {
    let _ = std::fs::remove_file(log);

    let status = Command::new("cargo")
        .current_dir(dir)
        .args(["nextest", "run"])
        .env(oracle_switch_env(), id.to_string())
        .env(oracle_switch_log_env(), log)
        .status()
        .with_context(|| format!("running the suite with mutant {id} active"))?;

    if !status.success() {
        return Ok(FastOutcome::Caught);
    }

    // Tests passed. The note says whether the mutation was ever really made.
    let notes = std::fs::read_to_string(log).unwrap_or_default();
    Ok(if notes.contains("applied") {
        FastOutcome::Survived
    } else if notes.contains("inert") {
        FastOutcome::Inert
    } else {
        FastOutcome::Unreached
    })
}

// Kept as functions so the constants live in one crate only.
fn oracle_switch_env() -> &'static str {
    "ORACLE_MUTANT"
}
fn oracle_switch_log_env() -> &'static str {
    "ORACLE_MUTANT_LOG"
}

#[cfg(test)]
mod driver_tests {
    use super::*;

    #[test]
    fn patching_a_manifest_that_already_has_dependencies_adds_no_second_table() {
        let text = "[package]\nname = \"x\"\n\n[dependencies]\nserde = \"1\"\n";
        let out = patch_manifest_text(text, "/s");
        assert_eq!(
            out.matches("[dependencies]").count(),
            1,
            "a second [dependencies] header is a TOML error"
        );
        assert!(out.contains("serde = \"1\""), "existing deps survive");
        assert!(out.contains("oracle-switch = { path = \"/s\" }"));
    }

    #[test]
    fn patching_a_manifest_without_dependencies_appends_the_table() {
        let out = patch_manifest_text("[package]\nname = \"x\"\n", "/s");
        assert_eq!(out.matches("[dependencies]").count(), 1);
        assert!(out
            .trim_end()
            .ends_with("oracle-switch = { path = \"/s\" }"));
    }

    #[test]
    fn dev_dependencies_are_not_mistaken_for_dependencies() {
        let text = "[package]\nname = \"x\"\n\n[dev-dependencies]\nrstest = \"1\"\n";
        let out = patch_manifest_text(text, "/s");
        assert!(
            out.contains("[dev-dependencies]\nrstest"),
            "left alone: {out}"
        );
        assert!(out.contains("\n[dependencies]\noracle-switch"), "{out}");
    }

    #[test]
    fn outcome_labels_distinguish_a_survivor_from_an_unmade_mutation() {
        assert_eq!(FastOutcome::Survived.label(), "SURVIVED");
        assert!(FastOutcome::Inert.label().contains("no Default"));
        assert_ne!(
            FastOutcome::Unreached.label(),
            FastOutcome::Survived.label(),
            "cargo-mutants conflates these two; the runtime note separates them"
        );
    }
}

/// Whether a manifest is a virtual workspace root — `[workspace]` with no
/// `[package]`.
///
/// Such a manifest has no `[dependencies]` table to patch, and each member
/// needs the switch dependency separately. The prototype does not do that yet,
/// so it refuses rather than producing a tree that fails to build.
///
/// ```
/// use oracle_core::fastmutate::is_virtual_workspace;
///
/// assert!(is_virtual_workspace("[workspace]\nmembers = [\"a\"]\n"));
/// assert!(!is_virtual_workspace("[workspace]\n\n[package]\nname = \"x\"\n"));
/// assert!(!is_virtual_workspace("[package]\nname = \"x\"\n"));
/// ```
pub fn is_virtual_workspace(manifest: &str) -> bool {
    let has_workspace = manifest
        .lines()
        .any(|l| l.trim_start().starts_with("[workspace]"));
    let has_package = manifest
        .lines()
        .any(|l| l.trim_start().starts_with("[package]"));
    has_workspace && !has_package
}
