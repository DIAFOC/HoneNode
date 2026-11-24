from __future__ import annotations

import json
from pathlib import Path
import sys
import tarfile
import tempfile
import unittest

OS_DIR = Path(__file__).parents[1] / "os"
sys.path.insert(0, str(OS_DIR))
import build_os_bundle  # noqa: E402

ROOT = Path(__file__).parents[2]
APP = ROOT / "backend" / "target" / "release" / "honenode"
BROKER = ROOT / "backend" / "target" / "release" / "honed"
STATIC = ROOT / "backend" / "static"


class OsBundleTests(unittest.TestCase):
    def binaries(self):
        if not (APP.is_file() and BROKER.is_file()):
            self.skipTest("release binaries are not built")
        return APP, BROKER

    def test_fedora_bundle_is_tagged_and_contains_both_binaries(self):
        app, broker = self.binaries()
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary) / "honenode-0.8.0-fedora-x86_64.tar.gz"
            build_os_bundle.build_bundle(
                os_name="fedora",
                version="0.8.0",
                app_binary=app,
                broker_binary=broker,
                static_dir=STATIC,
                output=output,
            )
            with tarfile.open(output) as archive:
                names = archive.getnames()
                manifest = json.load(
                    archive.extractfile(
                        "honenode-0.8.0-fedora-x86_64/manifest.json"
                    )
                )
        self.assertEqual(manifest["os"], "fedora")
        self.assertEqual(manifest["arch"], "x86_64")
        self.assertEqual(manifest["binaries"]["app"]["libc"], "glibc")
        self.assertIn("honenode-0.8.0-fedora-x86_64/native/honenode", names)
        self.assertIn("honenode-0.8.0-fedora-x86_64/broker/honed", names)
        self.assertIn("honenode-0.8.0-fedora-x86_64/install.py", names)
        self.assertIn("honenode-0.8.0-fedora-x86_64/install.sh", names)
        self.assertIn("honenode-0.8.0-fedora-x86_64/examples/policy.selfservice.json", names)
        self.assertIn("honenode-0.8.0-fedora-x86_64/SHA256SUMS", names)

    def test_rocky_bundle_uses_the_same_layout_but_a_different_tag(self):
        app, broker = self.binaries()
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary) / "honenode-0.8.0-rocky-x86_64.tar.gz"
            build_os_bundle.build_bundle(
                os_name="rocky",
                version="0.8.0",
                app_binary=app,
                broker_binary=broker,
                static_dir=STATIC,
                output=output,
            )
            with tarfile.open(output) as archive:
                manifest = json.load(
                    archive.extractfile(
                        "honenode-0.8.0-rocky-x86_64/manifest.json"
                    )
                )
        self.assertEqual(manifest["os"], "rocky")

    def test_unknown_os_and_bad_suffix_are_rejected(self):
        app, broker = self.binaries()
        with tempfile.TemporaryDirectory() as temporary:
            with self.assertRaises(build_os_bundle.BundleError):
                build_os_bundle.build_bundle(
                    os_name="debian",
                    version="0.8.0",
                    app_binary=app,
                    broker_binary=broker,
                    static_dir=STATIC,
                    output=Path(temporary) / "x.tar.gz",
                )
            with self.assertRaises(build_os_bundle.BundleError):
                build_os_bundle.build_bundle(
                    os_name="fedora",
                    version="0.8.0",
                    app_binary=app,
                    broker_binary=broker,
                    static_dir=STATIC,
                    output=Path(temporary) / "x.tar",
                )


if __name__ == "__main__":
    unittest.main()
