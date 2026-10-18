import { test } from "node:test";
import assert from "node:assert/strict";
import { lines } from "../lines.js";

test("counts lines", () => {
  assert.equal(lines(""), 0);
  assert.equal(lines("a\nb\n"), 2);
  assert.equal(lines("a\nb"), 2);
});
