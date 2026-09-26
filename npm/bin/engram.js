#!/usr/bin/env node
'use strict';

const fs = require('fs');
const path = require('path');
const { spawnSync } = require('child_process');

const binName = process.platform === 'win32' ? 'engram.exe' : 'engram';
const binPath = path.join(__dirname, binName);

if (!fs.existsSync(binPath)) {
  console.error(`Engram native binary is missing at ${binPath}.`);
  console.error('Try reinstalling the package, or install from source with: cargo install --git https://github.com/clickety-clacks/engram');
  process.exit(1);
}

const result = spawnSync(binPath, process.argv.slice(2), { stdio: 'inherit' });
if (result.error) {
  console.error(result.error.message);
  process.exit(1);
}

if (result.signal) {
  process.kill(process.pid, result.signal);
} else {
  process.exit(result.status == null ? 1 : result.status);
}
