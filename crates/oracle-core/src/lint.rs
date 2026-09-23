//! Can this test's oracles fail at all?
//!
//! Not *is there an assertion* — that question does not separate good tests
//! from bad ones. Empirical work on AI-authored test commits finds agent tests
//! carry roughly twice the assertions of human ones while detecting fewer
//! injected faults. Density is, if anything, an inverted signal.
//!
//! What these rules look for instead is assertions that are *structurally
//! incapable* of failing, or capable of failing only for reasons unrelated to
//! the code under test. Every rule here is a pure `syn` pass: no build, no
//! execution, milliseconds on a whole workspace. They are cheap enough to run
//! before spending a single mutant, and they catch the patterns that mutation
//! testing would otherwise bill you an hour to discover.

use crate::inventory::{Inventory, ShouldPanic, TestId, TestItem};
use quote::ToTokens;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use syn::visit::Visit;

/// How much a test's *strongest* oracle can discriminate.
///
/// Ordering matters: a test is scored by its best oracle, not its average, so
/// one real `assert_eq!` redeems a body full of `is_ok()` checks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OracleStrength {
    /// Nothing in the body can fail except a panic escaping the code under test.
    None,
    /// Fails only on a coarse property: a panic, a discriminant, a wildcard match.
    Weak,
    /// Observes a real value, but only part of it.
    Partial,
    /// Observes a value against an independently written expectation.
    Strong,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
/// An oracle-strength rule. See `docs/oracle-lints.md` for worked examples.
pub enum Rule {
    /// ORC001: nothing in the body can fail except a panic from the code under test.
    NoOracle,
    /// ORC002: `is_ok()`/`is_some()` checks the discriminant and discards the payload.
    DiscriminantOnly,
    /// ORC003: the only failure mode is a panic from `unwrap`, `expect` or `?`.
    UnwrapOnly,
    /// ORC004: `matches!(x, V { .. })` wildcards every field.
    MatchesWildcard,
    /// ORC005: `#[should_panic]` with no `expected`, so any panic passes.
    ShouldPanicUnqualified,
    /// ORC006: `let _ = f();` runs `f` and observes nothing.
    DiscardedResult,
    /// ORC007: both operands are the same expression, or the condition is a literal.
    TautologicalAssert,
    /// ORC008: `#[ignore]`, so whatever it verifies is not verified.
    IgnoredTest,
    /// ORC009: the expectation is computed by the function under test.
    ComputedExpectation,
    /// ORC010: the claimed symbol mutates its receiver and no assertion observes it.
    OracleShapeMismatch,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
/// How much a finding should weigh in a gate.
pub enum Severity {
    /// Worth knowing; not worth blocking on.
    Low,
    /// A real weakness, usually fixable in one line.
    Medium,
    /// The oracle cannot do its job at all.
    High,
}

impl Rule {
    /// Every rule, in id order.
    pub const ALL: [Rule; 10] = [
        Rule::NoOracle,
        Rule::DiscriminantOnly,
        Rule::UnwrapOnly,
        Rule::MatchesWildcard,
        Rule::ShouldPanicUnqualified,
        Rule::DiscardedResult,
        Rule::TautologicalAssert,
        Rule::IgnoredTest,
        Rule::ComputedExpectation,
        Rule::OracleShapeMismatch,
    ];

    /// Look a rule up by id (`ORC002`) or name (`discriminant-only`), ignoring
    /// case.
    ///
    /// ```
    /// use oracle_core::lint::Rule;
    ///
    /// assert_eq!(Rule::find("ORC002"), Some(Rule::DiscriminantOnly));
    /// assert_eq!(Rule::find("orc002"), Some(Rule::DiscriminantOnly));
    /// assert_eq!(Rule::find("discriminant-only"), Some(Rule::DiscriminantOnly));
    /// assert_eq!(Rule::find("nope"), None);
    /// ```
    pub fn find(needle: &str) -> Option<Rule> {
        let needle = needle.trim().to_ascii_lowercase();
        Rule::ALL
            .into_iter()
            .find(|r| r.id().to_ascii_lowercase() == needle || r.name() == needle)
    }

