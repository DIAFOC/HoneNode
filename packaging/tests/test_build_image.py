from __future__ import annotations

import importlib.util
from pathlib import Path
import subprocess
import tempfile
import unittest


MODULE_PATH = Path(__file__).parents[1] / "oci" / "build_image.py"
SPEC = importlib.util.spec_from_file_location("build_image", MODULE_PATH)
assert SPEC is not None and SPEC.loader is not None
build_image = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(build_image)


ELF_HEADER = "ELF Header:\n  Class: ELF64\n  Machine: Advanced Micro Devices X86-64\n"
STATIC_PROGRAM_HEADERS = "Program Headers:\n  Type Offset VirtAddr\n  LOAD 0x0 0x0\n"
NO_DYNAMIC_SECTION = "There is no dynamic section in this file.\n"


class FakeRunner:
    def __init__(self, handler=None):
        self.calls: list[list[str]] = []
        self.handler = handler

    def __call__(self, argv, **kwargs):
        self.calls.append(list(argv))
        assert kwargs["check"] is False
        assert kwargs["text"] is True
        if self.handler is not None:
            return self.handler(list(argv))
        if argv[0] == "readelf":
            output = {
                "-hW": ELF_HEADER,
                "-lW": STATIC_PROGRAM_HEADERS,
                "-dW": NO_DYNAMIC_SECTION,
            }[argv[1]]
            return subprocess.CompletedProcess(argv, 0, output, "")
        if argv[:2] == ["buildah", "from"]:
            return subprocess.CompletedProcess(argv, 0, f"{argv[3]}\n", "")
        return subprocess.CompletedProcess(argv, 0, "", "")


class StaticElfTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.binary = Path(self.temporary.name) / "honenode"
        self.binary.write_bytes(b"test fixture, not a real ELF")

    def tearDown(self):
        self.temporary.cleanup()

    def test_accepts_elf_without_interpreter_or_needed_entries(self):
        runner = FakeRunner()
        build_image.verify_static_elf(self.binary, runner=runner)
        self.assertEqual([call[1] for call in runner.calls], ["-hW", "-lW", "-dW"])

    def test_rejects_program_interpreter(self):
        def handler(argv):
            output = ELF_HEADER if argv[1] == "-hW" else "  INTERP 0x0 0x0 [Requesting program interpreter: /lib64/ld-linux-x86-64.so.2]\n"
            return subprocess.CompletedProcess(argv, 0, output, "")

        with self.assertRaisesRegex(build_image.BuildError, "dynamic program interpreter"):
            build_image.verify_static_elf(self.binary, runner=FakeRunner(handler))

    def test_rejects_needed_shared_library(self):
        def handler(argv):
            outputs = {
                "-hW": ELF_HEADER,
                "-lW": STATIC_PROGRAM_HEADERS,
                "-dW": " 0x0000000000000001 (NEEDED) Shared library: [libc.so.6]\n",
            }
            return subprocess.CompletedProcess(argv, 0, outputs[argv[1]], "")

        with self.assertRaisesRegex(build_image.BuildError, "DT_NEEDED"):
            build_image.verify_static_elf(self.binary, runner=FakeRunner(handler))

    def test_rejects_symlink_input(self):
        target = Path(self.temporary.name) / "target"
        target.write_bytes(b"fixture")
        self.binary.unlink()
        self.binary.symlink_to(target)
        with self.assertRaisesRegex(build_image.BuildError, "non-symlink"):
            build_image.verify_static_elf(self.binary, runner=FakeRunner())


class BuildahWorkflowTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        root = Path(self.temporary.name)
        self.binary = root / "honenode"
        self.binary.write_bytes(b"test fixture, not a real ELF")
        self.css = root / "hone.css"
        self.css.write_text("body { color: black; }\n", encoding="utf-8")

    def tearDown(self):
        self.temporary.cleanup()

    def test_builds_from_scratch_as_nonroot_and_cleans_up(self):
        runner = FakeRunner()
        result = build_image.build_image(
            binary=self.binary,
            css=self.css,
            image="localhost/honenode-app:test",
            runner=runner,
        )
        self.assertEqual(result, "localhost/honenode-app:test")
        from_call = next(call for call in runner.calls if call[:2] == ["buildah", "from"])
        self.assertEqual(from_call[-1], "scratch")
        config_call = next(call for call in runner.calls if call[:2] == ["buildah", "config"])
        self.assertIn("65532:65532", config_call)
        self.assertIn("HONE_TENANTS=", config_call)
        self.assertIn("HONE_STATIC_DIR=/app/static", config_call)
        commit_call = next(call for call in runner.calls if call[:2] == ["buildah", "commit"])
        self.assertEqual(commit_call[2:4], ["--format", "oci"])
        self.assertEqual(runner.calls[-1][:2], ["buildah", "rm"])
        self.assertTrue(all("shell" not in call for call in runner.calls))

    def test_failed_copy_still_removes_working_container_and_does_not_commit(self):
        def handler(argv):
            if argv[0] == "readelf":
                return FakeRunner()(argv, check=False, text=True)
            if argv[:2] == ["buildah", "from"]:
                return subprocess.CompletedProcess(argv, 0, "container\n", "")
            if argv[:2] == ["buildah", "copy"]:
                return subprocess.CompletedProcess(argv, 1, "", "injected copy failure")
            return subprocess.CompletedProcess(argv, 0, "", "")

        runner = FakeRunner(handler)
        with self.assertRaisesRegex(build_image.BuildError, "injected copy failure"):
            build_image.build_image(
                binary=self.binary,
                css=self.css,
                image="localhost/honenode-app:test",
                runner=runner,
            )
        self.assertEqual(runner.calls[-1][:2], ["buildah", "rm"])
        self.assertFalse(any(call[:2] == ["buildah", "commit"] for call in runner.calls))

    def test_cleanup_failure_does_not_mask_the_build_failure(self):
        def handler(argv):
            if argv[0] == "readelf":
                return FakeRunner()(argv, check=False, text=True)
            if argv[:2] == ["buildah", "from"]:
                return subprocess.CompletedProcess(argv, 0, "container\n", "")
            if argv[:2] == ["buildah", "copy"]:
                return subprocess.CompletedProcess(argv, 1, "", "original copy failure")
            if argv[:2] == ["buildah", "rm"]:
                return subprocess.CompletedProcess(argv, 1, "", "secondary cleanup failure")
            return subprocess.CompletedProcess(argv, 0, "", "")

        with self.assertRaisesRegex(build_image.BuildError, "original copy failure"):
            build_image.build_image(
                binary=self.binary,
                css=self.css,
                image="localhost/honenode-app:test",
                runner=FakeRunner(handler),
            )

    def test_cleanup_failure_after_success_is_fatal(self):
        def handler(argv):
            if argv[0] == "readelf":
                return FakeRunner()(argv, check=False, text=True)
            if argv[:2] == ["buildah", "from"]:
                return subprocess.CompletedProcess(argv, 0, "container\n", "")
            if argv[:2] == ["buildah", "rm"]:
                return subprocess.CompletedProcess(argv, 1, "", "injected cleanup failure")
            return subprocess.CompletedProcess(argv, 0, "", "")

        with self.assertRaisesRegex(build_image.BuildError, "injected cleanup failure"):
            build_image.build_image(
                binary=self.binary,
                css=self.css,
                image="localhost/honenode-app:test",
                runner=FakeRunner(handler),
            )

    def test_empty_css_is_rejected_before_buildah_starts(self):
        self.css.write_text("", encoding="utf-8")
        runner = FakeRunner()
        with self.assertRaisesRegex(build_image.BuildError, "CSS is empty"):
            build_image.build_image(
                binary=self.binary,
                css=self.css,
                image="localhost/honenode-app:test",
                runner=runner,
            )
        self.assertFalse(any(call[0] == "buildah" for call in runner.calls))


if __name__ == "__main__":
    unittest.main()
