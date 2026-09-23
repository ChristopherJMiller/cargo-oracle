# cargo-oracle

**Symbol-level test attribution for Rust.** Not "was this line executed?" but
"**would any test fail if this symbol stopped working?**"

## The problem

Code coverage answers *did we run the line*. It cannot answer *did we check the
result*. That gap has always existed; agent-written tests widen it sharply.

Counting assertions does not help. Empirical work on AI-authored test commits
finds agent tests carry a **median of 2.0 assertions against 1.0 for human
ones** — while detecting *fewer* injected faults. Assertion density is not a
weak signal for agent tests, it is an inverted one. The presence of an oracle is
not the question. Whether it can fail is.

## What it looks like

Findings are rendered as rustc-style diagnostics, because a Rust developer
should not have to learn a new report format:

```
$ cargo oracle lint

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

warning: nothing observes the receiver this call mutates
  --> src/config.rs:72:9
   |
72 |         c.set_retries(3);
   |         ^^^^^^^^^^^^^^^^ weak_suite::config::Config::set_retries
   |
   = help: assert on the receiver after the call: `obj.mutate();
           assert_eq!(obj.field, expected);` -- or compare the whole value if
           it derives PartialEq.
   = note: oracle lint `oracle_shape_mismatch` (ORC010)

warning: `weak-suite` generated 4 warnings across 3 of 5 tests
  oracle strength: 2 strong, 0 partial, 1 weak, 2 none
  5 symbols scorable, all claimed by some test

For more information about a rule, try `cargo oracle explain ORC002`.
```

Messages are lowercase and unpunctuated, `help` carries the fix while `note`
carries context, lint names are `snake_case`, diagnostics go to stderr, and
`--color` and `--message-format human|short|json` behave as cargo's do.

`oracle_shape_mismatch` is the finding you cannot get anywhere else. Rust puts
effects in types, so `fn set_retries(&mut self, n: u8)` announces that its
result lives in the receiver — and a test that calls it and never mentions `c`
again is *structurally* blind to it, no matter how many assertions it has.

Any rule explains itself, as `rustc --explain` does:

```
$ cargo oracle explain ORC010

ORC010  oracle_shape_mismatch   severity: high

why it matters
  The claimed symbol mutates its receiver, but no assertion observes that
  receiver after the call. The required oracle is post-state; the test only
  has return-value oracles.

how to fix it
  assert on the receiver after the call: `obj.mutate();
  assert_eq!(obj.field, expected);` -- or compare the whole value if it
  derives PartialEq.
```

### And what v3 says, with evidence

```
$ cargo oracle verify --with-attribution

src/config.rs
  Config::new           not mutated       no mutant exists for this signature -- unscorable, not unverified
  Config::set_retries   PSEUDO-TESTED     1 mutant survived: ()
                        ^ predicted by ORC010 -- `c.set_retries(..)` mutates `c`, which no assertion observes
  Config::validate      PSEUDO-TESTED     1 mutant survived: Ok(())
  config::parse         verified          by parse_extracts_host_and_port
  config::redact        PSEUDO-TESTED     5 mutants survived: String::new(), "xyzzy".into(), ==
                        ^ no test executes it at all

per-test verdict
  TEST                                          EXECUTES  VERIFIES
  weak_suite::config::tests::test_parse                2         0  <- runs code, verifies none of it
  weak_suite::config::tests::test_validate             2         0  <- runs code, verifies none of it
  weak_suite::config::tests::test_set_retries          2         0  <- runs code, verifies none of it
  weak_suite::config::tests::parse_extracts_ho~        2         1
```

Two lines there need all four slices at once.

`executes 2, verifies 0` is the question this project started from, answered
directly — and no other tool reports mutation score per *test*.

`predicted by ORC010` is the argument for building it in layers. The static rule
costs milliseconds and no build; the mutation evidence costs a rebuild per
mutant. When the cheap check calls it correctly, you run that on every commit
and save the expensive one for the diff.

## Honesty about evidence

Each slice claims only what its evidence supports. v0 and v1 never report a
symbol as *verified*. v3 never reports an unscorable symbol as a passing one —
and there are **three** distinct ways to be unscorable, none of which mean
verified:

