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
lists here (`MUTANTS_RESULTS` names another directory). Manual runs share
`~/tmp/calliope-mutants.lock` and the scheduled run takes it exclusively, so
neither starts while the other holds it.

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
| g2g-core (caps, solver, clock, memory, frame, channel, runner) | 1438 | 1011 | 119 | 27 | 281 | 89% | 1h17m |
| g2g-plugins aacparse.rs | 253 | 168 | 60 | 1 | 24 | 74% | 9m |
| g2g-plugins av1parse.rs | 276 | 246 | 3 | 1 | 26 | 99% | 15m |
| g2g-plugins flv.rs | 217 | 159 | 21 | 0 | 37 | 88% | 9m |
| g2g-plugins h264parse.rs | 70 | 64 | 0 | 0 | 6 | 100% | 6m |
| g2g-plugins h265parse.rs | 215 | 200 | 7 | 0 | 8 | 97% | 12m |
| g2g-plugins hls.rs | 134 | 109 | 7 | 1 | 17 | 94% | 6m |
| g2g-plugins matroska.rs | 804 | 574 | 85 | 6 | 139 | 87% | 31m |
| g2g-plugins mp4demuxn.rs | 114 | 57 | 24 | 6 | 27 | 70% | 8m |
| g2g-plugins mpegts.rs | 1175 | 884 | 174 | 16 | 101 | 84% | 51m |
| g2g-plugins ogg.rs | 361 | 253 | 61 | 5 | 42 | 81% | 17m |
| g2g-plugins opusparse.rs | 101 | 70 | 12 | 0 | 19 | 85% | 5m |
| g2g-plugins rtcp.rs | 141 | 95 | 20 | 3 | 23 | 83% | 5m |
| g2g-plugins rtpdepay.rs | 53 | 42 | 10 | 0 | 1 | 81% | 3m |
| g2g-plugins st2110sdp.rs | 58 | 35 | 1 | 0 | 22 | 97% | 3m |
| g2g-plugins subparse.rs | 697 | 534 | 87 | 19 | 57 | 86% | 28m |
| g2g-plugins typefind.rs | 134 | 91 | 25 | 1 | 17 | 78% | 7m |
| g2g-plugins vp8parse.rs | 50 | 34 | 9 | 0 | 7 | 79% | 3m |
| g2g-plugins vp9parse.rs | 40 | 28 | 5 | 0 | 7 | 85% | 3m |

The g2g-core row is g2g 0825085, after M1530 and M1531 added tests for 245 of
the 351 mutants missed at 439f4e3. The seven files now yield 1438 mutants. Of the
119 missed, 31 are in test helper code and 31 in `Debug` output, solver labels
and the run report. The other 57 are not triaged yet: mostly solver preference
and index arithmetic, plus the `SourceLoop` trait defaults, the ProRes fourcc
and alpha plane lookups, `SystemSlice::make_shared` on a shared slice and two
channel flags. `i *= 1` in the `DomainSet::ALL` const block makes rustc loop in
const evaluation, so that mutant was killed after an hour and counts as unviable.

The av1parse, h264parse and h265parse rows are g2g c073b11, after M1620 to M1625.
Their scopes now compile the `ffmpeg` feature for h264parse, whose reorder depth
code it gates, and run `m1555_hevc_leading_rasl` for h265parse. The 10 still
missed are a 2-byte SPS NAL, which parses to nothing either way, a deltaRps of 0,
which the syntax cannot code, a 17th picture in a loop over at most 16, rejecting
16 pictures a side, which the spec already forbids, the element description text,
and the 16 MiB temporal unit cap, which the tests use by name.

## Scheduled runs

`deploy/mutants/g2g-mutants.timer` runs `tools/mutants-scheduled.sh` on hercules
daily at noon, or at the next boot if the box was off. It fetches g2g master over
https, runs cargo-mutants `--in-diff` on the commits since
`~/g2g-mutants/last-finished-commit` for g2g-core (the scope's features and tests)
and g2g-plugins (`--lib` plus the test files naming a changed module), writes
`~/g2g-mutants/runs/<date>-<from>-<to>/`, and moves the commit file only when
both runs finish. It waits up to 8 hours for 20 minutes of uptime, no active
`glass2glass-validation@` unit and a 5 minute load average of at most 4.

```bash
ssh hercules 'mkdir -p ~/g2g-mutants ~/.config/systemd/user'
scp tools/mutants-scheduled.sh tools/mutants-g2g.sh hercules:g2g-mutants/
scp deploy/mutants/g2g-mutants.{service,timer} hercules:.config/systemd/user/
ssh hercules 'git ls-remote https://github.com/Glass2GlassHQ/glass2glass.git master | cut -f1 > ~/g2g-mutants/last-finished-commit'
ssh hercules 'systemctl --user daemon-reload && systemctl --user enable --now g2g-mutants.timer'
```

The timer needs `loginctl enable-linger`. `systemctl --user start --no-block
g2g-mutants.service` runs it now, `journalctl --user -u g2g-mutants` shows the log.
