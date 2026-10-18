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
Video surface: raw h264 (4:2:0 / 4:2:2 / 4:4:4, progressive and field-coded
tff), h265 (8/10-bit, raw + mp4), av1 (ivf + mp4), interlaced mpeg2 in program
stream and transport stream (the DVD path: ps demux, per-picture pts synthesis,
mpeg2 decode), h264 in matroska (avcc to annex-b) and mpeg-ts. Result: clean
after two fixes (bugs 5 and 6 below, both found by this pass).

### 2. Golden conformance
Decode Fluster conformance vectors and assert each engine's whole output matches
the official `decoded-md5`. No reference engine; an absolute oracle.
```
calliope conformance --corpus corpus/vectors.toml
```
Corpus: 945 vectors (AV1 242, VP9 305, HEVC 196, AVC 135, VP8 61). Result: clean.

Method: Fluster's `result` is a plain MD5 of the whole raw decoded file
(`utils.py::file_checksum`, 64KiB chunks) from the codec's software reference
decoder (H.264 = `ldecod` JM, `h264_jct_vt.py`; AV1 = `aomdec`), written as
`-f rawvideo` in `output-format`. calliope reproduces this exactly: decode with
`-vf format=<fmt> -f rawvideo`, MD5 the file. The method is identical, so no
recalibration was needed beyond the import source.

Import only from canonical Fluster (github.com/fluendo/fluster); its H.264 JM
hashes reproduce under ffmpeg. Do NOT import the gstreamer-CI `visl_references`
fork: those are VISL V4L2-stateless driver hashes, not reproducible by a plain
rawvideo md5 (a full JVT-AVC_V1 import once scored 0/135). The suite JSON names
no reference decoder, so `corpus-import` guards on the source path: any component
containing `visl` errors out (override with `--allow-driver-fork`).

Validated (canonical JVT-AVC_V1, 5 cached vectors, all engines golden ok):
AUD_MW_E, BA1_Sony_D, CABA1_Sony_D, CVWP1_TOSHIBA_E, MR1_BT_A. ffmpeg 5/5,
gstreamer 5/5, g2g 5/5. A fresh canonical re-import showed zero hash drift from
the committed corpus.

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

### 7b. LeakSanitizer over the conformance corpus
The ASan runs above set `detect_leaks=0` (a decoder that exits mid-stream on a
fault leaves expected allocations). A clean whole-stream conformance decode
should free everything, so run the corpus once with leak detection ON: any
report is a real leak in g2g's own code or the libav it drives.
```
tools/build-g2g-asan.sh
tools/lsan-g2g.sh              # runs conformance with ASAN_OPTIONS=detect_leaks=1
```
Result: clean (60 AV1 vectors, no leak reported).

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
- network / RTP: rtp_depay, flexfec, ulpfec (ULPFEC single-loss recovery over a
  decoder fed length-prefixed media / repair packets), rtx (RFC 4588 retransmit:
  header-offset walk + OSN unwrap / re-wrap), rtpjitter (reorder buffer: RTP
  header parse + deadline bookkeeping over a monotonic clock), st2110_dedup,
  rtcp, st2110anc (ST 2110-40 / SMPTE 291 ancillary depacketization: the 10-bit-
  word bit reader + parity / checksum over an RFC 8331 datagram)
- WebRTC / signalling: sdp (session description), rtcp (control channel),
  turn_stun (hand-rolled TURN / STUN over untrusted UDP: ChannelData + DATA-
  INDICATION framing and the XOR-PEER / XOR-MAPPED-ADDRESS + ERROR-CODE attribute
  walk the relay data plane runs on inbound datagrams, RFC 5766 / 8489)
- ST 2110 SDP: st2110sdp (the media / rtpmap / fmtp / ptp session-description
  text a -20/-30/-40 receiver configures from)
- streaming protocol: rtmp_handshake, rtmp_chunk (server-side chunk-stream
  reassembly + AMF0 command parsing a malicious publisher reaches post-handshake,
  via a `#[cfg(fuzzing)]` shim that forces the Streaming state), srt (SRT control /
  handshake CIF / NAK range-list + data-packet headers), rtsp (RTSP request line +
  headers + content-length framing)
