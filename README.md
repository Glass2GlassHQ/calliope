# calliope

Differential QA harness for media pipelines. Runs the same content through
multiple engines (ffmpeg, GStreamer, [glass2glass](https://github.com/Glass2GlassHQ/glass2glass))
as black-box subprocesses and asserts their outputs are bit-exact, so a
divergence is a real bug in one of them.

Two scenario modes:
- **differential**: decode and compare per-frame MD5 (ffmpeg's framemd5
  format) against a reference engine; a divergence is a real bug.
- **robustness**: corrupt the input (`[fault]`: bit-flip, truncate, byte-drop)
  and require every engine to degrade gracefully (clean exit or error), never
  crash or hang. Targets parser / demuxer hardening against malformed input.

Both track crash/signal/timeout status and peak RSS. Engine-neutral by
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

Engine binaries resolve from PATH; override with `CALLIOPE_FFMPEG`,
`CALLIOPE_GST_LAUNCH`, `CALLIOPE_G2G_LAUNCH`. `g2g-launch` comes from a
glass2glass build: `cargo build -p g2g-plugins --features std --bin g2g-launch`.

## Scenarios

One TOML per scenario, see `scenarios/`. Input is a local `path` or a `corpus`
vector id from `corpus/vectors.toml`; vectors download on demand into
`~/.cache/calliope` (override: `CALLIOPE_CACHE`) and verify by sha256.

A differential scenario declares `[video]` geometry (raw-dump engines are
hashed by chunking; wrong geometry fails loudly as a trailing partial frame). A
robustness scenario declares `[fault]` instead and needs no geometry:

```toml
[fault]
mode = "bit-flip"   # or truncate | byte-drop
seed = 1            # reproducible corruption
count = 500         # bit-flip / byte-drop operations
keep-percent = 50   # truncate: front fraction kept
```

The corrupted input is generated once and fed to every engine identically.

Timing and RSS are recorded per run for within-engine regression tracking;
they are never compared across engines (different buffering models make that
meaningless).

## CI vs local

CI runs the file-corpus subset with ffmpeg + GStreamer. g2g, GPU, and
live-stream scenarios run on a dev host with the same harness.
