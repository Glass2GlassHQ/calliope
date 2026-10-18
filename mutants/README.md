# Mutation testing

cargo-mutants results over g2g. Each mutant is one injected bug (a function body
replaced by a default value, an operator swapped, a match arm or a `!` deleted).
A mutant the test suite fails on is caught. One it passes is missed, so no test
pins down that behavior. Per scope, the subdir holds the four outcome lists and
`summary.txt` (command, g2g commit, wall time, cargo-mutants totals).

Run a scope with `tools/mutants-g2g.sh <scope> <cargo-mutants args>`. It runs one
build at a time with two rustc jobs at nice 10, keeps the tree copy and logs
under `~/tmp/calliope-mutants`, and copies the lists here.

Score is caught / (caught + missed). Timeouts and unviable mutants are left out
of it: a timeout is a mutant that hung the tests, an unviable one does not
compile.

## Results

| Scope | Mutants | Caught | Missed | Timeout | Unviable | Score | Wall |
| :--- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
