#!/usr/bin/env node
'use strict';

const { existsSync } = require('node:fs');
const { spawn } = require('node:child_process');
const { installedBinary } = require('../lib.cjs');

async function main() {
  const binary = installedBinary();
  if (!existsSync(binary)) await require('../install.cjs').install();
  const child = spawn(binary, process.argv.slice(2), { stdio: 'inherit' });
  const handlers = new Map();
  for (const signal of ['SIGINT', 'SIGTERM', 'SIGHUP']) {
    const handler = () => { if (!child.killed) child.kill(signal); };
    handlers.set(signal, handler);
    process.on(signal, handler);
  }
  const cleanup = () => {
    for (const [signal, handler] of handlers) process.removeListener(signal, handler);
  };
  child.on('error', error => {
    cleanup();
    console.error(`rpi: ${error.message}`);
    process.exitCode = 1;
  });
  child.on('exit', (code, signal) => {
    cleanup();
    if (signal && process.platform !== 'win32') process.kill(process.pid, signal);
    else process.exitCode = code ?? 1;
  });
}

main().catch(error => {
  console.error(`rpi: ${error.message}`);
  process.exitCode = 1;
});
