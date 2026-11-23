from __future__ import annotations

import hashlib

import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tarfile
import tempfile
import unittest
from unittest.mock import patch

OCI_DIR = Path(__file__).parents[1] / "oci"
sys.path.insert(0, str(OCI_DIR))
import build_archive
from test_build_image import FakeRunner


class ArchiveTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.binary = self.root / "honenode"
        self.binary.write_bytes(b"ELF fixture; readelf is mocked")
        self.static = self.root / "static"
        self.static.mkdir()
        (self.static / "hone.css").write_text("body {}\n")
        (self.static / "nested").mkdir()
        (self.static / "nested" / "asset.txt").write_bytes(b"nested content")
        self.output = self.root / "app.oci.tar"
        self.runner = FakeRunner()

    def build(self, **kwargs):
        return build_archive.build_archive(binary=self.binary, static_dir=self.static,
                                           output=self.output, runner=self.runner, **kwargs)

    def contents(self):
        with tarfile.open(self.output) as archive:
            return {entry.name: archive.extractfile(entry).read() for entry in archive.getmembers()}

    def test_oci_descriptors_digests_layer_metadata_and_runtime_config(self):
        self.build()
        files = self.contents()
        self.assertEqual(json.loads(files["oci-layout"]), {"imageLayoutVersion": "1.0.0"})
        index = json.loads(files["index.json"])
        manifest_desc = index["manifests"][0]
        self.assertEqual(manifest_desc["annotations"]["org.opencontainers.image.ref.name"],
                         "localhost/honenode-app:poc")

        def blob(desc):
            data = files["blobs/sha256/" + desc["digest"].split(":")[1]]
            self.assertEqual(len(data), desc["size"])
            self.assertEqual("sha256:" + hashlib.sha256(data).hexdigest(), desc["digest"])
            return data

        manifest = json.loads(blob(manifest_desc))
        self.assertEqual(manifest["mediaType"], build_archive.OCI + "manifest.v1+json")
        config = json.loads(blob(manifest["config"]))
        layer_desc = manifest["layers"][0]
        layer = blob(layer_desc)
        self.assertEqual(config["architecture"], "amd64")
        self.assertEqual(config["os"], "linux")
        self.assertEqual(config["rootfs"]["diff_ids"], [layer_desc["digest"]])
        runtime = config["config"]
        self.assertEqual(runtime["User"], "65532:65532")
        self.assertEqual(runtime["Entrypoint"], ["/usr/local/bin/honenode"])
        self.assertEqual(runtime["WorkingDir"], "/app")
        self.assertEqual(runtime["Volumes"], {"/var/lib/honenode": {}})
        self.assertEqual(runtime["ExposedPorts"], {"8080/tcp": {}})
        self.assertIn("HONE_TENANTS=", runtime["Env"])
        self.assertIn("HONE_THEME=sharpnode", runtime["Env"])
        self.assertNotIn("HONE_THEME=neutral", runtime["Env"])
        self.assertIn("HONE_BROKER_SOCKET=/run/honenode/broker.sock", runtime["Env"])
        self.assertFalse(any("PASSWORD" in item or "OPERATOR_ACCOUNT" in item for item in runtime["Env"]))
        with tarfile.open(fileobj=io.BytesIO(layer)) as archive:
            self.assertTrue(all(member.isfile() or member.isdir() for member in archive))
            self.assertTrue(all(not member.name.startswith("/") and ".." not in Path(member.name).parts
                                for member in archive))
            self.assertEqual(archive.extractfile("app/static/nested/asset.txt").read(), b"nested content")
            self.assertEqual(archive.extractfile("usr/local/bin/honenode").read(), self.binary.read_bytes())
            for name in ("var/lib/honenode", "var/lib/honenode/registry", "run/honenode"):
                member = archive.getmember(name)
                self.assertEqual((member.uid, member.gid, member.mode), (65532, 65532, 0o700))
            self.assertEqual(archive.getmember("tmp").mode, 0o1777)
            self.assertEqual(archive.getmember("app/static/hone.css").mode, 0o444)
            self.assertTrue(all(member.mtime == 0 for member in archive))
        self.assertTrue(all(call[0] == "readelf" for call in self.runner.calls))

    def test_reproducible_and_aarch64_from_artifact_not_host(self):
        def handler(argv):
            result = FakeRunner()(argv, check=False, text=True)
            result.stdout = result.stdout.replace("Advanced Micro Devices X86-64", "AArch64")
            return result
        self.runner = FakeRunner(handler)
        self.build()
        original = self.output.read_bytes()
        os.utime(self.binary, (1000, 1000))
        self.build()
        self.assertEqual(self.output.read_bytes(), original)
        files = self.contents()
        index = json.loads(files["index.json"])
        self.assertEqual(index["manifests"][0]["platform"]["architecture"], "arm64")

    def test_rejects_symlinks_files_directories_and_root_without_replacing_output(self):
        self.output.write_bytes(b"previous output")
        for target in (self.binary, self.static / "nested", Path("missing")):
            with self.subTest(target=target):
                link = self.static / "link"
                link.symlink_to(target)
                try:
                    with self.assertRaisesRegex(build_archive.BuildError, "symlink or special"):
                        self.build()
                finally:
                    link.unlink()
                self.assertEqual(self.output.read_bytes(), b"previous output")
        alias = self.root / "static-alias"
        alias.symlink_to(self.static, target_is_directory=True)
        with self.assertRaises(OSError):
            build_archive.build_archive(binary=self.binary, static_dir=alias, output=self.output,
                                         runner=self.runner)

    def test_rejects_fifo_without_opening_or_hanging(self):
        os.mkfifo(self.static / "fifo")
        with self.assertRaisesRegex(build_archive.BuildError, "special file"):
            self.build()
        self.assertFalse(self.output.exists())

    def test_rejects_dynamic_binary_before_writing(self):
        def handler(argv):
            if argv[1] == "-lW":
                return subprocess.CompletedProcess(argv, 0, "  INTERP\n", "")
            return FakeRunner()(argv, check=False, text=True)
        self.runner = FakeRunner(handler)
        with self.assertRaisesRegex(build_archive.BuildError, "dynamic program interpreter"):
            self.build()
        self.assertFalse(self.output.exists())

    def test_rejects_shared_libraries_before_writing(self):
        def handler(argv):
            if argv[1] == "-dW":
                return subprocess.CompletedProcess(argv, 0, " (NEEDED) libc.so.6\n", "")
            return FakeRunner()(argv, check=False, text=True)
        self.runner = FakeRunner(handler)
        with self.assertRaisesRegex(build_archive.BuildError, "DT_NEEDED"):
            self.build()
        self.assertFalse(self.output.exists())

    def test_unknown_architecture_is_not_guessed(self):
        def handler(argv):
            result = FakeRunner()(argv, check=False, text=True)
            result.stdout = result.stdout.replace("Advanced Micro Devices X86-64", "Unknown")
            return result
        self.runner = FakeRunner(handler)
        with self.assertRaisesRegex(build_archive.BuildError, "architecture"):
            self.build()

    def test_output_cannot_overwrite_input_or_be_inside_static_tree(self):
        for output in (self.binary, self.static / "output.tar"):
            with self.subTest(output=output), self.assertRaises(build_archive.BuildError):
                build_archive.build_archive(binary=self.binary, static_dir=self.static,
                                             output=output, runner=self.runner)

    def test_failed_final_write_removes_staging_and_preserves_old_output(self):
        self.output.write_bytes(b"previous")
        with patch.object(build_archive.os, "replace", side_effect=OSError("injected write failure")):
            with self.assertRaisesRegex(OSError, "injected"):
                self.build()
        self.assertEqual(self.output.read_bytes(), b"previous")
        self.assertEqual(list(self.root.glob(".honenode-*")), [])

    def test_cli_does_not_check_for_buildah_docker_or_podman(self):
        with patch.object(build_archive, "build_archive") as build:
            self.assertEqual(build_archive.main(["--binary", str(self.binary), "--output", str(self.output)]), 0)
        self.assertEqual(build.call_args.kwargs["static_dir"], Path("backend/static"))


if __name__ == "__main__":
    unittest.main()
