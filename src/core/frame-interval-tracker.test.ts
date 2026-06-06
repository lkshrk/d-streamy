import { expect, test } from "bun:test";
import { FrameIntervalTracker } from "./frame-interval-tracker.js";

test("samples counts frames, not gaps", () => {
  const t = new FrameIntervalTracker(120);
  t.record(1000);
  t.record(1020);
  t.record(1050);
  const s = t.drain();
  expect(s.samples).toBe(3); // 3 frames recorded
  expect(s.maxMs).toBe(30);
  expect(s.truncated).toBe(false);
});

test("single frame counts as one sample with no gap", () => {
  const t = new FrameIntervalTracker(120);
  t.record(1000);
  const s = t.drain();
  expect(s.samples).toBe(1);
  expect(s.p95Ms).toBe(0);
  expect(s.maxMs).toBe(0);
});

test("p95 picks the high gap", () => {
  const t = new FrameIntervalTracker(120);
  let ts = 0;
  for (let i = 0; i < 100; i++) {
    ts += 10;
    t.record(ts);
  }
  ts += 200; // one big stutter
  t.record(ts);
  const s = t.drain();
  expect(s.p95Ms).toBe(10);
  expect(s.maxMs).toBe(200);
});

test("drain resets state; gap does not span across drains", () => {
  const t = new FrameIntervalTracker(120);
  t.record(1000);
  t.record(1010);
  t.drain();
  t.record(5000); // first record of new interval — no gap vs pre-drain
  t.record(5010);
  const s = t.drain();
  expect(s.samples).toBe(2); // 2 frames this interval
  expect(s.maxMs).toBe(10); // single 10ms gap, not spanning the drain
});

test("exceeding cap truncates gap sampling but still counts all frames", () => {
  const t = new FrameIntervalTracker(3);
  for (let i = 1; i <= 10; i++) t.record(i * 10);
  const s = t.drain();
  expect(s.truncated).toBe(true);
  expect(s.samples).toBe(10); // every frame counted even past the gap cap
});
