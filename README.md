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
python3 -I tools/sweep-triage.py runs/fate-suite.json fate-suite \
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

More modes sweep the same corpus. `--mode timestamps` decodes each stream
into `checksumsink` on g2g and GStreamer and compares g2g's per-frame pts with
ffprobe's (TESTING.md technique 19). `--mode chunk-size` decodes each stream on
g2g with `filesrc blocksize=` 65536, 4096 and 188 and requires the same output
at every size (technique 20). `--mode tracks` decodes every stream of a file
with more than one video or audio stream by index, `decodebin current-video=N`
against `ffmpeg -map 0:v:N` (technique 21). `--mode seek` seeks to three
keyframes per video stream and compares the frames after each with `ffmpeg -ss`
(technique 22). `--mode remux` copies each stream into mp4, mkv, ts and flv with
`parsebin` and requires g2g to decode the copy as it decodes the original
(technique 23). Seek and remux take `--agreeing <decode report>` to keep only
the streams a decode sweep found in agreement. Give each mode its own
`--workdir`, since the run directories are named by scenario:

```sh
calliope sweep --mode timestamps --corpus corpus/fate-suite.toml --workdir runs/timestamps \
  --title "fate-suite timestamps" --report runs/fate-suite-timestamps.json --triage runs/fate-suite-timestamps.md
calliope sweep --mode chunk-size --corpus corpus/fate-suite.toml --workdir runs/chunk-size \
  --title "fate-suite chunk-size" --report runs/fate-suite-chunk-size.json --triage runs/fate-suite-chunk-size.md
calliope sweep --corpus corpus/fate-suite.toml --workdir runs/decode --report runs/fate-suite.json
calliope sweep --mode seek --agreeing runs/fate-suite.json --corpus corpus/fate-suite.toml --workdir runs/seek \
  --title "fate-suite seek" --report runs/fate-suite-seek.json
```

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

### Nightly GPU gate

`tools/conformance-nightly.sh` runs on hercules (RTX 2080 Ti) every evening. It
builds g2g master and runs the JVT-AVC_V1, JCT-VC-HEVC_V1 and JCT-VC-RExt
vectors through `nvdec` and `vulkanvideodec`, and the VP9 vectors through
`nvdec`. It then sorts each vector into
pass, refused, silent (exit 0 with wrong or empty output), crash, timeout or
device-lost, and compares the result with `conformance/rtx2080ti.json`.
Vectors whose outcome differs from the baseline are rerun once. A vector that
still lands on a failing outcome it did not have in the baseline turns the run
red, and so does a g2g build failure. A red run opens or comments on a GitHub
issue titled "conformance gate red on hercules", and the next green run closes
it. Runs are kept for 14 days in `~/g2g-conformance/runs`. Each run that
finishes publishes its per-vector page to
https://g2g-canary-000152811496.s3.us-east-1.amazonaws.com/g2g-conformance/index.html
as the `g2g-conformance` IAM user, which can only write that prefix.

Install it on the GPU host once, after putting an access key for the
`g2g-conformance` IAM user in `~/.aws/credentials` under the `g2g-conformance`
profile:

```sh
git clone -q https://github.com/Glass2GlassHQ/calliope.git ~/g2g-conformance/calliope
cp ~/g2g-conformance/calliope/deploy/conformance/g2g-conformance.* ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now g2g-conformance.timer
```

To accept a change in outcomes, copy the run's table over the baseline and
commit it:

```sh
scp hercules:g2g-conformance/latest/table.json conformance/rtx2080ti.json
```

## Canary

`calliope canary --config deploy/canary/canary.toml` keeps every `[[pipeline]]`
in the config running as a `g2g-launch --observe` subprocess. It restarts a run
that exits or decodes no frame for `stall-secs`, with exponential backoff. Once
per `sample-secs` it appends one line per pipeline to
`log/samples-<utc-date>.jsonl` with frames decoded, fps, decoder p50 latency, RSS,
restart count and the frame checks below. Each failure opens an incident in
`log/incidents-<utc-date>.jsonl` with the stderr tail under `log/stderr/`, and the
incident closes when frames flow again. For `startup-grace-secs` (default 120)
after the canary starts, a failing run restarts without opening an incident,
because mediamtx, the feeder and the egress publishers start alongside it. A run
still failing after that opens one. The status page is
`status/index.html` plus `status/status.json`, rewritten every sample.

