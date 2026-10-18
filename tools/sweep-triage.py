import collections, json, os, re, subprocess, sys

if len(sys.argv) != 5:
    raise SystemExit('usage: sweep-triage.py <sweep report json> <title> <g2g revision> <triage md>')
report_path, title, g2g_revision, out_path = sys.argv[1:5]
entries = json.load(open(report_path))
BUGS_HEADING = '## g2g bugs'
# the bug list is written by hand, so a rerun keeps the one already in the triage file
previous = open(out_path).read() if os.path.exists(out_path) else ''
bugs_text = previous[previous.index(BUGS_HEADING):].split('\n## ', 1)[0] if BUGS_HEADING in previous else ''

SUPPORTED = set('h264 hevc vp8 vp9 av1 mpeg4 mpeg1video mpeg2video flv1 vp6f vp6a mjpeg png apng webp pgm ppm '
                'aac opus mp2 ac3 mp3 speex flac vorbis pcm_s16le pcm_f32le pcm_s24le pcm_s32le pcm_u8 pcm_mulaw pcm_alaw'.split())
# same lookup order as calliope's corpus cache_dir
CACHE = os.environ.get('CALLIOPE_CACHE') or os.path.join(os.environ.get('XDG_CACHE_HOME') or os.path.expanduser('~/.cache'), 'calliope')
FFMPEG_VERSION = subprocess.run(['ffmpeg', '-version'], capture_output=True, text=True).stdout.split()[2].split('-')[0]
TITLES = {
    'g2g-crash-or-hang': 'g2g crash / hang', 'g2g-diverges': 'g2g diverges', 'g2g-no-output': 'g2g no output',
    'gstreamer-diverges': 'gstreamer diverges', 'reference-quirk': 'reference quirk',
    'unsupported-codec': 'unsupported codec', 'agree': 'agree',
}
ORDER = list(TITLES)

FFMPEG_EXTRA_LAST_FRAME = 'reference quirk: ffmpeg emits one more final frame, gstreamer and g2g emit the same length. g2g is bit-exact with ffmpeg over every common sample'

FFMPEG_DECODES_TRUNCATED_AC3 = 'reference quirk: the last AC-3 packet is truncated and ffmpeg decodes it (1536 samples more), g2g drops it. g2g is bit-exact with ffmpeg over every common sample'

def resolution_change(frames):
    return f'harness: resolution changes mid-stream and the raw dump is hashed at one geometry. g2g is bit-exact with ffmpeg -noautoscale on all {frames} frames'

