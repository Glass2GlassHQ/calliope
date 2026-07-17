# calliope

Differential QA harness for media pipelines. Runs the same content through
multiple engines (ffmpeg, GStreamer, [glass2glass](https://github.com/Glass2GlassHQ/glass2glass))
as black-box subprocesses and asserts their outputs are bit-exact, so a
divergence is a real bug in one of them.

Scenario modes:
- **differential**: decode and compare per-frame MD5 (ffmpeg's framemd5
  format) against a reference engine; a divergence is a real bug.
- **golden**: decode a conformance vector and assert every engine's whole
  decoded output matches the vector's official MD5 (`decoded-md5`), reproducing
  the Fluster oracle. No reference engine, an absolute correctness check.
- **robustness**: corrupt the input (`[fault]`: bit-flip, truncate, byte-drop)
  and require every engine to degrade gracefully (clean exit or error), never
  crash or hang. Targets parser / demuxer hardening against malformed input.
- **soak**: repeat a run (`[soak]`) and fail on any crash or hang.

All modes track crash/signal/timeout status and peak RSS. Engine-neutral by
construction: engine knowledge lives only in `calliope-adapter-*` crates.

## Layout

| Crate | Role |
| :--- | :--- |
| `calliope-core` | scenarios, corpus fetcher, subprocess runner, frame hashing, comparison |
| `calliope-adapter-ffmpeg` | ffmpeg adapter (native framemd5, usual reference engine) |
| `calliope-adapter-gst` | GStreamer adapter (`gst-launch-1.0`, raw dump hashed by the runner) |
| `calliope-adapter-g2g` | glass2glass adapter (`g2g-launch`, raw dump hashed by the runner) |
| `calliope-cli` | the `calliope` binary |

## Use

```sh
tools/gen-local-corpus.sh        # small generated vectors, needs ffmpeg
cargo run -p calliope-cli -- engines
cargo run -p calliope-cli -- run scenarios/h264-decode-smoke.toml --report report.json
```

Non-zero exit on any divergence, crash, or timeout. `--engines ffmpeg,gstreamer`
restricts a run to installed engines (the scenario's reference must stay).

When a robustness run finds a crash or hang, shrink the offending input to a
minimal reproducer (ddmin delta-debugging) for the affected engine:

```sh
calliope minimize --engine g2g --input runs/<scenario>/input.corrupted
# -> writes input.min, the smallest byte sequence that still crashes/hangs g2g
```

Engine binaries resolve from PATH; override with `CALLIOPE_FFMPEG`,
`CALLIOPE_GST_LAUNCH`, `CALLIOPE_G2G_LAUNCH`. Build `g2g-launch` with the codec
features you want to exercise and point the env var at a stable copy, not
`target/debug` (a background `cargo`/rust-analyzer rebuild can overwrite it with
a different feature set):

```sh
cargo build -p g2g-plugins --features ffmpeg --bin g2g-launch
cp target/debug/g2g-launch ~/.local/bin/g2g-launch-ffmpeg
export CALLIOPE_G2G_LAUNCH=~/.local/bin/g2g-launch-ffmpeg
```

## Scenarios

One TOML per scenario, see `scenarios/`. Input is a local `path` or a `corpus`
vector id from `corpus/vectors.toml`; vectors download on demand into
`~/.cache/calliope` (override: `CALLIOPE_CACHE`) and verify by sha256.

A differential scenario needs decoded geometry to chunk the raw-dump engines.
Give it explicitly as `[video]`, or omit it and calliope probes the input with
`ffprobe` (`CALLIOPE_FFPROBE` overrides). Supported decoded formats are 8-bit
planar `yuv420p` / `yuv422p` / `yuv444p`; the raw-dump engines convert to the
probed format as an identity so the comparison stays bit-exact (a 10-bit or
packed source is rejected with a clear message, use an explicit `[video]` or a
robustness/soak scenario). A
`[soak]` scenario repeats the run `iterations` times and passes only if no
iteration crashes or hangs (catches intermittent failures; each iteration is a
fresh process, so this is a stability probe, not a memory-leak endurance test).
A robustness scenario declares `[fault]` instead and needs no geometry:

```toml
[fault]
mode = "bit-flip"   # or truncate | byte-drop
seed = 1            # reproducible corruption
count = 500         # bit-flip / byte-drop operations
keep-percent = 50   # truncate: front fraction kept
```

The corrupted input is generated once and fed to every engine identically.

A `golden = true` scenario needs a `corpus` input; the vector's `decoded-md5`
and `output-format` (populated by `calliope corpus-import --fluster`) are the
oracle. Each engine decodes the whole stream to that format and its output MD5
must equal `decoded-md5`. Run the imported Fluster corpus this way to check a
decoder against official conformance output without a reference engine:

```sh
calliope corpus-import --fluster /path/to/fluster/test_suites
calliope run scenarios/jvt-golden.toml   # golden = true, input.corpus = "fluster/..."
```

Timing and RSS are recorded per run for within-engine regression tracking;
they are never compared across engines (different buffering models make that
meaningless).

## CI vs local

CI runs the file-corpus subset with ffmpeg + GStreamer. g2g, GPU, and
live-stream scenarios run on a dev host with the same harness.