The `self-hosted` tier decodes a looped clip that mediamtx and an ffmpeg feeder
serve on the same host over RTSP, HLS, SRT and WebRTC (WHEP). A failure there
is g2g's. The clip has no B-frames because mediamtx closes a WebRTC session on
H.264 with B-frames. The `public` tier pulls third-party HLS and RTSP streams
with no uptime promise. There is no public SRT entry because g2g's `srtsrc` only
listens. The `faulted` tier runs self-hosted streams through injected network
faults. The status page shows each tier as its own table, so faulted uptime never
counts toward the self-hosted numbers.

The canary refuses to start unless `G2G_COMMIT` names the g2g commit its
`g2g-launch` was built from. The commit is `g2g_commit` in `status.json` and is
shown on the page. Each pipeline entry carries its `launch` line: the page lists
them in a table and shows one on hover over the pipeline name, as the g2g docs
site does over its status rows.

### Frame checks

`checksumsink` posts each frame's pts and digest under its `name=`, and the
canary reads them from the `--observe` socket. Three optional `[[pipeline]]`
fields use them:

```toml
video-sink = "video"                           # the checksumsink name= on the video branch
audio-sink = "audio"                           # the checksumsink name= on the audio branch
expect-hashes = "/etc/canary/hashes/baseline.txt"  # reference table for video-sink
```

`expect-hashes` and `audio-sink` need `video-sink`. Every row ends in a
checksumsink with `video-sink`, so the timestamps check covers it, public rows
included. The exception is `opus-webrtc`, which decodes only audio and keeps
`fakesink`, because an audio sink needs a video sink. Every sample line then has:

- `content`, with `expect-hashes`: `matched` frames have a digest in the table,
  `mismatched` frames do not, and `skipped` counts the frames missing between
  two matched ones, by table index modulo the clip length. A mismatched frame
  takes the next index, so one corrupt frame counts once. Each run starts at
  whatever frame it joins on.
- `timestamps`, from every checksumsink: `backward` counts a pts below the same
  sink's previous one, `forward_jumps` a pts step more than 1 s longer than the
  wall clock time between the two frames arriving. A correct pipeline stays
  monotonic across the MPEG-TS 33-bit and RTP timestamp wraps.
- `av_drift_ms`, with `audio-sink`: the mean of arrival time minus pts for the
  audio sink minus the same mean for the video sink, over the sample. Positive
  means audio arrives later.
- `rss_growth_kib_per_hour`: the least squares slope of RSS over the last 24 h
  of one `g2g-launch` run, empty until a run has lasted 24 h.

Besides the `down` incidents for exits, signals, stalls and spawn failures,
three kinds open from the samples. `content` opens on a sample with mismatched
or skipped frames and closes on one with matched frames and neither.
`timestamps` opens on a backward or forward jump and closes on a sample with
neither. `memory-growth` opens when the RSS slope exceeds 512 KiB/h, about
2 GiB over six months, and closes when a whole day's slope is back under it.
Open incidents are kept in `log/open-incidents.json`, rewritten on every change
and reloaded at start, so an incident spans a canary restart with the same id.

#### Reference tables

A table is the stdout of decoding the clip the feeder loops, through the same
chain from the parser to `checksumsink` as the live pipeline, with the same
`hash=`:

```sh
g2g-launch -q filesrc location=<clip> ! <demuxer> ! <same decode chain> ! checksumsink > <table>
```

For the baseline clip and a live line ending in
`tsdemux ! h264parse ! avdec_h264 ! checksumsink name=video`:

```sh
g2g-launch -q filesrc location=/clip/baseline-no-bframes.mp4 ! qtdemux ! h264parse ! avdec_h264 ! checksumsink \
    > /etc/canary/hashes/baseline.txt
```

Each line is `<h:mm:ss.nnnnnnnnn> <lowercase hex digest>`, one per frame in
output order. The canary rejects a table with any other line, an empty table, or
digests of different lengths, so a `g2g-launch` run without `-q` fails at
startup instead of reporting every frame as mismatched. The clip must loop with
`-c copy` so the live frames are the table's frames.

### Feeds

