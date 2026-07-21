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

## Bugs found

All found by coverage-guided fuzzing (technique 8), fixed in g2g with regression
tests.

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
- **Audio decode differential.** The aac / opus *parsers* are fuzzed (technique
  8), but no scenario compares decoded audio output. Two blockers, the first
  upstream in g2g: a `g2g-launch` built with `ffmpeg,opus` still won't negotiate
  a decode-to-PCM pipeline from the CLI (`OpusDec -> AudioConvert: unconstrained`;
  aac `decodebin ! filesink` is a `CapsMismatch`; no raw-audio file sink is
  registered), so there is no way to dump comparable PCM yet. Second, even once
  that works, only Opus is defined to be bit-exact across decoders; lossy AAC is
  not, so an AAC cross-engine differential would false-fail and must use golden
  (vs a reference vector) or determinism (self-comparison) instead. The calliope
  side then needs an `audio/x-raw` path in all three adapters, whole-stream PCM
  hashing (audio frame boundaries differ across decoders, so per-frame md5 is
  wrong), and an `[audio]` spec. Deferred: the g2g audio pipeline has to
  decode-to-PCM from the CLI before the harness work is worth doing.
