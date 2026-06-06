import { describe, expect, test } from "bun:test";
import { mkdtempSync, readFileSync, statSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { FileLogger } from "./file-logger.js";

describe("FileLogger", () => {
  test("creates the log directory and appends sanitized lines", () => {
    const directory = mkdtempSync(join(tmpdir(), "dstreamy-log-"));
    const filePath = join(directory, "nested", "stream.log");
    const logger = new FileLogger(filePath);

    logger.write("daemon", "first\nline");
    logger.write("app", "second");

    const contents = readFileSync(filePath, "utf8");
    expect(contents).toContain("[daemon] first\\nline");
    expect(contents).toContain("[app] second");
    expect(contents.trimEnd().split("\n")).toHaveLength(2);
  });

  test("truncates the file once it exceeds the size cap", () => {
    const directory = mkdtempSync(join(tmpdir(), "dstreamy-log-"));
    const filePath = join(directory, "stream.log");
    const logger = new FileLogger(filePath, 200); // tiny cap

    for (let i = 0; i < 50; i++) logger.write("daemon", `line ${i} ${"x".repeat(40)}`);

    const size = statSync(filePath).size;
    expect(size).toBeLessThan(2000); // bounded, not 50 * ~70 bytes
    expect(readFileSync(filePath, "utf8")).toContain("line 49");
  });
});