OVERRIDES = {
    # fate-suite g2g diverges
    'ac3/diatonis_invisible_order_anfos_ac3-small.wav/audio': 'reference quirk: the WAV header says 2 ch PCM, the payload is S/PDIF-wrapped AC-3. ffmpeg sniffs and decodes the AC-3, gstreamer and g2g play the PCM',
    'ac3/monsters_inc_2.0_192_small.ac3/audio': 'reference quirk: ffmpeg decodes the truncated last frame (1536 samples more), gstreamer and g2g drop it. g2g is bit-exact with ffmpeg over every common sample',
    'ac3/monsters_inc_5.1_448_small.ac3/audio': 'reference quirk: ffmpeg decodes the truncated last frame (1536 samples more), gstreamer and g2g drop it. g2g is bit-exact with ffmpeg over every common sample',
    'mpegts/mpegts_sdt_data_stream.ts/audio': FFMPEG_EXTRA_LAST_FRAME,
    'cavs/cavs.mpg/audio': FFMPEG_EXTRA_LAST_FRAME,
    'probe-format/roundup1414/audio': FFMPEG_EXTRA_LAST_FRAME,
    'h264/h264_intra_first-small.ts/audio': 'reference quirk: ffmpeg emits one more final MP2 frame, gstreamer fails. g2g is bit-exact with ffmpeg over every common sample',
    'mpegts/pmtchange.ts/audio': FFMPEG_DECODES_TRUNCATED_AC3,
    'ac3/mp3ac325-4864-small.ts/audio': FFMPEG_DECODES_TRUNCATED_AC3,
    'audiomatch/tones_opus_48000_stereo.mka/audio': 'reference quirk: Opus is not bit-exact across decoders (TESTING.md section 13). gstreamer and g2g differ by at most 4, ffmpeg by up to 126',
    'audiomatch/tones_opus_48000_stereo.opus/audio': 'reference quirk: Opus is not bit-exact across decoders (TESTING.md section 13)',
    'ogg-opus/chained-meta.ogg/audio': 'reference quirk: Opus is not bit-exact across decoders and gstreamer stops at the chain boundary, so there is no third vote. Lengths match',
    'av1-test-vectors/av1-1-b8-03-sizedown.ivf/video': resolution_change(20),
    'av1-test-vectors/av1-1-b8-03-sizeup.ivf/video': resolution_change(20),
    'av1/switch_frame.ivf/video': resolution_change(32),
    'h264/reinit-large_420_8-to-small_420_8.h264/video': resolution_change(100),
    'mpeg4/resize_down-down.h263/video': resolution_change(150),
    'mpeg4/resize_down-up.h263/video': resolution_change(150),
    'mpeg4/resize_up-down.h263/video': resolution_change(150),
    'mpeg4/resize_up-up.h263/video': resolution_change(150),
    'vp8/frame_size_change.webm/video': resolution_change(300),
    'vp9-test-vectors/vp90-2-05-resize.ivf/video': resolution_change(10),
    'h264-conformance/FM1_FT_E.264/video': 'reference quirk: FMO / ASO baseline stream libavcodec does not support. All three engines conceal differently from frame 119',
    'h264/data_partitioning.h264/video': 'reference quirk: libavcodec does not support data partitioning, ffmpeg and gstreamer fail. g2g output unverified',
    'h264/data_partitioning_ab.h264/video': 'reference quirk: data partitioning, see h264/data_partitioning.h264',
    'h264/data_partitioning_cip.h264/video': 'reference quirk: data partitioning, see h264/data_partitioning.h264',
    'mov/dovi-p7.mp4/video': 'reference quirk: framemd5 drops a frame with a repeated timestamp. g2g is bit-exact with ffmpeg -fps_mode passthrough on all 10 frames',
    'mov/empty_edit_5s.mp4/video': 'open: the stream has decode errors from frame 0 and gstreamer fails. ffmpeg threaded and single-thread decodes differ from frame 10, g2g differs from frame 1',
    'mpeg4/mpeg4_sstp_dpcm.m4v/video': 'open: Studio profile 4:2:2 10-bit, 25% of samples differ from ffmpeg with the same libavcodec 62, gstreamer fails',
    'probe-format/mp3-with-art/audio': 'reference quirk: ffmpeg n8.1 probes this MP3 as MPEG-PS and fails. With -f mp3 it decodes. gstreamer fails too',
    'webp/anim.webp/video': 'reference quirk: neither ffmpeg n8.1 nor gstreamer decodes animated WebP. g2g output unverified',
    'webp/anim_rgb_yuv.webp/video': 'reference quirk: animated WebP, see webp/anim.webp',
    'webp/anim_yuv_rgb.webp/video': 'reference quirk: animated WebP, see webp/anim.webp',
    # fate-suite no output and unsupported, cases the rules below would get wrong
    'jpg/jpg-8930-3.jpg/video': 'open: progressive 4:4:4 JPEG, MjpegDec fails CapsMismatch',
    'probe-format/roundup997/video': 'reference quirk: ffmpeg drops the first B-frame by timestamp in its default constant frame rate mode. g2g is bit-exact with ffmpeg -fps_mode passthrough on all 5 frames',
    'probe-format/roundup997/audio': 'reference quirk: the first PS audio packet opens on a partial MP2 frame and ffmpeg rejects the whole packet, losing the first whole frame, then decodes the truncated last one. g2g decodes the 7 whole frames, bit-exact with ffmpeg on the same frames',
    'pmp/demo.pmp/video': 'unsupported container: typefind finds the MP3 audio, PMP is not demuxed',
    'probe-format/mpegts-png-prefix/video': 'unsupported: PNG behind a junk prefix (an ffmpeg probe test)',
    'probe-format/mpegts-png-prefix-unaligned/video': 'unsupported: PNG behind a junk prefix (an ffmpeg probe test)',
    # gst-medias
    'edit-lists/simple/edit_list_video_23456789_implicit_duration.mp4/video': 'reference quirk: the track has duration 0 and ffmpeg emits nothing, gstreamer and g2g decode',
}

