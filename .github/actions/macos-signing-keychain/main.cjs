const { spawnSync } = require("node:child_process");
const path = require("node:path");

const env = {
  ...process.env,
  CERT_P12: process.env["INPUT_CERT-P12"] ?? "",
  CERT_PASSWORD: process.env["INPUT_CERT-PASSWORD"] ?? "",
};
const result = spawnSync("bash", [path.join(__dirname, "import.sh")], { env, stdio: "inherit" });
process.exit(result.status ?? 1);