`feeder.sh` encodes six 16 s clips: H.264 (`baseline`), H.265, AV1 and VP9 at
640x360 and 30 fps, and AAC and Opus tones. It encodes with ffmpeg's plain C code on
one thread, so every machine writes the same bytes, and checks the video clips
against the md5 list in the script. The image build runs the same script in the
same mediamtx image and decodes each video clip into a reference table under
`/etc/canary/hashes/`. The feeder publishes the clips to mediamtx over RTSP as
`baseline`, `hevc`, `av1`, `vp9-opus` and `h264-aac`, pushes `baseline` to
g2g's SRT listener, and sends it with ffmpeg's librist to g2g's RIST listener.

The egress rows encode a 320x240 `videotestsrc pattern=moving-bar` and publish it
into mediamtx over RTMP, WHIP, SRT and RTSP (`rtspclientsink`), or serve it over
RTSP for mediamtx to pull as the `egress-rtsp` path. A second row reads each one
back over RTSP or WHEP. The encode is lossless (`x264enc pass=quant quantizer=0`,
H.264 High 4:4:4 Predictive, about 45 kbit/s), and the bar moves 4 pixels a frame
and wraps, so the source repeats every 80 frames. The readers' `expect-hashes` is
`moving-bar.txt`, which the image build writes from the source's own 80 frames,
so every decoded frame must equal the frame g2g generated. The `ball` pattern
would not do: its bounce plays each loop forward and then backward, so a frame's
digest does not say where in the loop it is.

The DASH rows read `g2g-canary-livesim2`, DASH-IF's live simulator built from a
pinned release into the image. It loops its bundled `testpic_2s` asset as a live
stream on port 18900 of the host loopback, once with a new Period every minute.
The public livesim2.dashif.org allows 10000 requests per IP address per 24 hours,
which one live row spends in about 3 hours.

The `faulted` tier reads the baseline clip from `g2g-canary-faulted`, a
container with its own network namespace that runs a second mediamtx, a
gst-rtsp-server relay of the clean `baseline` path and a feeder. Netem on its
loopback runs the windows in `deploy/canary/fault-schedule.txt` on the
reader-facing ports and the ports g2g publishes to only: loss, jitter, reorder, burst loss, rate caps, link
outages and a mediamtx restart, each followed by a clean window. A rate cap is a
multiple of the traffic the netem band carried in the clean window before it.
Windows sit at fixed places in a cycle counted from the unix epoch, so the
container and the canary agree on them from the clock alone. Rootless podman
allows this: the container has `CAP_NET_ADMIN` in its own namespace, and pasta
publishes its RTSP, HLS, WHIP, WebRTC and SRT ports on the host loopback and forwards its SRT and
RIST sends to the host. Netem runs only inside that namespace, so the
self-hosted rows never see it.

Every faulted row needs `video-sink` and `expect-hashes`, or `checked-by` as
below. At the end of each clean window the canary judges each row's recovery from the fault before it:
the time from the fault's end to the first frame of an unbroken run of right,
on-time frames that lasts to the window's end. A frame is late when its arrival
minus its pts exceeds the smallest such delay in its run by more than 5 s, as
the frames a stalled TCP connection delivers in a burst do. It appends one line per row to
`recoveries-<date>.jsonl` (`recovery_secs` is null when the row did not
recover, and `av_drift_ms` covers the run on a row with `audio-sink`). A row
that did not recover is killed and restarted, which opens a `no-recovery`
incident that closes on its next recovery. Faulted rows are never killed for a
stall, and their wrong or skipped frames open no `content` incident. The status
page shows each row's slowest recovery per fault over 24 hours and 7 days.

The faulted egress rows put the fault on g2g's publishing side. `rtspclientsink`,
`whipsink` and `srtsink` publish the moving bar into the faulted mediamtx on
faulted ports: RTSP on 19554, WHIP on 19889 with WebRTC media on UDP 19189, and
SRT on UDP 19891. The faulted mediamtx's `runOnAvailable` starts an ffmpeg that
copies each `egress-faulted-*` path over its RTMP port, which netem leaves alone,
into the clean mediamtx. A reader row reads it there over RTSP and checks it
against `moving-bar.txt`, so only the publisher's leg is faulted. Each publisher
row names its reader in `checked-by`, and the canary refuses to start unless that
row exists, is faulted and has `expect-hashes`. The reader's recoveries are the
publisher's, plus the time ffmpeg and the reader take to reconnect. A publisher
row has no recovery line of its own, its exits open incidents as on any row, and
when its reader does not recover the canary restarts both, each with a
`no-recovery` incident.