- codec bitstream: h264parse, h265parse, av1parse, vp9parse, vp8parse, aacparse,
  opusparse (SPS / PPS / OBU / ADTS / TOC; the per-frame hand-written bit
  readers, reached via a `#[cfg(fuzzing)] pub fn fuzz_parse` shim in each module)
- containers (element-driven): ivfdemux (DKIF header + 12-byte frame headers),
  fmp4 (fragmented-MP4 / CMAF: moof / traf / trun / senc box parsing the HLS-
  fMP4 path runs, distinct from the progressive mp4_streams box parser). Both are
  async demux elements, so a `#[cfg(fuzzing)] fuzz_parse` shim drives the real
  `process` path over a no-op sink via a spin `block_on` (they parse buffered
  bytes into a synchronous sink, never awaiting real IO).
- content sniffing: typefind (container magic probes + Annex-B / text detection
  FileSrc runs on any input's leading bytes)
- crypto keying: srtcrypto (SRT KM control-message parse: header layout, wrapped-
  key length, salt, unwrapped under a fixed passphrase; gates hard on the KM
  magic / version, so it wants a valid-KM seed for depth)
- text / manifest: subparse (SRT / WebVTT / SSA-ASS / TTML subtitle text, byte vs
  char-boundary slicing), hls (m3u8 playlist tags / attributes). Attacker text fed
  as `from_utf8_lossy`; the fuzzer reached every format from an empty corpus.
- pipeline description: parse_launch (the gst-launch-style element / property /
  caps / link / bin text parser the g2g-capi `g2g_pipeline_launch` C entry
  forwards after its NUL / UTF-8 checks; parse only, no execution)

`gen-seeds.sh` rebuilds the corpora for the magic-gated formats (demuxers via
ffmpeg, rtmp C1/S1 via the `seedgen` helper) plus real elementary streams for the
codec parsers; the rest self-bootstrap. `ivfdemux` / `fmp4` / `srtcrypto` gate on
a magic / box structure, so `gen-seeds.sh` builds a real IVF, a fragmented MP4,
and a structurally valid KM message (garbage wrapped key, clears every header
gate into PBKDF2 + AES-KW). The fuzz crate builds with `std, rtmp, srt, webrtc`
so `srtcrypto` and `turn_stun` compile; the `webrtc` set pulls str0m + reqwest, a
heavier one-time build. Findings: **3 bugs** (see below). Everything else clean
over multi-minute-to-15-minute runs. The targets added in this pass (ulpfec, rtx,
rtpjitter, turn_stun, st2110sdp, ivfdemux, fmp4, typefind, srtcrypto) each ran a
full 600 s campaign, all clean, no crash / panic / leak, no artifacts. Final
coverage: fmp4 768, st2110sdp 634, ulpfec 594, turn_stun 572, typefind 278,
rtpjitter 247, srtcrypto 222, ivfdemux 168, rtx 78.

### 9. Miri (undefined behavior / data races)
Interpret g2g-core's unsafe (pools, SPSC ring, runtime) under Miri to catch
aliasing / stacked-borrows UB and data races that ASan and fuzzing miss. Miri
can't run C FFI, so this is scoped to g2g-core.
```
tools/miri-g2g.sh
```
Coverage: 202 g2g-core tests (`std` + `multi-thread`). Result: no UB, no data
races, no leaks. Miri runs one interleaving; loom (technique 11) explores them
all for the SPSC ring.

### 11. loom (exhaustive concurrency model check)
Model-check g2g-core's one hand-written cross-thread lock-free primitive, the
`SpscFrameRing` (an ISR-to-pipeline capture ring using a no-CAS Acquire/Release
head/tail protocol), under every thread interleaving. Its atomics + `UnsafeCell`
route through `crate::sync`, which swaps in loom's primitives under `--cfg loom`;
the normal / no_std build is unchanged (a zero-cost `core` wrapper). A producer
thread fills the ring while a consumer drains it with backpressure, and loom
verifies no interleaving lets the two touch a slot concurrently, loses,
duplicates, or reorders a frame. The other "primitives" (`slot`, channels,
memory refcounts) delegate to `ArcSwap` / `spin::Mutex` / `Arc`, whose lock-free
logic is upstream and already model-checked, so they are out of scope.
```
tools/loom-g2g.sh
```
Coverage: the SPSC producer/consumer handoff at `LOOM_MAX_PREEMPTIONS=3`. A
negative control (neutering the ring's full check) makes loom report a
"Concurrent read and write" causality violation, confirming the check has teeth.
Result: clean, no interleaving violates the protocol.

### 12. ThreadSanitizer (data races under real threads)
Miri and loom never run true parallel code: Miri interprets one interleaving,
loom model-checks the SPSC ring exhaustively but abstractly. TSan runs g2g-core's
`multi-thread` tests (which spawn the real producer / consumer threads) natively
under `-Zsanitizer=thread` (+ `-Zbuild-std` so std is instrumented), catching
races at runtime. Scoped to g2g-core: TSan can't instrument the C libav, so the
whole-pipeline race surface stays with loom + Miri + the deterministic
differential runs.
```
tools/tsan-g2g.sh
```
Coverage: 202 g2g-core tests (`std` + `multi-thread`) under TSan. Result: clean,
no data race reported.

### 10. Corrupt-input differential (decode-outcome divergence)
Corrupt the input (`[fault]`) and, with `outcome-diff = true`, cross-compare each
engine's decode *outcome* against the reference, not just crash / hang. A pixel
compare is meaningless on corrupt input (error concealment is
implementation-defined), so the signal is structural:
- **crash / hang**: fails the run, same bar as robustness.
- **LENIENT**: g2g decoded a stream ffmpeg refused. The too-lenient-parser class
  where memory bugs hide (the untrusted-input parsing the found bugs live in). The
  headline finding.
- **stricter**: g2g refused a stream ffmpeg decoded. Interop, lower value.

Divergences are advisory triage (only crash / hang fails); the driver sweeps
seeds / inputs and saves each LENIENT reproducer plus the corrupted bytes.
```
calliope run scenarios/h264-outcome-diff.toml
CALLIOPE_G2G_LAUNCH=~/.local/bin/g2g-launch-asan tools/outcome-diff-g2g.sh
```
Volume: 1000 runs (4 fault modes x local corpus x 25 seeds). Result: no crash /
hang. Two LENIENT splits, both triaged to a false positive: ffmpeg's CLI rejects
a corrupt raw HEVC elementary stream at its demux probe ("Invalid data found"),
while g2g's decodebin framer is more permissive, decodes it, and safely skips the
invalid NALUs. g2g is more robust there, not unsafely lenient. Caveat: for raw
elementary streams the oracle's accept/reject signal includes this demux-boundary
strictness gap; it is sharpest on g2g's native parsers (e.g. dav1d AV1) and on
container inputs where both sides run a real demuxer.

### 13. Audio decode differential + determinism
Decode compressed audio to normalized interleaved PCM and hash the whole stream
(frame boundaries differ across decoders, so per-frame md5 is wrong). The
bit-exact policy is per codec: opus cross-compares across libopus integer-api
engines; flac is lossless and cross-compares everywhere (raw .flac + ogg-flac);
ac3 / mp2 cross-compare ffmpeg vs g2g only (both decode through the same
libavcodec, gstreamer may plug a52dec); AAC and vorbis (g2g decodes via
symphonia) are not bit-exact across decoders, so they self-compare
(determinism), as does chained-ogg opus (see the quirks below).
```
calliope run scenarios/opus-decode-diff.toml       # gstreamer + g2g
calliope run scenarios/flac-decode-diff.toml       # all three engines
calliope run scenarios/ac3-ts-decode-diff.toml     # ffmpeg + g2g
calliope run scenarios/mp2-ts-decode-diff.toml
calliope run scenarios/vorbis-determinism.toml
calliope run scenarios/opus-chained-determinism.toml
calliope run scenarios/aac-determinism.toml
```
Result: all clean. The opus differential initially **failed**, naming g2g as the
outlier (bug 4 below, plus a one-sample loss in g2g's identity `audioresample`),
fixed in g2g M750/M751. The ac3 differential then **failed** on ~75% of samples,
exposing g2g's float->s16 conversion (bug 6 below, fixed M937).

Reference quirks found while re-validating (2026-08-06): ffmpeg n8.x's libopus
wrapper decodes float internally and quantizes itself, drifting +-1 lsb from the
libopus integer api on ~7% of samples (gstreamer == g2g, ffmpeg the outlier, in
both s16 and float output), so the opus differential is now gstreamer vs g2g.
gstreamer's static decodebin stops at a chained-ogg boundary (not-linked on the
second serial), so the chained scenario is g2g determinism only; g2g decoding
both chain links (4.0s from 2x2s) was hand-verified against ffmpeg's length.

### 14. Deinterlace differential (filter stage, bit-exact)
A `deinterlace = true` scenario inserts each engine's single-rate yadif between
decode and the raw dump (ffmpeg `yadif=mode=send_frame:parity=tff:deint=all`,
g2g `deinterlace method=yadif mode=interlaced`), then judges the deinterlaced
frames with the normal per-frame compare. g2g's kernel is a port of ffmpeg's,
so the output must stay bit-exact; the input is field-coded (tff) h264.
```
calliope run scenarios/yadif-deinterlace-diff.toml
```
Result: clean, g2g bit-exact with ffmpeg over 50 frames. Reference quirk:
gstreamer's `deinterlace method=yadif fields=top mode=interlaced` emitted 52
frames for these 50 and diverged from frame 0, so it stays out of the scenario
(the 3-engine run named it the outlier, conclusively).

### 15. Real-world corpus differential (fate-suite + gst-medias)
Every file in the ffmpeg fate-suite and the gst-integration-testsuites medias
that ffprobe finds a video or audio stream in is imported as a corpus vector,
and `calliope sweep` runs one scenario per stream on ffmpeg, GStreamer and g2g
with ffmpeg as the reference. Planar YUV video gets the per-frame differential
and the bit-exact audio codecs the whole-stream differential. Packed video and
the other audio codecs get a determinism run. When ffmpeg rejects a file the
decode outcome is the oracle.
```
calliope sweep --corpus corpus/fate-suite.toml --title fate-suite --report runs/fate-suite.json
calliope sweep --corpus corpus/gst-medias.toml --title gst-medias --report runs/gst-medias.json
python3 -I tools/sweep-triage.py runs/fate-suite.json corpus/fate-suite.toml fate-suite \
  "master 42c0a809 built with linux-full in release" triage/fate-suite.md
```
Volume: fate-suite 2221 files, 2411 scenarios (25 minutes at `--jobs 3`).
gst-medias 218 files, 284 scenarios (6 minutes). One gst-medias file has a
space in its name and is skipped. g2g-launch was g2g master 42c0a809,
linux-full, release.

Result, fate-suite: 948 scenarios agree (627 differential, 255 determinism, 66
rejected by every engine). Of the other 1463, 176 are g2g bugs, 1109 are codecs,
containers or pixel formats g2g does not support, 98 are harness limits, 38 are
reference quirks and 42 are GStreamer-only failures. gst-medias: 149 agree (36
differential, 103 determinism, 10 rejected by every engine). Of the other 135, 86
are g2g bugs, 36 unsupported, 6 reference quirks, 6 GStreamer-only and 1 open.
In the open case ffmpeg drops every frame of an HEVC TS and g2g decodes it,
unverified.

The 262 g2g-bug scenarios come from 22 bugs, listed with a reproducer in
`triage/fate-suite.md`. The largest are decodebin decoding the video stream when
the pipeline asks for audio (104 scenarios), Mp4Demux CapsMismatch on H.264,
HEVC, MPEG-4, AV1, VP8 and MJPEG tracks (43), and audio-only files getting a
video chain (20). Edit lists are not applied (8 MOV / MP4 files), and 18 MP4
AAC conformance files decode to nothing with exit status 0. None is fixed yet,
so they are not in the list below.

Harness limits: 6 resolution-changing streams cannot be hashed at one geometry,
and g2g is bit-exact with `ffmpeg -noautoscale` on all of them. GStreamer pads
raw rows to 4 bytes or changes resolution mid-stream, so 88 of its dumps do not
split into frames. Determinism scenarios pin I420, so the g2g adapter inserts
`videoconvert` when a scenario has no `[video]` table. Without it every
packed-RGB decoder (PNG, APNG) fails the capsfilter.

Reference quirks: ffmpeg decodes the truncated last AC-3 or MP2 frame where
GStreamer and g2g drop it, and g2g is bit-exact over every common sample. ffmpeg
sniffs S/PDIF AC-3 inside a PCM WAV. Opus is not bit-exact across any two of
the three engines on the RFC 6716 vectors. ffmpeg n8.1 decodes neither
animated WebP nor `probe-format/mp3-with-art`.
### 16. Output validators and browser playback
A scenario with a `[produce]` table has each engine encode or remux one input
into a container, then external validators judge the file or live stream it
wrote. There is no reference decode: the validator is the oracle. The input is
`local-corpus/produce-src.mp4`, 12 s of 320x240 h264 at 25 fps (300 frames) with
48 kHz aac, plus an h265 copy for the remux scenario. g2g re-encodes with
`x264enc backend=software` or `av1enc` (rav1e), ffmpeg with libx264 or
libsvtav1. ffmpeg's output is the control, so a validator failure its output
shares is reported as advisory.

The validators: ffprobe decodes every frame and checks the codecs, the video
frame count, every frame interval against the input's 40 ms, and that HLS
EXTINF durations add up to the media duration. GPAC MP4Box `-info` reports box
parse errors and checks for a CMAF brand on CMAF output. mkvalidator 0.6.0
checks matroska. TSDuck `tsp` with the analyze and pcrextract plugins checks
sync, transport errors, continuity, PTS/DTS/PCR leaps and a PCR interval of at
most 100 ms, on each HLS segment separately. The DASH-IF conformance tool runs
its MPD schema and Schematron checks plus the IOP MPD and segment modules.
Headless Chromium driven by Playwright plays HLS with hls.js and DASH with
dash.js, reads a WHIP publish back over WHEP through mediamtx, and decodes mp4,
fragmented mp4 and CMAF files with WebCodecs after mp4box.js demuxes them. A
file has to render all 300 frames, a live read at least half the frames of a
6 s window. Apple's mediastreamvalidator is wired in for both HLS scenarios but
runs on macOS only, so it was skipped on this Linux host.
```
tools/validators/mediamtx.sh
CALLIOPE_MP4BOX=$PWD/tools/validators/mp4box.sh CALLIOPE_TSP=$PWD/tools/validators/tsp.sh \
    calliope run scenarios/*-produce.toml
```
Volume (2026-10-18): 12 scenarios, 23 engine runs, g2g-launch from g2g master
42c0a809 (linux-full, release). ffprobe n8.1.2, mkvalidator 0.6.0 (built from
source) and TSDuck 3.44-4676 (unpacked from the Fedora 43 RPM) ran natively.
MP4Box (GPAC 26.08-DEV-rev192, `docker.io/gpac/ubuntu`), the DASH-IF tool
(commit e311ec6, built into a local image) and mediamtx v1.21.2 ran from podman.
The browser was Chrome for Testing 153.0.8010.12 under playwright-core 1.64.1,
with hls.js 1.7.3, dash.js 5.2.1 and mp4box.js 2.4.1.

Result: g2g fails four checks, all open in g2g at 42c0a809. ffprobe flags the
first frame interval of every g2g mp4 output that re-encodes (mp4, fmp4, CMAF,
HLS fmp4, AV1 mp4): frame 1 starts 33.333 ms after frame 0. mp4mux gives a
first sample without a duration 1/30 s, and the encoders set none. The h265
remux, whose samples carry the demuxer's duration, passes. ffprobe also flags
g2g's HLS fmp4 playlist: its EXTINF values sum to 24.024 s for 12 s of media,
because hlssink adds the video and audio durations of a segment together (a
video-only run lists 11.993 s). mkvalidator fails g2g's matroska: Info has no
MuxingApp or WritingApp, and the EBML header says DocTypeVersion 2 while the
opus TrackEntry carries CodecDelay and SeekPreRoll, which need version 4.
g2g's mpegtsmux has no Opus mapping, so the TS opus scenario stops with
CapsMismatch after negotiation accepted `audio/x-opus`.

Everything else passes on g2g output. MP4Box finds no box errors in any g2g
mp4, fmp4, CMAF, AV1 or h265 file. TSDuck finds nothing in g2g's TS or in any
of its HLS TS segments. mkvalidator's only other notes are warnings: no
SeekHead, and clusters that start on a non-keyframe (ffmpeg's file has one of
those too). The browser renders all 300 frames of g2g's mp4, fmp4, CMAF and AV1
files and of both HLS variants (hls.js plays through the wrong EXTINF values),
and reads 120 frames over WHEP from g2g's `webrtcsink` (ffmpeg: 134, minimum
75). g2g has no DASH packager, so the DASH-IF tool and dash.js ran on ffmpeg's
output only. dash.js renders all 300 frames. The DASH-IF tool fails ffmpeg's
manifest on one Schematron rule: the first video SegmentTimeline entry lasts
10 s and `maxSegmentDuration` says 5 s. ffmpeg's output passes every other
validator.

A manual probe beside the scenarios found a fifth g2g bug. Fragmented mp4mux
writes `trun` without composition offsets, so h264 with B-frames remuxed into
fmp4 gets pts equal to dts in decode order and plays out of order. No scenario
catches it: g2g's x264enc output has no B-frames, and the interval check sorts
pts.
### 17. Canary (24/7 public streams)
`calliope canary` keeps g2g pipelines decoding live streams and restarts each
one when it exits or decodes no frame for 30 s. Once a minute it appends frames,
fps, decoder latency, RSS and restart count per pipeline to a jsonl log, and it
writes a status page. The self-hosted tier decodes a looped 720p30 h264 clip
that mediamtx and an ffmpeg feeder serve on the same host over RTSP, HLS, SRT
and WebRTC (WHEP), so a failure there is g2g's. The public tier pulls third-party streams. The
deploy is a podman image under user systemd units, see the Canary section of
the README.
```
tools/canary-image.sh ~/src/glass2glass
tools/canary-deploy.sh local
```
Volume: a 5.5 minute foreground run, then a 40 minute run of the deployed units
on the dev box (2026-10-18, 40 samples per pipeline), seven pipelines. The image
carries `g2g-launch` built from g2g 439f4e31. Latency is the decoder's p50
`process()` time as g2g's power-of-two histogram bucket, not glass-to-glass
latency.

Result: the supervisor works. A `SIGKILL` of a running `g2g-launch` was
restarted within a second, logged as an incident with cause signal 9, and closed
2 s later when frames flowed. Finished VOD playlists restarted without an
incident. The publish timer copied the page every 5 minutes. Over the 40 minutes
the whole canary container used 44% of one core on average (59% peak) and
236 to 452 MB.

Five pipelines decoded every interval. `baseline-rtsp` (30 fps, RSS 25 to 53 MB,
decoder p50 4.2 ms) and `baseline-hls` (30 fps, 73 to 119 MB, 0.5 ms) each
restarted twice at startup, when mediamtx returned 404 before the feeder had
published. The public `unified-live-hls` (25 fps, 65 to 110 MB, 0.5 ms),
`mux-vod-hls` (480p60, 56 to 110 MB, 0.3 to 1.0 ms) and `apple-vod-hls` (30 fps,
38 to 82 MB, 0.3 ms) never restarted.

Two pipelines found g2g bugs. `baseline-srt` was up in 14 of 40 intervals with
13 stall restarts: each run delivers about 8192 SRT packets (about 30 s of this
clip) and then no more, while ffmpeg's own SRT listener receives the same push
for 2 minutes without a gap. The count matches the 8192 packet flow window g2g
advertises in its handshake. The public `strba-rtsp` webcam
(`rtsp://stream.strba.sk:1935/strba/VYHLAD_JAZERO.stream`, h264 1280x720) was
up in 0 of 40 intervals with 16 restarts: every run fails negotiation with
`RtspSrc -> NalParse: video/x-h264,width=1280,height=720 cannot fixate` and
exits `CapsMismatch`. The stream's SPS carries no frame rate (ffprobe reports
`avg_frame_rate=0/0`). No public SRT stream is in the config because g2g's
`srtsrc` only listens.

