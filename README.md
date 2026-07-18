# calliope

Differential QA harness for media pipelines. Runs the same content through
multiple engines (ffmpeg, GStreamer, [glass2glass](https://github.com/Glass2GlassHQ/glass2glass))
as black-box subprocesses and asserts their outputs are bit-exact, so a
divergence is a real bug in one of them.

Scenario modes:
- **differential**: decode and compare per-frame MD5 (ffmpeg's framemd5
  format) against a reference engine; a divergence is a real bug. With three or
  more engines, a majority vote names the outlier when they diverge (even if
  that outlier is the reference), so a reference quirk cannot mask a real bug.
- **golden**: decode a conformance vector and assert every engine's whole
  decoded output matches the vector's official MD5 (`decoded-md5`), reproducing
  the Fluster oracle. No reference engine, an absolute correctness check.
- **robustness**: corrupt the input (`[fault]`: bit-flip, truncate, byte-drop,
  nal-payload) and require every engine to degrade gracefully (clean exit or
  error), never crash or hang. Targets parser / demuxer hardening against
  malformed input; nal-payload drives corruption past the framer into decode.
- **soak**: repeat a run (`[soak]`) and fail on any crash or hang.
- **determinism**: run each engine repeatedly (`[determinism]`) and require
  byte-identical output every time. No reference engine, a self-comparison that
  isolates nondeterminism. `threads = true` also runs g2g's `--threads` variant
  (needs a multi-thread build; skipped otherwise) and requires it to match.
- **roundtrip**: the engine transcodes the input (`[roundtrip]`: decode ->
  re-encode with a named encoder); ffmpeg then decodes that bitstream and
  PSNR-compares it to the reference decode of the original. Exercises the
  encoder (undecodable output, crashes, gross corruption), which the decode-only
  modes never touch. A blunt smoke test: lossy encode has no bit-exact oracle,
  so it catches catastrophic encoder failure, not subtle quality regressions.
- **encode**: ffmpeg encodes a synthetic lavfi source (`[encode]`) into an
  elementary stream, which then feeds the differential decode compare. ffmpeg
  goes forward (encode), the other engines go reverse (decode), and the frames
  are compared bit-exact. Feeds decoders feature combos the conformance corpus
  never produced (profiles / flags chosen via `args`), so a divergence is a real
  decoder bug against a hard oracle.

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

To hunt g2g decoder crashes at volume, build g2g under AddressSanitizer and fuzz
it. ASan intercepts malloc process-wide, so a heap overflow / use-after-free in
g2g's own code or the system libav it calls aborts loudly instead of corrupting
silently; the fuzz loop feeds seeded corruption to real streams and minimizes any
crash into `fuzz-out/`:

```sh
tools/build-g2g-asan.sh                 # -> ~/.local/bin/g2g-launch-asan
CALLIOPE_G2G_LAUNCH=~/.local/bin/g2g-launch-asan tools/fuzz-g2g.sh
# FUZZ_SEEDS / FUZZ_MODE / FUZZ_COUNT tune it; add dav1d,mjpeg via G2G_FEATURES
```

Running the normal differential / golden scenarios against the ASan binary also
catches memory bugs on well-formed input, not just corrupted streams.

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
`ffprobe` (`CALLIOPE_FFPROBE` overrides). Supported decoded formats are the
planar `i420` / `i422` / `i444` family at 8-, 10-, and 12-bit
(`yuv4xxp[10|12]le`) plus semi-planar `nv12`; the raw-dump engines convert to
the probed format as an identity so the comparison stays bit-exact. Packed RGB
/ YUYV is matrix- or order-dependent across engines and stays unsupported (use
an explicit `[video]` or a robustness/soak scenario). A
`[soak]` scenario repeats the run `iterations` times and passes only if no
iteration crashes or hangs (catches intermittent failures; each iteration is a
fresh process, so this is a stability probe, not a memory-leak endurance test).
A `[determinism]` scenario repeats each engine `runs` times and passes only if
its output is byte-identical every time (`threads = true` also checks g2g's
`--threads` variant, which needs a multi-thread build). A robustness scenario
declares `[fault]` instead and needs no geometry:

```toml
[fault]
mode = "bit-flip"   # or truncate | byte-drop | nal-payload
seed = 1            # reproducible corruption
count = 500         # bit-flip / byte-drop / nal-payload operations
keep-percent = 50   # truncate: front fraction kept
```

The corrupted input is generated once and fed to every engine identically.

An `[encode]` scenario has no `[input]`: ffmpeg generates the differential input
by encoding a lavfi source, then every engine decodes it and the frames are
compared bit-exact. `[video]` must match the source geometry.

```toml
[encode]
source = "testsrc2=size=352x288:rate=30:duration=2"   # ffmpeg lavfi source
encoder = "libx264"
args = ["-profile:v", "high", "-x264-params", "cabac=1:bframes=3"]
output-ext = "h264"                                   # elementary stream type
```

A `[roundtrip]` scenario names the engine's `encoder` (e.g. `x264enc`) and a
`psnr-min` floor; the engine decodes + re-encodes the input, ffmpeg decodes the
result and PSNR-compares it to the reference decode.

A `golden = true` scenario needs a `corpus` input; the vector's `decoded-md5`
and `output-format` (populated by `calliope corpus-import --fluster`) are the
oracle. Each engine decodes the whole stream to that format and its output MD5
must equal `decoded-md5`. Run the imported Fluster corpus this way to check a
decoder against official conformance output without a reference engine:

```sh
calliope corpus-import --fluster /path/to/fluster/test_suites
calliope run scenarios/jvt-golden.toml   # golden = true, input.corpus = "fluster/..."
```

To golden-check an entire imported suite in one pass (a conformance run), skip
the per-vector scenarios:

```sh
calliope conformance --corpus corpus/vectors.toml --limit 20
# fetches each vector, decodes on every engine, compares to its official MD5;
# prints N/total passed and exits non-zero on any mismatch.
```

Timing and RSS are recorded per run for within-engine regression tracking;
they are never compared across engines (different buffering models make that
meaningless).

## CI vs local

CI runs the file-corpus subset with ffmpeg + GStreamer. g2g, GPU, and
live-stream scenarios run on a dev host with the same harness.