| State | Cause |
|---|---|
| `no viable mutant` | The mutant did not compile. Partly type enforcement, partly our operator being weak — not a guarantee |
| `not mutated` | cargo-mutants examined the file and produced nothing for this signature (`-> Self` on a constructor) |
| `out of scope` | The run never examined this file at all (`--file`, `--in-diff`) |

That last distinction is not pedantry. Scoping a run to one file leaves the rest
of the workspace unexamined, and an earlier version reported all of it as "no
mutant exists" — a gap laundered into a reassurance, which is precisely the
failure that makes coverage numbers untrustworthy.

## Install

Not on crates.io yet, so install from git:

```sh
cargo install --git https://github.com/ChristopherJMiller/cargo-oracle cargo-oracle
```

Or run it without installing, if you use nix:

```sh
nix run github:ChristopherJMiller/cargo-oracle -- lint
```

## Setup

There isn't one. No config file, no attributes, no changes to your tests —
cargo-oracle reads the code you already have.

What it relies on instead is the conventional layout, which *is* the
configuration:

```rust
// src/config.rs
#[derive(Debug, Default)]
pub struct Config {
    pub host: String,
    pub retries: u8,
}

impl Config {
    /// `&mut self` with no return value, so the result lives in the receiver.
    /// cargo-oracle reads that off the signature: a test must observe `c`
    /// after the call or it cannot detect a fault here.
    pub fn set_retries(&mut self, n: u8) {
        self.retries = n.min(10);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Living in the same file is what makes this module *claim* the symbols
    // above. Containment is the claim; nothing needs annotating.
    #[test]
    fn set_retries_clamps() {
        let mut c = Config::default();
        c.set_retries(99);
        assert!(true);
    }
}
```

```
$ cargo oracle lint

warning: nothing observes the receiver this call mutates
  --> src/config.rs:21:9
   |
21 |         c.set_retries(99);
   |         ^^^^^^^^^^^^^^^^^ demo::config::Config::set_retries
   |
   = help: assert on the receiver after the call: `obj.mutate();
           assert_eq!(obj.field, expected);` -- or compare the whole value if
           it derives PartialEq.
   = note: oracle lint `oracle_shape_mismatch` (ORC010)

warning: this assertion cannot fail
  --> src/config.rs:22:9
   |
22 |         assert!(true);
   |         ^^^^^^^^^^^^^
   |
   = help: replace one side with an independently written expected value. If
           there is nothing to compare against, the test has no subject.
   = note: oracle lint `tautological_assert` (ORC007)

warning: `demo` generated 2 warnings across 1 of 1 test
  oracle strength: 0 strong, 0 partial, 0 weak, 1 none
  1 symbol scorable, all claimed by some test
```

Replace `assert!(true)` with `assert_eq!(c.retries, 10)` and it goes quiet:

```
$ cargo oracle lint

ok: `demo`: every test has an oracle that can fail
  oracle strength: 1 strong, 0 partial, 0 weak, 0 none
  1 symbol scorable, all claimed by some test
```

Two conventions carry all the attribution, and both are ones you already
follow:

- A `#[cfg(test)] mod tests` **in the same file** claims that file's symbols.
- A **doctest** claims the item it documents.

Name similarity and the public surface reachable from `tests/` are used too,
but only as explicitly weaker evidence.

### What each command needs

Only the first row is free. Everything below it costs a build, which is why
`--since` exists.

| Command | Needs | Cost |
|---|---|---|
| `lint` `explain` `inventory` `claims` | nothing beyond cargo | milliseconds, no build |
| `coverage` | `cargo-llvm-cov`, `llvm-tools-preview` | one instrumented build |
| `attribute` | + `cargo-nextest` | one run per test |
| `verify` | `cargo-mutants`, `cargo-nextest` | one rebuild per mutant |
| `fastverify` | `cargo-nextest` | one build, one run per mutant |

```sh
rustup component add llvm-tools-preview
cargo install cargo-nextest cargo-llvm-cov cargo-mutants
```