def short_id(scenario):
    return scenario.split('/', 1)[1].removeprefix('defaults/')

def override(scenario):
    key = short_id(scenario)
    return OVERRIDES.get(key) or OVERRIDES.get(scenario.split('/', 1)[1])

def diagnose(e):
    scenario = e['report']['scenario']
    forced = override(scenario)
    if forced:
        return forced
    note = e['note']
    codec = e['codec'].split()[0]
    cls = e['class']
    if cls == 'gstreamer-diverges':
        if 'trailing partial frame' in note and codec in ('vp9', 'vp8', 'av1', 'h264', 'hevc', 'mpeg4', 'mpeg2video'):
            return 'harness: gstreamer pads raw rows to 4 bytes (odd sizes) or changes resolution, so its dump does not split into frames'
        if note.startswith('timeout'):
            return 'gstreamer timeout, not a g2g divergence'
        return 'gstreamer fails or differs, g2g agrees with ffmpeg'
    if cls == 'reference-quirk':
        if codec == 'opus':
            return 'reference quirk: ffmpeg Opus drift (TESTING.md section 13), gstreamer and g2g agree'
        return 'reference quirk: gstreamer and g2g agree, ffmpeg is the outlier'
    if cls == 'g2g-diverges' and codec == 'opus':
        return 'reference quirk: Opus is not bit-exact across decoders. All three emit the same length and no two are bit-exact, the RFC 6716 vectors need opus_compare'
    # M1492's error, which calliope sweep before 2026-10-18 filed as no output
    if 'decodebin: no decoder for' in note:
        return 'unsupported codec'
    if cls == 'unsupported-codec':
        if 'unrecognized content' in note:
            return 'unsupported container' if codec in SUPPORTED else 'unsupported container or codec'
        return 'unsupported codec'
    if cls != 'g2g-no-output':
        return ''
    if 'MjpegDec0: CapsMismatch' in note or 'FfmpegVideoDec0: CapsMismatch' in note:
        return 'unsupported pixel format'
    if 'rejected mid-stream caps video/x-raw' in note:
        return 'harness: pixel format changes mid-stream and the capsfilter pins one format'
    if 'WebPDec and CapsFilter' in note:
        return 'harness: g2g decodes lossy WebP to RGBA, the differential pins I420 without a converter'
    if 'has no video stream to decode' in note:
        return 'unsupported: attached picture' if codec in ('mjpeg', 'png') else 'unsupported codec'
    raise SystemExit(f'no diagnosis for {scenario}: {note}')

def reproducer(e):
    program, _, args = (e.get('reproducer') or '').partition(' ')
    return f'{os.path.basename(program)} {args}'.replace(CACHE.rstrip('/') + '/', '$HOME/.cache/calliope/')

def cell(text):
    return text.replace('|', '/')

def bucket(diagnosis):
    if diagnosis.startswith('g2g bug'):
        return 'g2g bug'
    for prefix in ('unsupported', 'harness', 'reference quirk', 'open'):
        if diagnosis.startswith(prefix):
            return prefix
    if diagnosis.startswith('gstreamer'):
        return 'gstreamer only'
    return 'agree'

rows = collections.defaultdict(list)
for e in entries:
    e['diagnosis'] = diagnose(e) if e['class'] != 'agree' else ''
    rows[e['class']].append(e)

