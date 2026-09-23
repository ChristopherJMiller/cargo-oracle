//! Parse a workspace into a [`Inventory`]: every testable symbol, and every
//! test that might speak for one.
//!
//! # Module paths without name resolution
//!
//! We do not resolve `mod` declarations. Instead a file's module path is read
//! off its location under `src/`, which is exactly what Rust's own convention
//! encodes: `src/parser/config.rs` is `parser::config`, `src/parser/mod.rs` is
//! `parser`, and `src/lib.rs` is the crate root. Inline `mod foo { .. }` blocks
//! nest further from there.
//!
//! The tradeoff is that a `.rs` file under `src/` that no `mod` declaration
//! reaches, such as an orphan left by a refactor, still gets inventoried.
//! That is the safe direction to be wrong in for an audit tool, and the symbol
//! will simply report as `unexecuted` once coverage runs.

use crate::symbol::*;
use anyhow::{Context, Result};
use quote::ToTokens;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use syn::spanned::Spanned;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
/// Where a test lives, which decides how its claim is derived.
pub enum TestKind {
    /// `#[test]` inside the crate, conventionally in a `#[cfg(test)] mod tests`.
    Unit,
    /// A `#[test]` in `tests/`, compiled as a separate binary against the public API.
    Integration,
    /// A fenced example in a doc comment. Attribution is exact and free: the
    /// doctest is lexically attached to the item it documents.
    Doctest,
}

/// How a test is addressed. `path` is written to match `cargo nextest list`
/// output so per-test attribution can join coverage profiles without a fuzzy match.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct TestId {
    /// Source file, relative to the workspace root.
    pub file: String,
    /// 1-based line of the test function's name.
    pub line: u32,
    /// e.g. `mycrate::config::tests::rejects_empty_host`
    pub path: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
/// Whether a `#[should_panic]` attribute names the panic it expects.
pub enum ShouldPanic {
    /// `#[should_panic]` with no `expected = "..."`. Passes on *any* panic,
    /// including one raised by an unrelated bug on the way to the code under test.
    Unqualified,
    /// `#[should_panic(expected = "...")]`. A real, if coarse, oracle.
    Qualified,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
/// One test, and everything needed to judge its oracles.
pub struct TestItem {
    /// How the test is addressed.
    pub id: TestId,
    /// Unit, integration, or doctest.
    pub kind: TestKind,
    /// Full extent of the test function.
    pub span: LineSpan,
    /// Span of the test function's name, which whole-test findings point at.
    pub name_span: crate::lint::Span,
    /// `#[ignore]`: it does not run in a default `cargo test`.
    pub is_ignored: bool,
    /// `async fn`, so it needs a runtime attribute to execute.
    pub is_async: bool,
    /// The `#[should_panic]` attribute, if present, and whether it is qualified.
    pub should_panic: Option<ShouldPanic>,
    /// Module path of the enclosing `#[cfg(test)] mod`, when there is one. This
    /// is the claim edge Rust gives us for free: a test module lives *inside*
    /// the file whose symbols it speaks for.
    pub enclosing_test_module: Option<String>,
    /// For `Doctest`, the symbol the example is attached to. An exact claim.
    pub doctest_target: Option<SymbolId>,
    /// Retained for the oracle lint; never serialized.
    #[serde(skip)]
    pub body: Option<syn::Block>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
/// Every symbol and test in a workspace.
pub struct Inventory {
    /// Absolute workspace root. Coverage reports use absolute paths; symbol
    /// IDs are relative to this, so the join needs it.
    pub root: String,
    /// Testable symbols, sorted by definition site.
    pub symbols: Vec<Symbol>,
    /// Tests, including doctests, sorted by definition site.
    pub tests: Vec<TestItem>,
    /// Source files parsed, relative to the workspace root.
    pub files: Vec<String>,
    /// Files that failed to parse, with the reason. Reported rather than
    /// swallowed: a crate we cannot parse is a crate we cannot audit.
    pub parse_failures: BTreeMap<String, String>,
}

impl Inventory {
    /// Look up a symbol by its ID.
    pub fn symbol(&self, id: &SymbolId) -> Option<&Symbol> {
        self.symbols.iter().find(|s| &s.id == id)
    }

