# cargo-oracle

Symbol-level test attribution for Rust. It answers a question coverage cannot:
would any test fail if this function stopped working?

## The problem

Coverage tells you a line ran. It says nothing about whether anything checked
the result.

That gap has always existed, but agent-written tests widen it, and counting
assertions will not find them. A study of AI-authored test commits found agent
tests carry a median of 2.0 assertions against 1.0 for human ones while
detecting fewer injected faults. Density is an inverted signal. The question is
not whether an oracle exists but whether it can fail.

## What it looks like

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

Diagnostics follow rustc's shape, so there is no new report format to learn.
Messages are lowercase and unpunctuated, `help` carries the fix while `note`
carries context, lint names are snake_case, and `--color` and
`--message-format` behave the way cargo's do.

`oracle_shape_mismatch` is the finding you cannot get from any other tool.
Rust puts effects in the type. A `fn set_retries(&mut self, n: u8)` announces
that its result lives in the receiver, so a test that calls it and never
mentions `c` again is structurally blind to it, however many assertions it
contains.

Every rule explains itself, the way `rustc --explain` does:

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

There isn't one. No config file, no attributes, nothing to add to your tests.

What cargo-oracle uses instead is the layout you already write. A
`#[cfg(test)] mod tests` sitting in the same file as the code it exercises is
how the tool knows which symbols that module speaks for. Doctests attach to the
item they document. Those two conventions carry the whole attribution model.

```rust
// src/config.rs
#[derive(Debug, Default)]
pub struct Config {
    pub host: String,
    pub retries: u8,
}

impl Config {
    /// `&mut self` with no return value, so the result lives in the receiver.
    /// cargo-oracle reads that off the signature: a test has to observe `c`
    /// after the call or it cannot detect a fault here.
    pub fn set_retries(&mut self, n: u8) {
        self.retries = n.min(10);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Living in the same file is what makes this module claim the symbols
    // above. Containment is the claim, so nothing needs annotating.
    #[test]
    fn set_retries_clamps() {
        let mut c = Config::default();
        c.set_retries(99);
        assert!(true);
    }
}
```

That produces the two warnings shown above. Replace `assert!(true)` with
`assert_eq!(c.retries, 10)` and the tool goes quiet:

```
$ cargo oracle lint

ok: `demo`: every test has an oracle that can fail
  oracle strength: 1 strong, 0 partial, 0 weak, 0 none
  1 symbol scorable, all claimed by some test
```

Name similarity and the public surface reachable from `tests/` are used as
well, but only as weaker evidence, and the report says which kind of claim it
is relying on.

## The four commands

Each is useful on its own. They are listed in order of what they cost.

### `cargo oracle lint`

Parses the workspace and reports oracles that cannot discriminate. No build, no
test run, milliseconds on a whole workspace. Ten rules, documented in
[docs/oracle-lints.md](docs/oracle-lints.md).

It can also tell you which symbols no test claims at all, which is a coverage
proxy that costs nothing.

### `cargo oracle coverage`

Runs `cargo llvm-cov` and joins the result onto the symbol inventory, so you
get execution per symbol rather than per line. It separates a symbol nothing
runs from one a test module claims and then never exercises. The second is a
broken promise and worth knowing about separately.

An uncovered closure inside a covered function shows up here too, which is
usually an error path nobody tested.

### `cargo oracle attribute`

Profiles each test separately, using nextest's process-per-test model, so
`executes` becomes an edge between a test and a symbol rather than a single
bit. Cost is one instrumented run per test, so scope it with `--tests` on
anything large.

The output worth reading is tests with broad reach and no discrimination:
"executes 34 symbols, strongest oracle is `is_ok()`". That is not proof the
test verifies nothing, but it is the shape of a test that raises coverage
without raising confidence.

### `cargo oracle verify`

Runs `cargo-mutants`, which replaces each function body with a default value,
and attributes the kills back to whichever test caught them. This is the only
command that can say a symbol is verified.

Combined with attribution it produces the per-test verdict:

```
TEST                                     EXECUTES  VERIFIES
weak_suite::config::tests::test_parse           2         0  <- runs code, verifies none of it
weak_suite::config::tests::test_validate        2         0  <- runs code, verifies none of it
parse_extracts_host_and_port                    2         1
```

No other tool reports mutation score per test. Aggregating to file or project
is exactly the granularity at which one weak new test disappears into a healthy
average.

It also confirms or refutes what the static rules predicted:

```
Config::set_retries   PSEUDO-TESTED   1 mutant survived: ()
                      ^ predicted by ORC010 -- `c.set_retries(..)` mutates `c`, which no assertion observes
```

