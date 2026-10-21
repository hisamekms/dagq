import { test } from "node:test";
import assert from "node:assert/strict";
import { Ledger } from "../src/ledger.js";

test("an item added is counted", () => {
  const ledger = new Ledger();
  ledger.add("pen", 3);
  assert.equal(ledger.count("pen"), 3);
});

test("taking more than is in stock is refused", () => {
  const ledger = new Ledger();
  ledger.add("pen", 1);
  assert.throws(() => ledger.take("pen", 2));
  assert.equal(ledger.count("pen"), 1);
});
