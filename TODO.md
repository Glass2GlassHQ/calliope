# Outstanding

Production-readiness evidence for g2g. Each item ends with a section in
TESTING.md stating method, volume, and result. Delete an item when it lands.

## Real-world corpus differential
- Import the ffmpeg fate-suite (`fate-suite/` rsync tree) as corpus vectors:
  fetch-on-demand + sha256, per-file license from the tree, skip non-media.
- Import gst-integration-testsuites `medias/` the same way.
- Run decode differential (video per-frame, audio whole-stream) over every file
  ffmpeg decodes, with the outcome-diff oracle for files ffmpeg rejects.
- Classify each divergence (g2g bug, reference quirk, unsupported codec) in a
  committed triage table, file g2g bugs with reproducers.

## Canary
- `calliope canary`: keeps N g2g pipelines alive against public RTSP, HLS and
  SRT streams, restarts on exit, records uptime, restarts, RSS and latency per
  pipeline to an append-only log, writes a static status page.
- Deploy recipe: systemd units + binary sync script, host-agnostic (first
  deployment on the dev box, then a 2 vCPU ARM VM).
- Status page published to the g2g docs site.

## Mutation testing
- cargo-mutants over g2g-core, then the g2g-plugins parsers the fuzz targets
  cover. Report mutants caught / missed / timeout, add tests for missed
  mutants that mark real gaps.

## Network impairment against reference peers
- Compose file with reference peers: mediamtx, gst-rtsp-server, janus, livekit,
  srt-live-server, nginx-rtmp, moq-rs.
- tc netem profiles: loss, reorder, jitter, bandwidth cap, link flap.
- Scenario asserts frame count, A/V drift bound, reconnect within budget, no
  crash or hang.

## Long-duration soak
- In-process soak mode: one g2g run for hours, RSS and latency sampled per
  minute, fail on growth beyond a bound or a stall.
- Synthesized streams starting just below the RTP timestamp, RTP sequence and
  MPEG-TS PCR wrap points so wrap is hit within minutes.

## Hardware decode matrix
- Golden conformance corpus through VulkanVideoDec (RADV + NVIDIA), NVDEC,
  MediaCodec (Pixel), Media Foundation (Windows), results per backend in
  TESTING.md.

## Fluster upstream
- Contribute a g2g decoder class to fluendo/fluster so g2g appears in the
  published results tables. Prepare locally, open the PR only on confirmation.