    /// What to do about it.
    ///
    /// `why` explains the problem; this says how to make the oracle able to
    /// fail. A finding without a fix is a complaint.
    pub fn fix(self) -> &'static str {
        match self {
            Rule::NoOracle => {
                "Assert on what the call produced. If it returns a value, compare it to an \
                 expected one; if it mutates a receiver, assert on the receiver afterwards."
            }
            Rule::DiscriminantOnly => {
                "Unwrap and assert on the payload: `assert_eq!(parse(s).unwrap().port, 8080)` \
                 rather than `assert!(parse(s).is_ok())`."
            }
            Rule::UnwrapOnly => {
                "Keep the unwrap for convenience, then assert something about the value it \
                 produced."
            }
            Rule::MatchesWildcard => {
                "Bind the fields you care about and check them: \
                 `assert!(matches!(e, Error::Parse(m) if m.contains(\"port\")))`, or compare \
                 the whole value with `assert_eq!`."
            }
            Rule::ShouldPanicUnqualified => {
                "Add `expected = \"...\"` naming the panic message, so an unrelated panic \
                 on the way in no longer passes the test."
            }
            Rule::DiscardedResult => {
                "Bind the result and assert on it, or delete the call if it is only there \
                 for coverage."
            }
            Rule::TautologicalAssert => {
                "Replace one side with an independently written expected value. If there is \
                 nothing to compare against, the test has no subject."
            }
            Rule::IgnoredTest => {
                "Fix and re-enable it, or delete it. An ignored test is documentation that \
                 looks like coverage."
            }
            Rule::ComputedExpectation => {
                "Write the expected value out as a literal or constructor instead of \
                 computing it with the function under test."
            }
            Rule::OracleShapeMismatch => {
                "Assert on the receiver after the call: `obj.mutate(); assert_eq!(obj.field, \
                 expected);` -- or compare the whole value if it derives PartialEq."
            }
        }
    }

