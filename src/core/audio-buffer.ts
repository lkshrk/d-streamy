export class AudioFrameBuffer {
  private chunks: Buffer[] = [];
  private headOffset = 0;
  private queuedBytes = 0;

  constructor(private readonly frameBytes: number) {
    if (!Number.isInteger(frameBytes) || frameBytes <= 0) {
      throw new Error("frameBytes must be a positive integer");
    }
  }

  get pendingBytes(): number {
    return this.queuedBytes;
  }

  push(chunk: Buffer): void {
    if (chunk.length === 0) return;
    this.chunks.push(chunk);
    this.queuedBytes += chunk.length;
  }

  clear(): void {
    this.chunks = [];
    this.headOffset = 0;
    this.queuedBytes = 0;
  }

  drainFrames(consume: (frame: Buffer) => void): void {
    while (this.queuedBytes >= this.frameBytes) {
      consume(this.takeFrame());
    }
  }

  private takeFrame(): Buffer {
    const first = this.chunks[0];
    const firstAvailable = first.length - this.headOffset;

    if (firstAvailable >= this.frameBytes) {
      const frame = first.subarray(this.headOffset, this.headOffset + this.frameBytes);
      this.advance(this.frameBytes);
      this.queuedBytes -= this.frameBytes;
      return frame;
    }

    const frame = Buffer.allocUnsafe(this.frameBytes);
    let written = 0;

    while (written < this.frameBytes) {
      const chunk = this.chunks[0];
      const available = chunk.length - this.headOffset;
      const copied = Math.min(available, this.frameBytes - written);
      chunk.copy(frame, written, this.headOffset, this.headOffset + copied);
      written += copied;
      this.advance(copied);
    }

    this.queuedBytes -= this.frameBytes;
    return frame;
  }

  private advance(bytes: number): void {
    let remaining = bytes;

    while (remaining > 0) {
      const chunk = this.chunks[0];
      const available = chunk.length - this.headOffset;

      if (remaining < available) {
        this.headOffset += remaining;
        return;
      }

      remaining -= available;
      this.chunks.shift();
      this.headOffset = 0;
    }
  }
}
