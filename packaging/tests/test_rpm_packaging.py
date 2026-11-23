from pathlib import Path
import unittest


ROOT = Path(__file__).parents[1]
RPM = ROOT / "rpm"


class RpmPackagingTests(unittest.TestCase):
    def test_unit_overrides_socket_policy_and_clears_tenants(self):
        unit = (RPM / "honed.service").read_text(encoding="utf-8")
        self.assertIn("Environment=HONE_BROKER_SOCKET=/run/honenode/broker.sock\n", unit)
        self.assertIn("Environment=HONE_POLICY_FILE=/etc/honenode/policy.json\n", unit)
        self.assertIn("Environment=HONE_TENANTS=\n", unit)
        self.assertIn("Group=honenode\n", unit)
        self.assertIn("Restart=no\n", unit)

    def test_no_active_policy_or_preprovisioned_tenant_is_shipped(self):
        spec = (RPM / "honed.spec").read_text(encoding="utf-8")
        self.assertNotIn("policy.example", spec)
        self.assertNotIn("policy.json", spec)
        self.assertNotRegex(spec, r"(?m)^.*t[0-9]{2}.*$")
        self.assertFalse((RPM / "policy.json").exists())
        self.assertFalse((RPM / "policy.example.json").exists())

    def test_scriptlets_do_not_manage_service_lifecycle_or_delete_state(self):
        spec = (RPM / "honed.spec").read_text(encoding="utf-8")
        scriptlets = spec.split("%pre", 1)[1].split("%files", 1)[0]
        for forbidden in (
            "systemctl enable",
            "systemctl start",
            "systemctl restart",
            "systemctl stop",
            "systemctl disable",
            "rm -rf /etc/honenode",
            "rm -rf /var/lib/honenode",
            "groupdel",
        ):
            self.assertNotIn(forbidden, scriptlets)
        self.assertIn("systemd-tmpfiles --create", scriptlets)
        self.assertIn("systemctl daemon-reload", scriptlets)

    def test_sysusers_and_tmpfiles_declare_required_group_and_directories(self):
        sysusers = (RPM / "honenode.sysusers.conf").read_text(encoding="utf-8")
        tmpfiles = (RPM / "honenode.tmpfiles.conf").read_text(encoding="utf-8")
        self.assertIn("g honenode - - -", sysusers)
        for directory in ("/etc/honenode", "/run/honenode", "/var/lib/honenode"):
            self.assertIn(directory, tmpfiles)

    def test_spec_has_rhel10_gate_and_prebuilt_binary_input(self):
        spec = (RPM / "honed.spec").read_text(encoding="utf-8")
        self.assertIn("%if 0%{?rhel} != 10", spec)
        self.assertIn("Source0:        honed", spec)
        self.assertNotIn("cargo build", spec)
        self.assertNotIn("honenode-app", spec)


if __name__ == "__main__":
    unittest.main()
