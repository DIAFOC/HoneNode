from __future__ import annotations

import hashlib
import io
import json
import os
from pathlib import Path, PurePosixPath
import subprocess
import sys
import tarfile
import tempfile
import unittest
from unittest.mock import patch

RELEASE_DIR = Path(__file__).parents[1] / "release"
sys.path.insert(0, str(RELEASE_DIR))
import build_release

X86_64 = "Advanced Micro Devices X86-64"
ELF_HEADER = "ELF Header:\n  Class: ELF64\n  Machine: {machine}\n"
STATIC_PROGRAM_HEADERS = "Program Headers:\n  Type Offset VirtAddr\n  LOAD 0x0 0x0\n"
NO_DYNAMIC_SECTION = "There is no dynamic section in this file.\n"


class FakeRunner:
    def __init__(self):
        self.calls: list[list[str]] = []
        self.architectures: dict[str, str] = {}
        self.handler = None

    def __call__(self, argv, **kwargs):
        self.calls.append(list(argv))
        assert kwargs["check"] is False
        assert kwargs["text"] is True
        if self.handler is not None:
            return self.handler(list(argv))
        return self.default(list(argv))

    def default(self, argv):
        if argv[0] != "readelf":
            raise AssertionError(f"unexpected host command: {argv}")
        binary_name = Path(argv[-1]).name
        machine = self.architectures.get(binary_name, X86_64)
        output = {
            "-hW": ELF_HEADER.format(machine=machine),
            "-lW": STATIC_PROGRAM_HEADERS,
            "-dW": NO_DYNAMIC_SECTION,
        }[argv[1]]
        return subprocess.CompletedProcess(argv, 0, output, "")


class ReleaseTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.repo = self.root / "repo"
        self.inputs = self.root / "inputs"
        self.inputs.mkdir()
        self.app = self.inputs / "honenode"
        self.broker = self.inputs / "honed"
        self.app.write_bytes(b"app ELF fixture; readelf is mocked")
        self.broker.write_bytes(b"broker ELF fixture; readelf is mocked")
        self.static = self.inputs / "static"
        (self.static / "nested").mkdir(parents=True)
        (self.static / "empty").mkdir()
        (self.static / "hone.css").write_text("body {}\n", encoding="utf-8")
        (self.static / "nested" / "asset.txt").write_bytes(b"nested static content")
        self.output = self.root / "honenode-0.8.0-x86_64.tar.gz"
        self.runner = FakeRunner()

        for destination, relative in build_release.SOURCE_FILES.items():
            path = self.repo / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            if destination.endswith("policy.example.json"):
                content = '{"version": 1, "plans": [], "tenants": []}\n'
            else:
                content = f"fixture source: {destination}\n"
            path.write_text(content, encoding="utf-8")

    def build(self, **kwargs):
        arguments = dict(
            app_binary=self.app,
            broker_binary=self.broker,
            static_dir=self.static,
            output=self.output,
            version="0.8.0",
            source_root=self.repo,
            runner=self.runner,
        )
        arguments.update(kwargs)
        return build_release.build_release(**arguments)

    def read_bundle(self, output: Path | None = None):
        output = output or self.output
        with tarfile.open(output, "r:gz") as archive:
            members = archive.getmembers()
            files = {
                member.name: archive.extractfile(member).read()
                for member in members
                if member.isfile()
            }
        return members, files

    def test_required_entries_app_only_oci_and_no_state_or_secrets(self):
        self.build()
        prefix = "honenode-0.8.0-x86_64"
        members, files = self.read_bundle()
        required = {
            "LICENSE",
            "README.md",
            "SHA256SUMS",
            "manifest.json",
            "oci/honenode-app.oci.tar",
            "native/honenode",
            "native/static/hone.css",
            "native/static/nested/asset.txt",
            "broker/honed",
            "examples/app.env.example",
            "examples/policy.example.json",
            "examples/quadlet/honenode.container",
            "examples/systemd/honed.service",
            "examples/systemd/honenode.sysusers.conf",
            "examples/systemd/honenode.tmpfiles.conf",
        }
        self.assertTrue({f"{prefix}/{name}" for name in required}.issubset(files))
        self.assertEqual(files[f"{prefix}/native/honenode"], self.app.read_bytes())
        self.assertEqual(files[f"{prefix}/broker/honed"], self.broker.read_bytes())
        for destination, relative in build_release.SOURCE_FILES.items():
            self.assertEqual(
                files[f"{prefix}/{destination}"], (self.repo / relative).read_bytes()
            )
        self.assertIn(f"{prefix}/native/static/empty", {member.name for member in members})
        self.assertFalse(any(name.endswith("/policy.json") for name in files))

        for member in members:
            self.assertTrue(member.isfile() or member.isdir())
            self.assertFalse(member.issym() or member.islnk())
            parts = set(PurePosixPath(member.name).parts)
            self.assertTrue(parts.isdisjoint({".local-run", ".native-state", "secrets"}))
            self.assertNotIn("registry.db", parts)
        environment = files[f"{prefix}/examples/app.env.example"]
        self.assertIn(b"HONE_TENANTS=\n", environment)
        self.assertNotIn(b"HONE_OPERATOR_PASSWORD=", environment)
        self.assertNotIn(b"HONE_API_TOKENS=", environment)
        self.assertNotIn(b"change-me", environment.lower())

        with tarfile.open(fileobj=io.BytesIO(files[f"{prefix}/oci/honenode-app.oci.tar"])) as oci:
            oci_files = {
                member.name: oci.extractfile(member).read()
                for member in oci.getmembers()
                if member.isfile()
            }
        index = json.loads(oci_files["index.json"])
        descriptor = index["manifests"][0]
        self.assertEqual(descriptor["platform"]["architecture"], "amd64")
        self.assertEqual(
            descriptor["annotations"]["org.opencontainers.image.ref.name"],
            "localhost/honenode-app:0.8.0",
        )

        def blob(desc):
            data = oci_files["blobs/sha256/" + desc["digest"].split(":", 1)[1]]
            self.assertEqual(hashlib.sha256(data).hexdigest(), desc["digest"].split(":", 1)[1])
            return data

        image_manifest = json.loads(blob(descriptor))
        layer = blob(image_manifest["layers"][0])
        with tarfile.open(fileobj=io.BytesIO(layer)) as layer_archive:
            layer_names = set(layer_archive.getnames())
            self.assertIn("usr/local/bin/honenode", layer_names)
            self.assertIn("app/static/nested/asset.txt", layer_names)
            self.assertFalse(any("honed" in name for name in layer_names))
            self.assertEqual(
                layer_archive.extractfile("usr/local/bin/honenode").read(),
                self.app.read_bytes(),
            )
        self.assertTrue(all(call[0] == "readelf" for call in self.runner.calls))

    def test_manifest_artifact_digests_and_sha256sums_cover_payload(self):
        self.build()
        prefix = "honenode-0.8.0-x86_64/"
        _, files = self.read_bundle()
        manifest = json.loads(files[prefix + "manifest.json"])
        self.assertEqual(manifest["version"], "0.8.0")
        self.assertEqual(manifest["arch"], "x86_64")
        self.assertEqual(manifest["oci_arch"], "amd64")
        self.assertTrue(manifest["experimental"])

        relative_files = {name[len(prefix):]: data for name, data in files.items()}
        self.assertEqual(
            set(manifest["artifacts"]),
            set(relative_files) - {"manifest.json", "SHA256SUMS"},
        )
        for name, record in manifest["artifacts"].items():
            data = relative_files[name]
            self.assertEqual(record["sha256"], hashlib.sha256(data).hexdigest())
            self.assertEqual(record["size"], len(data))

        sums = {}
        for line in relative_files["SHA256SUMS"].decode("utf-8").splitlines():
            digest, name = line.split("  ", 1)
            sums[name] = digest
        self.assertEqual(set(sums), set(relative_files) - {"SHA256SUMS"})
        for name, digest in sums.items():
            self.assertEqual(digest, hashlib.sha256(relative_files[name]).hexdigest())

    def test_deterministic_bytes_sorted_paths_and_normalized_metadata(self):
        self.build()
        original = self.output.read_bytes()
        for index, path in enumerate(
            [self.app, self.broker, self.static / "hone.css", *(
                self.repo / relative for relative in build_release.SOURCE_FILES.values()
            )]
        ):
            os.utime(path, (1000 + index, 2000 + index))
        self.app.chmod(0o600)
        self.broker.chmod(0o700)
        self.build()
        self.assertEqual(self.output.read_bytes(), original)
        self.assertEqual(original[4:8], b"\0\0\0\0")
        self.assertFalse(original[3] & 0x08, "gzip must not embed a staging filename")

        members, _ = self.read_bundle()
        names = [member.name for member in members]
        self.assertEqual(names, sorted(names))
        for member in members:
            self.assertEqual(member.mtime, 0)
            self.assertEqual((member.uid, member.gid), (0, 0))
            self.assertEqual((member.uname, member.gname), ("", ""))
            if member.isdir():
                self.assertEqual(member.mode, 0o755)
            else:
                relative = member.name.split("/", 1)[1]
                expected = 0o555 if relative in build_release.EXECUTABLES else 0o644
                self.assertEqual(member.mode, expected)

    def test_rejects_dynamic_app_and_broker_without_replacing_output(self):
        self.output.write_bytes(b"previous output")
        for binary_name in ("honenode", "honed"):
            with self.subTest(binary=binary_name):
                def dynamic(argv, selected=binary_name):
                    if Path(argv[-1]).name == selected and argv[1] == "-lW":
                        return subprocess.CompletedProcess(argv, 0, "  INTERP\n", "")
                    return self.runner.default(argv)

                self.runner.handler = dynamic
                with self.assertRaisesRegex(build_release.BuildError, "dynamic program interpreter"):
                    self.build()
                self.assertEqual(self.output.read_bytes(), b"previous output")
        self.runner.handler = None

    def test_rejects_mismatched_and_non_x86_architectures(self):
        self.output.write_bytes(b"previous output")
        self.runner.architectures["honed"] = "AArch64"
        with self.assertRaisesRegex(build_release.BuildError, "architectures differ"):
            self.build()
        self.assertEqual(self.output.read_bytes(), b"previous output")

        self.runner.architectures = {"honenode": "AArch64", "honed": "AArch64"}
        with self.assertRaisesRegex(build_release.BuildError, "requires two amd64 ELFs"):
            self.build()
        self.assertEqual(self.output.read_bytes(), b"previous output")

    def test_rejects_symlinked_binaries_static_root_and_static_entry(self):
        self.output.write_bytes(b"previous output")
        for path in (self.app, self.broker):
            with self.subTest(binary=path.name):
                original = path.read_bytes()
                path.unlink()
                path.symlink_to(self.root / "elsewhere")
                try:
                    with self.assertRaisesRegex(build_release.BuildError, "non-symlink"):
                        self.build()
                finally:
                    path.unlink()
                    path.write_bytes(original)
                self.assertEqual(self.output.read_bytes(), b"previous output")

        link = self.static / "nested" / "asset-link"
        link.symlink_to(self.static / "hone.css")
        with self.assertRaisesRegex(build_release.BuildError, "symlink or special"):
            self.build()
        link.unlink()
        alias = self.inputs / "static-alias"
        alias.symlink_to(self.static, target_is_directory=True)
        with self.assertRaisesRegex(build_release.BuildError, "non-symlink directory"):
            self.build(static_dir=alias)
        self.assertEqual(self.output.read_bytes(), b"previous output")

    def test_rejects_invalid_versions_missing_sources_and_output_collisions(self):
        for version in ("", "0.8", "../0.8.0", "0.8.0+build"):
            with self.subTest(version=version), self.assertRaises(build_release.BuildError):
                self.build(version=version)
        self.assertFalse(self.output.exists())

        policy = self.repo / build_release.SOURCE_FILES["examples/policy.example.json"]
        policy.unlink()
        with self.assertRaisesRegex(build_release.BuildError, "bundle source"):
            self.build()
        self.assertFalse(self.output.exists())
        policy.write_text('{"version": 1, "plans": [], "tenants": []}\n', encoding="utf-8")

        with self.assertRaisesRegex(build_release.BuildError, "overwrite an input"):
            self.build(output=self.app)
        with self.assertRaisesRegex(build_release.BuildError, "outside the static"):
            self.build(output=self.static / "bundle.tar.gz")
        target = self.root / "target.tar.gz"
        target.write_bytes(b"target")
        alias = self.root / "output-alias.tar.gz"
        alias.symlink_to(target)
        with self.assertRaisesRegex(build_release.BuildError, "output must not be a symlink"):
            self.build(output=alias)
        self.assertEqual(target.read_bytes(), b"target")

    def test_failed_final_replace_preserves_output_and_removes_staging(self):
        self.output.write_bytes(b"previous output")
        real_replace = os.replace

        def fail_final_replace(source, destination):
            if Path(destination) == self.output:
                raise OSError("injected final replace failure")
            return real_replace(source, destination)

        with patch.object(build_release.os, "replace", side_effect=fail_final_replace):
            with self.assertRaisesRegex(OSError, "injected final replace"):
                self.build()
        self.assertEqual(self.output.read_bytes(), b"previous output")
        self.assertEqual(list(self.root.glob(".honenode-release-*")), [])

    def test_cli_uses_requested_paths_and_does_not_invoke_container_tools(self):
        with patch.object(build_release, "build_release") as build:
            result = build_release.main([
                "--app-binary", str(self.app),
                "--broker-binary", str(self.broker),
                "--static-dir", str(self.static),
                "--output", str(self.output),
                "--version", "0.8.0",
            ])
        self.assertEqual(result, 0)
        self.assertEqual(build.call_args.kwargs, {
            "app_binary": self.app,
            "broker_binary": self.broker,
            "static_dir": self.static,
            "output": self.output,
            "version": "0.8.0",
        })

    def test_readme_states_rocky_security_and_provisioning_boundaries(self):
        text = (RELEASE_DIR / "README.md").read_text(encoding="utf-8")
        for required in (
            "not a one-command install",
            "No Docker daemon",
            "The OCI app alone cannot provision",
            "host broker needs root/systemd",
            "rootless tenant",
            "socket-group",
            "not SELinux-ready",
            "log shipper",
            "not an app",
            "inline at the edge",
            "not active policy",
        ):
            self.assertIn(required, text)


if __name__ == "__main__":
    unittest.main()
