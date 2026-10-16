// Postinstall: fetch the prebuilt Jiandu binary for this package version from
// its GitHub Release and vendor it next to the launcher.
//
// The package version matches the release tag (`jiandu-v${VERSION}-…`). macOS
// gets the universal archive; Windows x64 gets the zip; Linux x64 gets the
// gnu tarball. There is nothing to compile.
//
// The release publishes one SHA256SUMS file (two spaces between digest and
// name). This installer refuses to write a binary unless that file has a
// matching entry for the archive.
//
// Idempotent and best-effort: if it fails here (for example `npm ci
// --ignore-scripts`, or an offline install), bin/jiandu.js downloads on first
// launch instead, so the package still works. Unsupported machines skip the
// download here and get a hard error from the launcher.

"use strict";

const fs = require("fs");
const os = require("os");
const path = require("path");
const https = require("https");
const crypto = require("crypto");
const zlib = require("zlib");
const { execFileSync } = require("child_process");

const VERSION = require("./package.json").version;
const REPO = "bigduu/Jiandu";
const BASE = `https://github.com/${REPO}/releases/download/v${VERSION}`;
const VENDOR_DIR = path.join(__dirname, "bin");
const MAX_ARCHIVE_BYTES = 64 * 1024 * 1024;
const MAX_SUMS_BYTES = 1024 * 1024;
const MAX_REDIRECTS = 5;
const CHECKSUM_LINE = /^([a-f0-9]{64})  ([A-Za-z0-9._-]+)$/;

// Which published v${VERSION} archive this machine should run.
// Returns null when this release has no asset for the pair.
function releaseFor(platform, arch) {
  if (platform === "darwin" && (arch === "arm64" || arch === "x64")) {
    return {
      asset: `jiandu-v${VERSION}-universal-apple-darwin.tar.gz`,
      binaryName: "jiandu",
      format: "tar.gz",
      kind: "macho",
    };
  }
  if (platform === "win32" && arch === "x64") {
    return {
      asset: `jiandu-v${VERSION}-x86_64-pc-windows-msvc.zip`,
      binaryName: "jiandu.exe",
      format: "zip",
      kind: "pe",
    };
  }
  if (platform === "linux" && arch === "x64") {
    return {
      asset: `jiandu-v${VERSION}-x86_64-unknown-linux-gnu.tar.gz`,
      binaryName: "jiandu",
      format: "tar.gz",
      kind: "elf",
    };
  }
  return null;
}

function unsupportedMessage(platform, arch) {
  const supported =
    `Jiandu's npm package installs the v${VERSION} release binaries for ` +
    "macOS (Apple Silicon and Intel, universal), Windows x64, and Linux x64 (gnu).";
  if (platform === "win32" && arch === "arm64") {
    return `Windows ARM64 is not in the v${VERSION} release (x64 only). ${supported}`;
  }
  if (platform === "linux" && arch === "arm64") {
    return (
      `Linux ARM64 is not in the v${VERSION} release ` +
      `(x86_64-unknown-linux-gnu only). ${supported}`
    );
  }
  if (platform === "darwin") {
    return `macOS ${arch} is not a published Jiandu build. ${supported}`;
  }
  if (platform === "win32") {
    return `Windows ${arch} is not a published Jiandu build. ${supported}`;
  }
  if (platform === "linux") {
    return `Linux ${arch} is not a published Jiandu build. ${supported}`;
  }
  return `${platform} ${arch} is not a published Jiandu build. ${supported}`;
}

function binaryPath(binaryName) {
  return path.join(VENDOR_DIR, binaryName);
}

function parseSha256Sums(text) {
  const sums = new Map();
  for (const line of String(text).split(/\r?\n/)) {
    if (line.length === 0) continue;
    const match = CHECKSUM_LINE.exec(line);
    if (!match) throw new Error(`invalid SHA256SUMS line: ${line}`);
    if (sums.has(match[2])) throw new Error(`duplicate SHA256SUMS entry for ${match[2]}`);
    sums.set(match[2], match[1]);
  }
  return sums;
}

function verifyArchive(archive, sumsText, asset) {
  const sums = parseSha256Sums(sumsText);
  const want = sums.get(asset);
  if (!want) {
    throw new Error(
      `release SHA256SUMS has no entry for ${asset}; refusing to install an unverified binary`,
    );
  }
  const got = crypto.createHash("sha256").update(archive).digest("hex");
  if (got !== want) {
    throw new Error(`checksum mismatch for ${asset}\n  expected ${want}\n  got      ${got}`);
  }
}

