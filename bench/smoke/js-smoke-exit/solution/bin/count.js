#!/usr/bin/env node
import { readFileSync } from "node:fs";
import { lines } from "../lines.js";

const [file] = process.argv.slice(2);
let text;
try {
  text = readFileSync(file, "utf8");
} catch {
  process.stderr.write(`count: cannot read ${file}\n`);
  process.exit(1);
}
console.log(lines(text));
