import { expect, test } from "bun:test";
import { AsyncTaskQueue } from "./async-task-queue.js";

test("AsyncTaskQueue runs tasks sequentially even when callers do not await", async () => {
  const queue = new AsyncTaskQueue();
  const events: string[] = [];
  let releaseFirst: (() => void) | undefined;

  queue.add(async () => {
    events.push("first:start");
    await new Promise<void>((resolve) => {
      releaseFirst = resolve;
    });
    events.push("first:end");
  });

  queue.add(async () => {
    events.push("second");
  });

  await new Promise((resolve) => setImmediate(resolve));
  expect(events).toEqual(["first:start"]);

  releaseFirst?.();
  await queue.idle();

  expect(events).toEqual(["first:start", "first:end", "second"]);
});
