# gst-medias sweep triage

218 files, 284 scenarios (one per video / audio stream), reference ffmpeg 8.1.3. g2g-launch is g2g master a988298b built with linux-full minus vaapi, pipewire and portal in release. The class comes from `calliope sweep`. The diagnosis comes from rules over the g2g error and the stream layout, checked by rerunning representative files against ffmpeg and gstreamer. Every g2g diverges row was rerun by hand. Reproducers read the sample tree from `$HOME/.cache/calliope/gst-medias`.

| class | scenarios |
| :--- | ---: |
| gstreamer diverges | 9 |
| reference quirk | 3 |
| unsupported codec | 34 |
| agree | 238 |

Every scenario outside the agree class, by diagnosis:

| diagnosis | scenarios |
| :--- | ---: |
| unsupported | 34 |
| reference quirk | 3 |
| gstreamer only | 9 |

## g2g bugs

No scenario in this tree hits a g2g bug. Bug numbers elsewhere refer to the list in `triage/fate-suite.md`. The HLS playlists and DASH manifests are listed as unsupported container because `filesrc ! decodebin` does not open them, g2g reads them through its own source elements.

## gstreamer diverges

| scenario | codec | result | diagnosis | reproducer |
| :--- | :--- | :--- | :--- | :--- |
| `gst-medias/defaults/h265/alternate.h265/video` | hevc yuv420p | engine produced no frames | gstreamer fails or differs, g2g agrees with ffmpeg | `g2g-launch -q filesrc location=$HOME/.cache/calliope/gst-medias/defaults/h265/alternate.h265 ! decodebin ! video/x-raw,format=I420 ! filesink location=/tmp/calliope-repro/out.yuv` |
| `gst-medias/defaults/matroska/flac_jpeg.0.mkv/audio` | flac | exit 1 | gstreamer fails or differs, g2g agrees with ffmpeg | `g2g-launch -q filesrc location=$HOME/.cache/calliope/gst-medias/defaults/matroska/flac_jpeg.0.mkv ! decodebin ! audioconvert dithering=none ! audioresample ! audio/x-raw,format=S16LE,rate=44100,channels=1 ! filesink location=/tmp/calliope-repro/out.pcm` |
| `gst-medias/defaults/matroska/flac_jpeg.0.mkv/video` | mjpeg yuvj420p | exit 1 | gstreamer fails or differs, g2g agrees with ffmpeg | `g2g-launch -q filesrc location=$HOME/.cache/calliope/gst-medias/defaults/matroska/flac_jpeg.0.mkv ! decodebin ! videoconvert ! video/x-raw,format=I420 ! filesink location=/tmp/calliope-repro/out.yuv` |
| `gst-medias/defaults/matroska/flac_jpeg.1.mkv/audio` | flac | exit 1 | gstreamer fails or differs, g2g agrees with ffmpeg | `g2g-launch -q filesrc location=$HOME/.cache/calliope/gst-medias/defaults/matroska/flac_jpeg.1.mkv ! decodebin ! audioconvert dithering=none ! audioresample ! audio/x-raw,format=S16LE,rate=44100,channels=1 ! filesink location=/tmp/calliope-repro/out.pcm` |
| `gst-medias/defaults/matroska/flac_jpeg.1.mkv/video` | mjpeg yuvj420p | exit 1 | gstreamer fails or differs, g2g agrees with ffmpeg | `g2g-launch -q filesrc location=$HOME/.cache/calliope/gst-medias/defaults/matroska/flac_jpeg.1.mkv ! decodebin ! videoconvert ! video/x-raw,format=I420 ! filesink location=/tmp/calliope-repro/out.yuv` |
| `gst-medias/defaults/mp4/av1-image-sequence.mp4/video` | av1 yuv444p | exit 1 | gstreamer fails or differs, g2g agrees with ffmpeg | `g2g-launch -q filesrc location=$HOME/.cache/calliope/gst-medias/defaults/mp4/av1-image-sequence.mp4 ! decodebin ! video/x-raw,format=Y444 ! filesink location=/tmp/calliope-repro/out.yuv` |
| `gst-medias/defaults/mpegts/cm5000_hevc_1080i_colorbars.ts/video` | hevc yuv420p | exit 1 | gstreamer fails or differs, g2g agrees with ffmpeg | `g2g-launch -q filesrc location=$HOME/.cache/calliope/gst-medias/defaults/mpegts/cm5000_hevc_1080i_colorbars.ts ! decodebin ! video/x-raw,format=I420 ! filesink location=/tmp/calliope-repro/out.yuv` |
| `gst-medias/defaults/webm/opus_vp9-alpha.webm/video` | vp9 yuv420p | 139 frames vs reference 140 | gstreamer fails or differs, g2g agrees with ffmpeg | `g2g-launch -q filesrc location=$HOME/.cache/calliope/gst-medias/defaults/webm/opus_vp9-alpha.webm ! decodebin ! video/x-raw,format=I420 ! filesink location=/tmp/calliope-repro/out.yuv` |
| `gst-medias/defaults/webm/vp8-alpha.webm/video` | vp8 yuv420p | 60 frames vs reference 61 | gstreamer fails or differs, g2g agrees with ffmpeg | `g2g-launch -q filesrc location=$HOME/.cache/calliope/gst-medias/defaults/webm/vp8-alpha.webm ! decodebin ! video/x-raw,format=I420 ! filesink location=/tmp/calliope-repro/out.yuv` |