    /// Symbols worth scoring: real logic, not accessors or empty bodies.
    pub fn scorable(&self) -> impl Iterator<Item = &Symbol> {
        self.symbols.iter().filter(|s| s.triviality.is_scorable())
    }
}

/// Where a source file sits, which decides how its tests are classified.
#[derive(Clone, Copy, PartialEq, Eq)]
enum FileRole {
    /// Under `src/`: contains the symbols under audit.
    Source,
    /// Under `tests/`: integration tests, claiming the public API.
    IntegrationTest,
}

/// Parse a Cargo workspace into an inventory.
///
/// Runs `cargo metadata` to find workspace members, then parses every `.rs`
/// file under each package's `src/` and `tests/`. Directories with their own
/// `Cargo.toml` are pruned, so a fixture crate nested inside `tests/` is not
/// attributed to its host package.
///
/// # Errors
///
/// Fails if `cargo metadata` cannot run. Individual files that fail to parse
/// are recorded in [`Inventory::parse_failures`] rather than aborting the walk.
pub fn walk_workspace(manifest_dir: &Path) -> Result<Inventory> {
    let metadata = cargo_metadata::MetadataCommand::new()
        .current_dir(manifest_dir)
        .no_deps()
        .exec()
        .context("running `cargo metadata` (is this a cargo workspace?)")?;

    let root = PathBuf::from(metadata.workspace_root.as_std_path());
    let mut inv = Inventory {
        root: root.display().to_string(),
        ..Default::default()
    };

    for package in metadata.workspace_packages() {
        let pkg_dir = package
            .manifest_path
            .parent()
            .context("package manifest has no parent directory")?
            .as_std_path()
            .to_path_buf();
        // Cargo allows `-` in package names but the crate path uses `_`.
        let crate_name = package.name.replace('-', "_");

        for (subdir, role) in [
            ("src", FileRole::Source),
            ("tests", FileRole::IntegrationTest),
        ] {
            let dir = pkg_dir.join(subdir);
            if !dir.is_dir() {
                continue;
            }
            // A directory with its own Cargo.toml is a separate package -- a
            // test fixture crate, or something vendored. Its symbols belong to
            // that package, so prune the subtree rather than adopting it here.
            let walker = walkdir::WalkDir::new(&dir).into_iter().filter_entry(|e| {
                !(e.file_type().is_dir()
                    && e.path() != dir.as_path()
                    && e.path().join("Cargo.toml").is_file())
            });

            for entry in walker.filter_map(|e| e.ok()) {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                    continue;
                }
                let rel = path
                    .strip_prefix(&root)
                    .unwrap_or(path)
                    .display()
                    .to_string();
                let module_path = module_path_for(path, &dir, role);

                match parse_file(path, &rel, &crate_name, &module_path, role) {
                    Ok(mut file_inv) => {
                        inv.symbols.append(&mut file_inv.symbols);
                        inv.tests.append(&mut file_inv.tests);
                        inv.files.push(rel);
                    }
                    Err(e) => {
                        inv.parse_failures.insert(rel, e.to_string());
                    }
                }
            }
        }
    }

