# Oracle lints

Ten static rules, all pure `syn` passes — no build, no execution, milliseconds
on a whole workspace. They exist to be run *before* spending a single mutant:
they catch the patterns that mutation testing would otherwise bill you an hour
to discover.

Every rule asks the same question. Not *is there an assertion* — agent-written
tests have plenty — but *can this assertion fail?*

## Output conventions

Findings are rendered as rustc-style diagnostics, following the [rustc
diagnostic style guide][style] and cargo's console conventions, because a Rust
developer should not have to learn a new report format:

```
warning: this assertion checks the discriminant and discards the value
  --> src/config.rs:60:9
   |
60 |         assert!(parse("h:1").is_ok());
   |         ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
   |
   = help: unwrap and assert on the payload:
           `assert_eq!(parse(s).unwrap().port, 8080)` rather than
           `assert!(parse(s).is_ok())`.
   = note: oracle lint `discriminant_only` (ORC002)
```

- **Messages are lowercase and unpunctuated**, with code in backticks.
- **`help` is what to change; `note` is context.** rustc draws that line
  strictly and readers rely on it.
- **Lint names are `snake_case`**, reading grammatically as "allow *lint-name*".
  The `ORCnnn` code is for `--deny` and for `cargo oracle explain`; the name
  carries the meaning.
- **Diagnostics go to stderr**, as clippy's do, so `--message-format json` on
  stdout stays pipeable.
- `--color auto|always|never`, honouring `NO_COLOR` under `auto`.
- `--message-format human|short|json`; `short` is the one-line
  `file:line:col: warning: message [CODE]` form editors parse.

[style]: https://rustc-dev-guide.rust-lang.org/diagnostics.html

| ID | Name | Severity |
|---|---|---|
| ORC001 | `no_oracle` | high |
| ORC002 | `discriminant_only` | high |
| ORC003 | `unwrap_only` | medium |
| ORC004 | `matches_wildcard` | medium |
| ORC005 | `should_panic_unqualified` | medium |
| ORC006 | `discarded_result` | low |
| ORC007 | `tautological_assert` | high |
| ORC008 | `ignored_test` | low |
| ORC009 | `computed_expectation` | high |
| ORC010 | `oracle_shape_mismatch` | high |

---

## ORC001 `no_oracle`

No assertion, no `?`, no unwrap. The test can only fail if the code under test
panics — a smoke test wearing a test's name.

```rust
#[test]
fn test_validate() {
    let c = Config::new("h", 1);
    c.validate();          // result dropped on the floor
}
```

## ORC002 `discriminant_only`

`is_ok()` / `is_err()` / `is_some()` / `is_none()` check the discriminant and
discard the payload. A function returning `Ok(garbage)` passes.

```rust
assert!(parse("h:1").is_ok());          // flagged
assert_eq!(parse("h:1").unwrap().port, 1);  // observes the value
```

This is the single most common weak oracle in Rust test code, and the one worth
fixing first.

## ORC003 `unwrap_only`

Every oracle in the body is a panic-on-failure: `unwrap`, `expect`, or `?`. That
proves the code did not blow up, not that it was right.

```rust
#[test]
fn test_parse() {
    let v = parse("h:1").unwrap();   // nothing is asserted about `v`
}
```

## ORC004 `matches_wildcard`

`matches!(x, Variant { .. })` checks the variant and wildcards every field, so
any field can be wrong without the test noticing. A bare variant path
(`matches!(e, Error::Eof)`) is the same check with different syntax.

Binding a field is not flagged — `matches!(e, Error::Parse(msg) if msg.len() > 2)`
observes something real. An identifier that merely *contains* an underscore
(`Error::Parse(my_value)`) is a real binding, not a `_` pattern.

## ORC005 `should_panic_unqualified`

`#[should_panic]` with no `expected = "..."` passes on *any* panic — including
one raised by an unrelated bug on the way to the code under test. It will keep
passing after the code it was written for is deleted.

```rust
#[should_panic]                           // flagged
#[should_panic(expected = "empty host")]  // a real, if coarse, oracle
```

## ORC006 `discarded_result`

`let _ = f();` executes `f` and observes nothing. It raises line coverage
specifically without raising confidence, which is the exact trade this tool
exists to make visible.

## ORC007 `tautological_assert`

Both sides of the assertion are the same expression, or the condition is a
literal. It cannot fail under any implementation.

```rust
assert!(true);
assert_eq!(cfg.len(), cfg.len());
```

## ORC008 `ignored_test`

`#[ignore]` means this never runs in a default `cargo test`, so whatever it
verifies is not verified. Reported alongside the test's oracle verdict, so an
ignored test with a strong oracle reads differently from an ignored stub.

## ORC009 `computed_expectation`

The expected value is produced by calling the same function as the actual value.
The assertion compares the implementation to itself and holds for any behaviour,
correct or not.

```rust
assert_eq!(normalize(a), normalize(b));   // flagged
assert_eq!(got, Config::default());       // fine: a written-down expectation
```

Constructors (`new`, `default`, `from`, `clone`, ...) are excluded, since
comparing against a known starting point is legitimate.

## ORC010 `oracle_shape_mismatch`

The rule that only works in Rust.

Rust puts effects in the type. A `fn(&mut self)` announces that its result lives
in the receiver, so a test that calls it and never mentions that binding in an
assertion is *structurally* blind to it — however many assertions it contains,
and whatever its coverage says.

```rust
#[test]
fn test_set_retries() {
    let mut c = Config::new("h", 1);
    c.set_retries(3);
    assert!(true);        // `c` is never observed again
}
```

```
high:62 ORC010 oracle-shape-mismatch    test_set_retries
        on weak_suite::config::Config::set_retries
        `c.set_retries(..)` mutates `c`, which no assertion observes
```

The check is deliberately narrow, because a false positive here is expensive:
it fires only on *direct* claims (a same-file test module or a doctest), only
when the symbol takes `&mut self`, and only when the test demonstrably calls it
on a named binding that no assertion mentions again. Mutated `&mut` *arguments*
need dataflow we do not yet have, and are not flagged.

---

## What is deliberately not a rule

**Assertion count.** Agent tests carry roughly twice the assertions of human
ones while catching fewer faults. A density threshold would rank the worst
suites highest.

**Snapshot assertions.** `insta` snapshots observe a value completely, so they
score as `partial` rather than weak. Their real weakness is provenance — a
snapshot accepted in the same commit that introduced the code records current
behaviour, bugs included — and that is a git question, not a syntax one. It
belongs in a later slice that can read commit history.

**`panic!` in a branch.** A `panic!` in an `else` arm is a real oracle; one
reached unconditionally is not. Telling them apart statically needs reachability
analysis, so `panic!` currently contributes nothing either way rather than
guessing.
