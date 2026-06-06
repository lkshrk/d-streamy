import { appendFileSync, mkdirSync, statSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { dirname, join } from "node:path";

const MAX_LOG_BYTES = 10 * 1024 * 1024; // truncate past 10 MB

export class FileLogger {
  private bytes = -1; // lazily seeded from the file on first write

  constructor(
    readonly filePath = defaultLogPath(),
    private readonly maxBytes = MAX_LOG_BYTES,
  ) {}

  write(component: string, message: string): void {
    const cleanMessage = message.replace(/\r/g, "\\r").replace(/\n/g, "\\n");
    const line = `${new Date().toISOString()} [${component}] ${cleanMessage}\n`;
    try {
      mkdirSync(dirname(this.filePath), { recursive: true });
      if (this.bytes < 0) {
        try {
          this.bytes = statSync(this.filePath).size;
        } catch {
          this.bytes = 0;
        }
      }
      if (this.bytes > this.maxBytes) {
        writeFileSync(this.filePath, "");
        this.bytes = 0;
      }
      appendFileSync(this.filePath, line, "utf8");
      this.bytes += Buffer.byteLength(line);
    } catch {
      // Logging must never affect capture/streaming.
    }
  }
}

export function defaultLogPath(): string {
  return join(homedir(), "Library", "Logs", "D-Streamy", "stream.log");
}