`nix develop` in a checkout provides all of them.

### In CI

`lint` needs no build, so it is cheap enough to run on every push:

```yaml
- run: cargo install --git https://github.com/ChristopherJMiller/cargo-oracle cargo-oracle
- run: cargo oracle lint --deny high
```

`verify` costs a rebuild per mutant, so scope it to the diff:

```yaml
- uses: actions/checkout@v4
  with: { fetch-depth: 0 }
- run: cargo oracle verify --since origin/${{ github.base_ref }}
```

A change touching no Rust source reports "nothing to verify" and passes, so a
docs-only PR is not failed by the gate.

## Commands

```sh
# Static, no build required
cargo oracle lint                       # audit test oracles
cargo oracle lint --deny high           # exit 1 on a high-severity finding
cargo oracle explain ORC010             # what a rule means and how to fix it
cargo oracle explain                    # list every rule
cargo oracle inventory                  # symbols, and the oracle each requires
cargo oracle claims                     # which tests speak for which symbols

# Execution: needs a build
cargo oracle coverage                   # which symbols actually run
cargo oracle coverage --coverage-json report.json     # reuse an existing report
cargo oracle attribute --dry-run        # what a per-test run would profile
cargo oracle attribute --tests parse    # which test runs which symbol

# Verification: needs a rebuild per mutant
cargo oracle verify --since origin/main # mutate only what the branch changed
cargo oracle verify --in-diff pr.diff   # or supply the diff yourself
cargo oracle verify --with-attribution  # adds the per-test executes/verifies verdict
cargo oracle fastverify --dry-run       # experimental: one build, N runs
```

Output follows cargo's conventions throughout:

```sh
--message-format human   # rustc-style diagnostics (default), on stderr
--message-format short   # file:line:col: warning: message [CODE], for editors
--message-format json    # machine-readable, on stdout
--color auto|always|never   # honours NO_COLOR under `auto`
-v                       # every test, and why each rule matters
```

## Status

Built in slices, each independently useful.

- **v0 — static** ✅ : symbol inventory, claim map, ten oracle-strength lints.
  No build, no execution, milliseconds on a whole workspace.
- **v1 — execution** ✅ : function-level coverage from `cargo llvm-cov --json`,
  joined to the inventory by definition span. Separates monomorphized entries
  from nested closures, and `claimed, not run` from plain `unexecuted`.
- **v2 — per-test attribution** ✅ : `cargo-nextest`'s process-per-test model
  gives one profile per test, so *executes* becomes an edge rather than a bit.
  Surfaces tests with broad reach and no discrimination, symbols no test
  reaches, and symbols only one test reaches.
- **v3 — verification** ✅ : `cargo-mutants` body-replacement mutants, with
  kills attributed back to the individual test that caught them. Produces the
  per-test `executes N, verifies M` verdict, and confirms or refutes the ORC010
  predictions v0 made for free.

All four slices are implemented and each runs on its own.

`cargo oracle fastverify` is an experimental fifth: it compiles every mutation
in at once behind a runtime switch, so the cost is one build plus N test runs
rather than N builds. It can also report a symbol as *unreached* rather than
*missed*, a distinction cargo-mutants cannot make. See
[docs/design.md](docs/design.md) for how it avoids one uncompilable default
breaking the whole build -- and for its limitations, which are real.

## Library use

`oracle-core` is the library behind the subcommand, and every slice is usable on
its own:

```sh
git clone https://github.com/ChristopherJMiller/cargo-oracle
cd cargo-oracle

cargo doc -p oracle-core --open
cargo run --example static_audit -- path/to/crate   # v0, no build required
cargo run --example verdicts -- . mutants.out       # read an existing run
```

## Documentation

- [docs/design.md](docs/design.md) — why symbol identity is a definition span,
  what each symbol state does and does not claim, how each slice joins onto the
  next, and why Rust's mutation cost model is inverted relative to every other
  ecosystem.
- [docs/oracle-lints.md](docs/oracle-lints.md) — every rule, with the pattern it
  catches and the three that were deliberately *not* made rules.

## License

MIT
