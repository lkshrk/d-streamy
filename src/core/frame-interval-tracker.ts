export interface FrameIntervalSnapshot {
  samples: number;
  p95Ms: number;
  maxMs: number;
  truncated: boolean;
}

export class FrameIntervalTracker {
  private gaps: number[] = [];
  private frames = 0;
  private lastTs: number | null = null;
  private truncated = false;

  constructor(private readonly maxSamples = 120) {}

  record(tsMs: number): void {
    this.frames++;
    if (this.lastTs !== null) {
      if (this.gaps.length < this.maxSamples) {
        this.gaps.push(tsMs - this.lastTs);
      } else {
        this.truncated = true;
      }
    }
    this.lastTs = tsMs;
  }

  drain(): FrameIntervalSnapshot {
    const snapshot = this.compute();
    this.gaps = [];
    this.frames = 0;
    this.lastTs = null;
    this.truncated = false;
    return snapshot;
  }

  private compute(): FrameIntervalSnapshot {
    if (this.gaps.length === 0) {
      return { samples: this.frames, p95Ms: 0, maxMs: 0, truncated: this.truncated };
    }
    const sorted = [...this.gaps].sort((a, b) => a - b);
    const idx = Math.min(sorted.length - 1, Math.ceil(0.95 * sorted.length) - 1);
    return {
      samples: this.frames,
      p95Ms: Math.round(sorted[Math.max(0, idx)]),
      maxMs: Math.round(sorted[sorted.length - 1]),
      truncated: this.truncated,
    };
  }
}