    /// Stable identifier, e.g. `ORC002`.
    ///
    /// ```
    /// use oracle_core::lint::{Rule, Severity};
    ///
    /// assert_eq!(Rule::DiscriminantOnly.id(), "ORC002");
    /// assert_eq!(Rule::DiscriminantOnly.name(), "discriminant-only");
    /// assert_eq!(Rule::DiscriminantOnly.severity(), Severity::High);
    ///
    /// // Every rule explains itself, so a report can say why.
    /// assert!(Rule::OracleShapeMismatch.why().contains("post-state"));
    /// ```
    pub fn id(self) -> &'static str {
        match self {
            Rule::NoOracle => "ORC001",
            Rule::DiscriminantOnly => "ORC002",
            Rule::UnwrapOnly => "ORC003",
            Rule::MatchesWildcard => "ORC004",
            Rule::ShouldPanicUnqualified => "ORC005",
            Rule::DiscardedResult => "ORC006",
            Rule::TautologicalAssert => "ORC007",
            Rule::IgnoredTest => "ORC008",
            Rule::ComputedExpectation => "ORC009",
            Rule::OracleShapeMismatch => "ORC010",
        }
    }

    /// Short kebab-case name, e.g. `discriminant-only`.
    pub fn name(self) -> &'static str {
        match self {
            Rule::NoOracle => "no-oracle",
            Rule::DiscriminantOnly => "discriminant-only",
            Rule::UnwrapOnly => "unwrap-only",
            Rule::MatchesWildcard => "matches-wildcard",
            Rule::ShouldPanicUnqualified => "should-panic-unqualified",
            Rule::DiscardedResult => "discarded-result",
            Rule::TautologicalAssert => "tautological-assert",
            Rule::IgnoredTest => "ignored-test",
            Rule::ComputedExpectation => "computed-expectation",
            Rule::OracleShapeMismatch => "oracle-shape-mismatch",
        }
    }

    /// How much this finding should weigh in a `--deny` gate.
    pub fn severity(self) -> Severity {
        match self {
            Rule::NoOracle | Rule::TautologicalAssert | Rule::OracleShapeMismatch => Severity::High,
            Rule::DiscriminantOnly | Rule::ComputedExpectation => Severity::High,
            Rule::UnwrapOnly | Rule::MatchesWildcard | Rule::ShouldPanicUnqualified => {
                Severity::Medium
            }
            Rule::DiscardedResult | Rule::IgnoredTest => Severity::Low,
        }
    }

    /// Why this pattern means the oracle cannot do its job. Printed in reports
    /// because a finding nobody understands is a finding nobody acts on.
    pub fn why(self) -> &'static str {
        match self {
            Rule::NoOracle => {
                "The body contains no assertion, no `?`, and no unwrap. It can only \
                 fail if the code under test panics, so it is a smoke test wearing a \
                 test's name."
            }
            Rule::DiscriminantOnly => {
                "`is_ok()`/`is_some()` checks the discriminant and discards the payload. \
                 A function returning `Ok(garbage)` passes. This is the single most \
                 common weak oracle in Rust test code."
            }
            Rule::UnwrapOnly => {
                "The only way this test fails is a panic from `unwrap`/`expect`/`?`. \
                 That proves the code did not blow up, not that it was right."
            }
            Rule::MatchesWildcard => {
                "`matches!(x, Variant { .. })` checks the variant and wildcards every \
                 field, so any field can be wrong without the test noticing."
            }
            Rule::ShouldPanicUnqualified => {
                "`#[should_panic]` with no `expected = \"...\"` passes on *any* panic, \
                 including an unrelated one raised on the way to the code under test."
            }
            Rule::DiscardedResult => {
                "`let _ = f();` executes `f` and discards everything it produced. It \
                 raises line coverage and observes nothing."
            }
            Rule::TautologicalAssert => {
                "Both sides of the assertion are the same expression, or the condition \
                 is a literal. It cannot fail under any implementation."
            }
            Rule::IgnoredTest => {
                "`#[ignore]` means this never runs in a default `cargo test`, so whatever \
                 it verifies is not verified."
            }
            Rule::ComputedExpectation => {
                "The expected value is computed by calling the same function as the actual \
                 value. The assertion compares the implementation to itself and holds for \
                 any behaviour, correct or not."
            }
            Rule::OracleShapeMismatch => {
                "The claimed symbol mutates its receiver, but no assertion observes that \
                 receiver after the call. The required oracle is post-state; the test only \
                 has return-value oracles."
            }
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
/// One rule firing at one place in one test.
pub struct Finding {
    /// Which rule fired.
    pub rule: Rule,
    /// The test the finding belongs to.
    pub test: TestId,
    /// 1-based line the finding points at.
    pub line: u32,
    /// The symbol this finding is about, when the rule is symbol-specific.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
    /// The offending source fragment, normalized to one line.
    pub snippet: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
/// One place in a test body where an oracle could fail.
pub struct OracleSite {
    /// 1-based line of the oracle.
    pub line: u32,
    /// What the oracle is, e.g. `assert_eq!` or `.unwrap()`.
    pub kind: String,
    /// How much this particular oracle can discriminate.
    pub strength: OracleStrength,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
/// The oracle verdict for one test.
pub struct TestOracles {
    /// The test analyzed.
    pub test: TestId,
    /// The strongest oracle present. A test is judged by its best, not its average.
    pub strength: OracleStrength,
    /// Every oracle found, in source order.
    pub sites: Vec<OracleSite>,
    /// Every rule that fired.
    pub findings: Vec<Finding>,
    /// Method calls with a plain identifier receiver: `(receiver, method, line)`.
    /// Evidence for the post-state check; not part of the serialized report.
    #[serde(skip)]
    pub receiver_calls: Vec<ReceiverCall>,
    /// Every identifier appearing inside an assertion macro.
    #[serde(skip)]
    pub asserted_idents: BTreeSet<String>,
}

#[derive(Clone, Debug)]
/// A method call whose receiver is a plain identifier.
///
/// Evidence for ORC010: if a `&mut self` call's receiver never appears in an
/// assertion, nothing observed the state the call existed to change.
pub struct ReceiverCall {
    /// The receiver's identifier.
    pub receiver: String,
    /// The method called.
    pub method: String,
    /// 1-based line of the call.
    pub line: u32,
}

/// Analyze every test in the inventory.
pub fn analyze(inv: &Inventory) -> Vec<TestOracles> {
    inv.tests.iter().map(analyze_test).collect()
}

/// Analyze one test's oracles.
///
/// A test whose body is `None` is reported as *unanalyzed* rather than as
/// having no oracle, so an unparseable doctest does not become a finding.
pub fn analyze_test(test: &TestItem) -> TestOracles {
    let mut scan = Scan {
        test: test.id.clone(),
        sites: Vec::new(),
        findings: Vec::new(),
        receiver_calls: Vec::new(),
        asserted_idents: BTreeSet::new(),
        panic_only_sites: 0,
    };

    if let Some(body) = &test.body {
        scan.visit_block(body);
    }

    // `#[should_panic]` is itself an oracle, of two very different qualities.
    match test.should_panic {
        Some(ShouldPanic::Qualified) => scan.sites.push(OracleSite {
            line: test.span.start,
            kind: "should_panic(expected)".into(),
            strength: OracleStrength::Partial,
        }),
        Some(ShouldPanic::Unqualified) => {
            scan.sites.push(OracleSite {
                line: test.span.start,
                kind: "should_panic".into(),
                strength: OracleStrength::Weak,
            });
            scan.flag(
                Rule::ShouldPanicUnqualified,
                test.span.start,
                "#[should_panic]",
            );
        }
        None => {}
    }

    let Scan {
        mut sites,
        mut findings,
        receiver_calls,
        asserted_idents,
        panic_only_sites,
        ..
    } = scan;

    if sites.is_empty() && test.body.is_some() {
        // A `None` body means we could not parse the source (an unusual doctest,
        // say), not that the test asserts nothing. Do not invent a finding.
        findings.push(Finding {
            rule: Rule::NoOracle,
            test: test.id.clone(),
            line: test.span.start,
            symbol: None,
            snippet: String::new(),
        });
    } else if sites.iter().all(|s| s.strength == OracleStrength::Weak) && panic_only_sites > 0 {
        // Every oracle present is a panic-on-failure, nothing inspects a value.
        findings.push(Finding {
            rule: Rule::UnwrapOnly,
            test: test.id.clone(),
            line: sites[0].line,
            symbol: None,
            snippet: String::new(),
        });
    }

    if test.is_ignored {
        findings.push(Finding {
            rule: Rule::IgnoredTest,
            test: test.id.clone(),
            line: test.span.start,
            symbol: None,
            snippet: String::new(),
        });
    }

    sites.sort_by_key(|s| s.line);
    let strength = sites
        .iter()
        .map(|s| s.strength)
        .max()
        .unwrap_or(OracleStrength::None);

    TestOracles {
        test: test.id.clone(),
        strength,
        sites,
        findings,
        receiver_calls,
        asserted_idents,
    }
}

struct Scan {
    test: TestId,
    sites: Vec<OracleSite>,
    findings: Vec<Finding>,
    receiver_calls: Vec<ReceiverCall>,
    asserted_idents: BTreeSet<String>,
    /// Oracles whose only failure mode is a panic (`unwrap`, `expect`, `?`).
    panic_only_sites: usize,
}

impl Scan {
    fn flag(&mut self, rule: Rule, line: u32, snippet: impl Into<String>) {
        self.findings.push(Finding {
            rule,
            test: self.test.clone(),
            line,
            symbol: None,
            snippet: normalize(snippet.into()),
        });
    }

    fn macro_call(&mut self, mac: &syn::Macro) {
        let name = last_segment(&mac.path);
        let Some(first) = mac.path.segments.first() else {
            return;
        };
        let line = first.ident.span().start().line as u32;
        if is_assertion(&name) {
            // Any identifier named inside an assertion counts as observed. This
            // is what makes the post-state check possible: if the receiver of a
            // `&mut self` call never appears in an assertion, nothing looked at
            // the state that call existed to change.
            self.asserted_idents
                .extend(identifiers(&mac.tokens.to_string()));
            self.assert_macro(mac, &name, line);
        }
        // A bare `panic!()` reached unconditionally is not an oracle; one in a
        // branch is. We cannot tell statically, so it contributes nothing.
    }

    fn assert_macro(&mut self, mac: &syn::Macro, name: &str, line: u32) {
        let args = parse_args(mac);

        let strength = match name {
            "assert" | "debug_assert" => self.classify_condition(args.first(), line, name),
            "assert_eq" | "assert_ne" | "debug_assert_eq" | "debug_assert_ne" => {
                self.classify_equality(&args, line, mac)
            }
            "assert_matches" | "debug_assert_matches" => {
                let body = mac.tokens.to_string();
                if pattern_is_wildcard(&body) {
                    self.flag(Rule::MatchesWildcard, line, format!("{name}!({body})"));
                    OracleStrength::Weak
                } else {
                    OracleStrength::Partial
                }
            }
            // insta and friends: the value is fully observed, but the expectation
            // was recorded from an implementation rather than written by hand.
            n if n.starts_with("assert_") && n.ends_with("_snapshot") => OracleStrength::Partial,
            "assert_snapshot" => OracleStrength::Partial,
            _ => OracleStrength::Partial,
        };

        self.sites.push(OracleSite {
            line,
            kind: format!("{name}!"),
            strength,
        });
    }

    /// `assert!(cond)` — everything depends on what `cond` is.
    fn classify_condition(
        &mut self,
        cond: Option<&syn::Expr>,
        line: u32,
        macro_name: &str,
    ) -> OracleStrength {
        let Some(cond) = cond else {
            return OracleStrength::Weak;
        };
        // Quote the whole assertion: a snippet reading just `true` does not
        // look like a problem until you know it was an `assert!`.
        let text = format!("{macro_name}!({})", render(cond));

        match cond {
            // `assert!(true)` and friends.
            syn::Expr::Lit(_) => {
                self.flag(Rule::TautologicalAssert, line, text);
                OracleStrength::None
            }

            syn::Expr::MethodCall(m) => {
                let method = m.method.to_string();
                if matches!(method.as_str(), "is_ok" | "is_err" | "is_some" | "is_none") {
                    self.flag(Rule::DiscriminantOnly, line, text);
                    OracleStrength::Weak
                } else {
                    // Every other predicate -- `contains`, `starts_with`, a
                    // domain method -- observes something real about a value,
                    // but only one property of it.
                    OracleStrength::Partial
                }
            }

            syn::Expr::Macro(inner) => {
                let inner_name = last_segment(&inner.mac.path);
                if inner_name == "matches" {
                    let body = inner.mac.tokens.to_string();
                    if pattern_is_wildcard(&body) {
                        self.flag(Rule::MatchesWildcard, line, text);
                        return OracleStrength::Weak;
                    }
                    return OracleStrength::Partial;
                }
                OracleStrength::Partial
            }

            // A real comparison against something.
            syn::Expr::Binary(b) => {
                let left = render(&b.left);
                let right = render(&b.right);
                if left == right {
                    self.flag(Rule::TautologicalAssert, line, text);
                    return OracleStrength::None;
                }
                OracleStrength::Strong
            }

            syn::Expr::Unary(u) => self.classify_condition(Some(&u.expr), line, macro_name),

            _ => OracleStrength::Partial,
        }
    }

    /// `assert_eq!(actual, expected)` — strong unless the two sides are the
    /// same expression, or the expectation is computed by the code under test.
    fn classify_equality(
        &mut self,
        args: &[syn::Expr],
        line: u32,
        mac: &syn::Macro,
    ) -> OracleStrength {
        let (Some(actual), Some(expected)) = (args.first(), args.get(1)) else {
            // Did not parse as two expressions; fall back to the raw tokens.
            return OracleStrength::Partial;
        };

        let (a_text, e_text) = (render(actual), render(expected));
        if a_text == e_text {
            self.flag(
                Rule::TautologicalAssert,
                line,
                format!("assert_eq!({a_text}, {e_text})"),
            );
            return OracleStrength::None;
        }

        // The expectation should be written down, not computed by the same
        // function that produced the actual value.
        let shared = shared_calls(actual, expected);
        if let Some(call) = shared {
            self.flag(
                Rule::ComputedExpectation,
                line,
                format!("both sides call `{call}`: {}", mac.tokens.to_token_stream()),
            );
            return OracleStrength::Weak;
        }

        OracleStrength::Strong
    }
}

impl<'ast> Visit<'ast> for Scan {
    /// An assertion in expression position, e.g. the tail of a block.
    fn visit_expr_macro(&mut self, node: &'ast syn::ExprMacro) {
        self.macro_call(&node.mac);
        syn::visit::visit_expr_macro(self, node);
    }

    /// An assertion in *statement* position -- `assert_eq!(a, b);` -- which is
    /// where essentially every assertion actually appears. `syn` models this as
    /// `Stmt::Macro`, a separate node from `Expr::Macro`, so both need a hook.
    fn visit_stmt_macro(&mut self, node: &'ast syn::StmtMacro) {
        self.macro_call(&node.mac);
        syn::visit::visit_stmt_macro(self, node);
    }

    fn visit_expr_method_call(&mut self, node: &'ast syn::ExprMethodCall) {
        let method = node.method.to_string();
        if let syn::Expr::Path(p) = &*node.receiver {
            if let Some(ident) = p.path.get_ident() {
                self.receiver_calls.push(ReceiverCall {
                    receiver: ident.to_string(),
                    method: method.clone(),
                    line: node.method.span().start().line as u32,
                });
            }
        }
        if matches!(method.as_str(), "unwrap" | "expect" | "unwrap_err") {
            self.panic_only_sites += 1;
            self.sites.push(OracleSite {
                line: node.method.span().start().line as u32,
                kind: format!(".{method}()"),
                strength: OracleStrength::Weak,
            });
        }
        syn::visit::visit_expr_method_call(self, node);
    }

    fn visit_expr_try(&mut self, node: &'ast syn::ExprTry) {
        self.panic_only_sites += 1;
        self.sites.push(OracleSite {
            line: node.question_token.span.start().line as u32,
            kind: "?".into(),
            strength: OracleStrength::Weak,
        });
        syn::visit::visit_expr_try(self, node);
    }

    fn visit_local(&mut self, node: &'ast syn::Local) {
        // `let _ = f();` runs `f` and observes nothing.
        if matches!(&node.pat, syn::Pat::Wild(_)) {
            if let Some(init) = &node.init {
                let line = node.let_token.span.start().line as u32;
                let text = render(&init.expr);
                if matches!(
                    &*init.expr,
                    syn::Expr::Call(_) | syn::Expr::MethodCall(_) | syn::Expr::Macro(_)
                ) {
                    self.flag(Rule::DiscardedResult, line, format!("let _ = {text};"));
                }
            }
        }
        syn::visit::visit_local(self, node);
    }

    /// Nested items (helper fns declared inside a test) are not part of this
    /// test's oracle surface.
    fn visit_item(&mut self, _node: &'ast syn::Item) {}
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn is_assertion(name: &str) -> bool {
    name.starts_with("assert")
        || name.starts_with("debug_assert")
        || name.starts_with("prop_assert")
        || name.starts_with("expect_that")
        || name.starts_with("verify_that")
}

fn last_segment(path: &syn::Path) -> String {
    path.segments
        .last()
        .map(|s| s.ident.to_string())
        .unwrap_or_default()
}

/// Assertion macros take comma-separated expressions, except when they take a
/// pattern (`assert_matches!`). Callers handle the empty result.
fn parse_args(mac: &syn::Macro) -> Vec<syn::Expr> {
    use syn::punctuated::Punctuated;
    mac.parse_body_with(Punctuated::<syn::Expr, syn::Token![,]>::parse_terminated)
        .map(|p| p.into_iter().collect())
        .unwrap_or_default()
}

fn render(expr: &syn::Expr) -> String {
    normalize(expr.to_token_stream().to_string())
}

fn normalize(s: String) -> String {
    let collapsed = s.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed
        .replace(" . ", ".")
        .replace(" :: ", "::")
        .replace(" (", "(")
        .replace("( ", "(")
        .replace(" )", ")")
        .replace(" ,", ",")
        .replace(" !", "!")
}

/// Does the pattern side of a `matches!` wildcard away the payload?
fn pattern_is_wildcard(body: &str) -> bool {
    let Some((_, pattern)) = body.split_once(',') else {
        return false;
    };
    let pattern = pattern.trim();

    // A bare variant path (`Foo::Bar`) checks the discriminant and nothing else.
    if !pattern.contains('(') && !pattern.contains('{') {
        return true;
    }
    // `Foo::Bar { .. }` and `Foo::Bar(..)` discard every field.
    if pattern.contains("..") {
        return true;
    }
    // So does a standalone `_` binding -- but an identifier that merely
    // contains an underscore (`Foo::Bar(my_value)`) binds a real field.
    pattern
        .split(|c: char| c.is_whitespace() || "(),{}[]".contains(c))
        .any(|tok| tok == "_")
}

/// Function or method names called on *both* sides of an equality assertion.
///
/// Constructors are excluded: `assert_eq!(got, Config::default())` compares an
/// implementation against a known starting point, which is a legitimate
/// expectation. `assert_eq!(parse(a), parse(b))` is not.
fn shared_calls(actual: &syn::Expr, expected: &syn::Expr) -> Option<String> {
    const CONSTRUCTORS: &[&str] = &[
        "new",
        "default",
        "from",
        "into",
        "with_capacity",
        "to_string",
        "to_owned",
        "as_str",
        "clone",
        "vec",
        "some",
        "ok",
    ];

    let a = called_names(actual);
    let b = called_names(expected);
    a.into_iter()
        .find(|n| b.contains(n) && !CONSTRUCTORS.contains(&n.to_ascii_lowercase().as_str()))
}

fn called_names(expr: &syn::Expr) -> Vec<String> {
    struct Collect(Vec<String>);
    impl<'ast> Visit<'ast> for Collect {
        fn visit_expr_call(&mut self, node: &'ast syn::ExprCall) {
            if let syn::Expr::Path(p) = &*node.func {
                self.0.push(last_segment(&p.path));
            }
            syn::visit::visit_expr_call(self, node);
        }
        fn visit_expr_method_call(&mut self, node: &'ast syn::ExprMethodCall) {
            self.0.push(node.method.to_string());
            syn::visit::visit_expr_method_call(self, node);
        }
    }
    let mut c = Collect(Vec::new());
    c.visit_expr(expr);
    c.0
}

// ---------------------------------------------------------------------------
// ORC010: does the oracle have the right *shape* for what it claims?
// ---------------------------------------------------------------------------

/// Flag tests whose oracles cannot observe what the claimed symbol changes.
///
/// This rule exists because Rust puts effects in the type. A `fn(&mut self)`
/// communicates its result by mutating the receiver, so a test that only
/// inspects return values is structurally blind to it — however many assertions
/// it contains and whatever its coverage says. Checking that is a matter of
/// reading the signature, which no other language makes this easy.
///
/// The check is deliberately narrow: it fires only on *direct* claims (a
/// same-file test module, or a doctest), only when the symbol takes `&mut self`,
/// and only when the test demonstrably calls it on a named binding that no
/// assertion ever mentions again.
pub fn shape_mismatches(
    inv: &Inventory,
    claims: &crate::claims::ClaimMap,
    oracles: &[TestOracles],
) -> Vec<Finding> {
    let mut findings = Vec::new();

    for result in oracles {
        let direct: Vec<&crate::symbol::SymbolId> = claims
            .claims
            .iter()
            .filter(|c| c.test == result.test && c.kind.is_direct())
            .map(|c| &c.symbol)
            .collect();

        for symbol_id in direct {
            let Some(symbol) = inv.symbol(symbol_id) else {
                continue;
            };
            if !matches!(
                symbol.required_oracle,
                crate::symbol::RequiredOracle::PostState | crate::symbol::RequiredOracle::Both
            ) {
                continue;
            }
            if symbol.self_kind != crate::symbol::SelfKind::RefMut {
                continue; // `&mut` arguments need dataflow we do not have yet.
            }

            // Did this test actually call it, on something we can name?
            let Some(call) = result
                .receiver_calls
                .iter()
                .find(|c| c.method == symbol.name)
            else {
                continue;
            };

            if !result.asserted_idents.contains(&call.receiver) {
                findings.push(Finding {
                    rule: Rule::OracleShapeMismatch,
                    test: result.test.clone(),
                    line: call.line,
                    symbol: Some(symbol.path.clone()),
                    snippet: format!(
                        "`{}.{}(..)` mutates `{}`, which no assertion observes",
                        call.receiver, call.method, call.receiver
                    ),
                });
            }
        }
    }

    findings
}

/// Pull identifier-shaped words out of a token string.
fn identifiers(tokens: &str) -> Vec<String> {
    tokens
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|w| !w.is_empty() && !w.chars().next().unwrap().is_numeric())
        .map(|w| w.to_string())
        .collect()
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::inventory::{TestId, TestKind};
    use crate::symbol::LineSpan;

    /// Build a `TestItem` from a source fixture so each rule is exercised
    /// against real syntax rather than a hand-built AST.
    fn analyze_src(src: &str) -> TestOracles {
        let f: syn::ItemFn = syn::parse_str(src).expect("fixture must parse");
        let should_panic = f
            .attrs
            .iter()
            .find(|a| a.path().is_ident("should_panic"))
            .map(|a| match &a.meta {
                syn::Meta::List(l) if l.tokens.to_string().contains("expected") => {
                    ShouldPanic::Qualified
                }
                _ => ShouldPanic::Unqualified,
            });

        let item = TestItem {
            id: TestId {
                file: "fixture.rs".into(),
                line: 1,
                path: format!("fixture::{}", f.sig.ident),
            },
            kind: TestKind::Unit,
            span: LineSpan { start: 1, end: 99 },
            is_ignored: f.attrs.iter().any(|a| a.path().is_ident("ignore")),
            is_async: f.sig.asyncness.is_some(),
            should_panic,
            enclosing_test_module: Some("fixture::tests".into()),
            doctest_target: None,
            body: Some(*f.block),
        };
        analyze_test(&item)
    }

    fn rules(o: &TestOracles) -> Vec<Rule> {
        let mut r: Vec<Rule> = o.findings.iter().map(|f| f.rule).collect();
        r.sort();
        r.dedup();
        r
    }

    #[test]
    fn body_with_no_assertion_has_no_oracle() {
        let o = analyze_src("fn t() { let c = Config::new(); c.validate(); }");
        assert_eq!(rules(&o), vec![Rule::NoOracle]);
        assert_eq!(o.strength, OracleStrength::None);
        assert!(o.sites.is_empty());
    }

    #[test]
    fn is_ok_check_is_discriminant_only() {
        let o = analyze_src(r#"fn t() { assert!(parse("x").is_ok()); }"#);
        assert_eq!(rules(&o), vec![Rule::DiscriminantOnly]);
        assert_eq!(o.strength, OracleStrength::Weak);
        assert_eq!(o.sites.len(), 1);
        assert_eq!(o.sites[0].kind, "assert!");
    }

    #[test]
    fn a_flagged_snippet_quotes_the_whole_assertion_not_just_its_condition() {
        // A snippet reading `true` does not look like a problem in a report
        // until you know it was the argument to an `assert!`.
        let o = analyze_src("fn t() { assert!(true); }");
        assert_eq!(o.findings[0].snippet, "assert!(true)");

        let o = analyze_src(r#"fn t() { assert!(parse("h:1").is_ok()); }"#);
        assert_eq!(o.findings[0].snippet, r#"assert!(parse("h:1").is_ok())"#);
    }

    #[test]
    fn identical_operands_are_tautological() {
        let o = analyze_src("fn t() { assert_eq!(cfg.len(), cfg.len()); }");
        assert_eq!(rules(&o), vec![Rule::TautologicalAssert]);
        assert_eq!(o.strength, OracleStrength::None);
    }

    #[test]
    fn assert_on_literal_is_tautological() {
        let o = analyze_src("fn t() { assert!(true); }");
        assert_eq!(rules(&o), vec![Rule::TautologicalAssert]);
        assert_eq!(o.strength, OracleStrength::None);
    }

    #[test]
    fn expectation_computed_by_the_same_function_is_flagged() {
        let o = analyze_src("fn t() { assert_eq!(normalize(a), normalize(b)); }");
        assert_eq!(rules(&o), vec![Rule::ComputedExpectation]);
        assert_eq!(o.strength, OracleStrength::Weak);
        assert!(o.findings[0].snippet.contains("normalize"));
    }

    #[test]
    fn constructor_on_the_expected_side_is_a_legitimate_expectation() {
        let o = analyze_src("fn t() { assert_eq!(got, Config::default()); }");
        assert!(rules(&o).is_empty(), "unexpected findings: {:?}", rules(&o));
        assert_eq!(o.strength, OracleStrength::Strong);
    }

    #[test]
    fn matches_with_rest_pattern_wildcards_the_payload() {
        let o = analyze_src("fn t() { assert!(matches!(e, Error::Parse { .. })); }");
        assert_eq!(rules(&o), vec![Rule::MatchesWildcard]);
        assert_eq!(o.strength, OracleStrength::Weak);
    }

    #[test]
    fn bare_variant_pattern_checks_only_the_discriminant() {
        let o = analyze_src("fn t() { assert!(matches!(e, Error::Eof)); }");
        assert_eq!(rules(&o), vec![Rule::MatchesWildcard]);
    }

    #[test]
    fn matches_binding_a_field_is_not_a_wildcard() {
        let o = analyze_src("fn t() { assert!(matches!(e, Error::Parse(msg) if msg.len() > 2)); }");
        assert!(rules(&o).is_empty(), "unexpected findings: {:?}", rules(&o));
        assert_eq!(o.strength, OracleStrength::Partial);
    }

    #[test]
    fn underscore_inside_an_identifier_is_not_a_wildcard_binding() {
        let o = analyze_src("fn t() { assert!(matches!(e, Error::Parse(my_value))); }");
        assert!(
            !rules(&o).contains(&Rule::MatchesWildcard),
            "`my_value` binds a real field; it is not a `_` pattern"
        );
    }

    #[test]
    fn discarded_call_result_is_flagged_without_suppressing_real_oracles() {
        let o = analyze_src(r#"fn t() { let _ = compute(); assert_eq!(total, 7); }"#);
        assert_eq!(rules(&o), vec![Rule::DiscardedResult]);
        assert_eq!(o.strength, OracleStrength::Strong);
        assert_eq!(o.findings[0].snippet, "let _ = compute();");
    }

    #[test]
    fn value_equality_against_a_literal_is_a_strong_oracle() {
        let o = analyze_src(r#"fn t() { assert_eq!(parse("a=1").unwrap().key, "a"); }"#);
        assert!(rules(&o).is_empty(), "unexpected findings: {:?}", rules(&o));
        assert_eq!(o.strength, OracleStrength::Strong);
    }

    #[test]
    fn unwrap_as_the_only_failure_mode_is_flagged() {
        let o = analyze_src(r#"fn t() { let v = parse("x").unwrap(); }"#);
        assert_eq!(rules(&o), vec![Rule::UnwrapOnly]);
        assert_eq!(o.strength, OracleStrength::Weak);
        assert_eq!(o.sites[0].kind, ".unwrap()");
    }

    #[test]
    fn question_mark_counts_as_a_panic_only_oracle() {
        let o = analyze_src(r#"fn t() -> Result<()> { let v = parse("x")?; Ok(()) }"#);
        assert_eq!(rules(&o), vec![Rule::UnwrapOnly]);
        assert_eq!(o.sites[0].kind, "?");
    }

    #[test]
    fn unqualified_should_panic_accepts_any_panic() {
        let o = analyze_src(r#"#[should_panic] fn t() { parse("bad"); }"#);
        assert_eq!(rules(&o), vec![Rule::ShouldPanicUnqualified]);
        assert_eq!(o.strength, OracleStrength::Weak);
    }

    #[test]
    fn qualified_should_panic_is_a_real_if_coarse_oracle() {
        let o = analyze_src(r#"#[should_panic(expected = "empty host")] fn t() { parse(""); }"#);
        assert!(rules(&o).is_empty(), "unexpected findings: {:?}", rules(&o));
        assert_eq!(o.strength, OracleStrength::Partial);
    }

    #[test]
    fn ignored_test_is_reported_alongside_its_oracle_verdict() {
        let o = analyze_src("#[ignore] fn t() { assert_eq!(a, 1); }");
        assert_eq!(rules(&o), vec![Rule::IgnoredTest]);
        assert_eq!(o.strength, OracleStrength::Strong);
    }

    #[test]
    fn assertions_inside_a_nested_helper_are_not_this_test_s_oracles() {
        let o = analyze_src("fn t() { fn helper() { assert!(true); } helper(); }");
        assert_eq!(
            rules(&o),
            vec![Rule::NoOracle],
            "the helper's tautology belongs to the helper, and the test itself asserts nothing"
        );
    }

    #[test]
    fn an_assertion_in_expression_position_is_still_an_oracle() {
        // No trailing semicolon, so this is the block's tail expression --
        // syn's `Expr::Macro`, a different node from the `Stmt::Macro` that
        // every semicolon-terminated assertion produces. cargo-oracle itself
        // reported this path as claimed-but-never-run.
        let o = analyze_src(r#"fn t() { assert_eq!(cfg.port, 8080) }"#);
        assert!(rules(&o).is_empty(), "unexpected findings: {:?}", rules(&o));
        assert_eq!(o.strength, OracleStrength::Strong);
        assert_eq!(o.sites.len(), 1);
        assert_eq!(o.sites[0].kind, "assert_eq!");
    }

    #[test]
    fn a_tautology_in_expression_position_is_still_caught() {
        let o = analyze_src(r#"fn t() { assert!(true) }"#);
        assert_eq!(rules(&o), vec![Rule::TautologicalAssert]);
    }

    #[test]
    fn every_rule_has_a_distinct_id_and_an_explanation() {
        let all = Rule::ALL;
        let mut ids: Vec<&str> = all.iter().map(|r| r.id()).collect();
        let before = ids.len();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), before, "rule IDs must be unique");
        assert!(all.iter().all(|r| r.why().len() > 40));
        assert!(all.iter().all(|r| !r.name().is_empty()));
        // Every rule must say how to fix it. A finding without a fix is a
        // complaint, and `cargo oracle explain` would have nothing to print.
        assert!(
            all.iter().all(|r| r.fix().len() > 40),
            "every rule needs a fix"
        );
        // Every rule is reachable by both of the names a reader might type.
        for rule in all {
            assert_eq!(Rule::find(rule.id()), Some(rule));
            assert_eq!(Rule::find(rule.name()), Some(rule));
        }
    }
}