## reference quirk

| scenario | codec | result | diagnosis | reproducer |
| :--- | :--- | :--- | :--- | :--- |
| `gst-medias/defaults/dash/mp4/mp4box-tfdt-per-segment-not-per-fragment_dashinit.mp4/video` | h264 yuv444p10le | ffmpeg is the outlier, diverged at frame 25 of 354 | reference quirk: gstreamer and g2g agree, ffmpeg is the outlier | `g2g-launch -q filesrc location=$HOME/.cache/calliope/gst-medias/defaults/dash/mp4/mp4box-tfdt-per-segment-not-per-fragment_dashinit.mp4 ! decodebin ! video/x-raw,format=Y444_10LE ! filesink location=/tmp/calliope-repro/out.yuv` |
| `gst-medias/defaults/mpegps/sample_mpeg_program_stream_scr.mpg/video` | mpeg2video yuv420p | ffmpeg is the outlier, diverged at frame 0 of 386 | reference quirk: gstreamer and g2g agree, ffmpeg is the outlier | `g2g-launch -q filesrc location=$HOME/.cache/calliope/gst-medias/defaults/mpegps/sample_mpeg_program_stream_scr.mpg ! decodebin ! video/x-raw,format=I420 ! filesink location=/tmp/calliope-repro/out.yuv` |
| `gst-medias/edit-lists/simple/edit_list_video_23456789_implicit_duration.mp4/video` | h264 yuv444p | ffmpeg engine produced no frames, gstreamer and g2g decoded | reference quirk: the track has duration 0 and ffmpeg emits nothing, gstreamer and g2g decode | `g2g-launch -q filesrc location=$HOME/.cache/calliope/gst-medias/edit-lists/simple/edit_list_video_23456789_implicit_duration.mp4 ! decodebin ! video/x-raw,format=Y444 ! filesink location=/tmp/calliope-repro/out.yuv` |

## unsupported codec

g2g-launch reports these itself: no typefind match for the container, or no decoder chain for the stream. The diagnosis column splits containers g2g cannot read from codecs it has no decoder for.

| codec | diagnosis | scenarios |
| :--- | :--- | ---: |
| aac | unsupported container | 6 |
| dvvideo yuv420p | unsupported container or codec | 1 |
| h264 yuv420p | unsupported container | 8 |
| h264 yuv422p10le | unsupported container | 1 |
| mpeg2video yuv422p | unsupported container | 5 |
| msmpeg4v3 yuv420p | unsupported container or codec | 1 |
| opus | unsupported container | 1 |
| pcm_s16le | unsupported container | 5 |
| pcm_s24le | unsupported container | 1 |
| svg none | unsupported container or codec | 1 |
| vp8 yuv420p | unsupported container | 1 |
| wmav2 | unsupported container or codec | 2 |
| wmv1 yuv420p | unsupported container or codec | 1 |
