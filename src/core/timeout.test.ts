import { expect, test } from "bun:test";
import { withTimeout } from "./timeout.js";

test("withTimeout rejects when the wrapped operation stalls", async () => {
  await expect(
    withTimeout(
      new Promise(() => {}),
      1,
      "connect timed out"
    )
  ).rejects.toThrow("connect timed out");
});
