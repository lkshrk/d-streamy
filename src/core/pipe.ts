import { EventEmitter } from "events";
import type { Readable } from "stream";

export enum FrameType {
  Video = 0x01,
  Audio = 0x02,
}

// Header layout: [type 1B][timestamp 8B LE µs][length 4B LE][data NB]
const HEADER_SIZE = 13;
const DEFAULT_MAX_FRAME_BYTES = 16 * 1024 * 1024;

export interface PipeReaderOptions {
  maxFrameBytes?: number;
}

export interface PipeReaderEvents {
  video: (data: Buffer, timestamp: bigint) => void;
  audio: (data: Buffer, timestamp: bigint) => void;
  error: (err: Error) => void;
}

export declare interface PipeReader {
  on<K extends keyof PipeReaderEvents>(event: K, listener: PipeReaderEvents[K]): this;
  emit<K extends keyof PipeReaderEvents>(event: K, ...args: Parameters<PipeReaderEvents[K]>): boolean;
}

export class PipeReader extends EventEmitter {
  private buf: Buffer = Buffer.alloc(0);
  private source: Readable;
  private maxFrameBytes: number;
  private failed = false;

  private readonly onSourceData = (chunk: Buffer) => this.onData(chunk);
  private readonly onSourceError = (err: Error) => this.emit("error", err);
  private readonly onSourceEnd = () => this.flush();

  constructor(source: Readable, options: PipeReaderOptions = {}) {
    super();
    this.source = source;
    this.maxFrameBytes = options.maxFrameBytes ?? DEFAULT_MAX_FRAME_BYTES;
    source.on("data", this.onSourceData);
    source.on("error", this.onSourceError);
    source.on("end", this.onSourceEnd);
  }

  get pendingBytes(): number {
    return this.buf.length;
  }

  private onData(chunk: Buffer): void {
    if (this.failed) return;
    if (this.buf.length > 0 && this.buf.length + chunk.length > this.maxFrameBytes + HEADER_SIZE) {
      this.fail(new Error(`pipe buffer exceeded max frame length ${this.maxFrameBytes}`));
      return;
    }
    this.buf = this.buf.length === 0 ? chunk : Buffer.concat([this.buf, chunk], this.buf.length + chunk.length);
    this.drain();
  }

  private drain(): void {
    let offset = 0;

    while (this.buf.length - offset >= HEADER_SIZE) {
      const type = this.buf[offset];
      const timestamp = this.buf.readBigInt64LE(offset + 1);
      const length = this.buf.readUInt32LE(offset + 9);
      const totalNeeded = HEADER_SIZE + length;

      if (length > this.maxFrameBytes) {
        this.fail(new Error(`frame length ${length} exceeds max frame length ${this.maxFrameBytes}`));
        return;
      }

      if (this.buf.length - offset < totalNeeded) break;

      const data = this.buf.subarray(offset + HEADER_SIZE, offset + totalNeeded);
      offset += totalNeeded;

      if (type === FrameType.Video) {
        this.emit("video", data, timestamp);
      } else if (type === FrameType.Audio) {
        this.emit("audio", data, timestamp);
      }
      // Unknown types are silently dropped.
    }

    if (offset === 0) return;
    this.buf = offset === this.buf.length ? Buffer.alloc(0) : Buffer.from(this.buf.subarray(offset));
  }

  private flush(): void {
    // EOF — drain whatever is left (will be a no-op if incomplete frame)
    this.drain();
  }

  private fail(err: Error): void {
    if (this.failed) return;
    this.failed = true;
    this.buf = Buffer.alloc(0);
    this.source.off("data", this.onSourceData);
    this.source.off("error", this.onSourceError);
    this.source.off("end", this.onSourceEnd);
    if (typeof this.source.destroy === "function") {
      this.source.destroy();
    }
    this.emit("error", err);
  }
}