That line is the argument for having a cheap layer at all. ORC010 costs
milliseconds and can run on every commit, so the rebuild-per-mutant work can be
saved for the diff.

## What the answers mean

Each command claims only what its evidence supports. `lint` and `coverage`
never report a symbol as verified, because neither has run a mutation.

`verify` is careful in the other direction. There are three separate ways for a
symbol to be unscorable, and none of them mean the symbol is fine:

| State | Cause |
|---|---|
| `no viable mutant` | The mutant did not compile. Partly type enforcement, partly our operator being weak. Not a guarantee. |
| `not mutated` | cargo-mutants examined the file and produced nothing for this signature. `-> Self` on a constructor is the common case. |
| `out of scope` | The run never looked at this file, because it was scoped with `--file` or `--since`. |

The last distinction is not pedantry. Scoping a run to one file leaves the rest
of the workspace unexamined, and an earlier version of this tool reported all
of it as "no mutant exists". That turns a gap into a reassurance, which is the
failure that made coverage percentages untrustworthy in the first place.

## What each command needs

Only the first row is free.

| Command | Needs | Cost |
|---|---|---|
| `lint` `explain` `inventory` `claims` | nothing beyond cargo | milliseconds, no build |
| `coverage` | `cargo-llvm-cov`, `llvm-tools-preview` | one instrumented build |
| `attribute` | also `cargo-nextest` | one run per test |
| `verify` | `cargo-mutants`, `cargo-nextest` | one rebuild per mutant |
| `fastverify` | `cargo-nextest` | one build, one run per mutant |

```sh
rustup component add llvm-tools-preview
cargo install cargo-nextest cargo-llvm-cov cargo-mutants
```

`nix develop` in a checkout provides all of them.

## In CI

`lint` needs no build, so it is cheap enough for every push:

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
docs-only pull request is not failed by the gate.

On this workspace, a one-function change:

| Scope | Mutants | Wall clock |
|---|---|---|
| whole workspace | 100+ | tens of minutes |
| `--file claims.rs` | 41 | 2 minutes |
| `--since HEAD` | 2 | 18 seconds |

## Command reference

```sh
# Static, no build required
cargo oracle lint                       # audit test oracles
cargo oracle lint --deny high           # exit 1 on a high-severity finding
cargo oracle explain ORC010             # what a rule means and how to fix it
cargo oracle explain                    # list every rule
cargo oracle inventory                  # symbols, and the oracle each requires
cargo oracle claims                     # which tests speak for which symbols

# Execution
cargo oracle coverage
cargo oracle coverage --coverage-json report.json     # reuse an existing report
cargo oracle attribute --dry-run        # what a per-test run would profile
cargo oracle attribute --tests parse

# Verification
cargo oracle verify --since origin/main
cargo oracle verify --in-diff pr.diff
cargo oracle verify --with-attribution  # adds the per-test verdict
cargo oracle fastverify --dry-run       # experimental, see below
```

Output options follow cargo's:

```sh
--message-format human   # rustc-style diagnostics (default), on stderr
--message-format short   # file:line:col: warning: message [CODE], for editors
--message-format json    # machine-readable, on stdout
--color auto|always|never
-v                       # every test, and why each rule matters
```

## Experimental: single-compile mutation

`cargo oracle fastverify` compiles every mutation in at once behind a runtime
switch and selects one per test run, so the cost is one build plus N test runs
instead of N builds. It can also distinguish a surviving mutation from one no
test ever reached, which cargo-mutants reports identically.

The limits are real: virtual workspaces are refused outright, only body
replacement is implemented, and the measured advantage on the fixture crate is
0.7s against 0.4s per mutant. That advantage scales with build time and the
fixture builds in about a second, so the case where it should matter has not
been measured yet. `cargo oracle verify` remains the authoritative command.

[docs/design.md](docs/design.md) explains how it avoids one uncompilable
default breaking the entire build.

## Library use

`oracle-core` is the library behind the subcommand, and every stage is usable
on its own:

```sh
git clone https://github.com/ChristopherJMiller/cargo-oracle
cd cargo-oracle

cargo doc -p oracle-core --open
cargo run --example static_audit -- path/to/crate   # no build of the target crate
cargo run --example verdicts -- . mutants.out       # read an existing run
```

## Documentation

- [docs/design.md](docs/design.md) covers why symbol identity is a definition
  span rather than a name, what each state does and does not claim, how each
  stage joins onto the next, and why Rust's mutation cost model is inverted
  relative to other ecosystems.
- [docs/oracle-lints.md](docs/oracle-lints.md) documents every rule, the
  pattern it catches, and the three that were deliberately left out.

## License

MIT