files = {e['vector'] for e in entries}
out = [f'# {title} sweep triage', '',
       f'{len(files)} files, {len(entries)} scenarios (one per video / audio stream), reference ffmpeg {FFMPEG_VERSION}. '
       f'g2g-launch is g2g {g2g_revision}. The class comes from `calliope sweep`. '
       'The diagnosis comes from rules over the g2g error and the stream layout, checked by rerunning representative files against ffmpeg and gstreamer. Every g2g diverges row was rerun by hand. '
       f'Reproducers read the sample tree from `$HOME/.cache/calliope/{title}`.', '',
       '| class | scenarios |', '| :--- | ---: |']
for cls in ORDER:
    if rows[cls]:
        out.append(f'| {TITLES[cls]} | {len(rows[cls])} |')
for e in entries:
    if e['class'] != 'agree' and bucket(e['diagnosis']) == 'agree':
        raise SystemExit(f"no diagnosis for {e['report']['scenario']} in class {e['class']}")
by_bucket = collections.Counter(bucket(e['diagnosis']) for e in entries if e['class'] != 'agree')
out += ['', 'Every scenario outside the agree class, by diagnosis:', '', '| diagnosis | scenarios |', '| :--- | ---: |']
for name in ('g2g bug', 'unsupported', 'harness', 'reference quirk', 'gstreamer only', 'open'):
    if by_bucket[name]:
        out.append(f'| {name} | {by_bucket[name]} |')
by_bug = collections.Counter(int(m.group(1)) for e in entries if (m := re.match(r'g2g bug (\d+)', e['diagnosis'])))
if by_bug:
    out += ['', 'Scenarios per g2g bug:', '', '| bug | scenarios |', '| ---: | ---: |']
for n in sorted(by_bug):
    out.append(f'| {n} | {by_bug[n]} |')
if bugs_text:
    out += ['', bugs_text.strip()]
for cls in ORDER:
    if cls in ('agree', 'unsupported-codec') or not rows[cls]:
        continue
    out += ['', f'## {TITLES[cls]}', '', '| scenario | codec | result | diagnosis | reproducer |', '| :--- | :--- | :--- | :--- | :--- |']
    natural = lambda text: [int(t) if t.isdigit() else t for t in re.split(r'(\d+)', text)]
    for e in sorted(rows[cls], key=lambda e: (natural(e['diagnosis']), e['report']['scenario'])):
        note = re.sub(r'runs/\S+/out\.\w+: ', '', e['note']).replace('; ', ', ')
        out.append(f"| `{e['report']['scenario']}` | {e['codec']} | {cell(note)} | {cell(e['diagnosis'])} | `{cell(reproducer(e))}` |")
if rows['unsupported-codec']:
    out += ['', '## unsupported codec', '',
            'g2g-launch reports these itself: no typefind match for the container, or no decoder chain for the stream. '
            'The diagnosis column splits containers g2g cannot read from codecs it has no decoder for.', '',
            '| codec | diagnosis | scenarios |', '| :--- | :--- | ---: |']
    counts = collections.Counter((e['codec'], e['diagnosis']) for e in rows['unsupported-codec'])
    for (codec, diagnosis), n in sorted(counts.items()):
        out.append(f'| {codec} | {diagnosis} | {n} |')
    pcm = [e for e in rows['unsupported-codec'] if e['diagnosis'].startswith('g2g bug')]
    if pcm:
        out += ['', 'Rows the sweep filed as unsupported that are g2g bugs:', '', '| scenario | codec | diagnosis | reproducer |', '| :--- | :--- | :--- | :--- |']
        for e in pcm:
            out.append(f"| `{e['report']['scenario']}` | {e['codec']} | {e['diagnosis']} | `{cell(reproducer(e))}` |")
open(out_path, 'w').write('\n'.join(out) + '\n')
print(dict(by_bucket), dict(sorted(by_bug.items())))
