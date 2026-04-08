#!/usr/bin/env node

const { spawnSync } = require("child_process");
const path = require("path");
const fs = require("fs");
const os = require("os");

const isWindows = process.platform === "win32";
const cacheBinDir = path.join(os.homedir(), ".weave-patch", "bin");
const binName = isWindows ? "weave-patch-mcp.exe" : "weave-patch-mcp";
const cachedBinPath = path.join(cacheBinDir, binName);
const expectedVersion = require("../package.json").version;
const installer = path.join(__dirname, "..", "scripts", "install.js");

function spawnBinary(binPath) {
  const child = require("child_process").spawn(binPath, process.argv.slice(2), { stdio: "inherit" });
  child.on("exit", (code) => process.exit(code ?? 1));
}

function getInstalledVersion(binPath) {
  try {
    const out = require("child_process").execSync(`"${binPath}" --version`, {
      timeout: 5000,
      encoding: "utf-8",
    });
    return out.trim();
  } catch (e) {
    return null;
  }
}

function runInstaller() {
  const res = spawnSync(process.execPath, [installer], { stdio: ["inherit", process.stderr.fd, "inherit"] });
  if (res.status !== 0) {
    console.error("Installer failed.");
    process.exit(res.status || 1);
  }
}

(function main() {
  try {
    // Check if cached binary exists and is the correct version
    if (fs.existsSync(cachedBinPath)) {
      const installedVersion = getInstalledVersion(cachedBinPath);
      if (installedVersion !== expectedVersion) {
        console.error(`Cached binary is v${installedVersion || "unknown"}, need v${expectedVersion}. Reinstalling...`);
        try { fs.unlinkSync(cachedBinPath); } catch (_) {}
        runInstaller();
      }
      if (fs.existsSync(cachedBinPath)) {
        spawnBinary(cachedBinPath);
        return;
      }
    }

    // Fallback: try local node_modules bin path (for installed package)
    const localBin = path.join(__dirname, "..", "bin", binName);
    if (fs.existsSync(localBin)) {
      spawnBinary(localBin);
      return;
    }

    // If missing, run installer
    console.error("Binary not found in cache. Installing to global cache...");
    runInstaller();

    if (fs.existsSync(cachedBinPath)) {
      spawnBinary(cachedBinPath);
      return;
    }

    console.error("Binary installation completed but binary not found.");
    process.exit(1);
  } catch (err) {
    console.error(err);
    process.exit(1);
  }
})();
