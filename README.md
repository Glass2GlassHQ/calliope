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
- **resolution-change**: decode a stream whose frame geometry changes
  mid-playback (`resolution-change = true`) and require each engine to survive
  (no crash / hang) and emit the expected total decoded bytes (the per-frame
  size sequence from ffprobe). Targets the engine's own caps / buffer
  renegotiation, not the codec core. No pixel oracle: ffmpeg's CLI normalizes
  output geometry across a change, so it can't reference the pixels; the byte
  total catches crashes, dropped frames, and frozen / wrong resolution.
- **produce**: each engine encodes or remuxes the input into a container
  (`[produce]`) and external validators judge what it wrote: ffprobe, MP4Box,
  mkvalidator, TSDuck, the DASH-IF conformance tool, Apple's
  mediastreamvalidator and a headless browser. ffmpeg's output is the control,
  so a validator failure it shares is advisory.

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

To see where g2g and GStreamer build a different pipeline from the same
`gst-launch` line, diff one line across both engines:

```sh
calliope gst-parity "videotestsrc num-buffers=30 ! videoconvert ! filesink location=out.raw"
```

It reports the negotiated caps per link, the element graph each engine ended up
with after auto-plugging, and (when the line ends in a `filesink location=`) the
MD5 of each engine's output file. GStreamer is read from its
`GST_DEBUG_DUMP_DOT_DIR` graph dump written on the way out of PLAYING; g2g from
`g2g-launch --run-json`, which runs the line and reports the caps each link
carried. Both need a `g2g-launch` built with the `tooling-json` feature. A
g2g-launch too old for `--run-json` falls back to `--validate-json`, the solve
before the run: then a demuxed stream shows g2g's placeholder geometry against
gst's real one.

The two engines name elements after different things (gst after the factory,
`h264parse0`; g2g after the Rust type, `NalParse0`), so the element pairing is
fed the synonym table `g2g-inspect --gst-map` prints. Point
`CALLIOPE_G2G_INSPECT` at a `g2g-inspect` from the same build as `g2g-launch`;
without one, the elements the two spell differently stay unpaired and their
links compare nothing.

The equivalence judgment is approximate: element naming, a different auto-plug
chain, and caps fields only one engine models are reported as informational, and
only a caps conflict on a link both engines built (or a differing artifact, or a
failed run) exits non-zero.

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
`CALLIOPE_GST_LAUNCH`, `CALLIOPE_G2G_LAUNCH`, `CALLIOPE_G2G_INSPECT`. Build
`g2g-launch` with the codec
features you want to exercise and point the env var at a stable copy, not
`target/debug` (a background `cargo`/rust-analyzer rebuild can overwrite it with
a different feature set):

```sh
cargo build -p g2g-plugins --features ffmpeg --bin g2g-launch
cp target/debug/g2g-launch ~/.local/bin/g2g-launch-ffmpeg
export CALLIOPE_G2G_LAUNCH=~/.local/bin/g2g-launch-ffmpeg
```

### Produce scenarios and browser playback

The produce scenarios (`scenarios/*-produce.toml`) need their validators
installed. A validator missing from the host is reported as skipped, not as a
failure. ffprobe comes with ffmpeg. MP4Box runs from the GPAC image through a
wrapper. mkvalidator has no package, so a script builds it from source. TSDuck
is unpacked from its release RPM without root. The DASH-IF conformance tool
publishes no image, so a script builds one from its Dockerfile. The browser
check needs Playwright's Chromium, and the WHEP case needs mediamtx running:

```sh
podman pull docker.io/gpac/ubuntu:latest     # MP4Box
tools/validators/build-mkvalidator.sh        # -> ~/.local/bin/mkvalidator
mkdir -p ~/opt/tsduck && cd ~/opt/tsduck \
  && curl -LO https://github.com/tsduck/tsduck/releases/download/v3.44-4676/tsduck-3.44-4676.fc43.x86_64.rpm \
  && rpm2cpio tsduck-3.44-4676.fc43.x86_64.rpm | cpio -idm && cd -
tools/validators/build-dashif.sh             # -> localhost/calliope-dashif
(cd tools/playback && pnpm install && pnpm exec playwright-core install chromium)
tools/validators/mediamtx.sh                 # WHIP in, WHEP out on :8889
```

Run them from the repo root, where the DASH-IF wrapper and the playback harness
resolve:

```sh
export CALLIOPE_MP4BOX=$PWD/tools/validators/mp4box.sh CALLIOPE_TSP=$PWD/tools/validators/tsp.sh
calliope run scenarios/*-produce.toml --report produce.json
```

The playback check also runs on its own and prints one JSON line with the
frames rendered and any player error:

