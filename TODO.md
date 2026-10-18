# Outstanding

Production-readiness evidence for g2g. Each item ends with a section in
TESTING.md stating method, volume, and result. Delete an item when it lands.

## Canary
- Run the canary on the EC2 instance for 6 months and link the S3 status page
  from the g2g docs site.

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
