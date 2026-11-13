"""Offline publisher tests: mixed builds or corrupt assets must never be ready."""
import importlib.util
import io
import json
from pathlib import Path
import tarfile
import tempfile
import unittest
import zipfile
import sys

sys.dont_write_bytecode = True

spec = importlib.util.spec_from_file_location("prebuilt", Path(__file__).with_name("prebuilt-release.py"))
prebuilt = importlib.util.module_from_spec(spec)
spec.loader.exec_module(prebuilt)


class ReleaseManifestTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        for target in prebuilt.TARGETS:
            windows = "windows" in target
            filename = "rpi.exe" if windows else "rpi"
            asset = self.root / f"rpi-v1.2.3-{target}.{'zip' if windows else 'tar.gz'}"
            binary = b"test executable " + target.encode()
            if windows:
                with zipfile.ZipFile(asset, "w") as package:
                    package.writestr(filename, binary)
            else:
                with tarfile.open(asset, "w:gz") as package:
                    entry = tarfile.TarInfo("./" + filename)
                    entry.size = len(binary)
                    entry.mode = 0o755
                    package.addfile(entry, io.BytesIO(binary))
            row = prebuilt.describe(asset, target, "1.2.3", "a" * 40)
            asset.with_name(asset.name + ".prebuilt.json").write_text(json.dumps(row))
            asset.with_name(asset.name + ".sha256").write_text(f"{row['distribution']['sha256']}  {asset.name}\n")

    def test_six_targets_have_archive_and_executable_checksums(self):
        manifest = prebuilt.assemble(self.root, "v1.2.3", "a" * 40)
        self.assertEqual(set(manifest["targets"]), prebuilt.TARGETS)
        self.assertEqual(manifest["name"], "rpi-cli")
        self.assertEqual(manifest["tag"], "v1.2.3")
        for target, row in manifest["targets"].items():
            self.assertEqual(row["binary"]["path"], "rpi.exe" if "windows" in target else "rpi")
            self.assertEqual(len(row["binary"]["sha256"]), 64)

    def test_missing_platform_is_not_a_ready_release(self):
        next(self.root.glob("*.prebuilt.json")).unlink()
        with self.assertRaisesRegex(ValueError, "six target"):
            prebuilt.assemble(self.root, "v1.2.3", "a" * 40)

    def test_mixed_source_commits_are_rejected(self):
        path = next(self.root.glob("*.prebuilt.json"))
        row = json.loads(path.read_text())
        row["source_commit"] = "b" * 40
        path.write_text(json.dumps(row))
        with self.assertRaisesRegex(ValueError, "source commit"):
            prebuilt.assemble(self.root, "v1.2.3", "a" * 40)

    def test_corrupt_archive_and_legacy_checksum_are_rejected(self):
        path = next(self.root.glob("*.sha256"))
        path.write_text("0" * 64 + "  " + path.name.removesuffix(".sha256"))
        with self.assertRaisesRegex(ValueError, "checksum"):
            prebuilt.assemble(self.root, "v1.2.3", "a" * 40)
        next(self.root.glob("*.zip")).write_bytes(b"corrupt")
        with self.assertRaises((ValueError, zipfile.BadZipFile)):
            prebuilt.assemble(self.root, "v1.2.3", "a" * 40)


if __name__ == "__main__":
    unittest.main()
