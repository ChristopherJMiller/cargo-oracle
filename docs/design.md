# Design

## The question

Coverage answers *did this line run*. It structurally cannot answer *did anything
check the result*. Both questions matter, and only one of them has tooling.

The gap has always existed. What changed is who is writing the tests. Three
findings shape this design:

- Agent-authored tests carry a **median of 2.0 assertions against 1.0 for
  human-written ones**, while detecting *fewer* injected faults. Assertion
  density is not a weak signal for AI-written tests — it is an inverted one, so
  any metric built on counting assertions ranks the worst suites highest.
- Prompting a model with buggy code produces tests that pin the bug in place as
  expected behaviour. The oracle is faithful; it is faithful to the wrong thing.
- When one model writes both the implementation and its tests, a shared
  misunderstanding yields a green suite with no independent check anywhere in it.

So the question this tool asks is not "is there an assertion?" but "**would any
assertion fail if this symbol stopped working?**"

## Four slices

Each slice is independently useful and ships on its own. Later slices join onto
the symbol identity that v0 establishes.

| Slice | Question | Evidence | Cost |
|---|---|---|---|
| **v0** static | Can this oracle fail at all? Does anything claim this symbol? | `syn` parse | milliseconds, no build |
| **v1** coverage | Which symbols run? | `cargo llvm-cov --json` | one instrumented build |
| **v2** attribution | Which *test* runs which symbol? | `cargo-nextest`, one profile per test | N profiles + merges |
| **v3** verification | Which test *fails* when the symbol's body is destroyed? | `cargo-mutants` | one rebuild per mutant |

All four slices are implemented. The sections below record the decisions that
were expensive to get right.

## Symbol identity is a span, not a name

This is the decision everything else rests on.

Each tool we join against reports `file:line:col`, and none of them agree on
names:

- **llvm-cov** reports mangled, *monomorphized* symbols. One generic `fn parse<T>`
  appears once per instantiation; closures appear as `parse::{{closure}}`; an
  `async fn` appears as the state machine the compiler generated for it.
- **cargo-mutants** reports the source path and the unmangled function name.
- **`syn`** — our inventory — sees only what was written.

Reconciling mangled names across those three is a swamp. Definition spans are
not. Every tool emits one, and *containment* performs the join for free:

- N monomorphized coverage entries all fall inside the one source span that
  defined them, so instantiations collapse with no demangling.
- Code with no source span — derive output, macro expansions — falls outside
  every inventory span and filters itself out, which is the behaviour we want.

`SymbolId` is therefore `(file, line, col)` of the *name* in the definition,
with a `LineSpan` from the `fn` keyword to the closing brace used for
containment. Columns are 1-based to match `cargo-mutants` output; `proc-macro2`
hands us 0-based columns and `SymbolId::new` adjusts.

## Symbol states

| State | Meaning | Evidence needed |
|---|---|---|
| `unexecuted` | Nothing runs it | v1 |
| `claimed, not run` | A test module or doctest claims it, and nothing runs it | v0 + v1 |
| `executed` | Runs. Whether anything *checks* it is a separate question | v1 |
| `pseudo-tested` | Runs, but destroying its body fails no test | v3 |
| `verified` | A named test fails when the body is destroyed | v3 |
| `no viable mutant` | Mutant would not compile | v0 predicts, v3 confirms |
| `not mutated` | No mutant exists for the signature at all | v3 |

`claimed, not run` is worth separating from plain `unexecuted`. Both are
uncovered, but in the first case someone took responsibility for the symbol —
put a test module in its file, or wrote a doctest — and then did not exercise
it. That is a broken promise rather than an absence.

The last state needs care, and an earlier draft of this document got it wrong.
When `Default::default()` does not typecheck as a replacement body,
cargo-mutants drops the mutant as *unviable*, and it is tempting to read that as
"the type system carries this function's contract". Sometimes it is: a return
type with no meaningful `Default` genuinely constrains what the function can do.
But `-> impl Trait`, `-> !` and `const fn` are unviable for a duller reason —
our mutation operator is body replacement, and body replacement does not apply.
That is a **blind spot, not a guarantee**, and reporting it as "type-enforced"
would launder a gap into a reassurance.