```sh
node tools/playback/play.mjs hls runs/hls-ts-produce/g2g/index.m3u8   # or dash, webcodecs, whep
```

It takes the newest Chromium under `~/.cache/ms-playwright`, `CALLIOPE_CHROMIUM`
names another. `TSDUCK_ROOT`, `CALLIOPE_CONTAINER_RUNTIME`, `CALLIOPE_GPAC_IMAGE`
and `CALLIOPE_DASHIF_IMAGE` override the wrappers' defaults. mediastreamvalidator
ships only with Apple's macOS HLS tools, so it is skipped elsewhere.

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
an explicit `[video]` or a robustness/soak scenario).

An `[audio]` scenario decodes to normalized interleaved PCM instead of video and
hashes the whole stream (audio frame boundaries differ across decoders, so
per-frame md5 is wrong). `[audio]` and `[video]` are mutually exclusive.

```toml
[audio]
codec = "opus"    # opus | aac
rate = 48000      # target sample rate (default 48000)
channels = 1      # default 1
format = "s16le"  # normalized sample format (default; or f32le)
```

Only Opus is bit-exact across decoders, so it is the sole codec eligible for the
cross-engine differential (ffmpeg is pinned to its `libopus` decoder to match
gstreamer's `opusdec` and g2g's `OpusDec`). AAC is not bit-exact, so an AAC
`[audio]` scenario must use `[determinism]` (self-comparison); a cross-engine AAC
differential is rejected at load. Audio wires into the differential and
determinism modes only, not the video-geometry modes (encode / roundtrip /
resolution-change). A `[soak]` scenario repeats the run `iterations` times and passes only if no
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

A `resolution-change = true` scenario needs only an `input` (a stream whose
geometry switches mid-playback, e.g. the concatenated `res-change-*` vectors
from `tools/gen-local-corpus.sh`); no `[video]`, the per-frame sizes are probed.
Run it with the engine under test only (`engines = ["g2g"]`), since ffmpeg's
normalized output would false-fail the byte-total check.

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

To run real-world samples through every engine, import a sample tree and sweep
it. The ffmpeg fate-suite and the gst-integration-testsuites medias are the two
supported trees. Fetch them into the cache so the sweep reads them in place:

```sh
rsync -vrltLW rsync://fate-suite.ffmpeg.org/fate-suite/ ~/.cache/calliope/fate-suite/
GIT_LFS_SKIP_SMUDGE=1 git clone --depth 1 \
  https://gitlab.freedesktop.org/gstreamer/gst-integration-testsuites.git ~/.cache/calliope/gst-medias
git -C ~/.cache/calliope/gst-medias lfs pull
calliope corpus-import --fate ~/.cache/calliope/fate-suite --out corpus/fate-suite.toml
calliope corpus-import --gst-medias ~/.cache/calliope/gst-medias --out corpus/gst-medias.toml
calliope sweep --corpus corpus/fate-suite.toml --title fate-suite --report runs/fate-suite.json
python3 -I tools/sweep-triage.py runs/fate-suite.json corpus/fate-suite.toml fate-suite \
  "master <rev>" triage/fate-suite.md
```

The importer keeps every file ffprobe finds a video or audio stream in and records
its container, codecs, pixel format, and sha256. The sweep runs one scenario per
stream with ffmpeg as the reference. Planar video and the bit-exact audio codecs
get the per-frame / whole-stream differential. Every other stream gets a
determinism run. When ffmpeg rejects a file, the decode outcome is the oracle. The
triage table groups the results as agree, g2g diverges, g2g no output, g2g crash /
hang, gstreamer diverges, unsupported codec, or reference quirk, and gives a
`g2g-launch` reproducer for each g2g divergence. `--limit N` sweeps the first N
vectors. Decoded output is deleted after each batch, only the logs stay.
`tools/sweep-triage.py` turns the report into the committed `triage/<tree>.md`,
adding a diagnosis per row and keeping the hand-written `## g2g bugs` section
already in that file.

g2g picks its decoder through `decodebin`. To check one decoder backend, pin it
with `G2G_PLUGIN_FEATURE_RANK`, which g2g-launch inherits:

```sh
G2G_PLUGIN_FEATURE_RANK=vulkanvideodec:max calliope conformance --corpus corpus/vectors.toml
```

A pin to a decoder the g2g-launch build lacks panics instead of testing another
one. With a pin set, the adapter adds a `videoconvert` before the capsfilter,
because the GPU decoders emit NV12 or P010.

Timing and RSS are recorded per run for within-engine regression tracking;
they are never compared across engines (different buffering models make that
meaningless).

## Canary

