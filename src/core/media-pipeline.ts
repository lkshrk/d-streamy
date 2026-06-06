import { AudioFrameBuffer } from "./audio-buffer.js";
import { FrameIntervalTracker, type FrameIntervalSnapshot } from "./frame-interval-tracker.js";

export interface MediaEncoder {
  encode(frame: Buffer): Buffer;
}

export interface MediaStreamSender {
  sendVideo(data: Buffer, frametime: number): boolean;
  sendAudio(data: Buffer, frametime: number): boolean;
}

export interface MediaPipelineStats {
  frameCount: number;
  droppedFrames: number;
  bytesSent: number;
  audioPackets: number;
  audioBytesReceived: number;
  audioFramesEncoded: number;
  audioFramesSent: number;
  audioFramesDroppedNotReady: number;
  audioEncodeErrors: number;
  audioBufferBytes: number;
}

export interface MediaPipelineOptions {
  stream: MediaStreamSender;
  encoder: MediaEncoder;
  opusFrameBytes?: number;
  videoFrametimeMs?: number;
  audioFrametimeMs?: number;
  onFirstAudioPacket?: (bytes: number) => void;
  onFirstAudioFrameSent?: () => void;
  onFirstVideoFrameSent?: () => void;
  onAudioEncodeError?: (err: unknown, count: number) => void;
  now?: () => number;
}

const DEFAULT_OPUS_FRAME_BYTES = 960 * 2 * 2;
const DEFAULT_VIDEO_FRAMETIME_MS = 1000 / 30;
const DEFAULT_AUDIO_FRAMETIME_MS = 20;

export class MediaPipeline {
  private readonly stream: MediaStreamSender;
  private readonly encoder: MediaEncoder;
  private readonly audioBuffer: AudioFrameBuffer;
  private readonly audioFrametimeMs: number;
  private readonly onFirstAudioPacket?: (bytes: number) => void;
  private readonly onFirstAudioFrameSent?: () => void;
  private readonly onFirstVideoFrameSent?: () => void;
  private readonly onAudioEncodeError?: (err: unknown, count: number) => void;
  private videoFrametimeMs: number;
  private readonly frameInterval = new FrameIntervalTracker();
  private readonly now: () => number;

  private frameCount = 0;
  private droppedFrames = 0;
  private bytesSent = 0;
  private audioPackets = 0;
  private audioBytesReceived = 0;
  private audioFramesEncoded = 0;
  private audioFramesSent = 0;
  private audioFramesDroppedNotReady = 0;
  private audioEncodeErrors = 0;
  private hasSentFirstVideoFrame = false;

  constructor(options: MediaPipelineOptions) {
    this.stream = options.stream;
    this.encoder = options.encoder;
    this.audioBuffer = new AudioFrameBuffer(options.opusFrameBytes ?? DEFAULT_OPUS_FRAME_BYTES);
    this.videoFrametimeMs = options.videoFrametimeMs ?? DEFAULT_VIDEO_FRAMETIME_MS;
    this.audioFrametimeMs = options.audioFrametimeMs ?? DEFAULT_AUDIO_FRAMETIME_MS;
    this.onFirstAudioPacket = options.onFirstAudioPacket;
    this.onFirstAudioFrameSent = options.onFirstAudioFrameSent;
    this.onFirstVideoFrameSent = options.onFirstVideoFrameSent;
    this.onAudioEncodeError = options.onAudioEncodeError;
    this.now = options.now ?? (() => performance.now());
  }

  get stats(): MediaPipelineStats {
    return {
      frameCount: this.frameCount,
      droppedFrames: this.droppedFrames,
      bytesSent: this.bytesSent,
      audioPackets: this.audioPackets,
      audioBytesReceived: this.audioBytesReceived,
      audioFramesEncoded: this.audioFramesEncoded,
      audioFramesSent: this.audioFramesSent,
      audioFramesDroppedNotReady: this.audioFramesDroppedNotReady,
      audioEncodeErrors: this.audioEncodeErrors,
      audioBufferBytes: this.audioBuffer.pendingBytes,
    };
  }

  setVideoFps(fps: number): void {
    this.videoFrametimeMs = 1000 / (fps || 30);
  }

  resetCounters(): void {
    this.frameCount = 0;
    this.droppedFrames = 0;
    this.bytesSent = 0;
    this.audioPackets = 0;
    this.audioBytesReceived = 0;
    this.audioFramesEncoded = 0;
    this.audioFramesSent = 0;
    this.audioFramesDroppedNotReady = 0;
    this.audioEncodeErrors = 0;
    this.hasSentFirstVideoFrame = false;
  }

  clearAudioBuffer(): void {
    this.audioBuffer.clear();
  }

  handleVideo(data: Buffer): void {
    this.frameCount++;
    this.bytesSent += data.length;
    if (this.stream.sendVideo(data, this.videoFrametimeMs)) {
      this.frameInterval.record(this.now());
      if (!this.hasSentFirstVideoFrame) {
        this.hasSentFirstVideoFrame = true;
        this.onFirstVideoFrameSent?.();
      }
    }
  }

  drainFrameIntervalStats(): FrameIntervalSnapshot {
    return this.frameInterval.drain();
  }

  handleAudio(data: Buffer): void {
    this.audioPackets++;
    this.audioBytesReceived += data.length;
    if (this.audioPackets === 1) {
      this.onFirstAudioPacket?.(data.length);
    }

    this.audioBuffer.push(data);
    this.audioBuffer.drainFrames((frame) => this.encodeAndSendAudio(frame));
  }

  private encodeAndSendAudio(frame: Buffer): void {
    try {
      const opus = this.encoder.encode(frame);
      this.audioFramesEncoded++;
      if (this.stream.sendAudio(opus, this.audioFrametimeMs)) {
        this.audioFramesSent++;
        if (this.audioFramesSent === 1) {
          this.onFirstAudioFrameSent?.();
        }
      } else {
        this.audioFramesDroppedNotReady++;
      }
    } catch (err) {
      this.droppedFrames++;
      this.audioEncodeErrors++;
      this.onAudioEncodeError?.(err, this.audioEncodeErrors);
    }
  }
}