Second run: 25 minutes of the deployed units on the dev box (2026-10-18, 25
samples per pipeline), eight pipelines. The image carries `g2g-launch` built
from g2g a69dac41 with the `webrtc` feature. The feeder clip is now encoded
without B-frames, because mediamtx closes a WebRTC session on H.264 with
B-frames. The canary container used 57% of one core averaged over the run and
407 to 441 MB.

Seven pipelines decoded every interval. `baseline-srt` (30 fps, RSS 82 to 100
MB, decoder p50 0.5 ms) never restarted and opened no stall incident. The public
`strba-rtsp` webcam now decodes (10 to 12 fps, 26 to 45 MB, 2.1 to 4.2 ms) and
never restarted. `baseline-rtsp` (30 fps, 23 to 42 MB, 4.2 ms) and
`baseline-hls` (30 fps, 90 to 112 MB, 0.5 ms) again restarted twice at startup
on mediamtx's 404. `unified-live-hls` (25 fps, 89 to 111 MB, 0.5 ms),
`mux-vod-hls` (480p60, 84 to 109 MB, 0.3 to 0.5 ms) and `apple-vod-hls` (30
fps, 60 to 83 MB, 0.3 ms) never restarted.

The new `baseline-webrtc` row found a g2g bug:
```
whepsrc location=http://127.0.0.1:18889/baseline/whep ! h264parse ! avdec_h264 ! fakesink
```
It was up in 7 of 25 intervals with 11 restarts. The first run failed at
startup with `pipeline error in WebRtcWhepSrc0: Hardware(Other)`, in the same
second `baseline-rtsp` got its 404. Every later failed run ended with
`[h264] non-existing PPS 0 referenced` and `pipeline error in FfmpegVideoDec0:
Hardware(Other)`. `whepsrc` forwards the P-frames it receives before the first
IDR, so the decoder gets slices before any SPS and PPS and fails the pipeline.
The other g2g WebRTC receivers drop video until the first keyframe and
`whepsrc` does not. A run that joins the stream on an IDR decodes: the run that
started 19 minutes in decoded 30 fps (RSS 29 to 51 MB, 4.2 ms) to the end.
### 18. Mutation testing (cargo-mutants)
The other techniques look for bugs in g2g. This one checks the g2g test suite
instead. cargo-mutants injects one bug at a time into g2g's source (a function
body replaced by a default value, an operator swapped, a match arm or a `!`
deleted), rebuilds, and runs the tests. A mutant the tests fail on is caught. A
mutant they pass is missed: no test pins down that behavior. g2g-core runs
against its in-crate suite with every feature that compiles code in (a mutant
in cfg'd-out code would count as missed). The g2g-plugins parsers the fuzz
targets cover run one file at a time against the module's unit tests plus the
integration tests that use the module directly.
```
tools/mutants-g2g.sh g2g-core -p g2g-core \
    --features std,multi-thread,runtime,metadata,dyn-slot,tracing -- --lib --tests
tools/mutants-g2g.sh g2g-plugins-mpegts -p g2g-plugins -f g2g-plugins/src/mpegts.rs \
    --features std,metadata,mp4-cenc -E fuzz_parse \
    --cargo-arg=--lib --cargo-arg=--test=m108_mpegts ...
```
Per-scope lists, commands and wall times are in `mutants/README.md`.
Result: g2g-core scored 59% (1114 mutants, 516 caught, 351 missed). g2g M1530
and M1531 added tests for 245 of the missed ones, and the other 106 are not gaps.
18 g2g-plugins parser files scored 79% (4886 mutants, 3365 caught, 882 missed,
59 timeouts, 580 unviable) against g2g 439f4e3. h265parse.rs (26%) and
h264parse.rs (52%) are the weakest. The missed plugin mutants have no tests yet.

## Bugs found

Bugs 1-3 were found by coverage-guided fuzzing (technique 8), bug 4 by the audio
decode differential (technique 13), bugs 5-6 by the interlace / audio surface
added 2026-08 (techniques 1 and 13). All are fixed in g2g with regression tests.

1. **FLV demuxer out-of-bounds panic.** `flv.rs::parse_tag` indexed the AVC
   composition-time bytes directly while guarding the rest; a video tag shorter
   than 5 bytes panicked. DoS on truncated FLV.
2. **ST 2110-7 dedup remote DoS.** `st2110dup::accept` looped ~2^64 times when a
   backward-wrapping RTP sequence number (e.g. 65535 after 0) resolved above the
   window head. A 32-byte packet pair wedged the receiver. Fixed by bounding the
   window advance and resolving the wrap by signed delta.
3. **HEVC SPS short-term-RPS integer overflow.** `h265parse::parse_h265_short_term_rps`
   accumulated Exp-Golomb POC deltas (`delta_poc_sX_minus1`, `ue(v)` up to
   `u32::MAX`) in unchecked `i32`; a malformed SPS overflowed the running POC. It
   bounded the picture *counts* (> 16) but not the per-delta values. Panics under
   overflow-checks (a hardened / debug-build DoS on attacker input), silent wrap
   otherwise. Fixed with checked arithmetic that rejects the malformed SPS.
4. **Opus pre-skip / end-trim not applied.** g2g's Opus decode emits every
   decoded sample instead of trimming the OpusHead pre-skip and the granule-
   position end padding. A 1s mono 48k clip decodes to 48960 samples (312
   pre-skip + 48000 + 648 end pad) where ffmpeg/gstreamer via libopus emit
   exactly 48000; the retained samples are otherwise bit-exact (g2g[312:48312]
   matches the reference). Not a memory-safety bug, a correctness / A-V-sync one:
   every decoded stream was shifted and over-long. Fixed in g2g M750 (the demuxer
   forwards `OpusHead` in-band and marks the end-trim via the final granule
   position; the decoder drops the pre-skip window), with the adjacent M751
   fixing a one-sample loss in the identity `audioresample` path the same
   differential exposed.
5. **decodebin on MPEG-TS ignored the PMT's video codec.** The `decodebin`
   primary-stream sniff declined to select a stream whenever the transport
   stream carried any video track, assuming `TsDemux`'s default H.264 port was
   right; an MPEG-2 (or H.265) video TS then negotiated an H.264 decoder and the
   pipeline failed `NotConfigured` before decoding a frame. Found by the
   mpeg2-ts-interlaced differential (g2g produced 0 bytes). Fixed in g2g M936:
   the sniff now returns the PMT's first video stream with an explicit
   `stream=<codec>` selection.
6. **Float->s16 audio conversion off by 1 LSB from every other engine.** g2g's
   libavcodec audio decode converted float samples with truncation at 32767
   scale where ffmpeg (swresample) and GStreamer round to nearest at 32768
   scale, so any float-codec decode (AC-3, AAC) differed from ffmpeg's decode of
   the same stream on ~75% of samples, a permanent quantization bias. Found by
   the ac3 differential. Fixed in g2g M937 (swresample-matching round-to-even).

## Not a gap

- Muxers (mp4mux, matroskamux, mpegtsmux, flvmux) are covered by g2g's own tests
  (`m291_mp4mux` ffmpeg-validated, `m120_flvmux` roundtrip, `m294`/`m296`/`m114`/
  `m115`). Their input is trusted encoder output, not attacker-controlled, so
  they are low-yield for a differential / fuzz harness.
- The g2g-capi C ABI wrapper was read end to end: every `extern "C"` fn NUL-checks
  its handles via `ptr::as_ref` / `as_mut`, validates C strings as UTF-8, guards
  `data.is_null() && len != 0`, and returns error codes instead of panicking
  across the boundary. The pointer / lifetime discipline is caller-contract (a
  fuzzer passing garbage pointers only reproduces caller misuse), so the fuzzable
  surface it adds is the launch-string parser it forwards (`parse_launch`, fuzzed
  above). No wrapper-level gap found.
- Feature-gated parsers behind the HTTP stack (`mpd` / DASH via `roxmltree` +
  http-src, `onvif` via `reqwest`) are a poor fit for the in-process ASan
  libFuzzer rig (network runtime, huge build); fuzzing them needs a harness that
  extracts the pure parse fn or mocks the transport. Deferred, not covered.
- **Audio golden.** Opus / AAC decode is now differential + determinism
  (technique 13), but there is no golden audio oracle: RFC 6716 Opus conformance
  uses a tolerance compare (`opus_compare`), not a bit-exact `decoded-md5`, so no
  official audio vector maps onto the golden mode. Not covered.
