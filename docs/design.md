# Design

## The question

Coverage answers whether a line ran. It cannot answer whether anything checked
the result. Both questions matter and only one of them has tooling.

The gap is old. What changed is who writes the tests, and three findings shape
this design:

- Agent-authored tests carry a median of 2.0 assertions against 1.0 for
  human-written ones, while detecting fewer injected faults. For AI-written
  tests, assertion density is not a weak signal, it is an inverted one, so any
  metric built on counting assertions ranks the worst suites highest.
- Prompting a model with buggy code produces tests that pin the bug in place as
  expected behaviour. The oracle is faithful to the wrong thing.
- When one model writes both the implementation and its tests, a shared
  misunderstanding yields a green suite with no independent check anywhere in
  it.

So the question is not "is there an assertion?" but "would any assertion fail
if this symbol stopped working?"

## What each command measures

Four stages, each useful alone. Later ones join onto the symbol identity the
first establishes.

| Command | Question | Evidence | Cost |
|---|---|---|---|
| `lint` | Can this oracle fail at all? Does anything claim this symbol? | `syn` parse | milliseconds, no build |
| `coverage` | Which symbols run? | `cargo llvm-cov --json` | one instrumented build |
| `attribute` | Which test runs which symbol? | `cargo-nextest`, one profile per test | one run per test |
| `verify` | Which test fails when the symbol's body is destroyed? | `cargo-mutants` | one rebuild per mutant |

## Symbol identity is a span, not a name

This is the decision everything else rests on.

Each tool we join against reports `file:line:col`, and none of them agree on
names. llvm-cov reports mangled, monomorphized symbols: one generic
`fn parse<T>` appears once per instantiation, closures appear as
`parse::{{closure}}`, and an `async fn` appears as the state machine the
compiler generated for it. cargo-mutants reports the source path and the
unmangled function name. `syn`, which builds our inventory, sees only what was
written.

Reconciling mangled names across those three is a swamp. Definition spans are
not. Every tool emits one, and containment performs the join for free:

- N monomorphized coverage entries all fall inside the one source span that
  defined them, so instantiations collapse with no demangling.
- Code with no source span, like derive output and macro expansions, falls
  outside every inventory span and filters itself out.

`SymbolId` is therefore `(file, line, col)` of the name in the definition, with
a `LineSpan` from the `fn` keyword to the closing brace used for containment.
Columns are 1-based to match cargo-mutants output; `proc-macro2` hands us
0-based columns and `SymbolId::new` adjusts.

## Symbol states

| State | Meaning | Evidence needed |
|---|---|---|
| `unexecuted` | Nothing runs it | `coverage` |
| `claimed, not run` | A test module or doctest claims it, and nothing runs it | `lint` + `coverage` |
| `executed` | Runs. Whether anything checks it is a separate question | `coverage` |
| `pseudo-tested` | Runs, but destroying its body fails no test | `verify` |
| `verified` | A named test fails when the body is destroyed | `verify` |
| `no viable mutant` | Mutant would not compile | predicted statically, confirmed by `verify` |
| `not mutated` | No mutant exists for the signature at all | `verify` |
| `out of scope` | The run never examined this file | `verify` |

`claimed, not run` is worth separating from plain `unexecuted`. Both are
uncovered, but in the first case someone took responsibility for the symbol by
putting a test module in its file or writing a doctest, and then did not
exercise it. That is a broken promise rather than an absence.

The unscorable states need care, and an earlier draft of this document got them
wrong. When `Default::default()` does not typecheck as a replacement body,
cargo-mutants drops the mutant as unviable, and it is tempting to read that as
the type system carrying the contract. Sometimes it is: a return type with no
meaningful `Default` genuinely constrains what the function can do. But
`-> impl Trait`, `-> !` and `const fn` are unviable for a duller reason, which
is that our mutation operator is body replacement and body replacement does not
apply. That is a blind spot, and reporting it as "type-enforced" would launder
a gap into a reassurance.

## The oracle a signature requires

