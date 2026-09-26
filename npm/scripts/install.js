#!/usr/bin/env node
'use strict';

const fs = require('fs');
const https = require('https');
const os = require('os');
const path = require('path');

const pkg = require('../../package.json');
const repo = pkg.engram && pkg.engram.githubRepo ? pkg.engram.githubRepo : 'clickety-clacks/engram';
const dryRun = process.argv.includes('--dry-run');

function target() {
  const platform = process.platform;
  const arch = process.arch;

  if (platform === 'darwin' && arch === 'arm64') return { asset: 'engram-aarch64-apple-darwin', bin: 'engram' };
  if (platform === 'linux' && arch === 'x64') return { asset: 'engram-x86_64-unknown-linux-gnu', bin: 'engram' };
  if (platform === 'win32' && arch === 'x64') return { asset: 'engram-x86_64-pc-windows-msvc.exe', bin: 'engram.exe' };

  throw new Error(`Unsupported platform for prebuilt Engram binary: ${platform}/${arch}. Install from source with: cargo install --git https://github.com/${repo}`);
}

function download(url, destination) {
  return new Promise((resolve, reject) => {
    const request = https.get(url, { headers: { 'User-Agent': `${pkg.name}/${pkg.version}` } }, (response) => {
      if ([301, 302, 303, 307, 308].includes(response.statusCode)) {
        response.resume();
        download(response.headers.location, destination).then(resolve, reject);
        return;
      }
      if (response.statusCode !== 200) {
        response.resume();
        reject(new Error(`Download failed (${response.statusCode}) for ${url}`));
        return;
      }
      const file = fs.createWriteStream(destination, { mode: 0o755 });
      response.pipe(file);
      file.on('finish', () => file.close(resolve));
      file.on('error', reject);
    });
    request.on('error', reject);
  });
}

(async () => {
  const selected = target();
  const version = pkg.version.startsWith('v') ? pkg.version : `v${pkg.version}`;
  const url = `https://github.com/${repo}/releases/download/${version}/${selected.asset}`;
  const outDir = path.join(__dirname, '..', 'bin');
  const outPath = path.join(outDir, selected.bin);

  if (dryRun) {
    console.log(`Would install ${url} -> ${outPath}`);
    return;
  }

  fs.mkdirSync(outDir, { recursive: true });
  const tmpPath = path.join(os.tmpdir(), `${selected.asset}.${process.pid}`);
  await download(url, tmpPath);
  fs.renameSync(tmpPath, outPath);
  if (process.platform !== 'win32') fs.chmodSync(outPath, 0o755);
  console.log(`Installed Engram ${pkg.version} for ${process.platform}/${process.arch}`);
})().catch((error) => {
  console.error(error.message);
  process.exit(1);
});