    inv.symbols.sort_by(|a, b| a.id.cmp(&b.id));
    inv.tests.sort_by(|a, b| a.id.cmp(&b.id));
    inv.files.sort();
    Ok(inv)
}

/// `src/parser/config.rs` -> `["parser", "config"]`; `mod.rs` and the crate
/// root contribute nothing.
fn module_path_for(path: &Path, base: &Path, role: FileRole) -> Vec<String> {
    if role == FileRole::IntegrationTest {
        // Each file in `tests/` is its own crate root.
        return Vec::new();
    }
    let rel = match path.strip_prefix(base) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    let mut parts: Vec<String> = rel
        .parent()
        .map(|p| {
            p.components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();

    match rel.file_stem().and_then(|s| s.to_str()) {
        Some("lib") | Some("main") | Some("mod") | None => {}
        Some(stem) => parts.push(stem.to_string()),
    }
    parts
}

fn parse_file(
    path: &Path,
    rel: &str,
    crate_name: &str,
    module_path: &[String],
    role: FileRole,
) -> Result<Inventory> {
    let text = std::fs::read_to_string(path)?;
    let ast = syn::parse_file(&text).map_err(|e| anyhow::anyhow!("parse error: {e}"))?;

    let mut walker = Walker {
        file: rel.to_string(),
        crate_name: crate_name.to_string(),
        module_path: module_path.to_vec(),
        role,
        test_module: None,
        inv: Inventory::default(),
    };
    walker.items(&ast.items);
    Ok(walker.inv)
}

struct Walker {
    file: String,
    crate_name: String,
    module_path: Vec<String>,
    role: FileRole,
    /// Set while inside a `#[cfg(test)]` module; carries its full path.
    test_module: Option<String>,
    inv: Inventory,
}

impl Walker {
    fn qualified(&self, name: &str) -> String {
        let mut parts = vec![self.crate_name.clone()];
        parts.extend(self.module_path.iter().cloned());
        parts.push(name.to_string());
        parts.join("::")
    }

    fn module_prefix(&self) -> String {
        let mut parts = vec![self.crate_name.clone()];
        parts.extend(self.module_path.iter().cloned());
        parts.join("::")
    }

    fn items(&mut self, items: &[syn::Item]) {
        for item in items {
            match item {
                syn::Item::Fn(f) => self.free_fn(f),
                syn::Item::Impl(i) => self.impl_block(i),
                syn::Item::Trait(t) => self.trait_block(t),
                syn::Item::Mod(m) => self.module(m),
                _ => {}
            }
        }
    }

    fn module(&mut self, m: &syn::ItemMod) {
        let Some((_, items)) = &m.content else {
            return; // `mod foo;`, which the file walk reaches separately.
        };
        let name = m.ident.to_string();
        let was_test_module = self.test_module.clone();

        self.module_path.push(name);
        if self.test_module.is_none() && attrs_have_cfg_test(&m.attrs) {
            self.test_module = Some(self.module_prefix());
        }

        self.items(items);

        self.module_path.pop();
        self.test_module = was_test_module;
    }

    fn free_fn(&mut self, f: &syn::ItemFn) {
        if let Some(test) = self.as_test(&f.attrs, &f.sig, Some(&f.block)) {
            self.inv.tests.push(test);
            return;
        }
        if self.in_test_context() {
            return; // A helper inside `mod tests` is scaffolding, not a subject.
        }
        let symbol = self.make_symbol(
            &f.attrs,
            &f.sig,
            Some(&f.block),
            &f.vis,
            SymbolKind::Free,
            self.qualified(&f.sig.ident.to_string()),
        );
        self.inv.symbols.push(symbol);
    }

    fn impl_block(&mut self, i: &syn::ItemImpl) {
        let type_name = render_type(&i.self_ty);
        let trait_name = i
            .trait_
            .as_ref()
            .map(|(_, path, _)| path.to_token_stream().to_string().replace(' ', ""));

        for item in &i.items {
            let syn::ImplItem::Fn(f) = item else { continue };

            if let Some(test) = self.as_test(&f.attrs, &f.sig, Some(&f.block)) {
                self.inv.tests.push(test);
                continue;
            }
            if self.in_test_context() {
                continue;
            }

            let method = f.sig.ident.to_string();
            let (kind, display) = match &trait_name {
                Some(t) => (
                    SymbolKind::TraitImpl,
                    format!(
                        "{}::<{} as {}>::{}",
                        self.module_prefix(),
                        type_name,
                        t,
                        method
                    ),
                ),
                None => (
                    SymbolKind::Inherent,
                    format!("{}::{}::{}", self.module_prefix(), type_name, method),
                ),
            };

            let mut symbol =
                self.make_symbol(&f.attrs, &f.sig, Some(&f.block), &f.vis, kind, display);
            symbol.trait_name = trait_name.clone();
            self.inv.symbols.push(symbol);
        }
    }

    fn trait_block(&mut self, t: &syn::ItemTrait) {
        if self.in_test_context() {
            return;
        }
        let trait_name = t.ident.to_string();
        let trait_vis = t.vis.clone();
        for item in &t.items {
            let syn::TraitItem::Fn(f) = item else {
                continue;
            };
            // A declaration with no body defines no behaviour to destroy.
            let Some(block) = &f.default else { continue };

            let display = format!("{}::{}::{}", self.module_prefix(), trait_name, f.sig.ident);
            let symbol = self.make_symbol(
                &f.attrs,
                &f.sig,
                Some(block),
                &trait_vis,
                SymbolKind::TraitDefault,
                display,
            );
            self.inv.symbols.push(symbol);
        }
    }

    fn in_test_context(&self) -> bool {
        self.test_module.is_some() || self.role == FileRole::IntegrationTest
    }

    fn make_symbol(
        &mut self,
        attrs: &[syn::Attribute],
        sig: &syn::Signature,
        block: Option<&syn::Block>,
        vis: &syn::Visibility,
        kind: SymbolKind,
        display_path: String,
    ) -> Symbol {
        let ident = &sig.ident;
        let start = ident.span().start();
        let id = SymbolId::new(&self.file, start.line, start.column);

        let span = LineSpan {
            start: sig.fn_token.span().start().line as u32,
            end: block
                .map(|b| b.span().end().line as u32)
                .unwrap_or(start.line as u32),
        };

        let self_kind = self_kind_of(sig);
        let returns = return_shape_of(&sig.output);
        let has_mut_param = sig.inputs.iter().any(|arg| match arg {
            syn::FnArg::Typed(t) => is_mut_ref(&t.ty),
            syn::FnArg::Receiver(_) => false,
        });
        let triviality = block.map(triviality_of).unwrap_or(Triviality::Empty);

        let mut symbol = Symbol::new(
            id,
            display_path,
            ident.to_string(),
            kind,
            span,
            visibility_of(vis),
            self_kind,
            returns,
            has_mut_param,
            triviality,
        );
        symbol.is_async = sig.asyncness.is_some();
        symbol.is_const = sig.constness.is_some();
        symbol.is_unsafe = sig.unsafety.is_some();
        symbol.returns_unit_ok = wraps_unit(&sig.output);

        // Doctests attach to the item lexically: an exact claim, for free.
        let fences = extract_doctests(attrs);
        symbol.doctests = fences.len();
        for (n, code) in fences.iter().enumerate() {
            self.inv.tests.push(TestItem {
                id: TestId {
                    file: self.file.clone(),
                    line: symbol.span.start,
                    path: format!("{} (doctest {})", symbol.path, n + 1),
                },
                kind: TestKind::Doctest,
                span: symbol.span,
                name_span: crate::lint::Span::at(symbol.span.start, 1, 3),
                is_ignored: false,
                is_async: false,
                should_panic: None,
                enclosing_test_module: None,
                doctest_target: Some(symbol.id.clone()),
                body: parse_doctest_body(code),
            });
        }
        symbol
    }

    fn as_test(
        &self,
        attrs: &[syn::Attribute],
        sig: &syn::Signature,
        block: Option<&syn::Block>,
    ) -> Option<TestItem> {
        if !attrs_mark_test(attrs) {
            return None;
        }
        let start = sig.ident.span().start();
        let kind = match self.role {
            FileRole::IntegrationTest => TestKind::Integration,
            FileRole::Source => TestKind::Unit,
        };
        Some(TestItem {
            id: TestId {
                file: self.file.clone(),
                line: start.line as u32,
                path: self.qualified(&sig.ident.to_string()),
            },
            kind,
            span: LineSpan {
                start: sig.fn_token.span().start().line as u32,
                end: block
                    .map(|b| b.span().end().line as u32)
                    .unwrap_or(start.line as u32),
            },
            name_span: crate::lint::Span::at(
                start.line as u32,
                start.column as u32 + 1,
                sig.ident.to_string().chars().count() as u32,
            ),
            is_ignored: attrs.iter().any(|a| a.path().is_ident("ignore")),
            is_async: sig.asyncness.is_some(),
            should_panic: should_panic_of(attrs),
            enclosing_test_module: self.test_module.clone(),
            doctest_target: None,
            body: block.cloned(),
        })
    }
}

// ---------------------------------------------------------------------------
// Attribute and signature inspection
// ---------------------------------------------------------------------------

/// `#[test]`, `#[tokio::test]`, `#[rstest]`, and anything whose final path
/// segment names a test harness.
fn attrs_mark_test(attrs: &[syn::Attribute]) -> bool {
    const HARNESSES: &[&str] = &[
        "test",
        "rstest",
        "test_case",
        "proptest",
        "quickcheck",
        "bench",
    ];
    attrs.iter().any(|a| {
        a.path()
            .segments
            .last()
            .map(|s| HARNESSES.contains(&s.ident.to_string().as_str()))
            .unwrap_or(false)
    })
}

fn attrs_have_cfg_test(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|a| {
        if !a.path().is_ident("cfg") {
            return false;
        }
        let mut found = false;
        // Handles `cfg(test)` and one level of `cfg(all(test, ...))`.
        let _ = a.parse_nested_meta(|meta| {
            if meta.path.is_ident("test") {
                found = true;
            } else if meta.path.is_ident("all") || meta.path.is_ident("any") {
                let _ = meta.parse_nested_meta(|inner| {
                    if inner.path.is_ident("test") {
                        found = true;
                    }
                    Ok(())
                });
            }
            Ok(())
        });
        found
    })
}

fn should_panic_of(attrs: &[syn::Attribute]) -> Option<ShouldPanic> {
    let attr = attrs.iter().find(|a| a.path().is_ident("should_panic"))?;
    let qualified = match &attr.meta {
        syn::Meta::List(list) => list.tokens.to_string().contains("expected"),
        _ => false,
    };
    Some(if qualified {
        ShouldPanic::Qualified
    } else {
        ShouldPanic::Unqualified
    })
}

/// Extract the source of each fenced block rustdoc will actually compile.
///
/// `text`, `markdown` and `ignore` blocks are not compiled and yield nothing.
/// Lines hidden with a leading `#` are compiled and run, so they are kept;
/// dropping them would lose setup code that the assertions depend on.
fn extract_doctests(attrs: &[syn::Attribute]) -> Vec<String> {
    let mut doc = String::new();
    for attr in attrs.iter().filter(|a| a.path().is_ident("doc")) {
        if let syn::Meta::NameValue(nv) = &attr.meta {
            if let syn::Expr::Lit(syn::ExprLit {
                lit: syn::Lit::Str(s),
                ..
            }) = &nv.value
            {
                doc.push_str(&s.value());
                doc.push('\n');
            }
        }
    }

    let mut out = Vec::new();
    let mut current: Option<(bool, Vec<String>)> = None;

    for line in doc.lines() {
        let trimmed = line.trim_start();
        if let Some(info) = trimmed.strip_prefix("```") {
            match current.take() {
                Some((runnable, lines)) => {
                    if runnable {
                        out.push(lines.join("\n"));
                    }
                }
                None => {
                    let info = info.trim().to_ascii_lowercase();
                    let runnable = !info
                        .split(|c: char| c == ',' || c.is_whitespace())
                        .any(|tok| matches!(tok, "text" | "ignore" | "markdown"));
                    current = Some((runnable, Vec::new()));
                }
            }
        } else if let Some((_, lines)) = current.as_mut() {
            let code = if let Some(rest) = trimmed.strip_prefix("# ") {
                rest
            } else if trimmed == "#" {
                ""
            } else {
                line
            };
            lines.push(code.to_string());
        }
    }
    out
}

/// Turn doctest source into a block the oracle lint can read.
///
/// Doctests are real tests with real assertions; treating them as opaque would
/// report every one of them as having no oracle.
fn parse_doctest_body(code: &str) -> Option<syn::Block> {
    // A snippet that declares its own `fn main` supplies the block directly.
    if code.contains("fn main") {
        let file: syn::File = syn::parse_str(code).ok()?;
        return file.items.into_iter().find_map(|item| match item {
            syn::Item::Fn(f) if f.sig.ident == "main" => Some(*f.block),
            _ => None,
        });
    }
    // Otherwise rustdoc wraps the snippet in a main function; do the same.
    let wrapped = format!("fn __doctest() {{\n{code}\n}}");
    syn::parse_str::<syn::ItemFn>(&wrapped)
        .ok()
        .map(|f| *f.block)
}

fn visibility_of(vis: &syn::Visibility) -> Visibility {
    match vis {
        syn::Visibility::Public(_) => Visibility::Public,
        syn::Visibility::Restricted(r) => {
            if r.path.is_ident("crate") {
                Visibility::Crate
            } else {
                Visibility::Private
            }
        }
        syn::Visibility::Inherited => Visibility::Private,
    }
}

fn self_kind_of(sig: &syn::Signature) -> SelfKind {
    match sig.inputs.first() {
        Some(syn::FnArg::Receiver(r)) => {
            if r.reference.is_some() {
                if r.mutability.is_some() {
                    SelfKind::RefMut
                } else {
                    SelfKind::Ref
                }
            } else {
                SelfKind::Value
            }
        }
        _ => SelfKind::None,
    }
}

fn return_shape_of(output: &syn::ReturnType) -> ReturnShape {
    let syn::ReturnType::Type(_, ty) = output else {
        return ReturnShape::Unit;
    };
    match &**ty {
        syn::Type::Never(_) => ReturnShape::Never,
        syn::Type::Tuple(t) if t.elems.is_empty() => ReturnShape::Unit,
        syn::Type::ImplTrait(_) => ReturnShape::ImplTrait,
        syn::Type::Path(p) => match p.path.segments.last().map(|s| s.ident.to_string()) {
            Some(name) if name == "Result" => ReturnShape::ResultLike,
            Some(name) if name == "Option" => ReturnShape::OptionLike,
            _ => ReturnShape::Value,
        },
        _ => ReturnShape::Value,
    }
}

fn is_mut_ref(ty: &syn::Type) -> bool {
    matches!(ty, syn::Type::Reference(r) if r.mutability.is_some())
}

fn render_type(ty: &syn::Type) -> String {
    match ty {
        syn::Type::Path(p) => p
            .path
            .segments
            .last()
            .map(|s| s.ident.to_string())
            .unwrap_or_else(|| "_".into()),
        other => other.to_token_stream().to_string().replace(' ', ""),
    }
}

/// Classify bodies that carry no behaviour worth destroying.
fn triviality_of(block: &syn::Block) -> Triviality {
    if block.stmts.is_empty() {
        return Triviality::Empty;
    }
    if block.stmts.len() > 1 {
        return Triviality::Normal;
    }
    let expr = match &block.stmts[0] {
        syn::Stmt::Expr(e, _) => e,
        _ => return Triviality::Normal,
    };
    if is_accessor_expr(expr) {
        Triviality::Accessor
    } else {
        Triviality::Normal
    }
}

fn is_accessor_expr(expr: &syn::Expr) -> bool {
    const PASSTHROUGH: &[&str] = &[
        "clone",
        "to_string",
        "to_owned",
        "as_str",
        "as_ref",
        "as_slice",
        "to_vec",
        "into",
        "iter",
        "len",
        "is_empty",
        "borrow",
        "deref",
        "copied",
        "cloned",
    ];
    match expr {
        syn::Expr::Lit(_) => true,
        syn::Expr::Tuple(t) if t.elems.is_empty() => true,
        syn::Expr::Field(f) => matches!(&*f.base, syn::Expr::Path(p) if p.path.is_ident("self")),
        syn::Expr::Reference(r) => is_accessor_expr(&r.expr),
        syn::Expr::MethodCall(m) => {
            let name = m.method.to_string();
            PASSTHROUGH.contains(&name.as_str())
                && m.args.is_empty()
                && is_accessor_expr(&m.receiver)
        }
        _ => false,
    }
}

/// Does this return type wrap `()`, as `Result<(), E>` or `Option<()>` do?
///
/// Such a success case carries no payload, so a `is_ok()`/`is_some()` check on
/// it is a complete oracle rather than a discriminant-only one. `Result<(), E>`
/// is the shape of most fallible operations in Rust, so treating it as weak
/// would make ORC002 fire constantly and wrongly.
fn wraps_unit(output: &syn::ReturnType) -> bool {
    let syn::ReturnType::Type(_, ty) = output else {
        return false;
    };
    let syn::Type::Path(p) = &**ty else {
        return false;
    };
    let Some(last) = p.path.segments.last() else {
        return false;
    };
    if last.ident != "Result" && last.ident != "Option" {
        return false;
    }
    let syn::PathArguments::AngleBracketed(args) = &last.arguments else {
        // `Result<T>` behind a crate alias: the success type is not visible.
        return false;
    };
    matches!(
        args.args.first(),
        Some(syn::GenericArgument::Type(syn::Type::Tuple(t))) if t.elems.is_empty()
    )
}