Rust puts effects in types, which makes a check available here that is not
available elsewhere. The shape of a required oracle is readable from the
signature alone.

| Signature | Required oracle | A test that only asserts on the return value is |
|---|---|---|
| `fn(&self) -> T` | observe the return value | fine |
| `fn(&mut self)` | observe the receiver afterwards | structurally blind |
| `fn(&mut self) -> T` | both | half blind |
| `fn(..)` with neither | side channel: I/O, globals | out of reach of any signature-derived oracle |

`RequiredOracle::derive` computes this and ORC010 enforces it. No amount of
coverage or assertion density substitutes. A test that calls `fn(&mut self)`
and never mentions the receiver again cannot detect a fault in it.

## Joining coverage onto symbols

`-C instrument-coverage` is unusual in being natively function-granular.
`cargo llvm-cov --json` emits a `functions[]` array with an execution count and
source regions per function, so the symbol layer is the native unit here and
the line view is derived. Most tooling has that backwards.

Names are never compared. Each coverage entry is placed by the start line of
its first region, and the inventory span containing that line claims it:

| Entry starts | Classified as | Count handling |
|---|---|---|
| exactly at a symbol's `fn` line | an entry for that symbol | summed |
| strictly inside a symbol's span | nested: a closure, `async` block, generator | tracked separately |
| in no span at all | macro or derive output | counted and reported |

Verified against real llvm-cov output for the fixture crate. Every symbol's
first region started exactly at the `fn` line, and `parse`'s
`.map_err(|_| ..)` closure appeared as its own entry starting at line 40,
inside `parse`'s span of 38 to 42. Folding the closure's zero count into its
parent would have been wrong in both directions, since `parse` ran twice and
the closure genuinely never ran.

An uncovered closure inside a covered function is often the most useful line in
the report, because it is an error path nothing exercised.

One entry per symbol is not guaranteed, and more than one does not mean
generics. The same function compiled into a library and into the test harness
that links it produces two entries, exactly as a generic monomorphized twice
does. The join is correct either way and the counts sum to total executions,
but the number cannot be read as an instantiation count. An earlier version of
the report labelled it that way and was wrong on every non-generic function in
the workspace.

## Per-test attribution

Coverage answers whether anything executes a symbol, which is one bit. A bit
says nothing about an individual test, and the individual test is the unit an
audit of agent-written code cares about. `attribute` turns that bit into an
edge.

`cargo-nextest` makes this possible without cooperation from the test harness,
because it runs every test in its own process:

```text
for each test T:
    cargo llvm-cov clean --profraw-only --workspace
    cargo llvm-cov nextest --no-report -E 'test(=T)'
    cargo llvm-cov report --json
```

The build is shared, so only the run, the `llvm-profdata` merge and the export
repeat. That is still O(tests), and it is the dominant cost of this stage: a
nightly job on a large suite rather than a pre-commit hook. The intended fast
path is `--tests`, which bounds cost by the size of the change.

### Resolving nextest names

nextest addresses a lib test as `config::tests::test_parse` while the inventory
writes `mycrate::config::tests::test_parse`. The nextest name is a suffix of
ours, matched on a `::` boundary so `test_parse` cannot match
`test_parse_extended`.

Integration tests are addressed by a bare function name, unique only within
their binary. When a name resolves to more than one inventoried test the edge
is recorded as ambiguous and skipped, never guessed. A wrong attribution edge
is worse than a missing one: a missing edge understates coverage, while a wrong
one credits a test with verifying something it never touched.

### Reach without discrimination

The headline here is the closest thing to a verification verdict available
without paying for mutants:

```
broad reach, no discrimination -- these run a lot of code and check almost none of it
  mycrate::tests::test_end_to_end     executes  34, strongest oracle: weak
```

Only mutation proves a test verifies nothing. But "executes 34 symbols,
strongest oracle is `is_ok()`" is the exact shape of a test that raises
coverage without raising confidence, and it costs one instrumented run per test
rather than one rebuild per mutant.