// GET that follows GitHub's HTTPS redirect to the asset CDN.
function fetch(url, redirects = 0, maxBytes = MAX_ARCHIVE_BYTES) {
  return new Promise((resolve, reject) => {
    let parsed;
    try {
      parsed = new URL(url);
    } catch (err) {
      reject(err);
      return;
    }
    if (parsed.protocol !== "https:") {
      reject(new Error(`refusing non-HTTPS URL ${url}`));
      return;
    }
    if (redirects > MAX_REDIRECTS) {
      reject(new Error(`too many redirects for ${url}`));
      return;
    }
    const req = https.get(
      url,
      { headers: { "User-Agent": "jiandu-npm-installer", Accept: "application/octet-stream" } },
      (res) => {
        if (res.statusCode >= 300 && res.statusCode < 400 && res.headers.location) {
          res.resume();
          const next = new URL(res.headers.location, url).href;
          resolve(fetch(next, redirects + 1, maxBytes));
          return;
        }
        if (res.statusCode !== 200) {
          res.resume();
          reject(new Error(`GET ${url} → HTTP ${res.statusCode}`));
          return;
        }
        const chunks = [];
        let size = 0;
        let failed = false;
        res.on("data", (chunk) => {
          if (failed) return;
          size += chunk.length;
          if (size > maxBytes) {
            failed = true;
            req.destroy(new Error(`response exceeded ${maxBytes} bytes`));
            return;
          }
          chunks.push(chunk);
        });
        res.on("end", () => {
          if (!failed) resolve(Buffer.concat(chunks));
        });
      },
    );
    req.setTimeout(60_000, () => {
      req.destroy(new Error(`timed out fetching ${url}`));
    });
    req.on("error", reject);
  });
}

function cString(header, start, length) {
  const raw = header.subarray(start, start + length);
  const zero = raw.indexOf(0);
  return (zero === -1 ? raw : raw.subarray(0, zero)).toString("utf8");
}

// Extract one regular file from a gzipped ustar (the release archive is the
// `jiandu` binary alone, at the archive root).
function extractFromTarGz(gzBuf, binaryName) {
  const tar = zlib.gunzipSync(gzBuf);
  for (let off = 0; off + 512 <= tar.length; ) {
    const header = tar.subarray(off, off + 512);
    if (header.every((byte) => byte === 0)) break;
    const name = cString(header, 0, 100);
    const prefix = header.subarray(257, 262).toString("utf8").startsWith("ustar")
      ? cString(header, 345, 155)
      : "";
    const sizeText = cString(header, 124, 12).trim();
    const size = parseInt(sizeText || "0", 8);
    if (!Number.isSafeInteger(size) || size < 0) throw new Error("invalid tar entry size");
    const dataStart = off + 512;
    if (dataStart + size > tar.length) throw new Error("truncated tar");
    const typeflag = header[156];
    const fullName = prefix ? `${prefix.replace(/\/+$/, "")}/${name}` : name;
    const regular = typeflag === 0 || typeflag === 0x30;
    if (regular && path.posix.basename(fullName) === binaryName) {
      return Buffer.from(tar.subarray(dataStart, dataStart + size));
    }
    off = dataStart + Math.ceil(size / 512) * 512;
  }
  throw new Error(`\`${binaryName}\` not found inside the release tarball`);
}

function entryBase(name) {
  const normalized = name.replace(/\\/g, "/").replace(/\/+$/, "");
  const slash = normalized.lastIndexOf("/");
  return slash === -1 ? normalized : normalized.slice(slash + 1);
}

function need(buf, off, len) {
  if (off < 0 || len < 0 || off + len > buf.length) throw new Error("truncated ZIP");
}

// Extract one file from a ZIP (store or deflate). Sizes come from the central
// directory so a data descriptor in the local header does not matter.
function extractFromZip(zipBuf, binaryName) {
  const eocdSig = 0x06054b50;
  let eocd = -1;
  const min = Math.max(0, zipBuf.length - 22 - 65535);
  for (let i = zipBuf.length - 22; i >= min; i--) {
    if (zipBuf.readUInt32LE(i) === eocdSig) {
      eocd = i;
      break;
    }
  }
  if (eocd < 0) throw new Error("ZIP end-of-central-directory not found");
  need(zipBuf, eocd, 22);
  const total = zipBuf.readUInt16LE(eocd + 10);
  const cdSize = zipBuf.readUInt32LE(eocd + 12);
  const cdOffset = zipBuf.readUInt32LE(eocd + 16);
  if (cdOffset === 0xffffffff || cdSize === 0xffffffff) throw new Error("ZIP64 archives are not supported");
  need(zipBuf, cdOffset, cdSize);
  let off = cdOffset;
  for (let n = 0; n < total; n++) {
    need(zipBuf, off, 46);
    if (zipBuf.readUInt32LE(off) !== 0x02014b50) throw new Error("bad ZIP central directory");
    const method = zipBuf.readUInt16LE(off + 10);
    const compSize = zipBuf.readUInt32LE(off + 20);
    const uncompSize = zipBuf.readUInt32LE(off + 24);
    const nameLen = zipBuf.readUInt16LE(off + 28);
    const extraLen = zipBuf.readUInt16LE(off + 30);
    const commentLen = zipBuf.readUInt16LE(off + 32);
    const localOff = zipBuf.readUInt32LE(off + 42);
    need(zipBuf, off + 46, nameLen);
    const name = zipBuf.subarray(off + 46, off + 46 + nameLen).toString("utf8");
    off += 46 + nameLen + extraLen + commentLen;
    if (entryBase(name) !== binaryName) continue;
    need(zipBuf, localOff, 30);
    if (zipBuf.readUInt32LE(localOff) !== 0x04034b50) throw new Error("bad ZIP local header");
    const localNameLen = zipBuf.readUInt16LE(localOff + 26);
    const localExtraLen = zipBuf.readUInt16LE(localOff + 28);
    const dataStart = localOff + 30 + localNameLen + localExtraLen;
    need(zipBuf, dataStart, compSize);
    const data = zipBuf.subarray(dataStart, dataStart + compSize);
    let out;
    if (method === 0) out = Buffer.from(data);
    else if (method === 8) out = zlib.inflateRawSync(data);
    else throw new Error(`unsupported ZIP compression method ${method}`);
    if (out.length !== uncompSize) {
      throw new Error(`extracted ${binaryName} size ${out.length} != ${uncompSize}`);
    }
    return out;
  }
  throw new Error(`\`${binaryName}\` not found inside the release archive`);
}

