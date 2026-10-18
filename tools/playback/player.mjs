import { MediaPlayer } from '/lib/dashjs/dist/modern/esm/dash.all.min.js';
import { createFile, DataStream, Endianness, MP4BoxBuffer } from '/lib/mp4box/dist/mp4box.all.mjs';

const params = new URLSearchParams(location.search);
const mode = params.get('mode');
const source = params.get('src');
const liveSeconds = Number(params.get('seconds'));
const video = document.getElementById('video');
const MILLISECONDS_PER_SECOND = 1000;
const MICROSECONDS_PER_SECOND = 1e6;
// codec configuration boxes skip their 8 byte box header in a WebCodecs description
const BOX_HEADER_BYTES = 8;
const NOT_FOUND = 404;
const WHEP_RETRY_MILLISECONDS = 250;

function sleep(milliseconds) {
  return new Promise((resolve) => setTimeout(() => resolve(true), milliseconds));
}

let presented = 0;
function countPresentedFrames() {
  video.requestVideoFrameCallback(() => {
    presented += 1;
    countPresentedFrames();
  });
}

function finish(error) {
  if (window.calliopeResult) return;
  const quality = video.getVideoPlaybackQuality();
  window.calliopeResult = {
    mode,
    frames: quality.totalVideoFrames,
    dropped: quality.droppedVideoFrames,
    presented,
    error: error ?? (video.error ? `MediaError ${video.error.code}: ${video.error.message}` : null),
  };
}

function playToEnd() {
  countPresentedFrames();
  video.addEventListener('ended', () => finish(null));
  video.addEventListener('error', () => finish(null));
  video.play().catch((error) => finish(`play(): ${error}`));
}

function playHls() {
  if (!window.Hls.isSupported()) return finish('hls.js: media source extensions unsupported');
  const hls = new window.Hls();
  hls.on(window.Hls.Events.ERROR, (_, data) => {
    if (data.fatal) finish(`hls.js ${data.type}: ${data.details}`);
  });
  hls.loadSource(source);
  hls.attachMedia(video);
  playToEnd();
}

function playDash() {
  const player = MediaPlayer().create();
  player.on('error', (event) => finish(`dash.js: ${JSON.stringify(event.error)}`));
  player.initialize(video, source, false);
  playToEnd();
}

async function playWhep() {
  const connection = new RTCPeerConnection();
  connection.addTransceiver('video', { direction: 'recvonly' });
  connection.addTransceiver('audio', { direction: 'recvonly' });
  connection.ontrack = (event) => {
    if (event.track.kind === 'video') video.srcObject = event.streams[0] ?? new MediaStream([event.track]);
  };
  connection.onconnectionstatechange = () => {
    if (connection.connectionState === 'failed') finish('peer connection failed');
  };
  await connection.setLocalDescription(await connection.createOffer());
  await new Promise((resolve) => {
    if (connection.iceGatheringState === 'complete') return resolve();
    connection.onicegatheringstatechange = () => connection.iceGatheringState === 'complete' && resolve();
  });
  // the publisher may not be up yet, the server answers 404 until it is
  let response;
  do {
    response = await fetch(source, {
      method: 'POST',
      headers: { 'Content-Type': 'application/sdp' },
      body: connection.localDescription.sdp,
    });
  } while (response.status === NOT_FOUND && (await sleep(WHEP_RETRY_MILLISECONDS)));
  if (!response.ok) return finish(`WHEP POST ${response.status}: ${await response.text()}`);
  await connection.setRemoteDescription({ type: 'answer', sdp: await response.text() });
  countPresentedFrames();
  video.play().catch((error) => finish(`play(): ${error}`));
  setTimeout(() => finish(null), liveSeconds * MILLISECONDS_PER_SECOND);
}

function codecDescription(file, trackId) {
  const entry = file.getTrackById(trackId).mdia.minf.stbl.stsd.entries[0];
  const box = entry.avcC ?? entry.hvcC ?? entry.av1C ?? entry.vpcC;
  if (!box) return undefined;
  const stream = new DataStream(undefined, 0, Endianness.BIG_ENDIAN);
  box.write(stream);
  return new Uint8Array(stream.buffer, BOX_HEADER_BYTES);
}

async function decodeWebCodecs() {
  const bytes = await (await fetch(source)).arrayBuffer();
  const file = createFile();
  const samples = [];
  let info = null;
  let parseError = null;
  // extraction has to be armed inside onReady, before appendBuffer returns
  file.onError = (e) => {
    parseError = `mp4box.js: ${e}`;
  };
  file.onReady = (ready) => {
    info = ready;
    const track = ready.videoTracks[0];
    if (!track) return;
    file.setExtractionOptions(track.id, null, { nbSamples: Number.MAX_SAFE_INTEGER });
    file.start();
  };
  file.onSamples = (_, __, chunk) => samples.push(...chunk);
  file.appendBuffer(MP4BoxBuffer.fromArrayBuffer(bytes, 0));
  file.flush();
  const track = info?.videoTracks[0];
  if (!track) return finishDecode(0, 0, parseError ?? 'no video track');
  const config = {
    codec: track.codec,
    codedWidth: track.video.width,
    codedHeight: track.video.height,
    description: codecDescription(file, track.id),
  };
  const support = await VideoDecoder.isConfigSupported(config);
  if (!support.supported) return finishDecode(0, samples.length, `VideoDecoder does not support ${track.codec}`);
  let decoded = 0;
  let error = parseError;
  const decoder = new VideoDecoder({
    output: (frame) => {
      decoded += 1;
      frame.close();
    },
    error: (e) => {
      error ??= `VideoDecoder: ${e.message}`;
    },
  });
  decoder.configure(config);
  for (const sample of samples) {
    decoder.decode(new EncodedVideoChunk({
      type: sample.is_sync ? 'key' : 'delta',
      timestamp: (sample.cts * MICROSECONDS_PER_SECOND) / sample.timescale,
      duration: (sample.duration * MICROSECONDS_PER_SECOND) / sample.timescale,
      data: sample.data,
    }));
  }
  await decoder.flush().catch((e) => {
    error ??= `VideoDecoder flush: ${e.message}`;
  });
  finishDecode(decoded, samples.length, error);
}

function finishDecode(frames, samples, error) {
  window.calliopeResult = { mode, frames, samples, error };
}

const modes = { hls: playHls, dash: playDash, whep: playWhep, webcodecs: decodeWebCodecs };
const run = modes[mode];
if (!run) {
  window.calliopeResult = { mode, frames: 0, error: `unknown mode ${mode}` };
} else {
  Promise.resolve()
    .then(run)
    .catch((error) => {
      if (!window.calliopeResult) window.calliopeResult = { mode, frames: 0, error: String(error) };
    });
}
