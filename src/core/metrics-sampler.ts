import type { MediaPipelineStats } from "./media-pipeline.js";
import type { FrameIntervalSnapshot } from "./frame-interval-tracker.js";

export interface StreamMetrics {
  fps: number;
  gapP95Ms: number;
  gapMaxMs: number;
  gapTrunc: boolean;
  bitrateKbps: number;
  vDrop: number;
  aEnc: number;
  aSent: number;
  aDropNotReady: number;
  aEncErr: number;
}

export interface MetricsSamplerOptions {
  stats: () => MediaPipelineStats;
  drainJitter: () => FrameIntervalSnapshot;
  now: () => number;
}

export class MetricsSampler {
  private prev: MediaPipelineStats | null = null;
  private prevMs = 0;

  constructor(private readonly opts: MetricsSamplerOptions) {}

  reset(): void {
    this.prev = null;
    this.prevMs = 0;
  }

  sample(): StreamMetrics | null {
    const now = this.opts.now();
    const cur = this.opts.stats();
    const jitter = this.opts.drainJitter();

    if (this.prev === null) {
      this.prev = cur;
      this.prevMs = now;
      return null;
    }

    const dtSec = Math.max((now - this.prevMs) / 1000, 0.001);
    const dFrames = cur.frameCount - this.prev.frameCount;
    const dBytes = cur.bytesSent - this.prev.bytesSent;
    const sent = jitter.samples;

    const metrics: StreamMetrics = {
      fps: Math.round(sent / dtSec),
      gapP95Ms: jitter.p95Ms,
      gapMaxMs: jitter.maxMs,
      gapTrunc: jitter.truncated,
      bitrateKbps: Math.round((dBytes * 8) / dtSec / 1000),
      vDrop: Math.max(0, dFrames - sent),
      aEnc: cur.audioFramesEncoded - this.prev.audioFramesEncoded,
      aSent: cur.audioFramesSent - this.prev.audioFramesSent,
      aDropNotReady: cur.audioFramesDroppedNotReady - this.prev.audioFramesDroppedNotReady,
      aEncErr: cur.audioEncodeErrors - this.prev.audioEncodeErrors,
    };

    this.prev = cur;
    this.prevMs = now;
    return metrics;
  }
}
