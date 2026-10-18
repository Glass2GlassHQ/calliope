# gst-medias sweep triage

218 files, 284 scenarios (one per video / audio stream), reference ffmpeg n8.1.2. g2g-launch is g2g master b84a510a built with linux-full minus vaapi, pipewire and portal in release. The class comes from `calliope sweep`. The diagnosis comes from rules over the g2g error and the stream layout, checked by rerunning representative files against ffmpeg and gstreamer. Every g2g diverges row was rerun by hand. Reproducers read the sample tree from `$HOME/.cache/calliope/gst-medias`.

| class | scenarios |
| :--- | ---: |
| g2g diverges | 1 |
| g2g no output | 21 |
| gstreamer diverges | 9 |
| reference quirk | 3 |
| unsupported codec | 34 |
| agree | 216 |

Every scenario outside the agree class, by diagnosis:

| diagnosis | scenarios |
| :--- | ---: |
| g2g bug | 13 |
| unsupported | 43 |
| reference quirk | 3 |
| gstreamer only | 9 |

Scenarios per g2g bug:

| bug | scenarios |
| ---: | ---: |
| 1 | 1 |
| 2 | 1 |
| 4 | 1 |
| 6 | 9 |
| 7 | 1 |
| 11 | 3 |

Bug 11 rows are also counted under unsupported, the codec is missing either way.

## g2g bugs

Bug numbers refer to the list in `triage/fate-suite.md`, which gives one reproducer and the expected and actual result per bug. Bug 4 is reproduced on a file in this tree. The tables below give every affected scenario its own reproducer. The HLS playlists and DASH manifests are listed as unsupported container because `filesrc ! decodebin` does not open them, g2g reads them through its own source elements.

## g2g diverges

| scenario | codec | result | diagnosis | reproducer |
| :--- | :--- | :--- | :--- | :--- |
| `gst-medias/defaults/mpegps/sample_mpeg_program_stream_scr.mpg/audio` | mp2 | diverged at frame 0 of 1 | g2g bug 4: twice the samples, the two MP2 streams interleave | `g2g-launch -q filesrc location=$HOME/.cache/calliope/gst-medias/defaults/mpegps/sample_mpeg_program_stream_scr.mpg ! decodebin ! audioconvert dithering=none ! audioresample ! audio/x-raw,format=S16LE,rate=48000,channels=2 ! filesink location=/tmp/calliope-repro/out.pcm` |

## g2g no output