So v0 predicts the category from the signature and names it `no viable mutant`.
v3 confirms it and can finally distinguish the two causes, because cargo-mutants
reports *why* it skipped a mutant.


## The oracle a signature requires

Rust puts effects in types, which makes a check available here that is not
available elsewhere: the *shape* of a required oracle is readable from the
signature alone.

| Signature | Required oracle | A test that only asserts on the return value is |
|---|---|---|
| `fn(&self) -> T` | observe the return value | fine |
| `fn(&mut self)` | observe the receiver afterwards | **structurally blind** |
| `fn(&mut self) -> T` | both | half blind |
| `fn(..)` with neither | side channel (I/O, globals) | out of reach of any signature-derived oracle |

`RequiredOracle::derive` computes this, and ORC010 enforces it. No amount of
coverage or assertion density substitutes: a test that calls `fn(&mut self)` and
never mentions the receiver again cannot detect a fault in it.

## Slice v1: the coverage join

`-C instrument-coverage` is unusual in being natively *function*-granular:
`cargo llvm-cov --json` emits a `functions[]` array with an execution count and
source regions per function. The symbol layer is the native unit here and the
line view is derived; most tooling has that backwards.

Names are never compared. Each coverage entry is placed by the start line of its
first region, and the inventory span containing that line claims it:

| Entry starts | Classified as | Count handling |
|---|---|---|
| exactly at a symbol's `fn` line | an entry *for* that symbol | summed |
| strictly inside a symbol's span | **nested** — a closure, `async` block, generator | tracked separately |
| in no span at all | macro or derive output | counted and reported |

Verified against real llvm-cov output for the fixture crate: every symbol's
first region started exactly at the `fn` line, and `parse`'s `.map_err(|_| ..)`
closure appeared as its own entry starting at line 40, inside `parse`'s span of
38–42. Folding the closure's zero count into its parent would have been wrong
in both directions — `parse` ran twice, and the closure genuinely never ran.

An uncovered closure inside a covered function is often the most useful line in
the report: it is an error path nothing exercised.

**One entry per symbol is not guaranteed, and more than one does not mean
generics.** The same function compiled into a library and into the test harness
that links it produces two entries, exactly as a generic monomorphized twice
does. The join is correct either way and the counts sum to total executions, but
the number cannot be read as an instantiation count — an earlier version of the
report labelled it that way and was wrong on every non-generic function in the
workspace.

## Slice v2: per-test attribution

v1 answers "does anything execute this symbol" — a bit per symbol. A bit says
nothing about an *individual* test, and the individual test is the unit an audit
of agent-written code actually cares about. v2 turns that bit into an edge:
`executes(test, symbol)`.

`cargo-nextest` makes this possible without any cooperation from the test
harness, because it runs every test in its own process. So a profile can be
isolated per test:

```text
for each test T:
    cargo llvm-cov clean --profraw-only --workspace
    cargo llvm-cov nextest --no-report -E 'test(=T)'
    cargo llvm-cov report --json
```

The build is shared; only the run, the `llvm-profdata` merge and the export
repeat. That is still **O(tests)**, and it is the dominant cost of this slice —
a nightly job on a large suite, not a pre-commit hook. The intended fast path is
`--tests` to scope to what a diff touched, which bounds cost by the size of the
change rather than the size of the suite.

### Resolving nextest names to inventoried tests

nextest addresses a lib test as `config::tests::test_parse`, while the inventory
writes `mycrate::config::tests::test_parse`. The nextest name is a *suffix* of
ours, matched on a `::` boundary so `test_parse` cannot match
`test_parse_extended`.

Integration tests are addressed by a bare function name, unique only within
their binary. When a name resolves to more than one inventoried test the edge is
recorded as **ambiguous and skipped**, never guessed. A wrong attribution edge
is worse than a missing one: a missing edge understates coverage, a wrong one
credits a test with verifying something it never touched.

### Reach without discrimination

The v2 headline is the closest thing to a v3 verdict that can be had without
paying for mutants:

