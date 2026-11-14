const { spawnSync } = require("node:child_process");
const path = require("node:path");

const result = spawnSync("bash", [path.join(__dirname, "cleanup.sh")], { stdio: "inherit" });
process.exit(result.status ?? 1);