Two more views fall out of the edge set for free. Symbols no test executes are
an exact per-symbol version of the coverage gap. Symbols exactly one test
executes are a single point of failure: delete or skip that test and the symbol
silently becomes unexercised.

## Mutation verification

The stage the others exist to reach. Coverage says a symbol ran, attribution
says which test ran it, and only mutation says whether anything would notice it
breaking.

`cargo-mutants` replaces a function body with a type-appropriate default:
`Ok(Default::default())`, `()`, `""`, `0`. That is extreme mutation, the same
operator Descartes implements for Java to find pseudo-tested methods, and in
Rust it is the default behaviour of the mainstream tool rather than a bolt-on
engine. If no test notices the entire body vanishing, no test will notice a
subtler fault either.

### The join, again by span

cargo-mutants reports each mutant's replaced span as `file:line:col`, and that
span lies inside the enclosing function body. So the same containment rule used
for coverage places it: the innermost inventory `LineSpan` containing the
mutant's line owns it. A mutant inside a closure is attributed to the function
defining the closure, which is what we want.

Note that cargo-mutants' own `function.span.start` includes doc comments and
attributes, so it does not equal our `fn`-line start. Joining on the mutant
span rather than the function span sidesteps that entirely.

### Attributing the kill to a test

The per-mutant log under `mutants.out/log/` carries the test-run output, and
nextest's summary line names the failure:

```
FAIL [   0.006s] (4/4) weak-suite config::tests::parse_extracts_host_and_port
```

Parsed alongside libtest's `test <name> ... FAILED`, with the panicking thread
name as a fallback. Names resolve to inventoried tests by the same strict
suffix rule attribution uses, and an ambiguous name is dropped rather than
guessed.

nextest cancels the run at the first failure, so the log names a test that
killed the mutant, not every test that would have. "Verified by X" means X is
sufficient, never that X is the only one. Passing `--no-fail-fast` through
lifts this at proportional cost.

### Three ways to be unscorable

This is where an audit tool earns or loses its credibility, because every one
of these is a place where a gap could be laundered into a reassurance.

| State | Cause | What it does not mean |
|---|---|---|
| `PSEUDO-TESTED` | Viable mutants, all survived | this one is a real finding |
| `no viable mutant` | Mutant did not compile | not "type-enforced"; partly the type system, partly our operator being weak |
| `not mutated` | cargo-mutants examined the file and generated nothing | unscorable, not unverified |
| `out of scope` | The run never examined the file | nothing at all, in either direction |

The `not mutated` case is not hypothetical. In the fixture crate,
`Config::new` returns `Self` and cargo-mutants produces no mutant for it,
because body replacement has no default to substitute. The constructor is in
fact well tested and the tool simply cannot score it. Reporting that as
anything other than unscorable would be a lie of omission.

For the same reason the summary reports two mutant counts: those landing on
scorable symbols, and the total the run generated.

### The two things only mutation can say

Combining attribution edges with kill attribution gives the direct answer to
the question this tool exists for:

```
TEST                                     EXECUTES  VERIFIES
weak_suite::config::tests::test_parse           2         0  <- runs code, verifies none of it
weak_suite::config::tests::test_validate        2         0  <- runs code, verifies none of it
parse_extracts_host_and_port                    2         1
```

And where ORC010 predicted statically that a symbol could not be verified,
mutation reports whether it was right:

```
Config::set_retries   PSEUDO-TESTED   1 mutant survived: ()
                      ^ predicted by ORC010 -- `c.set_retries(..)` mutates `c`, which no assertion observes
```

That line is the argument for the layered design. The static rule costs
milliseconds and no build; the mutation evidence costs a rebuild per mutant.
When the cheap check calls it correctly, it can run on every commit and the
expensive one can be reserved for the diff.

## The claim map

In most languages, "what does this test claim to test?" is a guess from a
naming convention. Rust encodes it structurally in two places. A
`#[cfg(test)] mod tests` lives inside the file whose items it speaks for, which
is containment rather than heuristic. A doctest is lexically attached to the
item it documents, which is exact and free.

