import { test } from "node:test";
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const count = (...args) => spawnSync(process.execPath, ["bin/count.js", ...args], { encoding: "utf8" });

test("a missing file is reported, not thrown", () => {
  const r = count("no/such/file.txt");
  assert.equal(r.status, 1);
  assert.equal(r.stdout, "");
  assert.equal(r.stderr, "count: cannot read no/such/file.txt\n");
});

test("a readable file is counted", () => {
  const dir = mkdtempSync(join(tmpdir(), "count-"));
  const file = join(dir, "notes.txt");
  writeFileSync(file, "one\ntwo\nthree");
  const r = count(file);
  assert.equal(r.status, 0);
  assert.equal(r.stdout, "3\n");
  assert.equal(r.stderr, "");
});
