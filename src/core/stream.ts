import { EventEmitter } from "events";
import { Client } from "discord.js-selfbot-v13";
import { Streamer } from "@dank074/discord-video-stream";

export interface StreamManagerEvents {
  connected: () => void;
  disconnected: () => void;
  reconnecting: (attempt: number) => void;
  error: (err: Error) => void;
}

export declare interface StreamManager {
  on<K extends keyof StreamManagerEvents>(event: K, listener: StreamManagerEvents[K]): this;
  emit<K extends keyof StreamManagerEvents>(event: K, ...args: Parameters<StreamManagerEvents[K]>): boolean;
}

const BACKOFF_BASE_MS = 1_000;
const BACKOFF_MAX_MS = 30_000;
const MAX_RECONNECT_ATTEMPTS = 10;

export interface StreamOptions {
  width: number;
  height: number;
  fps: number;
}

export class StreamManager extends EventEmitter {
  private client: Client | null = null;
  private streamer: Streamer | null = null;
  private connection: Awaited<ReturnType<Streamer["createStream"]>> | null = null;
  private token = "";
  private guildId = "";
  private channelId = "";
  private reconnectAttempt = 0;
  private intentionalDisconnect = false;
  private streamOpts: StreamOptions = { width: 1280, height: 720, fps: 30 };

  async connect(token: string, guildId: string, channelId: string, opts?: Partial<StreamOptions>): Promise<void> {
    this.token = token;
    this.guildId = guildId;
    this.channelId = channelId;
    if (opts) this.streamOpts = { ...this.streamOpts, ...opts };
    this.intentionalDisconnect = false;
    await this.doConnect();
  }

  async disconnect(): Promise<void> {
    this.intentionalDisconnect = true;
    await this.doDisconnect();
  }

  sendVideo(data: Buffer, frametime: number): void {
    if (!this.connection) return;
    try {
      this.connection.sendVideoFrame(data, frametime);
    } catch {
      // Drop frame on transient error
    }
  }

  sendAudio(data: Buffer, frametime: number): void {
    if (!this.connection) return;
    try {
      this.connection.sendAudioFrame(data, frametime);
    } catch {
      // Drop frame on transient error
    }
  }

  private async doConnect(): Promise<void> {
    try {
      process.stderr.write("[stream] logging in...\n");
      const client = new Client();
      // Assign immediately so disconnect() can destroy it mid-flight
      this.client = client;

      await client.login(this.token);

      if (this.intentionalDisconnect) {
        client.destroy();
        this.client = null;
        return;
      }
      process.stderr.write("[stream] logged in, setting up listeners\n");

      // Detect gateway disconnects
      client.on("close", (code: number) => {
        process.stderr.write(`[stream] client WS closed: code=${code}\n`);
        this.onDisconnect();
      });
      client.on("error", (err: Error) => {
        process.stderr.write(`[stream] client error: ${err.message}\n`);
      });

      // Detect channel moves — restart stream in new channel (no re-login needed)
      client.on("voiceStateUpdate", (oldState: any, newState: any) => {
        if (newState.id !== client.user?.id) return;
        if (!newState.channelId) {
          // Ignore "removed" events during reconnect (old session cleanup)
          if (!this.connection) return;
          process.stderr.write("[stream] removed from voice channel\n");
          this.onDisconnect();
          return;
        }
        if (newState.channelId !== this.channelId) {
          process.stderr.write(`[stream] moved to channel ${newState.channelId}\n`);
          this.channelId = newState.channelId;
          if (newState.guild?.id) this.guildId = newState.guild.id;
          // Already in new channel — just restart the stream portion
          void this.restartStream();
        }
      });

      const streamer = new Streamer(client);
      this.streamer = streamer;

      // Join voice channel
      process.stderr.write(`[stream] joining voice ${this.guildId}/${this.channelId}\n`);
      await streamer.joinVoice(this.guildId, this.channelId);

      if (this.intentionalDisconnect) {
        streamer.leaveVoice();
        client.destroy();
        this.client = null;
        this.streamer = null;
        return;
      }
      process.stderr.write("[stream] voice joined, creating stream...\n");

      // createStream() calls signalStream() internally
      const connection = await streamer.createStream();

      if (this.intentionalDisconnect) {
        streamer.stopStream();
        streamer.leaveVoice();
        client.destroy();
        this.client = null;
        this.streamer = null;
        return;
      }
      process.stderr.write("[stream] stream created, configuring\n");
      connection.setPacketizer("H264");

      // Tell Discord we're sending video (required — without this, grey screen)
      connection.mediaConnection.setSpeaking(true);
      connection.mediaConnection.setVideoAttributes(true, {
        width: this.streamOpts.width,
        height: this.streamOpts.height,
        fps: this.streamOpts.fps,
      });

      // Monitor stream connection health
      this.startHealthCheck(connection);

      this.connection = connection;
      this.reconnectAttempt = 0;
      process.stderr.write("[stream] connected successfully\n");
      this.emit("connected");
    } catch (err) {
      process.stderr.write(`[stream] connect error: ${err instanceof Error ? err.message : err}\n`);
      this.emit("error", err instanceof Error ? err : new Error(String(err)));
      if (!this.intentionalDisconnect) {
        await this.scheduleReconnect();
      }
    }
  }

