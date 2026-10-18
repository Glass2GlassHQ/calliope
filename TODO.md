# Outstanding

Production-readiness evidence for g2g. Each item ends with a section in
TESTING.md stating method, volume, and result. Delete an item when it lands.

## Canary
- Run the canary on the EC2 instance for 6 months and link the S3 status page
  from the g2g docs site.

## Corpus differential depth
- Timestamp differential: compare each frame's PTS and duration with
  `ffprobe -show_frames` for every corpus scenario.
- Chunk-size invariance: decode each corpus file at several `filesrc` block
  sizes (188, 4096, 65536) and require identical output.
- Per-track differential: decode every audio and video track of a
  multi-track file by index and compare with `ffmpeg -map 0:a:N` /
  `-map 0:v:N`.
- Seek differential: seek to several positions per file and compare the
  first frame after the seek with an accurate `ffmpeg -ss` decode.
- Remux round-trip: remux the agreeing files through the g2g mp4, mkv, ts and
  flv muxers and require the remuxed file to decode the same.

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
