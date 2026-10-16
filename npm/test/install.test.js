"use strict";

const assert = require("assert");
const crypto = require("crypto");
const fs = require("fs");
const os = require("os");
const path = require("path");
const zlib = require("zlib");
const test = require("node:test");

const {
  VERSION,
  releaseFor,
  unsupportedMessage,
  parseSha256Sums,
  verifyArchive,
  extractBinary,
  installFromBuffers,
} = require("../install.js");

const ELF = Buffer.from([0x7f, 0x45, 0x4c, 0x46, 0x02, 0x01, 0x01, 0x00]);
const MACHO = Buffer.from([0xca, 0xfe, 0xba, 0xbe, 0x00, 0x00, 0x00, 0x02]);
const PE = Buffer.from([0x4d, 0x5a, 0x90, 0x00, 0x03, 0x00, 0x00, 0x00]);

function writeOctal(buf, offset, value, length) {
  const text = value.toString(8).padStart(length - 1, "0");
  buf.write(text, offset, length - 1, "ascii");
  buf[offset + length - 1] = 0;
}

function tarGz(entries) {
  const chunks = [];
  for (const entry of entries) {
    const header = Buffer.alloc(512);
    const slash = entry.name.lastIndexOf("/");
    const prefix = slash === -1 ? "" : entry.name.slice(0, slash);
    const name = slash === -1 ? entry.name : entry.name.slice(slash + 1);
    assert.ok(name.length < 100, "fixture name fits the ustar name field");
    assert.ok(prefix.length < 155, "fixture prefix fits the ustar prefix field");
    header.write(name, 0, "utf8");
    writeOctal(header, 100, entry.mode || 0o755, 8);
    writeOctal(header, 108, 0, 8);
    writeOctal(header, 116, 0, 8);
    writeOctal(header, 124, entry.data.length, 12);
    writeOctal(header, 136, 0, 12);
    header[156] = 0x30;
    header.write("ustar\0", 257, "binary");
    header.write("00", 263, "binary");
    if (prefix) header.write(prefix, 345, "utf8");
    for (let i = 148; i < 156; i++) header[i] = 0x20;
    let sum = 0;
    for (const byte of header) sum += byte;
    writeOctal(header, 148, sum, 7);
    header[155] = 0x20;
    const body = Buffer.alloc(Math.ceil(entry.data.length / 512) * 512);
    entry.data.copy(body);
    chunks.push(header, body);
  }
  chunks.push(Buffer.alloc(1024));
  return zlib.gzipSync(Buffer.concat(chunks));
}

function zipArchive(entries, method) {
  const locals = [];
  const centrals = [];
  let offset = 0;
  for (const entry of entries) {
    const nameBuf = Buffer.from(entry.name, "utf8");
    const payload = method === 8 ? zlib.deflateRawSync(entry.data) : entry.data;
    const local = Buffer.alloc(30 + nameBuf.length);
    local.writeUInt32LE(0x04034b50, 0);
    local.writeUInt16LE(20, 4);
    local.writeUInt16LE(method, 8);
    local.writeUInt32LE(payload.length, 18);
    local.writeUInt32LE(entry.data.length, 22);
    local.writeUInt16LE(nameBuf.length, 26);
    nameBuf.copy(local, 30);
    locals.push(local, payload);
    const central = Buffer.alloc(46 + nameBuf.length);
    central.writeUInt32LE(0x02014b50, 0);
    central.writeUInt16LE(20, 4);
    central.writeUInt16LE(20, 6);
    central.writeUInt16LE(method, 10);
    central.writeUInt32LE(payload.length, 20);
    central.writeUInt32LE(entry.data.length, 24);
    central.writeUInt16LE(nameBuf.length, 28);
    central.writeUInt32LE(offset, 42);
    nameBuf.copy(central, 46);
    centrals.push(central);
    offset += local.length + payload.length;
  }
  const cd = Buffer.concat(centrals);
  const eocd = Buffer.alloc(22);
  eocd.writeUInt32LE(0x06054b50, 0);
  eocd.writeUInt16LE(entries.length, 8);
  eocd.writeUInt16LE(entries.length, 10);
  eocd.writeUInt32LE(cd.length, 12);
  eocd.writeUInt32LE(offset, 16);
  return Buffer.concat([...locals, cd, eocd]);
}

