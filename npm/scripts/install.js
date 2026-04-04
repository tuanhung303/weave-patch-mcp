const https = require("https");
const fs = require("fs");
const path = require("path");
const { execSync } = require("child_process");
const os = require("os");
const crypto = require("crypto");

const VERSION = require("../package.json").version;
const REPO = "tuanhung303/apply-patch-mcp";

function getPlatformKey() {
  const platform = process.platform;
  const arch = process.arch;

  const map = {
    "darwin-arm64": "aarch64-apple-darwin",
    "darwin-x64": "x86_64-apple-darwin",
    "linux-x64": "x86_64-unknown-linux-gnu",
    "linux-arm64": "aarch64-unknown-linux-gnu",
    "win32-x64": "x86_64-pc-windows-msvc",
  };

  const key = `${platform}-${arch}`;
  if (!map[key]) {
    console.error(`Unsupported platform: ${key}`);
    console.error(`Supported: ${Object.keys(map).join(", ")}`);
    process.exit(1);
  }
  return map[key];
}

function download(url) {
  return new Promise((resolve, reject) => {
    const handler = (res) => {
      if (res.statusCode >= 300 && res.statusCode < 400 && res.headers.location) {
        return https.get(res.headers.location, handler).on("error", reject);
      }
      if (res.statusCode !== 200) {
        return reject(new Error(`HTTP ${res.statusCode} for ${url}`));
      }
      const chunks = [];
      res.on("data", (chunk) => chunks.push(chunk));
      res.on("end", () => resolve(Buffer.concat(chunks)));
      res.on("error", reject);
    };
    https.get(url, { headers: { "User-Agent": "apply-patch-mcp-installer" } }, handler).on("error", reject);
  });
}

function verifySha256(filePath, expectedHex) {
  const hash = crypto.createHash("sha256");
  hash.update(fs.readFileSync(filePath));
  const actual = hash.digest("hex");
  if (actual !== expectedHex) {
    throw new Error(
      `SHA256 mismatch!\n  Expected: ${expectedHex}\n  Actual:   ${actual}\n  File: ${filePath}`
    );
  }
}

async function doInstall() {
  const platformKey = getPlatformKey();
  const isWindows = process.platform === "win32";
  const ext = isWindows ? ".zip" : ".tar.gz";
  const assetName = `apply-patch-mcp-v${VERSION}-${platformKey}${ext}`;
  const shaAssetName = `${assetName}.sha256`;
  const assetUrl = `https://github.com/${REPO}/releases/download/v${VERSION}/${assetName}`;
  const shaUrl = `https://github.com/${REPO}/releases/download/v${VERSION}/${shaAssetName}`;

  const cacheBinDir = path.join(os.homedir(), ".mcp-apply-patch", "bin");
  const binName = isWindows ? "apply-patch-mcp.exe" : "apply-patch-mcp";
  const binPath = path.join(cacheBinDir, binName);

  // Check if correct version is already installed
  if (fs.existsSync(binPath)) {
    try {
      const out = require("child_process").execSync(`"${binPath}" --version`, {
        timeout: 5000,
        encoding: "utf-8",
      });
      const installed = out.toString().trim();
      if (installed === VERSION) {
        console.error(`apply-patch-mcp v${VERSION} already installed.`);
        return binPath;
      }
      console.error(`Cached binary is v${installed}, need v${VERSION}. Reinstalling...`);
      fs.unlinkSync(binPath);
    } catch (e) {
      // Binary crashed or can't run — force reinstall
      console.error(`Cached binary broken (${e.message || "unknown error"}). Reinstalling...`);
      try { fs.unlinkSync(binPath); } catch (_) { /* ignore */ }
    }
  }

  console.error(`Downloading apply-patch-mcp v${VERSION} for ${platformKey}...`);

  try {
    // Download the binary and its SHA256 checksum in parallel
    const [binaryData, shaData] = await Promise.all([
      download(assetUrl),
      download(shaUrl),
    ]);

    const tmpFile = path.join(os.tmpdir(), assetName);
    const tmpShaFile = path.join(os.tmpdir(), shaAssetName);
    fs.writeFileSync(tmpFile, binaryData);
    fs.writeFileSync(tmpShaFile, shaData);

    // Verify SHA256 checksum before extracting
    const expectedSha256 = shaData.toString("utf-8").trim().split(/\s+/)[0];
    console.error("Verifying SHA256 checksum...");
    verifySha256(tmpFile, expectedSha256);
    console.error("SHA256 checksum verified.");

    fs.mkdirSync(cacheBinDir, { recursive: true });

    if (isWindows) {
      execSync(`powershell -Command "Expand-Archive -Path '${tmpFile}' -DestinationPath '${cacheBinDir}' -Force"`, { stdio: "inherit" });
    } else {
      execSync(`tar xzf "${tmpFile}" -C "${cacheBinDir}"`, { stdio: "inherit" });
    }

    // Ensure executable permissions
    try { fs.chmodSync(binPath, 0o755); } catch (e) { /* ignore */ }

    // Remove macOS quarantine attribute if present
    if (process.platform === 'darwin') {
      try { execSync(`xattr -d com.apple.quarantine "${binPath}"`, { stdio: 'ignore' }); } catch (e) { /* ignore — attribute not present */ }
    }

    try { fs.unlinkSync(tmpFile); } catch (e) { /* ignore */ }
    try { fs.unlinkSync(tmpShaFile); } catch (e) { /* ignore */ }

    console.error(`apply-patch-mcp v${VERSION} installed to ${binPath}.`);
    return binPath;
  } catch (err) {
    if (err.message.includes("SHA256 mismatch")) {
      console.error(`SECURITY: ${err.message}`);
    } else {
      console.error(`Failed to download binary: ${err.message}`);
    }
    console.error(`URL: ${assetUrl}`);
    console.error("You can build from source: cargo build --release");
    throw err;
  }
}

module.exports = { doInstall };

if (require.main === module) {
  doInstall().catch((e) => process.exit(1));
}