```
broad reach, no discrimination -- these run a lot of code and check almost none of it
  mycrate::tests::test_end_to_end     executes  34, strongest oracle: weak
```

This is not proof that the test verifies nothing — only mutation proves that.
But "executes 34 symbols, strongest oracle is `is_ok()`" is the exact shape of a
test that raises coverage without raising confidence, and it costs one
instrumented run per test rather than one rebuild per mutant.

Two more views fall out of the edge set for free:

- **Executed by no test** — an exact, per-symbol version of the v1 gap.
- **Executed by exactly one test** — a single point of failure. If that test is
  deleted or skipped, the symbol silently becomes unexercised.

## Slice v3: mutation verification

The slice the other three exist to reach. v1 says a symbol ran; v2 says which
test ran it; only v3 says whether anything would *notice it breaking*.

`cargo-mutants` replaces a function body with a type-appropriate default
(`Ok(Default::default())`, `()`, `""`, `0`). That is **extreme mutation** — the
same operator Descartes implements for Java to find pseudo-tested methods — and
in Rust it is the default behaviour of the mainstream tool rather than a bolt-on
engine. The reasoning is that if no test notices the entire body vanishing, no
test will notice a subtler fault either.

### The join, again by span

cargo-mutants reports each mutant's replaced span as `file:line:col`, and that
span lies *inside* the enclosing function body. So the same containment rule
used for coverage places it: the innermost inventory `LineSpan` containing the
mutant's line owns it. A mutant inside a closure is attributed to the function
defining the closure, which is what we want.

Note that cargo-mutants' own `function.span.start` includes doc comments and
attributes, so it does **not** equal our `fn`-line start. Joining on the mutant
span rather than the function span sidesteps that entirely.

### Attributing the kill to a test

The per-mutant log under `mutants.out/log/` carries the test-run output, and
nextest's summary line names the failure:

```
FAIL [   0.006s] (4/4) weak-suite config::tests::parse_extracts_host_and_port
```

Parsed alongside libtest's `test <name> ... FAILED`, with the panicking thread
name as a fallback. Names resolve to inventoried tests by the same strict suffix
rule v2 uses; an ambiguous name is dropped rather than guessed.

**nextest cancels the run at the first failure**, so the log names *a* test that
killed the mutant, not every test that would have. `verified by X` means "X is
sufficient", never "X is the only one". Passing `--no-fail-fast` through lifts
this at proportional cost.

### Three ways to be unscorable, and none of them mean "verified"

This is where an audit tool earns or loses its credibility, because every one of
these is a place where a gap could be laundered into a reassurance:

| State | Cause | What it does *not* mean |
|---|---|---|
| `PSEUDO-TESTED` | Viable mutants, all survived | — this is a real finding |
| `no viable mutant` | Mutant did not compile | Not "type-enforced". Partly the type system, partly our operator being weak |
| `not mutated` | cargo-mutants examined the file and generated nothing | **Unscorable, not unverified** |
| `out of scope` | The run never examined the file (`--file`, `--in-diff`) | Nothing at all. Not evidence in either direction |

The last one is not hypothetical. In the fixture crate, `Config::new` returns
`Self` and cargo-mutants produces no mutant for it at all — body replacement has
no default to substitute. The constructor is in fact well tested; the tool
simply cannot score it. Reporting that as anything other than "unscorable" would
be a lie of omission.

For the same reason the summary reports two mutant counts: those landing on
scorable symbols, and the total the run generated.

### The two things only v3 can say

**Per-test verdict.** Combining v2's attribution edges with v3's kill
attribution gives the direct answer to the question this tool exists for:

```
TEST                                     EXECUTES  VERIFIES
weak_suite::config::tests::test_parse           2         0  <- runs code, verifies none of it
weak_suite::config::tests::test_validate        2         0  <- runs code, verifies none of it
parse_extracts_host_and_port                    2         1
```

**Confirmation of the static rules.** Where ORC010 predicted statically that a
symbol could not be verified, v3 reports whether it was right:

```
Config::set_retries   PSEUDO-TESTED   1 mutant survived: ()
                      ^ predicted by ORC010 -- `c.set_retries(..)` mutates `c`, which no assertion observes
```

