/**
 * D-Streamy daemon — background process managed by the SwiftUI app.
 * - Reads media frames from stdin (binary pipe protocol)
 * - Accepts control commands via Unix domain socket
 * - Manages Discord connection via discord-video-stream
 */

import net from "net";
import { PipeReader } from "../core/pipe.js";
import { StreamManager } from "../core/stream.js";
import { getAudioEncoder } from "../core/audio.js";

const SOCKET_PATH = process.env.DSTREAMY_SOCKET;
if (!SOCKET_PATH) {
  process.stderr.write("DSTREAMY_SOCKET env not set\n");
  process.exit(1);
}

// Media pipeline
const reader = new PipeReader(process.stdin);
const stream = new StreamManager();
const encoder = getAudioEncoder();

// Wire stream events once (not per-command)
stream.on("connected", () => {
  process.stderr.write("[daemon] stream connected\n");
  sendEvent("connected");
});
stream.on("disconnected", () => {
  process.stderr.write("[daemon] stream disconnected\n");
  sendEvent("disconnected");
});
stream.on("error", (err) => {
  process.stderr.write(`[daemon] stream error: ${err.message}\n`);
  sendEvent("error", err.message);
});
stream.on("reconnecting", (attempt) => {
  process.stderr.write(`[daemon] reconnecting attempt ${attempt}\n`);
  sendEvent("reconnecting", { attempt });
});

// Stats
let frameCount = 0;
let droppedFrames = 0;
let bytesSent = 0;
let startTime = 0;

// Audio ring buffer (Opus needs 960 samples × 2ch × 2B = 3840 bytes)
const OPUS_FRAME_BYTES = 960 * 2 * 2;
let audioBuffer = Buffer.alloc(0);

// Wire media
const videoFrametimeMs = 1000 / 30;
const audioFrametimeMs = 20;

reader.on("video", (data) => {
  frameCount++;
  bytesSent += data.length;
  stream.sendVideo(data, videoFrametimeMs);
});

reader.on("audio", (data) => {
  audioBuffer = Buffer.concat([audioBuffer, data]);
  while (audioBuffer.length >= OPUS_FRAME_BYTES) {
    const frame = audioBuffer.subarray(0, OPUS_FRAME_BYTES);
    audioBuffer = audioBuffer.subarray(OPUS_FRAME_BYTES);
    try {
      const opus = encoder.encode(frame);
      stream.sendAudio(opus, audioFrametimeMs);
    } catch {
      droppedFrames++;
    }
  }
});

// Control socket
let controlConn: net.Socket | null = null;

function sendEvent(type: string, payload?: unknown) {
  if (!controlConn) return;
  const msg = JSON.stringify({ type, payload }) + "\n";
  controlConn.write(msg);
}

const server = net.createServer((socket) => {
  controlConn = socket;
  process.stderr.write("[daemon] control connection established\n");

  let buf = "";
  socket.on("data", (chunk) => {
    buf += chunk.toString();
    const lines = buf.split("\n");
    buf = lines.pop() ?? "";
    for (const line of lines) {
      if (line.trim()) handleCommand(line.trim());
    }
  });

  socket.on("close", () => {
    controlConn = null;
  });
});

server.listen(SOCKET_PATH, () => {
  process.stderr.write(`[daemon] listening on ${SOCKET_PATH}\n`);
});

// Stats reporting
setInterval(() => {
  if (startTime === 0) return;
  const uptimeSec = Math.floor((Date.now() - startTime) / 1000);
  const elapsed = uptimeSec || 1;
  sendEvent("stats", {
    uptime: uptimeSec,
    fps: Math.round(frameCount / elapsed),
    bitrate: Math.round((bytesSent * 8) / elapsed / 1000),
    droppedFrames,
  });
}, 1000);

// Command handler
async function handleCommand(json: string) {
  try {
    const { type, payload } = JSON.parse(json);

    switch (type) {
      case "connect": {
        const { token, guildId, channelId, width, height, fps } = payload;
        process.stderr.write(`[daemon] connect: ${guildId}/${channelId} ${width}x${height}@${fps}\n`);
        await stream.connect(token, guildId, channelId, { width, height, fps });
        startTime = Date.now();
        frameCount = 0;
        bytesSent = 0;
        droppedFrames = 0;
        break;
      }
      case "disconnect": {
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
      default:
        process.stderr.write(`[daemon] unknown command: ${type}\n`);
    }
  } catch (err) {
    const msg = err instanceof Error ? err.message : String(err);
    sendEvent("error", msg);
  }
}

// Catch unhandled errors
process.on("uncaughtException", (err) => {
  process.stderr.write(`[daemon] uncaught: ${err.message}\n${err.stack}\n`);
  sendEvent("error", err.message);
});
process.on("unhandledRejection", (reason) => {
  process.stderr.write(`[daemon] unhandled rejection: ${reason}\n`);
  sendEvent("error", String(reason));
});

// Graceful shutdown
process.on("SIGTERM", () => {
  stream.disconnect().finally(() => {
    server.close();
    process.exit(0);
  });
});