function sumsFor(asset, bytes) {
  const digest = crypto.createHash("sha256").update(bytes).digest("hex");
  return {
    digest,
    text: `${digest}  ${asset}\n${"a".repeat(64)}  other.txt\n`,
  };
}

test("release assets match the published v0.3.3 set", () => {
  assert.strictEqual(VERSION, "0.3.3");
  const darwinArm = releaseFor("darwin", "arm64");
  const darwinIntel = releaseFor("darwin", "x64");
  assert.strictEqual(darwinArm.asset, "jiandu-v0.3.3-universal-apple-darwin.tar.gz");
  assert.deepStrictEqual(darwinArm, darwinIntel);
  assert.strictEqual(darwinArm.kind, "macho");
  assert.strictEqual(darwinArm.format, "tar.gz");
  assert.strictEqual(darwinArm.binaryName, "jiandu");

  const windows = releaseFor("win32", "x64");
  assert.strictEqual(windows.asset, "jiandu-v0.3.3-x86_64-pc-windows-msvc.zip");
  assert.strictEqual(windows.binaryName, "jiandu.exe");
  assert.strictEqual(windows.kind, "pe");

  const linux = releaseFor("linux", "x64");
  assert.strictEqual(linux.asset, "jiandu-v0.3.3-x86_64-unknown-linux-gnu.tar.gz");
  assert.strictEqual(linux.kind, "elf");
  assert.strictEqual(linux.binaryName, "jiandu");

  for (const [platform, arch] of [
    ["win32", "arm64"],
    ["linux", "arm64"],
    ["linux", "ia32"],
    ["darwin", "arm"],
    ["freebsd", "x64"],
  ]) {
    assert.strictEqual(releaseFor(platform, arch), null);
    const message = unsupportedMessage(platform, arch);
    assert.match(message, new RegExp(platform === "win32" ? "Windows" : platform === "linux" ? "Linux" : ".+"));
    assert.match(message, /v0\.3\.3/);
    if (platform === "win32" && arch === "arm64") assert.match(message, /ARM64/);
    if (platform === "linux" && arch === "arm64") assert.match(message, /ARM64/);
  }
});

test("SHA256SUMS uses the release's two-space inventory", () => {
  const parsed = parseSha256Sums(
    "bb11118c7a05fd2804ae732951af436dfbc2e0ffb073ffddc8fb181a1d9f3202  jiandu-v0.3.3-universal-apple-darwin.tar.gz\n\n",
  );
  assert.strictEqual(
    parsed.get("jiandu-v0.3.3-universal-apple-darwin.tar.gz"),
    "bb11118c7a05fd2804ae732951af436dfbc2e0ffb073ffddc8fb181a1d9f3202",
  );
  assert.throws(() => parseSha256Sums("abc  file\n"), /invalid SHA256SUMS/);
  assert.throws(
    () => parseSha256Sums(`${"ab".repeat(32)}  same\n${"cd".repeat(32)}  same\n`),
    /duplicate/,
  );
});

test("checksum mismatch does not write a binary", () => {
  const release = releaseFor("linux", "x64");
  const archive = tarGz([{ name: "jiandu", data: ELF }]);
  const dest = path.join(fs.mkdtempSync(path.join(os.tmpdir(), "jiandu-npm-")), "jiandu");
  const { text } = sumsFor(release.asset, Buffer.from("not-the-archive"));
  assert.throws(
    () => installFromBuffers({ archive, sumsText: text, release, dest }),
    /checksum mismatch/,
  );
  assert.strictEqual(fs.existsSync(dest), false);
});

