import { expect, test } from "bun:test";
import { AudioFrameBuffer } from "./audio-buffer.js";

test("AudioFrameBuffer emits fixed-size frames and preserves partial input", () => {
  const buffer = new AudioFrameBuffer(4);
  const frames: Buffer[] = [];

  buffer.push(Buffer.from([1, 2, 3]));
  buffer.drainFrames((frame) => frames.push(Buffer.from(frame)));
  expect(frames).toHaveLength(0);
  expect(buffer.pendingBytes).toBe(3);

  buffer.push(Buffer.from([4, 5, 6, 7, 8, 9]));
  buffer.drainFrames((frame) => frames.push(Buffer.from(frame)));

  expect(frames.map((frame) => [...frame])).toEqual([
    [1, 2, 3, 4],
    [5, 6, 7, 8],
  ]);
  expect(buffer.pendingBytes).toBe(1);
});

test("AudioFrameBuffer clears pending partial data", () => {
  const buffer = new AudioFrameBuffer(4);

  buffer.push(Buffer.from([1, 2, 3]));
  buffer.clear();

  expect(buffer.pendingBytes).toBe(0);
});
