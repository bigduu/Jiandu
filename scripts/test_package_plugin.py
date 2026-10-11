"""The producer's immutable-byte and Bamboo archive contracts."""

import base64
import io
import json
import subprocess
import tarfile
import tempfile
import unittest
from pathlib import Path

import package_plugin as producer
import sign_plugin as signer


class PluginPackageTests(unittest.TestCase):
    version = "0.3.2"
    test_public_key = "3d4017c3e843895a92b70aa74d1b7ebc9c982ccf2ec4968cc0cd55f12af4660c"

    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.binary = self.root / "source-binary"
        self.binary.write_bytes(b"fixture executable bytes\n")
        self.artifacts = self.root / "artifacts"
        self.output = self.root / "release"
        self.public_key_file = self.root / "test-only-signing-key.pub"
        self.public_key_file.write_text(self.test_public_key + "\n", encoding="ascii")

    def fixture_key_file(self):
        try:
            version = signer.openssl("version")
        except FileNotFoundError:
            self.skipTest("OpenSSL 3 required for publisher signing test")
        if not version.startswith(b"OpenSSL 3."):
            self.skipTest("OpenSSL 3 required for publisher signing test")
        # Public RFC 8032 section 7.1 TEST 2 seed, ONLY for temporary test data.
        # Never use this well-known seed as a publisher credential.
        seed = bytes.fromhex("4ccd089b28ff96da9db6c346ec114e0f5b8a319f35aba624da8cf6ed4fb8a6fb")
        pkcs8 = bytes.fromhex("302e020100300506032b657004220420") + seed
        key_file = self.root / "public-rfc8032-test-only.pem"
        key_file.write_bytes(b"-----BEGIN PRIVATE KEY-----\n" + base64.b64encode(pkcs8) + b"\n-----END PRIVATE KEY-----\n")
        key_file.chmod(0o600)
        return key_file

    def candidate(self):
        for platform in producer.TARGETS:
            producer.pack_artifact(self.binary, platform, self.version, self.artifacts)
        producer.assemble(self.artifacts, self.output, self.version)

    def rewrite_checksums(self):
        # Simulate internally consistent tampering; semantic checks must still fail.
        names = sorted(path.name for path in self.output.iterdir() if path.name != "SHA256SUMS")
        (self.output / "SHA256SUMS").write_text("".join(
            "{}  {}\n".format(producer.sha256((self.output / name).read_bytes()), name)
            for name in names), encoding="utf-8")

    def test_all_platform_archives_are_deterministic_with_one_root_binary(self):
        for platform in producer.TARGETS:
            with self.subTest(platform=platform):
                first = producer.pack_artifact(self.binary, platform, self.version, self.artifacts)
                second = producer.pack_artifact(self.binary, platform, self.version, self.root / "second")
                self.assertEqual(first.read_bytes(), second.read_bytes())
                producer.verify_artifact(first, platform, producer.sha256(first.read_bytes()))

    def test_candidate_has_exact_urls_two_checksum_layers_and_no_auto_registration(self):
        self.candidate()
        producer.verify_release(self.output, self.version)
        manifest = json.loads((self.output / "plugin.json").read_text())
        self.assertEqual(manifest["provides"], {})
        self.assertEqual(manifest["platforms"], ["macos", "windows", "linux"])
        for platform, artifact in manifest["artifacts"].items():
            self.assertEqual(artifact["url"], producer.RELEASE_BASE + "/v" + self.version + "/" + producer.artifact_name(self.version, platform))
            self.assertEqual(artifact["sha256"], producer.sha256((self.artifacts / producer.artifact_name(self.version, platform)).read_bytes()))
        identity = json.loads((self.output / "catalog-entry.json").read_text())
        self.assertEqual(identity["sha256"], producer.sha256((self.output / producer.bundle_name(self.version)).read_bytes()))
        with tarfile.open(self.output / producer.bundle_name(self.version), "r:gz") as bundle:
            guide = bundle.extractfile("README.md").read().decode("utf-8")
        self.assertIn("https://github.com/bigduu/Jiandu/blob/v" + self.version + "/plugin/signing-key.pub", guide)
        self.assertNotIn("](../plugin/signing-key.pub)", guide)
        first = {path.name: path.read_bytes() for path in self.output.iterdir()}
        producer.assemble(self.artifacts, self.output, self.version)
        self.assertEqual(first, {path.name: path.read_bytes() for path in self.output.iterdir()})

    def test_corrupted_archive_fails_checksum_before_archive_parse(self):
        path = self.root / "corrupt.tar.gz"
        path.write_bytes(b"not an archive")
        with self.assertRaisesRegex(ValueError, "sha256 mismatch"):
            producer.verify_artifact(path, "macos", "0" * 64)

    def test_archive_rejects_nested_extra_and_symlink_entries(self):
        for entries in [
            [("bin/jiandu", b"fixture", 0o755)],
            [("jiandu", b"fixture", 0o755), ("extra", b"surprise", 0o644)],
        ]:
            with self.subTest(entries=entries):
                path = self.root / "invalid.tar.gz"
                path.write_bytes(producer.tar_bytes(entries))
                with self.assertRaisesRegex(ValueError, "only the root executable"):
                    producer.verify_artifact(path, "linux", producer.sha256(path.read_bytes()))
        path = self.root / "symlink.tar.gz"
        with tarfile.open(path, "w:gz") as archive:
            info = tarfile.TarInfo("jiandu")
            info.type = tarfile.SYMTYPE
            info.linkname = "/outside/jiandu"
            archive.addfile(info)
        with self.assertRaisesRegex(ValueError, "regular file"):
            producer.verify_artifact(path, "linux", producer.sha256(path.read_bytes()))

    def test_assembly_refuses_missing_platform(self):
        producer.pack_artifact(self.binary, "macos", self.version, self.artifacts)
        with self.assertRaises(FileNotFoundError):
            producer.assemble(self.artifacts, self.output, self.version)
        self.assertFalse(self.output.exists())

    def test_release_rejects_corruption_and_duplicate_checksum_inventory(self):
        self.candidate()
        path = self.output / producer.artifact_name(self.version, "windows")
        path.write_bytes(path.read_bytes() + b"tampered")
        with self.assertRaisesRegex(ValueError, "release sha256 mismatch"):
            producer.verify_release(self.output, self.version)
        self.candidate()
        sums = self.output / "SHA256SUMS"
        sums.write_text(sums.read_text() + sums.read_text().splitlines()[0] + "\n")
        with self.assertRaisesRegex(ValueError, "duplicate checksum"):
            producer.verify_release(self.output, self.version)

    def test_release_rejects_mutable_url_even_with_matching_outer_checksums(self):
        self.candidate()
        path = self.output / "plugin.json"
        manifest = json.loads(path.read_text())
        manifest["artifacts"]["macos"]["url"] = "https://github.com/bigduu/Jiandu/releases/latest/download/jiandu.tar.gz"
        path.write_bytes(producer.json_bytes(manifest))
        self.rewrite_checksums()
        with self.assertRaisesRegex(ValueError, "artifact identity"):
            producer.verify_release(self.output, self.version)

    def test_release_rejects_duplicate_mcp_registration(self):
        self.candidate()
        path = self.output / "plugin.json"
        manifest = json.loads(path.read_text())
        manifest["provides"] = {"mcp_servers": [{"id": "jiandu"}]}
        path.write_bytes(producer.json_bytes(manifest))
        self.rewrite_checksums()
        with self.assertRaisesRegex(ValueError, "must not auto-register"):
            producer.verify_release(self.output, self.version)

    def test_release_rejects_catalog_hash_mismatch(self):
        self.candidate()
        path = self.output / "catalog-entry.json"
        catalog = json.loads(path.read_text())
        catalog["sha256"] = "0" * 64
        path.write_bytes(producer.json_bytes(catalog))
        self.rewrite_checksums()
        with self.assertRaisesRegex(ValueError, "catalog bundle identity"):
            producer.verify_release(self.output, self.version)

    def test_release_rejects_different_manifest_inside_bundle(self):
        self.candidate()
        path = self.output / producer.bundle_name(self.version)
        with tarfile.open(fileobj=io.BytesIO(path.read_bytes()), mode="r:gz") as archive:
            entries = [(entry.name, archive.extractfile(entry).read(), entry.mode) for entry in archive.getmembers()]
        entries[0] = ("plugin.json", b"{}\n", 0o644)
        path.write_bytes(producer.tar_bytes(entries))
        self.rewrite_checksums()
        with self.assertRaisesRegex(ValueError, "bundle manifest differs"):
            producer.verify_release(self.output, self.version)

    def test_version_rejects_paths_moving_selectors_and_nonstable_suffixes(self):
        for version in ["latest", "../0.3.1", "v0.3.1", "0.3", "0.3.1/asset", "0.3.1\n",
                        "0.4.0-rc.1", "0.4.0-beta", "0.4.0+build.1"]:
            with self.subTest(version=version), self.assertRaises(ValueError):
                producer.validate_version(version)

    def test_partial_signature_inventory_fails_closed(self):
        self.candidate()
        (self.output / "plugin.json.sig").write_text("0" * 128)
        with self.assertRaisesRegex(ValueError, "both sidecars and publisher key"):
            producer.verify_release(self.output, self.version)

    def test_release_rejects_unlisted_files_before_upload(self):
        self.candidate()
        (self.output / "unexpected.txt").write_text("not a release asset")
        with self.assertRaisesRegex(ValueError, "only the verified asset inventory"):
            producer.verify_release(self.output, self.version)

    def test_ed25519_verifier_accepts_public_rfc_vector_and_rejects_tampering(self):
        try:
            version = signer.openssl("version")
        except FileNotFoundError:
            self.skipTest("OpenSSL 3 required for publisher signature verification")
        if not version.startswith(b"OpenSSL 3."):
            self.skipTest("OpenSSL 3 required for publisher signature verification")
        # Public verification-only test vector; no private key is generated/stored.
        # https://datatracker.ietf.org/doc/html/rfc8032#section-7.1 TEST 2
        public_key = "3d4017c3e843895a92b70aa74d1b7ebc9c982ccf2ec4968cc0cd55f12af4660c"
        message = self.root / "rfc8032-message"
        signature = self.root / "rfc8032-signature"
        message.write_bytes(bytes.fromhex("72"))
        signature.write_text(
            "92a009a9f0d4cab8720e820b5f642540a2b27b5416503f8fb3762223ebdb69da"
            "085ac1e43e15996e458f3613d0f11d8c387b2eaeb4302aeeb00d291612bb0c00\n")
        signer.verify_signature(message, signature, public_key)
        message.write_bytes(b"tampered")
        with self.assertRaises(subprocess.CalledProcessError):
            signer.verify_signature(message, signature, public_key)

    def test_fixture_candidate_signs_and_verifies_both_sidecars_end_to_end(self):
        key_file = self.fixture_key_file()
        self.candidate()
        signer.sign(self.output, self.version, key_file, self.public_key_file)
        signer.verify_signed(self.output, self.version, self.public_key_file)
        producer.verify_release(self.output, self.version)
        self.assertEqual(json.loads((self.output / "publisher-key.json").read_text())["public_key"],
                         self.test_public_key)
        for name in ["plugin.json", producer.bundle_name(self.version)]:
            self.assertEqual(len((self.output / (name + ".sig")).read_text().strip()), 128)
        self.assertFalse(any("PRIVATE KEY" in path.read_bytes().decode("utf-8", errors="ignore")
                             for path in self.output.iterdir()))

    def test_wrong_signing_key_fails_before_changing_candidate_bytes(self):
        key_file = self.fixture_key_file()
        self.candidate()
        before = {path.name: path.read_bytes() for path in self.output.iterdir()}
        # A different valid Ed25519 public identity: RFC 8032 section 7.1 TEST 1.
        self.public_key_file.write_text("d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a\n")
        with self.assertRaisesRegex(ValueError, "provisioned signing key does not match"):
            signer.sign(self.output, self.version, key_file, self.public_key_file)
        self.assertEqual(before, {path.name: path.read_bytes() for path in self.output.iterdir()})

    def test_missing_or_malformed_anchor_fails_before_changing_candidate_bytes(self):
        key_file = self.fixture_key_file()
        self.candidate()
        before = {path.name: path.read_bytes() for path in self.output.iterdir()}
        self.public_key_file.unlink()
        with self.assertRaises(FileNotFoundError):
            signer.sign(self.output, self.version, key_file, self.public_key_file)
        for value in ["", "0" * 63, "A" * 64, "not-a-public-key"]:
            with self.subTest(value=value):
                self.public_key_file.write_text(value)
                with self.assertRaisesRegex(ValueError, "public anchor must be"):
                    signer.sign(self.output, self.version, key_file, self.public_key_file)
        self.assertEqual(before, {path.name: path.read_bytes() for path in self.output.iterdir()})

    def test_verification_uses_independent_anchor_instead_of_candidate_key_record(self):
        key_file = self.fixture_key_file()
        self.candidate()
        signer.sign(self.output, self.version, key_file, self.public_key_file)
        # These signatures and publisher-key.json are mutually consistent, but
        # must not authenticate themselves against a different reviewed anchor.
        self.public_key_file.write_text("d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a\n")
        with self.assertRaisesRegex(ValueError, "publisher key does not match"):
            signer.verify_signed(self.output, self.version, self.public_key_file)

    def test_signature_verifier_rejects_invalid_hex_shapes_before_openssl(self):
        signature = self.root / "bad-signature"
        signature.write_text("0" * 127)
        with self.assertRaisesRegex(ValueError, "64-byte Ed25519 signature"):
            signer.verify_signature(self.binary, signature, "0" * 64)
        with self.assertRaisesRegex(ValueError, "32-byte Ed25519 public key"):
            signer.verify_signature(self.binary, signature, "not-a-key")


if __name__ == "__main__":
    unittest.main()
