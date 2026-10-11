#!/usr/bin/env python3
"""Sign/verify a candidate with a provisioned Ed25519 key; never create a key.

Requires OpenSSL 3. Publication uses a protected environment's PEM secret.
The provisioned key must match the committed public anchor before signing.
Public-key review and acceptance in the consumer remain a separate action.
"""

import argparse
import json
import re
import subprocess
import tempfile
from pathlib import Path

from package_plugin import bundle_name, json_bytes, verify_release, workspace_version, write_checksums


ED25519_SPKI_PREFIX = bytes.fromhex("302a300506032b6570032100")
DEFAULT_PUBLIC_KEY_FILE = Path(__file__).resolve().parents[1] / "plugin" / "signing-key.pub"


def read_public_key(public_key_file):
    public_key = public_key_file.read_text(encoding="ascii").strip()
    if not re.fullmatch(r"[0-9a-f]{64}", public_key):
        raise ValueError("public anchor must be a raw 32-byte Ed25519 public key in lowercase hex")
    return public_key


def openssl(*arguments):
    return subprocess.run(["openssl", *map(str, arguments)], check=True, capture_output=True,
                          stdin=subprocess.DEVNULL, timeout=30).stdout


def verify_signature(path, signature, public_key):
    if not re.fullmatch(r"[0-9a-f]{64}", public_key):
        raise ValueError("publisher key must be a raw 32-byte Ed25519 public key in lowercase hex")
    text = signature.read_text(encoding="ascii").strip()
    if not re.fullmatch(r"[0-9a-f]{128}", text):
        raise ValueError("signature must be a raw 64-byte Ed25519 signature in lowercase hex")
    with tempfile.TemporaryDirectory(prefix="jiandu-signature-verify-") as temporary:
        key_path = Path(temporary) / "public.der"
        sig_path = Path(temporary) / "signature.bin"
        key_path.write_bytes(ED25519_SPKI_PREFIX + bytes.fromhex(public_key))
        sig_path.write_bytes(bytes.fromhex(text))
        openssl("pkeyutl", "-verify", "-rawin", "-pubin", "-keyform", "DER",
                "-inkey", key_path, "-in", path, "-sigfile", sig_path)


def verify_signed(directory, version, public_key_file=DEFAULT_PUBLIC_KEY_FILE):
    verify_release(directory, version)
    public_key = read_public_key(public_key_file)
    key = json.loads((directory / "publisher-key.json").read_text(encoding="utf-8"))
    if set(key) != {"algorithm", "label", "public_key"} or key["algorithm"] != "ed25519" or key["label"] != "jiandu-official":
        raise ValueError("unexpected publisher key record")
    if key["public_key"] != public_key:
        raise ValueError("publisher key does not match the reviewed public anchor")
    for name in ["plugin.json", bundle_name(version)]:
        verify_signature(directory / name, directory / (name + ".sig"), public_key)


def sign(directory, version, key_file, public_key_file=DEFAULT_PUBLIC_KEY_FILE):
    verify_release(directory, version)
    public_key = read_public_key(public_key_file)
    public_der = openssl("pkey", "-in", key_file, "-pubout", "-outform", "DER")
    if not public_der.startswith(ED25519_SPKI_PREFIX) or len(public_der) != len(ED25519_SPKI_PREFIX) + 32:
        raise ValueError("provisioned signing key is not Ed25519")
    if public_der[len(ED25519_SPKI_PREFIX):].hex() != public_key:
        raise ValueError("provisioned signing key does not match the reviewed public anchor")
    for name in ["plugin.json", bundle_name(version)]:
        signature = openssl("pkeyutl", "-sign", "-rawin", "-inkey", key_file, "-in", directory / name)
        if len(signature) != 64:
            raise ValueError("unexpected Ed25519 signature length")
        (directory / (name + ".sig")).write_text(signature.hex() + "\n", encoding="ascii")
    (directory / "publisher-key.json").write_bytes(json_bytes({
        "algorithm": "ed25519", "label": "jiandu-official",
        "public_key": public_key,
    }))
    names = [line.split("  ", 1)[1] for line in (directory / "SHA256SUMS").read_text().splitlines()]
    write_checksums(directory, set(names) | {"plugin.json.sig", bundle_name(version) + ".sig", "publisher-key.json"})
    verify_signed(directory, version, public_key_file)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--directory", type=Path, required=True)
    parser.add_argument("--version", default=workspace_version())
    parser.add_argument("--key-file", type=Path, help="Provisioned Ed25519 PEM; omitted for verification only")
    parser.add_argument("--public-key-file", type=Path, default=DEFAULT_PUBLIC_KEY_FILE,
                        help="Reviewed raw Ed25519 public anchor (default: plugin/signing-key.pub)")
    arguments = parser.parse_args()
    if arguments.key_file:
        sign(arguments.directory, arguments.version, arguments.key_file, arguments.public_key_file)
    else:
        verify_signed(arguments.directory, arguments.version, arguments.public_key_file)
    print("verified Ed25519 sidecars against the public anchor; consumer must separately trust that key")