  /** Restart stream after channel move.
   *  Bot is already in new channel — just stop stream, patch channelId, restart Go Live.
   *  No leaveVoice/joinVoice needed (would fail if bot lacks join perms). */
  private async restartStream(): Promise<void> {
    if (this.intentionalDisconnect) return;
    if (!this.streamer) return;
    process.stderr.write(`[stream] restarting stream in ${this.channelId}\n`);
    this.stopHealthCheck();
    this.connection = null;

    try {
      try { this.streamer.stopStream(); } catch {}

      // Patch voiceConnection.channelId so signalStream uses new channel
      const vc = (this.streamer as any).voiceConnection ?? (this.streamer as any)._voiceConnection;
      if (vc) {
        vc.channelId = this.channelId;
        process.stderr.write(`[stream] patched voiceConnection.channelId\n`);
      }

      const connection = await this.streamer.createStream();

      if (this.intentionalDisconnect) {
        try { this.streamer.stopStream(); } catch {}
        return;
      }

      connection.setPacketizer("H264");
      connection.mediaConnection.setSpeaking(true);
      connection.mediaConnection.setVideoAttributes(true, {
        width: this.streamOpts.width,
        height: this.streamOpts.height,
        fps: this.streamOpts.fps,
      });

      this.startHealthCheck(connection);
      this.connection = connection;
      this.reconnectAttempt = 0;
      process.stderr.write("[stream] stream restarted successfully\n");
      this.emit("connected");
    } catch (err) {
      process.stderr.write(`[stream] restartStream failed: ${err instanceof Error ? err.message : err}\n`);
      this.emit("error", err instanceof Error ? err : new Error(String(err)));
      if (!this.intentionalDisconnect) {
        await this.scheduleReconnect();
      }
    }
  }

  private healthInterval: ReturnType<typeof setInterval> | null = null;

  private startHealthCheck(connection: NonNullable<typeof this.connection>): void {
    this.stopHealthCheck();
    this.healthInterval = setInterval(() => {
      const mc = connection.mediaConnection as any;
      if (mc._closed) {
        process.stderr.write("[stream] mediaConnection closed, triggering disconnect\n");
        this.onDisconnect();
      }
    }, 5000);
  }

  private stopHealthCheck(): void {
    if (this.healthInterval) {
      clearInterval(this.healthInterval);
      this.healthInterval = null;
    }
  }

  private async doDisconnect(): Promise<void> {
    process.stderr.write(`[stream] doDisconnect: client=${!!this.client} streamer=${!!this.streamer}\n`);
    this.stopHealthCheck();
    try {
      this.streamer?.stopStream();
      this.streamer?.leaveVoice();
      this.client?.destroy();
    } catch {
      // Ignore cleanup errors
    }
    this.connection = null;
    this.streamer = null;
    this.client = null;
    this.emit("disconnected");
  }

  private onDisconnect(): void {
    if (this.intentionalDisconnect) return;
    if (!this.connection) return; // already handling disconnect
    this.stopHealthCheck();
    try { this.streamer?.stopStream(); } catch {}
    try { this.streamer?.leaveVoice(); } catch {}
    try { this.client?.destroy(); } catch {}
    this.connection = null;
    this.streamer = null;
    this.client = null;
    void this.scheduleReconnect();
  }

  private async scheduleReconnect(): Promise<void> {
    if (this.reconnectAttempt >= MAX_RECONNECT_ATTEMPTS) {
      this.emit("error", new Error("Max reconnect attempts reached"));
      this.emit("disconnected"); // final — tell Swift to stop
      return;
    }
    this.reconnectAttempt += 1;
    const delay = Math.min(
      BACKOFF_BASE_MS * 2 ** (this.reconnectAttempt - 1),
      BACKOFF_MAX_MS
    );
    this.emit("reconnecting", this.reconnectAttempt);
    await sleep(delay);
    if (!this.intentionalDisconnect) {
      await this.doConnect();
    }
  }
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}