test("a missing checksum entry is refused", () => {
  const archive = tarGz([{ name: "jiandu", data: ELF }]);
  assert.throws(
    () => verifyArchive(archive, `${"ab".repeat(32)}  other.txt\n`, "jiandu-v0.3.3-x86_64-unknown-linux-gnu.tar.gz"),
    /no entry/,
  );
});

test("ustar and zip payloads round-trip when the checksum matches", () => {
  const linux = releaseFor("linux", "x64");
  const tar = tarGz([
    { name: "README", data: Buffer.from("not the binary") },
    { name: "dist/jiandu", data: ELF },
  ]);
  const tarSums = sumsFor(linux.asset, tar);
  const tarDest = path.join(fs.mkdtempSync(path.join(os.tmpdir(), "jiandu-npm-")), "jiandu");
  installFromBuffers({ archive: tar, sumsText: tarSums.text, release: linux, dest: tarDest });
  assert.deepStrictEqual(fs.readFileSync(tarDest), ELF);
  assert.ok(fs.statSync(tarDest).mode & 0o111);

  const windows = releaseFor("win32", "x64");
  const zip = zipArchive([{ name: "nested/jiandu.exe", data: PE }], 8);
  const zipSums = sumsFor(windows.asset, zip);
  const zipDest = path.join(fs.mkdtempSync(path.join(os.tmpdir(), "jiandu-npm-")), "jiandu.exe");
  installFromBuffers({ archive: zip, sumsText: zipSums.text, release: windows, dest: zipDest });
  assert.deepStrictEqual(fs.readFileSync(zipDest), PE);

  const darwin = releaseFor("darwin", "arm64");
  const machoTar = tarGz([{ name: "jiandu", data: MACHO }]);
  assert.deepStrictEqual(extractBinary(machoTar, darwin.format, darwin.binaryName, darwin.kind), MACHO);
});

test("wrong magic, missing names, and store zips fail closed", () => {
  const linux = releaseFor("linux", "x64");
  assert.throws(
    () => extractBinary(tarGz([{ name: "jiandu", data: PE }]), linux.format, linux.binaryName, linux.kind),
    /not an ELF/,
  );
  assert.throws(
    () => extractBinary(tarGz([{ name: "other", data: ELF }]), linux.format, linux.binaryName, linux.kind),
    /not found/,
  );
  const stored = zipArchive([{ name: "jiandu.exe", data: PE }], 0);
  assert.deepStrictEqual(extractBinary(stored, "zip", "jiandu.exe", "pe"), PE);
  assert.throws(() => extractBinary(stored, "zip", "jiandu.exe", "elf"), /not an ELF/);
});

test("server.json and package.json name the same registry entry", () => {
  const pkg = require("../package.json");
  const server = JSON.parse(fs.readFileSync(path.join(__dirname, "..", "..", "server.json"), "utf8"));
  assert.strictEqual(pkg.name, "@bigduu/jiandu");
  assert.strictEqual(pkg.version, "0.3.3");
  assert.strictEqual(pkg.mcpName, "io.github.bigduu/jiandu");
  assert.strictEqual(pkg.license, "MIT");
  assert.strictEqual(pkg.publishConfig.access, "public");
  assert.strictEqual(pkg.bin.jiandu, "bin/jiandu.js");
  assert.deepStrictEqual(pkg.os, ["darwin", "win32", "linux"]);
  assert.ok(pkg.cpu.includes("x64") && pkg.cpu.includes("arm64"));
  assert.strictEqual(server.name, pkg.mcpName);
  assert.strictEqual(server.version, pkg.version);
  assert.ok(server.description.length >= 1 && server.description.length <= 100);
  assert.strictEqual(server.packages.length, 1);
  assert.strictEqual(server.packages[0].registryType, "npm");
  assert.strictEqual(server.packages[0].identifier, pkg.name);
  assert.strictEqual(server.packages[0].version, pkg.version);
  assert.strictEqual(server.packages[0].transport.type, "stdio");
  assert.strictEqual(server.websiteUrl, "https://github.com/bigduu/Jiandu#install");
});
