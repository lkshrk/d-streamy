import { expect, test } from "bun:test";
import { PassThrough } from "stream";
import { FrameType, PipeReader } from "./pipe.js";

test("PipeReader rejects oversized frames instead of buffering indefinitely", async () => {
  const source = new PassThrough();
  const reader = new PipeReader(source, { maxFrameBytes: 8 });
  const errors: Error[] = [];

  reader.on("error", (err) => errors.push(err));

  const header = Buffer.alloc(13);
  header[0] = FrameType.Video;
  header.writeBigInt64LE(0n, 1);
  header.writeUInt32LE(9, 9);

  source.write(header);
  await new Promise((resolve) => setImmediate(resolve));

  expect(errors).toHaveLength(1);
  expect(errors[0].message).toContain("frame length");
});

test("PipeReader emits frames split across chunks", async () => {
  const source = new PassThrough();
  const reader = new PipeReader(source);
  const videoFrames: Buffer[] = [];

  reader.on("video", (data) => videoFrames.push(Buffer.from(data)));

  const frame = Buffer.from([1, 2, 3, 4]);
  const header = Buffer.alloc(13);
  header[0] = FrameType.Video;
  header.writeBigInt64LE(123n, 1);
  header.writeUInt32LE(frame.length, 9);

  source.write(Buffer.concat([header.subarray(0, 7)]));
  source.write(Buffer.concat([header.subarray(7), frame]));
  await new Promise((resolve) => setImmediate(resolve));

  expect(videoFrames.map((data) => [...data])).toEqual([[1, 2, 3, 4]]);
  expect(reader.pendingBytes).toBe(0);
});

test("PipeReader emits end when source reaches EOF", async () => {
  const source = new PassThrough();
  const reader = new PipeReader(source);
  let ended = false;

  reader.on("end", () => {
    ended = true;
  });

  source.end();
  await new Promise((resolve) => setImmediate(resolve));

  expect(ended).toBe(true);
});
