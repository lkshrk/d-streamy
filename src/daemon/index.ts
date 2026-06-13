/**
 * D-Streamy daemon — background process managed by the SwiftUI app.
 * - Reads media frames from stdin (binary pipe protocol)
 * - Accepts control commands via Unix domain socket
 * - Manages Discord connection via discord-video-stream
 */

import net from "net";
import { PipeReader } from "../core/pipe.js";
import { StreamManager } from "../core/stream.js";
import { normalizeCodec } from "../core/codec.js";
import { getAudioEncoder } from "../core/audio.js";
import { AsyncTaskQueue } from "../core/async-task-queue.js";
import { FileLogger } from "../core/file-logger.js";
import { MediaPipeline } from "../core/media-pipeline.js";
import { MetricsSampler } from "../core/metrics-sampler.js";

const fileLogger = new FileLogger();
let stderrAvailable = true;

function logDaemon(message: string): void {
  // stderr is a pipe to the app; if the app went away the write throws EPIPE.
  // Never let logging throw — that would re-enter the uncaughtException handler
  // and spin a tight stderr loop.
  if (stderrAvailable) {
    try {
      process.stderr.write(`[daemon] ${message}\n`);
    } catch (err) {
      if ((err as NodeJS.ErrnoException).code === "EPIPE") stderrAvailable = false;
    }
  }
  fileLogger.write("daemon", message);
}

// Async EPIPE on the std streams surfaces as a stream 'error' event, not a
// throw from write(); swallow it so it doesn't become an uncaughtException.
process.stdout.on("error", () => {});
process.stderr.on("error", (err) => {
  if ((err as NodeJS.ErrnoException).code === "EPIPE") stderrAvailable = false;
});

const SOCKET_PATH = process.env.DSTREAMY_SOCKET;
if (!SOCKET_PATH) {
  logDaemon("DSTREAMY_SOCKET env not set");
  process.exit(1);
}

// Media pipeline
const reader = new PipeReader(process.stdin);
const stream = new StreamManager();
const encoder = getAudioEncoder();

// Wire stream events once (not per-command)
stream.on("connected", () => {
  logDaemon("stream connected");
  sendEvent("connected");
});
stream.on("disconnected", () => {
  logDaemon("stream disconnected");
  sendEvent("disconnected");
});
stream.on("error", (err) => {
  logDaemon(`stream error: ${err.message}`);
  sendEvent("error", err.message);
});
stream.on("reconnecting", (attempt) => {
  logDaemon(`reconnecting attempt ${attempt}`);
  sendEvent("reconnecting", { attempt });
});

let startTime = 0;
const pipeline = new MediaPipeline({
  stream,
  encoder,
  onFirstVideoFrameSent: () => {
    logDaemon("first video frame sent to WebRTC");
  },
  onFirstAudioPacket: (bytes) => {
    logDaemon(`first audio packet: ${bytes} bytes`);
  },
  onFirstAudioFrameSent: () => {
    logDaemon("first audio frame sent to WebRTC");
  },
  onAudioEncodeError: (err, count) => {
    if (count <= 5) {
      const msg = err instanceof Error ? err.message : String(err);
      logDaemon(`audio encode failed: ${msg}`);
    }
  },
});

let sessionId = "";
const metricsSampler = new MetricsSampler({
  stats: () => pipeline.stats,
  drainJitter: () => pipeline.drainFrameIntervalStats(),
  now: () => performance.now(),
});
let shuttingDown = false;
let statsInterval: ReturnType<typeof setInterval> | undefined;

async function shutdown(reason: string, exitCode = 0): Promise<void> {
  if (shuttingDown) return;
  shuttingDown = true;
  logDaemon(`shutdown: ${reason}`);

  if (statsInterval) clearInterval(statsInterval);
  controlConn?.destroy();
  controlConn = null;
  server.close();

  await Promise.race([
    stream.disconnect(),
    new Promise((resolve) => setTimeout(resolve, 1500)),
  ]).catch(() => {});

  process.exit(exitCode);
}

reader.on("error", (err) => {
  logDaemon(`pipe error: ${err.message}`);
  sendEvent("error", err.message);
  pipeline.clearAudioBuffer();
  void shutdown("media pipe error");
});
reader.on("end", () => {
  void shutdown("media pipe closed");
});

reader.on("video", (data) => {
  pipeline.handleVideo(data);
});

reader.on("audio", (data) => {
  pipeline.handleAudio(data);
});

// Control socket
let controlConn: net.Socket | null = null;
const commandQueue = new AsyncTaskQueue();

function sendEvent(type: string, payload?: unknown) {
  if (!controlConn) return;
  const msg = JSON.stringify({ type, payload }) + "\n";
  controlConn.write(msg);
}

const server = net.createServer((socket) => {
  controlConn = socket;
  logDaemon("control connection established");

  let buf = "";
  socket.on("data", (chunk) => {
    buf += chunk.toString();
    const lines = buf.split("\n");
    buf = lines.pop() ?? "";
    for (const line of lines) {
      const command = line.trim();
      if (command) void commandQueue.add(() => handleCommand(command));
    }
  });

  socket.on("close", () => {
    controlConn = null;
    void shutdown("control connection closed");
  });
});

