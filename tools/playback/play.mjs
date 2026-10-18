// usage: node play.mjs <hls|dash|webcodecs|whep> <file-or-url> [--seconds N] [--timeout N]
// prints one json line: {"mode","frames","error",...}, exits 3 when no browser could run
import { createReadStream, existsSync, readdirSync, statSync } from 'node:fs';
import { createServer } from 'node:http';
import { homedir } from 'node:os';
import { basename, dirname, extname, join, resolve, sep } from 'node:path';
import { fileURLToPath } from 'node:url';
import { chromium } from 'playwright-core';

const HARNESS_DIR = dirname(fileURLToPath(import.meta.url));
const LIBRARY_DIR = join(HARNESS_DIR, 'node_modules');
const LOOPBACK = '127.0.0.1';
const DEFAULT_LIVE_SECONDS = 8;
const DEFAULT_TIMEOUT_SECONDS = 60;
const MILLISECONDS_PER_SECOND = 1000;
const HARNESS_UNAVAILABLE_EXIT = 3;
const CONTENT_TYPES = {
  '.html': 'text/html',
  '.js': 'text/javascript',
  '.mjs': 'text/javascript',
  '.m3u8': 'application/vnd.apple.mpegurl',
  '.mpd': 'application/dash+xml',
  '.ts': 'video/mp2t',
  '.m4s': 'video/iso.segment',
  '.mp4': 'video/mp4',
  '.m4v': 'video/mp4',
  '.m4a': 'audio/mp4',
};

function option(name, fallback) {
  const index = process.argv.indexOf(`--${name}`);
  return index > 0 ? Number(process.argv[index + 1]) : fallback;
}

function report(result, exitCode) {
  console.log(JSON.stringify(result));
  process.exit(exitCode);
}

// playwright-core downloads no browser: take CALLIOPE_CHROMIUM, else the newest one playwright installed
function chromiumExecutable() {
  if (process.env.CALLIOPE_CHROMIUM) return process.env.CALLIOPE_CHROMIUM;
  const cache = join(homedir(), '.cache', 'ms-playwright');
  if (!existsSync(cache)) return undefined;
  const builds = readdirSync(cache)
    .filter((name) => /^chromium-\d+$/.test(name))
    .sort((a, b) => Number(b.split('-')[1]) - Number(a.split('-')[1]));
  for (const build of builds) {
    for (const platform of ['chrome-linux64', 'chrome-linux']) {
      const candidate = join(cache, build, platform, 'chrome');
      if (existsSync(candidate)) return candidate;
    }
  }
  return undefined;
}

// serves the harness page, the js libraries and one media directory, nothing else
function serve(mediaDir) {
  const roots = { harness: HARNESS_DIR, lib: LIBRARY_DIR, media: mediaDir };
  const server = createServer((request, response) => {
    const path = decodeURIComponent(new URL(request.url, `http://${LOOPBACK}`).pathname);
    const [, root, ...rest] = path.split('/');
    const base = roots[root];
    const file = base && resolve(base, rest.join('/'));
    if (!file || !file.startsWith(base + sep) || !existsSync(file) || !statSync(file).isFile()) {
      response.writeHead(404).end();
      return;
    }
    response.writeHead(200, {
      'Content-Type': CONTENT_TYPES[extname(file)] ?? 'application/octet-stream',
      'Access-Control-Allow-Origin': '*',
    });
    createReadStream(file).pipe(response);
  });
  return new Promise((ready) => server.listen(0, LOOPBACK, () => ready(server)));
}

const [mode, target] = process.argv.slice(2);
if (!mode || !target) report({ mode, frames: 0, error: 'usage: play.mjs <mode> <file-or-url>' }, HARNESS_UNAVAILABLE_EXIT);
const liveSeconds = option('seconds', DEFAULT_LIVE_SECONDS);
const timeoutSeconds = option('timeout', DEFAULT_TIMEOUT_SECONDS);

const live = mode === 'whep';
const server = await serve(live ? HARNESS_DIR : dirname(resolve(target)));
const source = live ? target : `/media/${encodeURIComponent(basename(target))}`;
const pageUrl = `http://${LOOPBACK}:${server.address().port}/harness/player.html?${new URLSearchParams({
  mode,
  src: source,
  seconds: String(liveSeconds),
})}`;

const browser = await chromium
  .launch({
    headless: true,
    executablePath: chromiumExecutable(),
    args: ['--autoplay-policy=no-user-gesture-required'],
  })
  .catch((error) => report({ mode, frames: 0, error: `launch chromium: ${error.message}` }, HARNESS_UNAVAILABLE_EXIT));
const page = await browser.newPage();
const consoleErrors = [];
page.on('console', (message) => message.type() === 'error' && consoleErrors.push(message.text()));
page.on('pageerror', (error) => consoleErrors.push(error.message));
await page.goto(pageUrl);
try {
  await page.waitForFunction(() => window.calliopeResult, null, {
    timeout: timeoutSeconds * MILLISECONDS_PER_SECOND,
  });
  const result = await page.evaluate(() => window.calliopeResult);
  await browser.close();
  server.close();
  report({ ...result, consoleErrors }, 0);
} catch (error) {
  const partial = await page.evaluate(() => ({
    frames: document.getElementById('video').getVideoPlaybackQuality().totalVideoFrames,
  }));
  await browser.close();
  server.close();
  report({ mode, frames: partial.frames, error: `no result: ${error.message}`, consoleErrors }, 0);
}
