// PCM → Opus encoder with @discordjs/opus / opusscript fallback

export interface OpusEncoder {
  encode(pcm: Buffer): Buffer;
}

// Lazy-loaded singleton
let _encoder: AudioEncoder | null = null;

class DiscordJsOpusEncoder implements OpusEncoder {
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  private enc: any;

  constructor(sampleRate: number, channels: number, _frameSize: number) {
    // @discordjs/opus exports OpusEncoder as a named export
    // eslint-disable-next-line @typescript-eslint/no-var-requires
    const { OpusEncoder: NativeEncoder } = require("@discordjs/opus");
    this.enc = new NativeEncoder(sampleRate, channels);
  }

  encode(pcm: Buffer): Buffer {
    return this.enc.encode(pcm) as Buffer;
  }
}

class OpusScriptEncoder implements OpusEncoder {
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  private enc: any;
  private frameSize: number;

  constructor(sampleRate: number, channels: number, frameSize: number) {
    // eslint-disable-next-line @typescript-eslint/no-var-requires
    const OpusScript = require("opusscript");
    this.enc = new OpusScript(sampleRate, channels, OpusScript.Application.AUDIO);
    this.frameSize = frameSize;
  }

  encode(pcm: Buffer): Buffer {
    return this.enc.encode(pcm, this.frameSize) as Buffer;
  }
}

function createInnerEncoder(
  sampleRate: number,
  channels: number,
  frameSize: number
): OpusEncoder {
  try {
    return new DiscordJsOpusEncoder(sampleRate, channels, frameSize);
  } catch {
    // fall through to opusscript
  }
  try {
    return new OpusScriptEncoder(sampleRate, channels, frameSize);
  } catch {
    // fall through
  }
  throw new Error(
    "No Opus encoder available. Install @discordjs/opus or opusscript."
  );
}

export class AudioEncoder implements OpusEncoder {
  private inner: OpusEncoder;

  constructor(
    sampleRate: number = 48000,
    channels: number = 2,
    frameSize: number = 960
  ) {
    this.inner = createInnerEncoder(sampleRate, channels, frameSize);
  }

  encode(pcm: Buffer): Buffer {
    return this.inner.encode(pcm);
  }
}

/** Singleton factory — reuses one encoder for the lifetime of the process. */
export function getAudioEncoder(
  sampleRate: number = 48000,
  channels: number = 2,
  frameSize: number = 960
): AudioEncoder {
  if (!_encoder) {
    _encoder = new AudioEncoder(sampleRate, channels, frameSize);
  }
  return _encoder;
}