| scenario | codec | result | diagnosis | reproducer |
| :--- | :--- | :--- | :--- | :--- |
| `gst-medias/defaults/matroska/test5.mkv/audio` | aac | no output | g2g bug 1 | `g2g-launch -q filesrc location=$HOME/.cache/calliope/gst-medias/defaults/matroska/test5.mkv ! decodebin ! audioconvert dithering=none ! audioresample ! audio/x-raw,format=S16LE,rate=48000,channels=2 ! filesink location=/tmp/calliope-repro/out.pcm` |
| `gst-medias/defaults/exMPD_BIP_TC1/ElephantsDream_AAC48K_064.mp4/audio` | aac | ERROR runtime pipeline error in Mp4Demux0: CapsMismatch | g2g bug 2 | `g2g-launch -q filesrc location=$HOME/.cache/calliope/gst-medias/defaults/exMPD_BIP_TC1/ElephantsDream_AAC48K_064.mp4 ! decodebin ! audioconvert dithering=none ! audioresample ! audio/x-raw,format=S16LE,rate=48000,channels=2 ! filesink location=/tmp/calliope-repro/out.pcm` |
| `gst-medias/defaults/mp4/mp3_h264.0.mp4/audio` | mp3 | parse error: decodebin: no decoder for .mp3 in /home/aaron/.cache/calliope/gst-medias/defaults/mp4/mp3_h264.0.mp4 | g2g bug 6 | `g2g-launch -q filesrc location=$HOME/.cache/calliope/gst-medias/defaults/mp4/mp3_h264.0.mp4 ! decodebin ! audioconvert dithering=none ! audioresample ! audio/x-raw,format=S16LE,rate=44100,channels=1 ! filesink location=/tmp/calliope-repro/out.pcm` |
| `gst-medias/defaults/mp4/mp3_h264.1.mp4/audio` | mp3 | parse error: decodebin: no decoder for .mp3 in /home/aaron/.cache/calliope/gst-medias/defaults/mp4/mp3_h264.1.mp4 | g2g bug 6 | `g2g-launch -q filesrc location=$HOME/.cache/calliope/gst-medias/defaults/mp4/mp3_h264.1.mp4 ! decodebin ! audioconvert dithering=none ! audioresample ! audio/x-raw,format=S16LE,rate=44100,channels=1 ! filesink location=/tmp/calliope-repro/out.pcm` |
| `gst-medias/defaults/mp4/mp3_h265.0.mp4/audio` | mp3 | parse error: decodebin: no decoder for mp4a in /home/aaron/.cache/calliope/gst-medias/defaults/mp4/mp3_h265.0.mp4 | g2g bug 6 | `g2g-launch -q filesrc location=$HOME/.cache/calliope/gst-medias/defaults/mp4/mp3_h265.0.mp4 ! decodebin ! audioconvert dithering=none ! audioresample ! audio/x-raw,format=S16LE,rate=44100,channels=1 ! filesink location=/tmp/calliope-repro/out.pcm` |
| `gst-medias/defaults/mp4/multi_audio_5tones.mov/audio` | pcm_s16le | parse error: decodebin: no decoder for sowt, sowt, sowt, sowt, sowt in /home/aaron/.cache/calliope/gst-medias/defaults/mp4/multi_audio_5tones.mov | g2g bug 6 | `g2g-launch -q filesrc location=$HOME/.cache/calliope/gst-medias/defaults/mp4/multi_audio_5tones.mov ! decodebin ! audioconvert dithering=none ! audioresample ! audio/x-raw,format=S16LE,rate=48000,channels=1 ! filesink location=/tmp/calliope-repro/out.pcm` |
| `gst-medias/defaults/mp4/prores_s8.mov/audio` | pcm_s8 | parse error: decodebin: no decoder for sowt, ap4h in /home/aaron/.cache/calliope/gst-medias/defaults/mp4/prores_s8.mov | g2g bug 6 | `g2g-launch -q filesrc location=$HOME/.cache/calliope/gst-medias/defaults/mp4/prores_s8.mov ! decodebin ! audioconvert dithering=none ! audioresample ! audio/x-raw,format=S16LE,rate=48000,channels=1 ! filesink location=/tmp/calliope-repro/out.pcm` |
| `gst-medias/defaults/mp4/raw_h264.0.mp4/audio` | pcm_s16le | parse error: decodebin: no decoder for sowt in /home/aaron/.cache/calliope/gst-medias/defaults/mp4/raw_h264.0.mp4 | g2g bug 6 | `g2g-launch -q filesrc location=$HOME/.cache/calliope/gst-medias/defaults/mp4/raw_h264.0.mp4 ! decodebin ! audioconvert dithering=none ! audioresample ! audio/x-raw,format=S16LE,rate=44100,channels=1 ! filesink location=/tmp/calliope-repro/out.pcm` |
| `gst-medias/defaults/mp4/raw_h264.1.mp4/audio` | pcm_s16le | parse error: decodebin: no decoder for sowt in /home/aaron/.cache/calliope/gst-medias/defaults/mp4/raw_h264.1.mp4 | g2g bug 6 | `g2g-launch -q filesrc location=$HOME/.cache/calliope/gst-medias/defaults/mp4/raw_h264.1.mp4 ! decodebin ! audioconvert dithering=none ! audioresample ! audio/x-raw,format=S16LE,rate=44100,channels=1 ! filesink location=/tmp/calliope-repro/out.pcm` |
| `gst-medias/defaults/mp4/rawaudioS32LE_prores.mov/audio` | pcm_s32le | parse error: decodebin: no decoder for apcn, sowt in /home/aaron/.cache/calliope/gst-medias/defaults/mp4/rawaudioS32LE_prores.mov | g2g bug 6 | `g2g-launch -q filesrc location=$HOME/.cache/calliope/gst-medias/defaults/mp4/rawaudioS32LE_prores.mov ! decodebin ! audioconvert dithering=none ! audioresample ! audio/x-raw,format=S16LE,rate=44100,channels=1 ! filesink location=/tmp/calliope-repro/out.pcm` |
| `gst-medias/defaults/ogg/wpt-test-ogg-vp8.ogv/video` | vp8 yuv420p | ERROR caps no caps overlap between VorbisDec and CapsFilter | g2g bug 6: VP8 in Ogg | `g2g-launch -q filesrc location=$HOME/.cache/calliope/gst-medias/defaults/ogg/wpt-test-ogg-vp8.ogv ! decodebin ! video/x-raw,format=I420 ! filesink location=/tmp/calliope-repro/out.yuv` |
| `gst-medias/defaults/avi/bowlerhatdancer.sleepytom.SGP.mjpeg.avi/video` | mjpeg yuvj422p | ERROR runtime pipeline error in MjpegDec0: CapsMismatch | g2g bug 7 | `g2g-launch -q filesrc location=$HOME/.cache/calliope/gst-medias/defaults/avi/bowlerhatdancer.sleepytom.SGP.mjpeg.avi ! decodebin ! videoconvert ! video/x-raw,format=I420 ! filesink location=/tmp/calliope-repro/out.yuv` |
| `gst-medias/defaults/avi/raw_video.avi/video` | rawvideo yuv420p | parse error: decodebin: no decoder for I420 in /home/aaron/.cache/calliope/gst-medias/defaults/avi/raw_video.avi | unsupported codec | `g2g-launch -q filesrc location=$HOME/.cache/calliope/gst-medias/defaults/avi/raw_video.avi ! decodebin ! video/x-raw,format=I420 ! filesink location=/tmp/calliope-repro/out.yuv` |
| `gst-medias/defaults/avi/samples.multimedia.cx_testsuite_iv31.avi/video` | indeo3 yuv410p | parse error: decodebin: no decoder for IV31 in /home/aaron/.cache/calliope/gst-medias/defaults/avi/samples.multimedia.cx_testsuite_iv31.avi | unsupported codec | `g2g-launch -q filesrc location=$HOME/.cache/calliope/gst-medias/defaults/avi/samples.multimedia.cx_testsuite_iv31.avi ! decodebin ! videoconvert ! video/x-raw,format=I420 ! filesink location=/tmp/calliope-repro/out.yuv` |
| `gst-medias/defaults/matroska/raw_video.mkv/video` | rawvideo yuv420p | parse error: decodebin: no decoder for V_UNCOMPRESSED in /home/aaron/.cache/calliope/gst-medias/defaults/matroska/raw_video.mkv | unsupported codec | `g2g-launch -q filesrc location=$HOME/.cache/calliope/gst-medias/defaults/matroska/raw_video.mkv ! decodebin ! video/x-raw,format=I420 ! filesink location=/tmp/calliope-repro/out.yuv` |
| `gst-medias/defaults/mp4/prores_s8.mov/video` | prores yuva444p12le | parse error: decodebin: no decoder for sowt, ap4h in /home/aaron/.cache/calliope/gst-medias/defaults/mp4/prores_s8.mov | unsupported codec | `g2g-launch -q filesrc location=$HOME/.cache/calliope/gst-medias/defaults/mp4/prores_s8.mov ! decodebin ! videoconvert ! video/x-raw,format=I420 ! filesink location=/tmp/calliope-repro/out.yuv` |
| `gst-medias/defaults/mp4/raw_video.mov/video` | rawvideo uyvy422 | parse error: decodebin: no decoder for 2vuy in /home/aaron/.cache/calliope/gst-medias/defaults/mp4/raw_video.mov | unsupported codec | `g2g-launch -q filesrc location=$HOME/.cache/calliope/gst-medias/defaults/mp4/raw_video.mov ! decodebin ! videoconvert ! video/x-raw,format=I420 ! filesink location=/tmp/calliope-repro/out.yuv` |
| `gst-medias/defaults/mp4/rawaudioS32LE_prores.mov/video` | prores yuv422p10le | parse error: decodebin: no decoder for apcn, sowt in /home/aaron/.cache/calliope/gst-medias/defaults/mp4/rawaudioS32LE_prores.mov | unsupported codec | `g2g-launch -q filesrc location=$HOME/.cache/calliope/gst-medias/defaults/mp4/rawaudioS32LE_prores.mov ! decodebin ! video/x-raw,format=I422_10LE ! filesink location=/tmp/calliope-repro/out.yuv` |
| `gst-medias/defaults/ogg/numerated_frames_blue.ogv/video` | theora yuv420p | ERROR caps no caps overlap between OpusDec and CapsFilter | unsupported codec, the theora video request gets an audio chain (bug 11) | `g2g-launch -q filesrc location=$HOME/.cache/calliope/gst-medias/defaults/ogg/numerated_frames_blue.ogv ! decodebin ! video/x-raw,format=I420 ! filesink location=/tmp/calliope-repro/out.yuv` |
| `gst-medias/defaults/ogg/vorbis_theora.0.ogg/video` | theora yuv420p | ERROR caps no caps overlap between VorbisDec and CapsFilter | unsupported codec, the theora video request gets an audio chain (bug 11) | `g2g-launch -q filesrc location=$HOME/.cache/calliope/gst-medias/defaults/ogg/vorbis_theora.0.ogg ! decodebin ! video/x-raw,format=I420 ! filesink location=/tmp/calliope-repro/out.yuv` |
| `gst-medias/defaults/ogg/vorbis_theora.1.ogg/video` | theora yuv420p | ERROR caps no caps overlap between VorbisDec and CapsFilter | unsupported codec, the theora video request gets an audio chain (bug 11) | `g2g-launch -q filesrc location=$HOME/.cache/calliope/gst-medias/defaults/ogg/vorbis_theora.1.ogg ! decodebin ! video/x-raw,format=I420 ! filesink location=/tmp/calliope-repro/out.yuv` |

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
