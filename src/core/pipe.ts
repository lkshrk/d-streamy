import { EventEmitter } from "events";
import type { Readable } from "stream";

export enum FrameType {
  Video = 0x01,
  Audio = 0x02,
}

// Header layout: [type 1B][timestamp 8B LE µs][length 4B LE][data NB]
const HEADER_SIZE = 13;

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

  constructor(source: Readable) {
    super();
    this.source = source;
    source.on("data", (chunk: Buffer) => this.onData(chunk));
    source.on("error", (err: Error) => this.emit("error", err));
    source.on("end", () => this.flush());
  }

  private onData(chunk: Buffer): void {
    this.buf = Buffer.concat([this.buf, chunk]);
    this.drain();
  }

  private drain(): void {
    while (this.buf.length >= HEADER_SIZE) {
      const type = this.buf[0];
      const timestamp = this.buf.readBigInt64LE(1);
      const length = this.buf.readUInt32LE(9);
      const totalNeeded = HEADER_SIZE + length;

      if (this.buf.length < totalNeeded) break;

      const data = Buffer.from(this.buf.subarray(HEADER_SIZE, totalNeeded));
      this.buf = this.buf.subarray(totalNeeded);

      if (type === FrameType.Video) {
        this.emit("video", data, timestamp);
      } else if (type === FrameType.Audio) {
        this.emit("audio", data, timestamp);
      }
      // Unknown types are silently dropped.
    }
  }

  private flush(): void {
    // EOF — drain whatever is left (will be a no-op if incomplete frame)
    this.drain();
  }
}
