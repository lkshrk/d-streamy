import { expect, test } from "bun:test";
import { MediaPipeline } from "./media-pipeline.js";

class FakeEncoder {
  readonly inputs: Buffer[] = [];
  shouldThrow = false;

  encode(frame: Buffer): Buffer {
    if (this.shouldThrow) throw new Error("encode failed");
    this.inputs.push(Buffer.from(frame));
    return Buffer.from([9, 8, 7]);
  }
}

class FakeStream {
  readonly audio: Buffer[] = [];
  readonly video: Buffer[] = [];
  audioReady = true;
  videoReady = true;

  sendAudio(data: Buffer): boolean {
    if (!this.audioReady) return false;
    this.audio.push(Buffer.from(data));
    return true;
  }

  sendVideo(data: Buffer): boolean {
    if (!this.videoReady) return false;
    this.video.push(Buffer.from(data));
    return true;
  }
}

test("MediaPipeline buffers audio into Opus-sized frames and counts sent audio", () => {
  const encoder = new FakeEncoder();
  const stream = new FakeStream();
  const pipeline = new MediaPipeline({ encoder, stream, opusFrameBytes: 4 });

  pipeline.handleAudio(Buffer.from([1, 2, 3]));
  expect(pipeline.stats.audioPackets).toBe(1);
  expect(pipeline.stats.audioBytesReceived).toBe(3);
  expect(pipeline.stats.audioFramesEncoded).toBe(0);
  expect(pipeline.stats.audioBufferBytes).toBe(3);

  pipeline.handleAudio(Buffer.from([4, 5, 6, 7, 8]));

  expect(encoder.inputs.map((frame) => [...frame])).toEqual([
    [1, 2, 3, 4],
    [5, 6, 7, 8],
  ]);
  expect(stream.audio.map((frame) => [...frame])).toEqual([
    [9, 8, 7],
    [9, 8, 7],
  ]);
  expect(pipeline.stats.audioPackets).toBe(2);
  expect(pipeline.stats.audioBytesReceived).toBe(8);
  expect(pipeline.stats.audioFramesEncoded).toBe(2);
  expect(pipeline.stats.audioFramesSent).toBe(2);
  expect(pipeline.stats.audioFramesDroppedNotReady).toBe(0);
  expect(pipeline.stats.audioBufferBytes).toBe(0);
});

test("MediaPipeline counts encoded audio dropped before WebRTC is ready", () => {
  const encoder = new FakeEncoder();
  const stream = new FakeStream();
  stream.audioReady = false;
  const pipeline = new MediaPipeline({ encoder, stream, opusFrameBytes: 4 });

  pipeline.handleAudio(Buffer.from([1, 2, 3, 4]));

  expect(pipeline.stats.audioFramesEncoded).toBe(1);
  expect(pipeline.stats.audioFramesSent).toBe(0);
  expect(pipeline.stats.audioFramesDroppedNotReady).toBe(1);
});

test("MediaPipeline counts audio encode failures as dropped frames", () => {
  const encoder = new FakeEncoder();
  encoder.shouldThrow = true;
  const stream = new FakeStream();
  const pipeline = new MediaPipeline({ encoder, stream, opusFrameBytes: 4 });

  pipeline.handleAudio(Buffer.from([1, 2, 3, 4]));

  expect(pipeline.stats.droppedFrames).toBe(1);
  expect(pipeline.stats.audioEncodeErrors).toBe(1);
  expect(pipeline.stats.audioFramesEncoded).toBe(0);
  expect(pipeline.stats.audioFramesSent).toBe(0);
});

test("MediaPipeline counts video frames and bytes", () => {
  const encoder = new FakeEncoder();
  const stream = new FakeStream();
  const pipeline = new MediaPipeline({ encoder, stream });

  pipeline.handleVideo(Buffer.from([1, 2, 3]));

  expect(stream.video.map((frame) => [...frame])).toEqual([[1, 2, 3]]);
  expect(pipeline.stats.frameCount).toBe(1);
  expect(pipeline.stats.bytesSent).toBe(3);
});

test("MediaPipeline reports only the first sent video frame", () => {
  const encoder = new FakeEncoder();
  const stream = new FakeStream();
  let firstVideoFrames = 0;
  const pipeline = new MediaPipeline({
    encoder,
    stream,
    onFirstVideoFrameSent: () => firstVideoFrames++,
  });

  pipeline.handleVideo(Buffer.from([1, 2, 3]));
  pipeline.handleVideo(Buffer.from([4, 5, 6]));

  expect(firstVideoFrames).toBe(1);
});

test("MediaPipeline reports first sent video after earlier frames were not ready", () => {
  const encoder = new FakeEncoder();
  const stream = new FakeStream();
  stream.videoReady = false;
  let firstVideoFrames = 0;
  const pipeline = new MediaPipeline({
    encoder,
    stream,
    onFirstVideoFrameSent: () => firstVideoFrames++,
  });

  pipeline.handleVideo(Buffer.from([1, 2, 3]));
  stream.videoReady = true;
  pipeline.handleVideo(Buffer.from([4, 5, 6]));

  expect(firstVideoFrames).toBe(1);
});

test("MediaPipeline records frame intervals for successfully sent video", () => {
  const encoder = new FakeEncoder();
  const stream = new FakeStream();
  let clock = 1000;
  const pipeline = new MediaPipeline({ encoder, stream, now: () => clock });

  pipeline.handleVideo(Buffer.from([1]));
  clock = 1020;
  pipeline.handleVideo(Buffer.from([2]));
  clock = 1050;
  pipeline.handleVideo(Buffer.from([3]));

  const jitter = pipeline.drainFrameIntervalStats();
  expect(jitter.samples).toBe(3); // 3 frames sent
  expect(jitter.maxMs).toBe(30);
});

test("MediaPipeline does not record interval for video dropped before send", () => {
  const encoder = new FakeEncoder();
  const stream = new FakeStream();
  stream.videoReady = false;
  let clock = 1000;
  const pipeline = new MediaPipeline({ encoder, stream, now: () => clock });

  pipeline.handleVideo(Buffer.from([1]));
  clock = 1020;
  pipeline.handleVideo(Buffer.from([2]));

  const jitter = pipeline.drainFrameIntervalStats();
  expect(jitter.samples).toBe(0);
});