### Deploy

The image bundles `calliope` and a `g2g-launch` built from one g2g commit, which
it reports in `G2G_COMMIT`. Five podman Quadlet units run under the user's
systemd: `g2g-canary-mediamtx`, `g2g-canary-feeder`, `g2g-canary-faulted`,
`g2g-canary-livesim2` and `g2g-canary`. The last three run
`ghcr.io/glass2glasshq/g2g-canary:latest`. A
`canary-publish.timer` copies the status page and sends incident alerts every 5
minutes.

The g2g repo's `canary image` workflow builds the image after each master commit
passes CI and pushes it as `:latest` and `:<short sha>`. On the host,
`podman-auto-update.timer` pulls `:latest` daily at midnight and restarts the
units whose image changed.

```sh
tools/canary-deploy.sh local               # or <ssh-host>
```

`canary-deploy.sh` pulls the image on the host and copies
`deploy/canary/*.container` to `~/.config/containers/systemd/`, the publish units
to `~/.config/systemd/user/`, and the config, `mediamtx.yml`, `feeder.sh`,
`canary-publish.sh` and `canary-alert.sh` to `~/g2g-canary/`. It then restarts
the units and enables `podman-auto-update.timer`. Logs and the status page land
in `~/g2g-canary/data/canary-out/`.

A unit that runs the canary image sets `Image=ghcr.io/glass2glasshq/g2g-canary:latest`
and `AutoUpdate=registry`, so one auto-update run restarts all of them on a new image.

To run an image built here instead of the registry's:

```sh
tools/canary-image.sh ~/src/glass2glass    # builds localhost/g2g-canary:latest
CANARY_IMAGE=localhost/g2g-canary:latest tools/canary-deploy.sh <ssh-host>
```

The deploy copies it with `podman save | ssh podman load` and tags it with the
ghcr name. It runs until the next auto-update, which replaces it with the
registry's `:latest` because the digests differ.

The publish timer starts only when `~/g2g-canary/publish.env` exists, see
`deploy/canary/publish.env.example`. `CANARY_PUBLISH_DESTINATION` is an `s3://`
url or a local directory. `CANARY_ALERT_TOPIC` is the SNS topic arn that
`canary-alert.sh` publishes to: one message when an incident opens and one when
it closes. It reads the incidents from `status.json`, because an open incident is
in no log file until it closes. It lists what it sent in `~/g2g-canary/alerts-sent`,
so a restart sends nothing twice, and its first run marks the closed history as sent.

### EC2

The canary runs on a c7a.xlarge (4 x86-64 vCPU, 8 GB) with the Fedora Cloud 43
x86_64 AMI from fedoraproject.org/cloud/download, default user `fedora`, a 20 GB
gp3 root volume, and a security group that allows inbound ssh only. Every
pipeline pulls outbound and mediamtx binds loopback.

```sh
ssh fedora@<host> 'sudo dnf -y install podman awscli2 jq && sudo loginctl enable-linger fedora'
tools/canary-deploy.sh fedora@<host>
```

With `CANARY_SECURITY_GROUP` set to the instance's security group id,
`canary-deploy.sh` first replaces the group's ssh rule with this machine's
current public address, so a changed home IP does not lock the deploy out. It
runs `aws` with whatever `AWS_PROFILE` the shell sets.

Linger starts the user units and timers at boot without a login. The image
package on ghcr.io is public, so the host pulls it without a login.

The status page goes to an S3 bucket and the g2g docs site links to it. Create a
bucket, turn off "block public access" for it, and add a bucket policy that
allows public `s3:GetObject` on `g2g-canary/*`. Give the instance an IAM role
that allows `s3:ListBucket` on the bucket, `s3:PutObject` and `s3:DeleteObject`
on `g2g-canary/*`, and `sns:Publish` on the alert topic. Then enable the timer on
the instance:

```sh
ssh fedora@<host> 'printf "%s\n" CANARY_PUBLISH_DESTINATION=s3://<bucket>/g2g-canary \
        CANARY_ALERT_TOPIC=arn:aws:sns:<region>:<account>:<topic> > g2g-canary/publish.env \
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
