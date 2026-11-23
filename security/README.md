# Security evidence and automated checks

The STRIDE model is `HoneNode_Threat_Model_STRIDE.json` (Threat Dragon v2,
two diagrams, 42 threats, owner/reviewer AgileCTI). This file maps the model to
the tools that run in `.github/workflows/ci.yml`.

| Threat area | Automated check | Workflow job | Gate |
|---|---|---|---|
| Command injection / host command construction | custom Semgrep `honenode-rust-command-interpolated`; Clippy | security / rust | yes |
| SQL injection | custom Semgrep `honenode-rust-sql-format`; `sqlx` bind review and tests | security / rust | yes |
| Credential protection / weak hashes | custom Semgrep `honenode-rust-weak-password-hash`; Argon2/bcrypt tests | security / rust | yes |
| Secret leakage | Betterleaks, Trivy secret scanner, Semgrep `p/secrets` | security | yes |
| Vulnerable dependencies / supply chain | Cargo Audit, OSV-Scanner, Trivy filesystem, Cargo Deny, Dependency Review | security | yes |
| Container/image hardening | Hadolint, Trivy misconfiguration, distroless `Containerfile` | security / release | yes |
| Web headers, XSS, CSRF, information disclosure | ZAP baseline against the login surface, app CSP/security headers | dast | advisory |
| Host hardening | OpenSCAP CIS profile and Lynis inside Rocky 10 | openscap / lynis | advisory |
| End-to-end provisioning | Rocky 10 smoke with nested Podman | smoke-rocky | advisory on GitHub |

## Accepted advisories (0.8.8)

Both are ignored by `cargo audit` and `cargo deny` with the rationale below.
They are tracked for the next dependency pass.

- **RUSTSEC-2026-0459** — `libsqlite3-sys` 0.30.1 via `sqlx` 0.8.6. The fix
  ships in `libsqlite3-sys` 0.39 / `sqlx` 0.9, which requires Rust 1.94 and
  cannot be built by the Rocky 10 distro toolchain. Exposure needs a crafted
  database plus `-wal` file written by a less-trusted local party; the
  registry directory is root-owned and tenants have no host access.
- **RUSTSEC-2023-0071** — `rsa` 0.9.10. It is pulled only by the optional
  `sqlx-mysql` crate, which this workspace does not enable or compile; the
  advisory is therefore not reachable in the shipped binary.

## Notes

- Semgrep runs the FOSS CLI with the Semgrep registry packs plus
  `semgrep.yml` in this directory. `--severity ERROR --error` gates the job.
- Betterleaks (MIT, the maintained successor to Gitleaks by the same
  authors) scans the full git history with `.betterleaks.toml`; findings fail
  the job and are also uploaded as SARIF.
- Trivy runs a filesystem scan on the source tree and an image scan on the
  image built by the DAST job. Cargo Audit and OSV-Scanner cover the Rust
  lockfile; Trivy covers the image layers and the Containerfile misconfiguration.
- OpenSCAP and Lynis audit the Rocky container, not the GitHub runner. Their
  reports are artifacts because the real target host is the SharpNode machine.
- ZAP scans the login/signup surface of the app image; the tenant pages need a
  live broker and are covered by the Rocky smoke instead.
- The custom rules intentionally avoid flagging `Command::new(program)` in the
  broker's centralized wrappers; the rule targets interpolation, which is the
  actual injection shape.
