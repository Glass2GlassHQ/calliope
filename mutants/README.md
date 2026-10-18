# Mutation testing

cargo-mutants results over g2g. Each mutant is one injected bug (a function body
replaced by a default value, an operator swapped, a match arm or a `!` deleted).
A mutant the test suite fails on is caught. One it passes is missed, so no test
pins down that behavior. Per scope, the subdir holds the four outcome lists and
`summary.txt` (command, g2g commit, wall time, cargo-mutants totals).

Run a scope with `tools/mutants-g2g.sh <scope> <cargo-mutants args>`. It runs one
build at a time with two rustc jobs at nice 10 (`MUTANTS_JOBS`,
`MUTANTS_JOBSERVER_TASKS` and `CARGO_BUILD_JOBS` raise that on a quiet box),
keeps the tree copy and logs under `~/tmp/calliope-mutants`, and copies the
lists here.

Name test targets with `--cargo-arg` (`--cargo-arg=--lib --cargo-arg=--test=m135_vp9parse`),
not after `--`. cargo-mutants builds each mutant with `cargo test --no-run` and
passes the `--` args only to the test step, so the build would link all ~700
g2g-plugins test binaries for every mutant.

Score is caught / (caught + missed). Timeouts and unviable mutants are left out
of it: a timeout is a mutant that hung the tests, an unviable one does not
compile.

## Results

| Scope | Mutants | Caught | Missed | Timeout | Unviable | Score | Wall |
| :--- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| g2g-core (caps, solver, clock, memory, frame, channel, runner) | 1114 | 516 | 351 | 26 | 221 | 59% | 3h |

The g2g-core row is the suite before g2g M1530 and M1531, which added tests for
245 of its 351 missed mutants. The other 106 are formatting, log lines, test
helper code, equivalent mutants or results the caller discards.
