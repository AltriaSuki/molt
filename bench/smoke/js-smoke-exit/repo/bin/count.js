#!/usr/bin/env node
import { readFileSync } from "node:fs";
import { lines } from "../lines.js";

const [file] = process.argv.slice(2);
console.log(lines(readFileSync(file, "utf8")));
