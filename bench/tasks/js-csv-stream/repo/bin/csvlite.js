#!/usr/bin/env node
import { readFile } from 'node:fs/promises';
import { main } from '../src/cli.js';

process.exitCode = await main(process.argv.slice(2), {
  stdin: process.stdin,
  stdout: process.stdout,
  stderr: process.stderr,
  readFile,
});
