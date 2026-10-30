# Outstanding

Production-readiness evidence for g2g. Each item ends with a section in
TESTING.md stating method, volume, and result. Delete an item when it lands.

## Canary
- Run the canary on the EC2 instance for 6 months and link the S3 status page
  from the g2g docs site.
- Add a public SRT server row: `srtsrc mode=caller` calls one now, so the "no
  public srt entry" comment in `deploy/canary/canary.toml` no longer holds.

## Corpus differential depth
- Timestamp differential: compare each frame's duration too, once a g2g sink
  prints it.
- Per-track differential for AAC tracks, which are only rerun for
  determinism because no two AAC decoders are bit-exact.

## Mutation testing
- Add g2g-plugins tests for the missed parser mutants that mark real gaps
  (`mutants/README.md` lists the per-file counts).

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
- Golden conformance corpus through VulkanVideoDec on RADV, MediaCodec (Pixel)
  and Media Foundation (Windows), results per backend in TESTING.md.

## Fluster upstream
- Contribute a g2g decoder class to fluendo/fluster so g2g appears in the
  published results tables. Prepare locally, open the PR only on confirmation.