server.listen(SOCKET_PATH, () => {
  logDaemon(`listening on ${SOCKET_PATH}`);
});

// Stats reporting
let lastLoggedUptime = -1;
statsInterval = setInterval(() => {
  if (startTime === 0) return;
  const uptimeSec = Math.floor((Date.now() - startTime) / 1000);
  const elapsed = uptimeSec || 1;
  const memory = process.memoryUsage();
  const stats = pipeline.stats;
  if (uptimeSec > 0 && uptimeSec % 5 === 0 && uptimeSec !== lastLoggedUptime) {
    lastLoggedUptime = uptimeSec;
    logDaemon(
      `stats uptime=${uptimeSec}s fps=${Math.round(stats.frameCount / elapsed)} bitrate=${Math.round((stats.bytesSent * 8) / elapsed / 1000)}kbps dropped=${stats.droppedFrames} rss=${Math.round(memory.rss / 1024 / 1024)}MB pipe=${reader.pendingBytes}B audioPackets=${stats.audioPackets} audioEncoded=${stats.audioFramesEncoded} audioSent=${stats.audioFramesSent} audioDroppedNotReady=${stats.audioFramesDroppedNotReady}`
    );
  }
  sendEvent("stats", {
    uptime: uptimeSec,
    fps: Math.round(stats.frameCount / elapsed),
    bitrate: Math.round((stats.bytesSent * 8) / elapsed / 1000),
    droppedFrames: stats.droppedFrames,
    rssBytes: memory.rss,
    heapUsedBytes: memory.heapUsed,
    externalBytes: memory.external,
    arrayBuffersBytes: memory.arrayBuffers,
    pipeBufferBytes: reader.pendingBytes,
    audioBufferBytes: stats.audioBufferBytes,
    audioPackets: stats.audioPackets,
    audioBytesReceived: stats.audioBytesReceived,
    audioFramesEncoded: stats.audioFramesEncoded,
    audioFramesSent: stats.audioFramesSent,
    audioFramesDroppedNotReady: stats.audioFramesDroppedNotReady,
    audioEncodeErrors: stats.audioEncodeErrors,
  });
  const intervalMetrics = metricsSampler.sample();
  if (intervalMetrics) {
    sendEvent("metrics", {
      session: sessionId,
      t: uptimeSec,
      ...intervalMetrics,
      pipeBuf: reader.pendingBytes,
      audioBuf: stats.audioBufferBytes,
      rssMb: Math.round(memory.rss / 1024 / 1024),
    });
  }
}, 1000);

// Command handler
async function handleCommand(json: string) {
  try {
    const { type, payload } = JSON.parse(json);

    switch (type) {
      case "connect": {
        const { token, guildId, channelId, width, height, fps, codec, session } = payload;
        sessionId = typeof session === "string" ? session : "";
        const videoCodec = normalizeCodec(codec);
        logDaemon(`connect: ${guildId}/${channelId} ${width}x${height}@${fps} codec=${videoCodec} session=${sessionId}`);
        pipeline.setVideoFps(fps);
        pipeline.clearAudioBuffer();
        pipeline.resetCounters();
        pipeline.drainFrameIntervalStats();
        metricsSampler.reset();
        await stream.connect(token, guildId, channelId, { width, height, fps, codec: videoCodec });
        startTime = Date.now();
        break;
      }
      case "disconnect": {
        logDaemon("disconnect");
        pipeline.clearAudioBuffer();
        await stream.disconnect();
        startTime = 0;
        sendEvent("disconnected");
        break;
      }
      case "getGuilds": {
        // Guilds fetched via REST in payload.token context
        // For now, return empty — guilds loaded from saved config
        sendEvent("guilds", []);
        break;
      }
      case "getChannels": {
        sendEvent("channels", []);
        break;
      }
      case "updateVideo": {
        const { width, height, fps } = payload;
        logDaemon(`updateVideo: ${width}x${height}@${fps}`);
        pipeline.setVideoFps(fps);
        stream.updateVideoAttributes(width, height, fps);
        break;
      }
      default:
        logDaemon(`unknown command: ${type}`);
    }
  } catch (err) {
    const msg = err instanceof Error ? err.message : String(err);
    sendEvent("error", msg);
  }
}

// Catch unhandled errors
process.on("uncaughtException", (err) => {
  // Broken pipe means the app (our stderr/IPC reader) is gone — exit instead of
  // logging, which would write to the same dead pipe and loop forever.
  if ((err as NodeJS.ErrnoException).code === "EPIPE") {
    void shutdown("broken pipe");
    return;
  }
  logDaemon(`uncaught: ${err.message}\n${err.stack}`);
  sendEvent("error", err.message);
});
process.on("unhandledRejection", (reason) => {
  logDaemon(`unhandled rejection: ${reason}`);
  sendEvent("error", String(reason));
});

// Graceful shutdown
process.on("SIGTERM", () => {
  void shutdown("SIGTERM");
});
process.on("SIGINT", () => {
  void shutdown("SIGINT");
});
