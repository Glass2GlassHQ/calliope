# g2g test campaign

Everything thrown at glass2glass through this harness and alongside it, so a
stability challenge can be answered from one place. Each technique lists how to
reproduce it and what it found. Bugs found are fixed in the g2g repo with
regression tests; commit hashes are in that repo's history.

## Techniques

### 1. Differential decode (bit-exact)
Decode the same stream on ffmpeg, GStreamer, and g2g; require per-frame MD5
equality. With 3+ engines a majority vote names the outlier. A divergence is a
real decoder bug.
```
calliope run scenarios/h264-decode-smoke.toml
```
Result: clean.

### 2. Golden conformance
Decode Fluster conformance vectors and assert each engine's whole output matches
the official `decoded-md5`. No reference engine; an absolute oracle.
```
calliope conformance --corpus corpus/vectors.toml
```
Corpus: 945 vectors (AV1 242, VP9 305, HEVC 196, AVC 135, VP8 61). Result: clean.

### 3. Robustness fuzzing + minimizer
Corrupt input (`[fault]`: bit-flip / truncate / byte-drop / nal-payload) and
require every engine to degrade gracefully (no crash / hang). Shrink any crash
to a minimal reproducer.
```
calliope run scenarios/h264-nal-payload-fuzz.toml
calliope minimize --engine g2g --input runs/<scenario>/input.corrupted
```
Result: clean (see 6 for the ASan-instrumented volume run).

### 4. Soak + determinism
Repeat a run and fail on any crash / hang (`[soak]`); run each engine repeatedly
and require byte-identical output, including g2g's `--threads` variant
(`[determinism]`). Result: clean.

### 5. Roundtrip + encode-differential + resolution-change
- roundtrip: g2g decodes and re-encodes; ffmpeg PSNR-checks the result. Encoder
  smoke test.
- encode: ffmpeg encodes a lavfi source; engines bit-exact decode it, feeding
  decoders feature combos the corpus lacks.
- resolution-change: decode a stream whose geometry switches mid-playback;
  require survival and the expected decoded byte total (per-frame sizes from
  ffprobe). Targets g2g's caps / buffer renegotiation.
```
calliope run scenarios/h264-x264enc-roundtrip.toml
calliope run scenarios/h264-ffmpeg-encode-diff.toml
calliope run scenarios/h264-resolution-change.toml
```
Result: clean.

### 6. AddressSanitizer build + robustness fuzz loop
Build g2g-launch under ASan (malloc interception catches heap overflow /
use-after-free process-wide, including the system libav it calls), then fuzz it
with seeded corruption over real streams (h264/h265/ts/av1) and minimize any
crash.
```
tools/build-g2g-asan.sh
CALLIOPE_G2G_LAUNCH=~/.local/bin/g2g-launch-asan tools/fuzz-g2g.sh
```
Volume: 6400 fuzz runs (nal-payload + bit-flip). Result: clean.

### 7. ASan over the conformance corpus
Run the golden conformance corpus with the ASan g2g-launch so memory bugs abort
on well-formed input, not just corrupted streams.
```
CALLIOPE_G2G_LAUNCH=~/.local/bin/g2g-launch-asan \
ASAN_OPTIONS=abort_on_error=1:detect_leaks=0 \
  calliope conformance --engines g2g --limit 150
```
Coverage: 150 AV1 vectors (incl. 10-bit, exercising g2g's native dav1d path).
Result: clean.

### 8. Coverage-guided fuzzing (cargo-fuzz)
In-process libFuzzer + ASan on g2g's own pure-Rust parsers of untrusted input.
Targets live in the g2g repo at `g2g-plugins/fuzz/`.
```
cd <g2g>/g2g-plugins/fuzz && ./gen-seeds.sh      # seed corpora (once)
cd <g2g>/g2g-plugins && cargo +nightly fuzz run <target> -- -max_total_time=600
```
Targets, all g2g-owned parsers of untrusted bytes:
- containers: mp4_streams, matroska, flv, ogg, mpegts
- captions: cea_cdp
- network / RTP: rtp_depay, flexfec, st2110_dedup, rtcp
- WebRTC / signalling: sdp (session description), rtcp (control channel)
- handshake: rtmp_handshake

`gen-seeds.sh` rebuilds the corpora for the magic-gated formats (demuxers via
ffmpeg, rtmp C1/S1 via the `seedgen` helper); the rest self-bootstrap. Findings:
**2 bugs** (see below). Everything else clean over multi-minute-to-15-minute runs.

### 9. Miri (undefined behavior / data races)
Interpret g2g-core's unsafe (pools, SPSC ring, runtime) under Miri to catch
aliasing / stacked-borrows UB and data races that ASan and fuzzing miss. Miri
can't run C FFI, so this is scoped to g2g-core.
```
tools/miri-g2g.sh
```
Coverage: 202 g2g-core tests (`std` + `multi-thread`). Result: no UB, no data
races, no leaks.

## Bugs found

Both found by coverage-guided fuzzing (technique 8), fixed in g2g with
regression tests.

1. **FLV demuxer out-of-bounds panic.** `flv.rs::parse_tag` indexed the AVC
   composition-time bytes directly while guarding the rest; a video tag shorter
   than 5 bytes panicked. DoS on truncated FLV.
2. **ST 2110-7 dedup remote DoS.** `st2110dup::accept` looped ~2^64 times when a
   backward-wrapping RTP sequence number (e.g. 65535 after 0) resolved above the
   window head. A 32-byte packet pair wedged the receiver. Fixed by bounding the
   window advance and resolving the wrap by signed delta.

## Not a gap

- Muxers (mp4mux, matroskamux, mpegtsmux, flvmux) are covered by g2g's own tests
  (`m291_mp4mux` ffmpeg-validated, `m120_flvmux` roundtrip, `m294`/`m296`/`m114`/
  `m115`). Their input is trusted encoder output, not attacker-controlled, so
  they are low-yield for a differential / fuzz harness.
