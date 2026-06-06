import { expect, test } from "bun:test";
import { MetricsSampler } from "./metrics-sampler.js";
import type { MediaPipelineStats } from "./media-pipeline.js";
import type { FrameIntervalSnapshot } from "./frame-interval-tracker.js";

function blankStats(over: Partial<MediaPipelineStats> = {}): MediaPipelineStats {
  return {
    frameCount: 0, droppedFrames: 0, bytesSent: 0, audioPackets: 0,
    audioBytesReceived: 0, audioFramesEncoded: 0, audioFramesSent: 0,
    audioFramesDroppedNotReady: 0, audioEncodeErrors: 0, audioBufferBytes: 0,
    ...over,
  };
}

function jitter(over: Partial<FrameIntervalSnapshot> = {}): FrameIntervalSnapshot {
  return { samples: 0, p95Ms: 0, maxMs: 0, truncated: false, ...over };
}

test("first sample returns null (baseline seed, no spike)", () => {
  const sampler = new MetricsSampler({
    stats: () => blankStats(),
    drainJitter: () => jitter(),
    now: () => 1000,
  });
  expect(sampler.sample()).toBeNull();
});

test("second sample computes per-interval deltas over elapsed time", () => {
  let clock = 1000;
  let stats = blankStats();
  const sampler = new MetricsSampler({
    stats: () => stats,
    drainJitter: () => jitter({ samples: 28, p95Ms: 34, maxMs: 120 }),
    now: () => clock,
  });
  sampler.sample(); // seed
  clock = 2000; // +1s
  stats = blankStats({ frameCount: 30, bytesSent: 250_000, audioFramesEncoded: 50, audioFramesSent: 49, audioFramesDroppedNotReady: 1 });

  const m = sampler.sample()!;
  expect(m).not.toBeNull();
  expect(m.fps).toBe(28);
  expect(m.vDrop).toBe(2);
  expect(m.bitrateKbps).toBe(2000);
  expect(m.gapP95Ms).toBe(34);
  expect(m.gapMaxMs).toBe(120);
  expect(m.aEnc).toBe(50);
  expect(m.aSent).toBe(49);
  expect(m.aDropNotReady).toBe(1);
});

test("reset re-seeds baseline so next sample returns null", () => {
  let clock = 1000;
  const sampler = new MetricsSampler({
    stats: () => blankStats(),
    drainJitter: () => jitter(),
    now: () => clock,
  });
  sampler.sample();
  clock = 2000;
  expect(sampler.sample()).not.toBeNull();
  sampler.reset();
  clock = 3000;
  expect(sampler.sample()).toBeNull();
});