That line is the argument for the whole layered design. The v0 rule costs
milliseconds and no build; the v3 evidence costs a rebuild per mutant. When the
cheap check calls it correctly, it can be run on every commit and the expensive
one reserved for the diff.

## The claim map

In most languages, "what does this test claim to test?" is a guess from naming
convention. Rust encodes it structurally in two places:

- A `#[cfg(test)] mod tests` lives **inside the file** whose items it speaks for.
  Containment, not heuristic.
- A doctest is lexically **attached to the item** it documents. Exact, free.

Those are `ClaimKind::SameFileTestModule` and `ClaimKind::Doctest`, the two
`is_direct()` edges. Name similarity and the public surface reachable from
`tests/` are kept as explicitly weaker edges, so a symbol reachable only through
an integration test is not reported as entirely unclaimed.

A claim is not evidence. It is the *denominator*: the set a test has taken
responsibility for, against which v1–v3 measure what it actually executes and
actually verifies. A wide gap between claimed and verified is the shape
agent-written suites take.

## Module paths without name resolution

We do not resolve `mod` declarations. A file's module path is read off its
location under `src/`, which is what Rust's own convention encodes:
`src/parser/config.rs` is `parser::config`, `src/parser/mod.rs` is `parser`,
`src/lib.rs` is the crate root.

The tradeoff: a `.rs` file under `src/` that no `mod` declaration reaches still
gets inventoried. That is the safe direction for an audit tool — the symbol
simply reports as unexecuted once v1 lands.

Directories containing their own `Cargo.toml` are pruned from the walk, so a
fixture crate or vendored dependency inside `tests/` is not attributed to its
host package. (Found by dogfooding, not by the unit tests.)

## The cost model is inverted in Rust

This is the constraint that shapes v3, and it is worth stating plainly because
it is the opposite of the situation in other ecosystems.

PIT mutates JVM bytecode in memory: no rebuild, thousands of mutants a minute.
**cargo-mutants must recompile for every mutant.** Build and link dominate;
test execution is the cheap half.

Two consequences:

1. **Coverage-guided test selection buys little here.** Running only the tests
   that touch the mutated function is the standard mutation-testing
   optimization, and PIT does it natively. In Rust it optimizes the half that
   was not expensive. cargo-mutants does not implement it, and that is arguably
   a rational omission rather than a gap.
2. **Diff scoping is doubly important**, and it bounds *both* expensive halves.
   Per-test coverage is only needed for the tests in the diff, and mutants only
   for the symbols in the diff, so cost is `k changed symbols × m new tests`
   rather than `N × M`.

The real fix is single-compile, runtime-switched mutants — rewrite each body
once to `if mutant_active(ID) { Default::default() } else { <original> }`,
compile once, select by environment variable. [mutagen] proved the approach and
is unmaintained; [muttest] is the active attempt. That is the difference between
a nightly CI job and an interactive tool, and it is the hardest part: `const fn`
cannot host the switch, the branch perturbs inlining, and `Default` bounds do
not always hold.

[mutagen]: https://github.com/llogiq/mutagen
[muttest]: https://github.com/samuelpilz/muttest-rs

## Known gaps

- `no_run` doctests compile but never execute, so their assertions never run.
  They are currently linted as if they do.
- ORC010 fires only for `&mut self`. Detecting a mutated `&mut` argument needs
  dataflow we do not have.
- Async state machines and macro-heavy crates will stress the span join once v1
  lands; untested until there is coverage data to join against.
- Visibility is read from the item, so a `pub` item inside a private module is
  reported as public. Only affects the weakest claim edge.
- Coverage entry counts merge across binaries (a lib and the test harness that
  links it), so `entries > 1` cannot be read as a generic instantiation count.
- Doctests are not included in coverage unless `cargo llvm-cov --doctests` is
  passed, so a symbol covered *only* by a doctest currently reports as
  unexecuted.
- A mutation run scoped with `--file` or `--in-diff` leaves most of the
  workspace unexamined. Those symbols report `out of scope`, which is tracked
  separately from `not mutated` — conflating them would report an unexamined
  crate as one with no mutants available.