Those are `ClaimKind::SameFileTestModule` and `ClaimKind::Doctest`, the two
`is_direct()` edges. Name similarity and the public surface reachable from
`tests/` are kept as explicitly weaker edges, so a symbol reachable only
through an integration test is not reported as entirely unclaimed.

A claim is not evidence. It is the denominator: the set a test has taken
responsibility for, against which the later stages measure what it actually
executes and actually verifies. A wide gap between claimed and verified is the
shape agent-written suites take.

## Module paths without name resolution

We do not resolve `mod` declarations. A file's module path is read off its
location under `src/`, which is what Rust's own convention encodes.
`src/parser/config.rs` is `parser::config`, `src/parser/mod.rs` is `parser`,
and `src/lib.rs` is the crate root.

The tradeoff: a `.rs` file under `src/` that no `mod` declaration reaches still
gets inventoried. That is the safe direction for an audit tool, since the
symbol simply reports as unexecuted once coverage runs.

Directories containing their own `Cargo.toml` are pruned from the walk, so a
fixture crate or vendored dependency inside `tests/` is not attributed to its
host package. Dogfooding found that one, not the unit tests.

## The cost model is inverted in Rust

This constraint shapes verification, and it is worth stating plainly because it
is the opposite of the situation in other ecosystems.

PIT mutates JVM bytecode in memory: no rebuild, thousands of mutants a minute.
cargo-mutants must recompile for every mutant, so build and link dominate and
test execution is the cheap half.

Two consequences follow. First, coverage-guided test selection buys little
here. Running only the tests that touch the mutated function is the standard
mutation-testing optimization and PIT does it natively, but in Rust it
optimizes the half that was not expensive. cargo-mutants does not implement it,
and that is arguably a rational omission rather than a gap.

Second, diff scoping is doubly important, and it bounds both expensive halves.
Per-test coverage is only needed for the tests in the diff, and mutants only
for the symbols in the diff, so cost is `k changed symbols × m new tests`
rather than `N × M`.

## Diff scoping in practice

`--since <ref>` is the intended way to run verification. It diffs the working
tree against the ref, so the new side of the diff is by construction what is on
disk, which is what cargo-mutants requires and what `git diff base..head` does
not give you on a clean checkout.

Three details decide whether the resulting report is honest.

A docs-only change is a pass, not an empty report. A diff touching no Rust
source has nothing to mutate. A CI gate must not fail a README edit, nor report
it as zero verified symbols.

Scope is recorded, not just applied. A symbol outside the diff gets no mutant,
which in the data is indistinguishable from a symbol the operator could not
mutate. `MutationMap` carries the `DiffScope` so the report can say `out of
scope` rather than `no mutant exists for this signature`.

Scope means added lines, not hunk ranges. A hunk header spans its context
lines, and cargo-mutants does not mutate on the strength of context. Scoping by
the header marks the symbols immediately above and below a change as examined.
`DiffScope::parse` walks hunk bodies and records only `+` lines.

That third point cost two false "no mutant exists" lines on a one-function
change. The general shape of this bug, absence of evidence rendered as evidence
of absence, has now appeared three times in this project: at file granularity,
at hunk granularity, and in the `not mutated` state itself. It recurs because
the two cases are identical in the data structure and only the caller knows
which one it is.

### Cost, measured

On this workspace, a one-function change:

| Scope | Mutants | Wall clock |
|---|---|---|
| whole workspace | 100+ | tens of minutes |
| `--file claims.rs` | 41 | 2 minutes |
| `--since HEAD` | 2 | 18 seconds |

That is the difference between a nightly job and a pull-request gate.

## Single-compile mutation (experimental)

`cargo oracle fastverify` takes the other side of Rust's cost trade. Instead of
rewriting and rebuilding per mutant, it compiles every mutation in at once
behind a runtime switch and selects one per test run:

```text
cargo-mutants:  N × (rewrite + build + test)
fastverify:     1 × (rewrite + build) + N × test
```