function assertExecutable(bin, kind) {
  if (!bin || bin.length < 4) throw new Error("extracted binary is empty");
  if (kind === "pe") {
    if (bin[0] !== 0x4d || bin[1] !== 0x5a) throw new Error("extracted jiandu.exe is not a PE executable");
    return;
  }
  if (kind === "elf") {
    if (bin[0] !== 0x7f || bin[1] !== 0x45 || bin[2] !== 0x4c || bin[3] !== 0x46) {
      throw new Error("extracted jiandu is not an ELF executable");
    }
    return;
  }
  if (kind === "macho") {
    const magic = bin.readUInt32BE(0);
    const machos = new Set([0xcafebabe, 0xbebafeca, 0xfeedface, 0xcefaedfe, 0xfeedfacf, 0xcffaedfe]);
    if (!machos.has(magic)) throw new Error("extracted jiandu is not a Mach-O executable");
    return;
  }
  throw new Error(`unknown executable kind ${kind}`);
}

function extractBinary(archive, format, binaryName, kind) {
  const bin = format === "tar.gz"
    ? extractFromTarGz(archive, binaryName)
    : extractFromZip(archive, binaryName);
  assertExecutable(bin, kind);
  return bin;
}

function writeBinary(dest, bytes) {
  fs.mkdirSync(path.dirname(dest), { recursive: true });
  const tmp = `${dest}.${process.pid}.download`;
  try {
    fs.writeFileSync(tmp, bytes, { mode: 0o755 });
    fs.renameSync(tmp, dest);
  } catch (err) {
    try {
      fs.rmSync(tmp, { force: true });
    } catch {}
    throw err;
  }
  try {
    fs.chmodSync(dest, 0o755);
  } catch {}
}

function installFromBuffers({ archive, sumsText, release, dest }) {
  verifyArchive(archive, sumsText, release.asset);
  const bin = extractBinary(archive, release.format, release.binaryName, release.kind);
  writeBinary(dest, bin);
}

async function main() {
  const release = releaseFor(os.platform(), os.arch());
  if (!release) {
    // Don't hard-fail installs on hosts that pull the package anyway
    // (`--force`, or a CI image). The launcher explains itself.
    console.warn(`[jiandu] skipping binary download: ${unsupportedMessage(os.platform(), os.arch())}`);
    return;
  }
  const dest = binaryPath(release.binaryName);
  try {
    const [archive, sums] = await Promise.all([
      fetch(`${BASE}/${release.asset}`, 0, MAX_ARCHIVE_BYTES),
      fetch(`${BASE}/SHA256SUMS`, 0, MAX_SUMS_BYTES),
    ]);
    installFromBuffers({
      archive,
      sumsText: sums.toString("utf8"),
      release,
      dest,
    });
    // node's download does not set quarantine; clear it if something else did.
    // Windows has no xattr.
    if (os.platform() === "darwin") {
      try {
        execFileSync("xattr", ["-dr", "com.apple.quarantine", dest]);
      } catch {}
    }
    console.log(`[jiandu] installed ${dest} (v${VERSION})`);
  } catch (err) {
    console.warn(
      `[jiandu] could not pre-download the binary (${err.message}); it will be fetched on first run.`,
    );
  }
}

if (require.main === module) {
  main();
}

module.exports = {
  VERSION,
  releaseFor,
  unsupportedMessage,
  binaryPath,
  parseSha256Sums,
  verifyArchive,
  extractBinary,
  writeBinary,
  installFromBuffers,
  fetch,
};
