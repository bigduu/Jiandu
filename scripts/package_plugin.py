#!/usr/bin/env python3
"""Build deterministic Jiandu archives and Bamboo's checksum-pinned plugin bundle.

Python standard library only. Packaging never runs Jiandu or accesses its store.
"""

import argparse
import gzip
import hashlib
import io
import json
import re
import tarfile
import zipfile
from pathlib import Path


ROOT = Path(__file__).resolve().parent.parent
TARGETS = {
    "macos": ("universal-apple-darwin", "tar.gz", "jiandu"),
    "windows": ("x86_64-pc-windows-msvc", "zip", "jiandu.exe"),
    "linux": ("x86_64-unknown-linux-gnu", "tar.gz", "jiandu"),
}
RELEASE_BASE = "https://github.com/bigduu/Jiandu/releases/download"


def workspace_version():
    source = (ROOT / "Cargo.toml").read_text(encoding="utf-8")
    section = source.split("[workspace.package]\n", 1)[1].split("\n[", 1)[0]
    return re.search(r'^version = "([^"]+)"$', section, re.MULTILINE).group(1)


def validate_version(version):
    if not re.fullmatch(r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(?:-[0-9A-Za-z.-]+)?", version):
        raise ValueError("version must be a release semver, never latest or a path")


def artifact_name(version, platform):
    validate_version(version)
    target, extension, _ = TARGETS[platform]
    return "jiandu-v{}-{}.{}".format(version, target, extension)


def bundle_name(version):
    validate_version(version)
    return "jiandu-plugin-v{}.tar.gz".format(version)


def sha256(data):
    return hashlib.sha256(data).hexdigest()


def tar_bytes(entries):
    """Entries are (relative name, bytes, mode); no timestamps/owners leak in."""
    output = io.BytesIO()
    with gzip.GzipFile(fileobj=output, mode="wb", filename="", mtime=0) as compressed:
        with tarfile.open(fileobj=compressed, mode="w", format=tarfile.USTAR_FORMAT) as archive:
            for name, data, mode in entries:
                info = tarfile.TarInfo(name)
                info.size = len(data)
                info.mode = mode
                archive.addfile(info, io.BytesIO(data))
    return output.getvalue()


def pack_artifact(binary, platform, version, output):
    if binary.is_symlink() or not binary.is_file():
        raise ValueError("binary must be a non-empty regular file, not a symlink")
    data = binary.read_bytes()
    if not data:
        raise ValueError("binary must be a non-empty regular file, not a symlink")
    _, extension, filename = TARGETS[platform]
    if extension == "zip":
        buffer = io.BytesIO()
        with zipfile.ZipFile(buffer, mode="w", compression=zipfile.ZIP_DEFLATED) as archive:
            info = zipfile.ZipInfo(filename, date_time=(1980, 1, 1, 0, 0, 0))
            info.create_system = 3
            info.external_attr = 0o100755 << 16
            info.compress_type = zipfile.ZIP_DEFLATED
            archive.writestr(info, data)
        packed = buffer.getvalue()
    else:
        packed = tar_bytes([(filename, data, 0o755)])
    output.mkdir(parents=True, exist_ok=True)
    path = output / artifact_name(version, platform)
    path.write_bytes(packed)
    return path


def verify_artifact(path, platform, expected_sha256):
    data = path.read_bytes()
    if not re.fullmatch(r"[0-9a-f]{64}", expected_sha256) or sha256(data) != expected_sha256:
        raise ValueError("artifact sha256 mismatch: {}".format(path.name))
    _, extension, filename = TARGETS[platform]
    if extension == "zip":
        with zipfile.ZipFile(io.BytesIO(data)) as archive:
            entries = archive.infolist()
            if len(entries) != 1 or entries[0].filename != filename:
                raise ValueError("archive must contain only the root executable " + filename)
            entry = entries[0]
            if (entry.external_attr >> 16) & 0o170000 != 0o100000 or not archive.read(entry):
                raise ValueError("archive executable must be a non-empty regular file")
    else:
        with tarfile.open(fileobj=io.BytesIO(data), mode="r:gz") as archive:
            entries = archive.getmembers()
            if len(entries) != 1 or entries[0].name != filename:
                raise ValueError("archive must contain only the root executable " + filename)
            entry = entries[0]
            if not entry.isfile() or not entry.mode & 0o111 or entry.size == 0:
                raise ValueError("archive executable must be a non-empty executable regular file")
            if len(archive.extractfile(entry).read()) != entry.size:
                raise ValueError("archive executable is truncated")


def json_bytes(value):
    return (json.dumps(value, ensure_ascii=False, indent=2) + "\n").encode("utf-8")


def assemble(artifacts, output, version):
    validate_version(version)
    base = RELEASE_BASE + "/v" + version + "/"
    manifest = {
        "id": "jiandu",
        "name": "Jiandu shared memory connector",
        "version": version,
        "description": "Optional CLI/MCP connector for other clients. Bodhi keeps its built-in memory.",
        "platforms": list(TARGETS),
        "provides": {},
        "artifacts": {},
    }
    for platform in TARGETS:
        path = artifacts / artifact_name(version, platform)
        digest = sha256(path.read_bytes())
        verify_artifact(path, platform, digest)
        manifest["artifacts"][platform] = {"url": base + path.name, "sha256": digest}
    output.mkdir(parents=True, exist_ok=True)
    for platform in TARGETS:
        path = artifacts / artifact_name(version, platform)
        destination = output / path.name
        if path.resolve() != destination.resolve():
            destination.write_bytes(path.read_bytes())
    manifest_data = json_bytes(manifest)
    (output / "plugin.json").write_bytes(manifest_data)
    bundle = output / bundle_name(version)
    guide = (ROOT / "docs/optional-plugin.md").read_text(encoding="utf-8").replace(
        "](design/mcp-call-context.md)",
        "](https://github.com/bigduu/Jiandu/blob/v{}/docs/design/mcp-call-context.md)".format(version),
    ).encode("utf-8")
    bundle.write_bytes(tar_bytes([
        ("plugin.json", manifest_data, 0o644),
        ("README.md", guide, 0o644),
        ("LICENSE", (ROOT / "LICENSE").read_bytes(), 0o644),
    ]))
    (output / "catalog-entry.json").write_bytes(json_bytes({
        "id": "jiandu", "version": version,
        "url": base + bundle.name, "sha256": sha256(bundle.read_bytes()),
    }))
    names = [artifact_name(version, platform) for platform in TARGETS]
    names += ["plugin.json", bundle.name, "catalog-entry.json"]
    sums = "".join("{}  {}\n".format(sha256((output / name).read_bytes()), name) for name in sorted(names))
    (output / "SHA256SUMS").write_text(sums, encoding="utf-8")
    return manifest


def write_checksums(directory, names):
    sums = "".join("{}  {}\n".format(sha256((directory / name).read_bytes()), name) for name in sorted(names))
    (directory / "SHA256SUMS").write_text(sums, encoding="utf-8")


def verify_release(directory, version):
    """Validate the candidate before publishing; never extract unverified data."""
    validate_version(version)
    required = {artifact_name(version, platform) for platform in TARGETS}
    required.update(["plugin.json", bundle_name(version), "catalog-entry.json"])
    signed_names = {"plugin.json.sig", bundle_name(version) + ".sig", "publisher-key.json"}
    if any((directory / name).exists() for name in signed_names):
        if not all((directory / name).is_file() for name in signed_names):
            raise ValueError("signed release requires both sidecars and publisher key")
        required.update(signed_names)
    if {path.name for path in directory.iterdir()} != required | {"SHA256SUMS"}:
        raise ValueError("release directory must contain only the verified asset inventory")
    checksums = {}
    for line in (directory / "SHA256SUMS").read_text(encoding="utf-8").splitlines():
        match = re.fullmatch(r"([0-9a-f]{64})  ([A-Za-z0-9._-]+)", line)
        if not match or match[2] in checksums:
            raise ValueError("invalid or duplicate checksum entry")
        checksums[match[2]] = match[1]
    if set(checksums) != required:
        raise ValueError("checksum inventory must match this exact release")
    for name, digest in checksums.items():
        path = directory / name
        if path.is_symlink() or sha256(path.read_bytes()) != digest:
            raise ValueError("release sha256 mismatch: " + name)
    manifest = json.loads((directory / "plugin.json").read_text(encoding="utf-8"))
    if manifest["id"] != "jiandu" or manifest["version"] != version or manifest["provides"] != {}:
        raise ValueError("Jiandu must not auto-register tools, services or Skills")
    if manifest["platforms"] != list(TARGETS) or set(manifest["artifacts"]) != set(TARGETS):
        raise ValueError("platform inventory differs from the producer contract")
    base = RELEASE_BASE + "/v" + version + "/"
    for platform in TARGETS:
        path = directory / artifact_name(version, platform)
        artifact = manifest["artifacts"][platform]
        if artifact != {"url": base + path.name, "sha256": checksums[path.name]}:
            raise ValueError("artifact identity differs from checksums: " + platform)
        verify_artifact(path, platform, artifact["sha256"])
    bundle = directory / bundle_name(version)
    with tarfile.open(bundle, "r:gz") as archive:
        entries = archive.getmembers()
        if [entry.name for entry in entries] != ["plugin.json", "README.md", "LICENSE"] or not all(entry.isfile() for entry in entries):
            raise ValueError("invalid plugin bundle layout")
        if archive.extractfile(entries[0]).read() != (directory / "plugin.json").read_bytes():
            raise ValueError("plugin bundle manifest differs from release manifest")
    catalog = json.loads((directory / "catalog-entry.json").read_text(encoding="utf-8"))
    if catalog != {"id": "jiandu", "version": version, "url": base + bundle.name, "sha256": checksums[bundle.name]}:
        raise ValueError("catalog bundle identity differs from checksums")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--version", default=workspace_version())
    commands = parser.add_subparsers(dest="command", required=True)
    pack = commands.add_parser("pack")
    pack.add_argument("--binary", type=Path, required=True)
    pack.add_argument("--platform", choices=TARGETS, required=True)
    pack.add_argument("--output", type=Path, required=True)
    combine = commands.add_parser("assemble")
    combine.add_argument("--artifacts", type=Path, required=True)
    combine.add_argument("--output", type=Path, required=True)
    verify = commands.add_parser("verify")
    verify.add_argument("--directory", type=Path, required=True)
    args = parser.parse_args()
    validate_version(args.version)
    if args.command == "pack":
        print(pack_artifact(args.binary, args.platform, args.version, args.output))
    elif args.command == "assemble":
        assemble(args.artifacts, args.output, args.version)
        verify_release(args.output, args.version)
        print(args.output / "catalog-entry.json")
    else:
        verify_release(args.directory, args.version)
        print("verified Jiandu v" + args.version)


if __name__ == "__main__":
    main()