This is the approach [mutagen] pioneered and [muttest] continues. What follows
is what it took to make it work on stable Rust.

### The build-poisoning problem

Body replacement needs a value of the return type, and not every type
implements `Default`. cargo-mutants shrugs this off, since the mutant fails to
compile and is reported unviable. Single-compile mutation cannot: one
uncompilable default breaks the entire build and takes every other mutant with
it.

`oracle_switch::default_for!` resolves this with autoref specialization. Two
traits share a method name, one implemented for `&Probe<T>` under a `Default`
bound and one for `Probe<T>` without, invoked through two autorefs so the
bounded arm is reached first when it applies. Stable Rust, no nightly, no bound
on the caller. A type with no `Default` yields `None`, its mutation is inert
rather than fatal, and it is reported unviable exactly as cargo-mutants would.

Getting the autoref count wrong by one makes the fallback win for every type,
silently disabling every mutant. The guard fires, the probe returns `None`,
nothing changes, and every test passes. That failure mode looks like a
suspiciously healthy codebase, which is why the switch crate tests both arms
explicitly.

### Two type-level traps

`Result<T, E>` has no `Default`. There is no reason to prefer `Ok` over `Err`,
so std does not implement it. Probing the return type directly would make every
fallible function inert, which is most of a Rust codebase. The rewriter probes
the success type and wraps in `Ok`, matching what cargo-mutants generates.

`Option<T>` needs no special case, because `impl<T> Default for Option<T>` is
unconditional and the probe already yields `None` for any `T`.

### Textual insertion, to preserve the join key

The guard is inserted by byte offset immediately after each body's opening
brace, leaving the rest of the file untouched. Reprinting the parsed AST would
be far easier but renumbers every line, and line numbers are this project's
join key. Keeping files byte-identical apart from the insertions means a
`SymbolId` computed against the original tree still addresses the same code in
the rewritten one.

### One thing it can report that cargo-mutants cannot

The switch writes a note when an active mutation is reached, which separates
three cases cargo-mutants reports as two:

| Note | Tests | Verdict |
|---|---|---|
| `applied` | passed | a genuine survivor: the body was destroyed and nothing noticed |
| `inert` | passed | no `Default` for the return type, so unviable |
| none | passed | no test ever called the function |

That last row is a real distinction. cargo-mutants reports an unreached
function and a badly-tested one identically, as a missed mutant.

On the fixture crate, fastverify also mutates `Config::new`, which returns
`Self`, where cargo-mutants generates nothing, because the probe finds that
`Config: Default`.

### Honest limitations

Virtual workspaces are unsupported. Every member needs the switch dependency
added separately, and the command refuses rather than producing a tree that
will not build.

Only body replacement is implemented. cargo-mutants also mutates binary
operators, and those mutants are often the interesting ones on well-tested
code.

The measured win is small on small crates. On the fixture: 7s for 10
cargo-mutants mutants against 2s for 5 fastverify mutants, which is 0.7s
against 0.4s per mutant. The advantage is proportional to build time and the
fixture builds in about a second. It has not been demonstrated on a crate where
a build takes twenty seconds, which is exactly where it should matter most, and
that is the next thing to measure rather than assert.

`cargo oracle verify` remains the authoritative command.

[mutagen]: https://github.com/llogiq/mutagen
[muttest]: https://github.com/samuelpilz/muttest-rs

## Known gaps

- `no_run` doctests compile but never execute, so their assertions never run.
  They are currently linted as if they do.
- ORC010 fires only for `&mut self`. Detecting a mutated `&mut` argument needs
  dataflow we do not have.
- Async state machines and macro-heavy crates will stress the span join;
  untested at scale.
- Visibility is read from the item, so a `pub` item inside a private module is
  reported as public. This only affects the weakest claim edge.
- Coverage entry counts merge across binaries, so `entries > 1` cannot be read
  as a generic instantiation count.
- Doctests are not included in coverage unless `cargo llvm-cov --doctests` is
  passed, so a symbol covered only by a doctest reports as unexecuted.