`calliope canary --config deploy/canary/canary.toml` keeps every `[[pipeline]]`
in the config running as a `g2g-launch --observe` subprocess. It restarts a run
that exits or decodes no frame for `stall-secs`, with exponential backoff. Once
per `sample-secs` it appends one line per pipeline to
`log/samples-<utc-date>.jsonl` with frames decoded, fps, decoder p50 latency, RSS
and restart count. Each failure opens an incident in
`log/incidents-<utc-date>.jsonl` with the stderr tail under `log/stderr/`, and the
incident closes when frames flow again. The status page is
`status/index.html` plus `status/status.json`, rewritten every sample.

The `self-hosted` tier decodes a looped clip that mediamtx and an ffmpeg feeder
serve on the same host over RTSP, HLS and SRT. A failure there is g2g's. The
`public` tier pulls third-party HLS and RTSP streams with no uptime promise.
There is no public SRT entry because g2g's `srtsrc` only listens.

### Deploy

The image bundles `calliope` and a `g2g-launch` built from the g2g checkout's
committed `HEAD`. Three podman Quadlet units run it under the user's systemd:
`g2g-canary-mediamtx`, `g2g-canary-feeder` and `g2g-canary`. A
`canary-publish.timer` copies the status page every 5 minutes.

```sh
tools/canary-image.sh ~/src/glass2glass    # builds localhost/g2g-canary:latest
tools/canary-deploy.sh local               # or <ssh-host>
```

`canary-deploy.sh` copies `deploy/canary/*.container` to
`~/.config/containers/systemd/`, the publish units to `~/.config/systemd/user/`,
and the config, `mediamtx.yml`, `feeder.sh` and `canary-publish.sh` to
`~/g2g-canary/`. Logs and the status page land in `~/g2g-canary/data/canary-out/`.
The publish timer starts only when `~/g2g-canary/publish.env` exists, see
`deploy/canary/publish.env.example`. Its destination is an `s3://` url or a local
directory.

### EC2

The canary runs on a t4g.small (2 Graviton vCPU, 2 GB) with the Fedora Cloud 43
aarch64 AMI from fedoraproject.org/cloud/download, default user `fedora`, a 20 GB
gp3 root volume, and a security group that allows inbound ssh only. Every
pipeline pulls outbound and mediamtx binds loopback.

The image is built here for arm64 under qemu and copied over ssh with
`podman save | ssh podman load`, so no registry is involved. The emulated g2g
build is slow, expect hours.

```sh
sudo dnf install qemu-user-static-aarch64
CANARY_PLATFORM=linux/arm64 CANARY_IMAGE=localhost/g2g-canary:arm64 \
    tools/canary-image.sh ~/src/glass2glass
ssh fedora@<host> 'sudo dnf -y install podman awscli2 && sudo loginctl enable-linger fedora'
CANARY_IMAGE=localhost/g2g-canary:arm64 tools/canary-deploy.sh fedora@<host>
```

Linger starts the user units at boot without a login. The deploy script tags the
loaded image `localhost/g2g-canary:latest` on the instance, which is the tag
`g2g-canary.container` runs.

The status page goes to an S3 bucket and the g2g docs site links to it. Create a
bucket, turn off "block public access" for it, and add a bucket policy that
allows public `s3:GetObject` on `g2g-canary/*`. Give the instance an IAM role
that allows `s3:ListBucket` on the bucket and `s3:PutObject` and
`s3:DeleteObject` on `g2g-canary/*`. Then enable the timer on the instance:

```sh
ssh fedora@<host> 'echo CANARY_PUBLISH_DESTINATION=s3://<bucket>/g2g-canary > g2g-canary/publish.env \
    && systemctl --user enable --now canary-publish.timer'
```

The page is then at `https://<bucket>.s3.<region>.amazonaws.com/g2g-canary/index.html`.
Add that url as a link in `docs/index.html` in the g2g repo, which GitHub Pages
serves. Pushing the page into the g2g repo instead would make a commit every
5 minutes.

## CI vs local

CI runs the file-corpus subset with ffmpeg + GStreamer. g2g, GPU, and
live-stream scenarios run on a dev host with the same harness.

## Test campaign

[`TESTING.md`](TESTING.md) is the system-of-record for everything thrown at g2g:
the scenario modes above plus the ASan build + fuzz loop (`tools/build-g2g-asan.sh`,
`tools/fuzz-g2g.sh`), the leak + data-race passes (`tools/lsan-g2g.sh`,
`tools/tsan-g2g.sh`), the cargo-fuzz targets (in the g2g repo), Miri
(`tools/miri-g2g.sh`), and loom (`tools/loom-g2g.sh`), each with how to reproduce
it and what it found.
