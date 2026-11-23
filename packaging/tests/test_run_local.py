from __future__ import annotations

import contextlib
import io
import os
from pathlib import Path
import socket
import stat
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).parents[1] / "oci"))
import run_local


class PodmanRunner:
    def __init__(self, *, rootless="true", remote="false", fail=None):
        self.calls = []
        self.rootless, self.remote, self.fail = rootless, remote, fail

    def __call__(self, argv, **kwargs):
        self.calls.append(list(argv))
        assert kwargs["check"] is False
        assert "shell" not in kwargs
        if self.fail is not None and argv[1] == self.fail:
            return subprocess.CompletedProcess(argv, 125, "", "injected podman failure")
        output = "container-id\n"
        if argv[1] == "info":
            output = self.rootless if "Rootless" in argv[-1] else self.remote
        return subprocess.CompletedProcess(argv, 0, output, "")


class LocalRunTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.run_dir = self.root / ".local-run"
        self.runner = PodmanRunner()
        # Tests are safe even in root-owned CI; host processes are always mocked.
        uid = os.geteuid()
        self.euid = patch.object(run_local.os, "geteuid", return_value=uid or 1000)
        self.euid.start()
        self.addCleanup(self.euid.stop)
        # In root-owned CI, simulate an ordinary launcher user but validate real
        # fixture ownership inside state preparation. Never chown host files.
        if uid == 0:
            prepare = run_local.prepare_state

            def prepare_as_fixture_owner(*args):
                with patch.object(run_local.os, "geteuid", return_value=uid):
                    return prepare(*args)

            state = patch.object(run_local, "prepare_state", side_effect=prepare_as_fixture_owner)
            state.start()
            self.addCleanup(state.stop)

    def launch(self, **kwargs):
        return run_local.launch(run_dir=self.run_dir, runner=self.runner, **kwargs)

    def saved(self):
        return dict(line.split("=", 1) for line in (self.run_dir / "bootstrap.env").read_text().splitlines())

    def provided(self, content):
        path = self.root / "provided.env"
        path.write_text(content)
        path.chmod(0o600)
        return path

    def test_app_only_detached_command_is_hardened_and_has_no_broker_mount(self):
        self.assertEqual(self.launch(), "container-id")
        command = self.runner.calls[-1]
        self.assertEqual(command[:3], ["podman", "run", "-d"])
        for flag in ("--read-only", "--read-only-tmpfs=false"):
            self.assertIn(flag, command)
        for flag, value in (("--pull", "never"), ("--cap-drop", "ALL"),
                            ("--security-opt", "no-new-privileges"),
                            ("--userns", "keep-id:uid=65532,gid=65532"), ("--user", "65532:65532"),
                            ("--pids-limit", "256"), ("--memory", "512m"),
                            ("--memory-swap", "512m"), ("--cpus", "1"),
                            ("--publish", "127.0.0.1:8088:8080")):
            self.assertEqual(command[command.index(flag) + 1], value)
        volumes = [command[index + 1] for index, value in enumerate(command) if value == "--volume"]
        self.assertEqual(volumes, [f"{self.run_dir}/data:/var/lib/honenode:rw,Z"])
        tmpfs = [command[index + 1] for index, value in enumerate(command) if value == "--tmpfs"]
        self.assertIn("/run/honenode:rw,nosuid,nodev,noexec,size=1m,mode=0755", tmpfs)
        for mount in tmpfs:
            self.assertFalse(any(option.startswith(("uid=", "gid=")) for option in mount.split(",")))
        self.assertIn("HONE_BROKER_SOCKET=/run/honenode/broker.sock", command)
        self.assertIn("HONE_TENANTS=t91,t92", command)
        self.assertIn("HONE_THEME=sharpnode", command)
        self.assertNotIn("HONE_THEME=neutral", command)
        self.assertIn("HONE_SECURE_COOKIES=false", command)
        self.assertIn("HONE_API_TOKENS=", command)
        self.assertFalse(any("label=disable" in value or "podman.sock" in value for value in command))
        self.assertNotIn("--privileged", command)
        self.assertNotIn("--replace", command)
        self.assertNotIn("--group-add", command)
        self.assertFalse(any(value.startswith("/etc/") for value in command))
        self.assertTrue(all(call[0] == "podman" for call in self.runner.calls))

    def test_generated_credentials_private_valid_and_reused_without_command_line_secrets(self):
        self.launch()
        values = self.saved()
        self.assertTrue(run_local.valid_account(values["HONE_OPERATOR_ACCOUNT"]))
        self.assertGreaterEqual(len(values["HONE_OPERATOR_PASSWORD"]), 32)
        self.assertEqual(stat.S_IMODE((self.run_dir / "bootstrap.env").stat().st_mode), 0o600)
        for path in (self.run_dir, self.run_dir / "data", self.run_dir / "data/registry", self.run_dir / "data/tenants"):
            self.assertEqual(stat.S_IMODE(path.stat().st_mode), 0o700)
        original = (self.run_dir / "bootstrap.env").read_bytes()
        self.launch()
        self.assertEqual((self.run_dir / "bootstrap.env").read_bytes(), original)
        self.assertNotIn(values["HONE_OPERATOR_PASSWORD"], " ".join(self.runner.calls[-1]))
        self.assertNotIn(values["HONE_OPERATOR_ACCOUNT"], " ".join(self.runner.calls[-1]))

    def test_luhn_matches_known_vectors_and_rejects_invalid_formats(self):
        # 11 payload digits + check digit, doubling from the rightmost payload digit.
        self.assertTrue(run_local.valid_account("123456789015"))
        self.assertEqual(run_local.check_digit("7992739871"), "3")
        for value in ("123456789014", "123", "１２３４５６７８９０１５", "12345678901x"):
            self.assertFalse(run_local.valid_account(value))
        with patch.object(run_local.secrets, "randbelow", side_effect=list(range(10)) + [0]):
            account = run_local.new_account()
        self.assertEqual(account[:-1], "01234567890")
        self.assertTrue(run_local.valid_account(account))

    def test_provided_bootstrap_is_validated_and_never_rotated_on_restart(self):
        provided = self.provided("HONE_OPERATOR_ACCOUNT=123456789015\nHONE_OPERATOR_PASSWORD=long-private-password\n")
        self.launch(bootstrap_env=provided)
        self.assertEqual(self.saved()["HONE_OPERATOR_PASSWORD"], "long-private-password")
        self.launch(bootstrap_env=provided)
        provided.write_text("HONE_OPERATOR_PASSWORD=different-private-password\n")
        before = len(self.runner.calls)
        with self.assertRaisesRegex(run_local.BuildError, "differs"):
            self.launch(bootstrap_env=provided)
        self.assertFalse(any(call[1] == "run" for call in self.runner.calls[before:]))

    def test_partial_bootstrap_generates_only_missing_value(self):
        provided = self.provided("HONE_OPERATOR_PASSWORD=long-private-password\n")
        self.launch(bootstrap_env=provided)
        self.assertEqual(self.saved()["HONE_OPERATOR_PASSWORD"], "long-private-password")
        self.assertTrue(run_local.valid_account(self.saved()["HONE_OPERATOR_ACCOUNT"]))

    def test_invalid_bootstrap_rejected_before_run_without_leaking_password(self):
        for text in ("HONE_OPERATOR_ACCOUNT=123456789014\n", "HONE_OPERATOR_PASSWORD=short\n",
                     "OTHER=unknown\n", "HONE_OPERATOR_PASSWORD=long-private-password\nHONE_OPERATOR_PASSWORD=duplicate-secret\n",
                     "HONE_OPERATOR_PASSWORD=contains\rcarriage-return\n"):
            with self.subTest(text=text):
                self.runner.calls.clear()
                with self.assertRaises(run_local.BuildError) as raised:
                    self.launch(bootstrap_env=self.provided(text))
                self.assertNotIn("long-private-password", str(raised.exception))
                self.assertFalse(any(call[1] == "run" for call in self.runner.calls))

    def test_insecure_or_symlink_bootstrap_and_state_are_rejected(self):
        provided = self.provided("HONE_OPERATOR_PASSWORD=long-private-password\n")
        provided.chmod(0o644)
        with self.assertRaisesRegex(run_local.BuildError, "0600"):
            self.launch(bootstrap_env=provided)
        provided.chmod(0o600)
        alias = self.root / "alias.env"
        alias.symlink_to(provided)
        with self.assertRaisesRegex(run_local.BuildError, "symlink"):
            self.launch(bootstrap_env=alias)
        # A symlink anywhere in the state path is not followed or relabelled.
        alias_dir = self.root / "alias"
        alias_dir.symlink_to(self.run_dir)
        with self.assertRaisesRegex(run_local.BuildError, "symlink"):
            run_local.launch(run_dir=alias_dir, runner=self.runner)
        self.run_dir.chmod(0o755)
        with self.assertRaisesRegex(run_local.BuildError, "0700"):
            self.launch()

    def test_registry_without_bootstrap_does_not_generate_misleading_credentials(self):
        self.launch()
        (self.run_dir / "bootstrap.env").unlink()
        (self.run_dir / "data/registry/registry.db").write_bytes(b"existing database")
        with self.assertRaisesRegex(run_local.BuildError, "registry already exists"):
            self.launch()
        self.assertFalse((self.run_dir / "bootstrap.env").exists())

    def test_explicit_broker_is_readonly_not_relabelled_or_assumed_live(self):
        broker = self.root / "broker"
        broker.mkdir()
        # A local test-only socket fixture; no broker or service is started.
        with socket.socket(socket.AF_UNIX) as fixture:
            fixture.bind(str(broker / "broker.sock"))
            self.launch(broker_dir=broker)
        command = self.runner.calls[-1]
        self.assertIn(f"{broker}:/run/honenode:ro", command)
        self.assertIn("keep-groups", command)
        self.assertFalse(any(value.startswith("/run/honenode:rw") for value in command))
        self.assertNotIn(f"{broker}:/run/honenode:ro,Z", command)
        self.assertNotIn("label=disable", command)

    def test_missing_broker_socket_is_fatal_only_when_explicit(self):
        broker = self.root / "broker"
        broker.mkdir()
        with self.assertRaises(OSError):
            self.launch(broker_dir=broker)
        self.assertFalse(self.run_dir.exists())
        self.launch()

    def test_load_is_optional_and_precedes_detached_run(self):
        archive = self.root / "app.oci.tar"
        archive.write_bytes(b"archive fixture; podman is mocked")
        self.launch(archive=archive)
        self.assertEqual([call[1] for call in self.runner.calls], ["info", "info", "load", "run"])
        self.assertEqual(self.runner.calls[2], ["podman", "load", "--input", str(archive)])

    def test_podman_failure_does_not_claim_success_and_preserves_credentials(self):
        self.runner.fail = "run"
        with self.assertRaisesRegex(run_local.BuildError, "injected podman failure"):
            self.launch()
        self.assertTrue((self.run_dir / "bootstrap.env").exists())
        self.runner.fail = "load"
        archive = self.root / "app.oci.tar"
        archive.write_bytes(b"fixture")
        self.runner.calls.clear()
        with self.assertRaisesRegex(run_local.BuildError, "injected"):
            self.launch(archive=archive)
        self.assertFalse(any(call[1] == "run" for call in self.runner.calls))

    def test_failed_start_has_manual_inspection_guidance_and_never_replaces_container(self):
        self.runner.fail = "run"
        with self.assertRaises(run_local.BuildError) as raised:
            self.launch(name="my-preview")
        message = str(raised.exception)
        self.assertIn("injected podman failure", message)
        self.assertIn("podman container inspect my-preview", message)
        self.assertIn("podman logs --tail 80 my-preview", message)
        self.assertIn("Only if you confirm", message)
        self.assertIn("podman stop --time 10 my-preview", message)
        self.assertIn("podman rm my-preview", message)
        self.assertIn("another --name", message)
        self.assertIn("Keep packaging/.local-run state", message)
        self.assertEqual([call[1] for call in self.runner.calls], ["info", "info", "run"])
        self.assertNotIn("--replace", self.runner.calls[-1])
        self.assertTrue((self.run_dir / "bootstrap.env").exists())

    def test_root_rootful_and_remote_engines_rejected_before_state_changes(self):
        with patch.object(run_local.os, "geteuid", return_value=0):
            with self.assertRaisesRegex(run_local.BuildError, "not root"):
                self.launch()
        self.assertFalse(self.runner.calls)
        for runner in (PodmanRunner(rootless="false"), PodmanRunner(remote="true")):
            with self.assertRaises(run_local.BuildError):
                run_local.launch(run_dir=self.run_dir, runner=runner)
            self.assertFalse(self.run_dir.exists())

    def test_mount_delimiters_rejected(self):
        with self.assertRaisesRegex(run_local.BuildError, "paths cannot contain"):
            run_local.podman_command(image="localhost/app:test", name="preview",
                                     data=Path("/tmp/with:colon"), env_file=Path("/tmp/env"), broker_dir=None)

    def test_main_never_claims_provisioning_live_or_prints_secrets(self):
        with patch.object(run_local, "launch", return_value="container-id"), contextlib.redirect_stdout(io.StringIO()) as output:
            self.assertEqual(run_local.main([]), 0)
        self.assertIn("broker unavailable; provisioning is NOT live", output.getvalue())
        with patch.object(run_local, "launch", return_value="container-id"), contextlib.redirect_stdout(io.StringIO()) as output:
            self.assertEqual(run_local.main(["--broker-dir", "/run/honenode"]), 0)
        self.assertIn("NOT verified", output.getvalue())


if __name__ == "__main__":
    unittest.main()
