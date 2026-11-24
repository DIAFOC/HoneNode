# Security evidence and automated checks

The STRIDE model is `HoneNode_Threat_Model_STRIDE.json` (Threat Dragon v2,
two diagrams, 42 threats, owner/reviewer AgileCTI). This file maps the model to
the tools that run in `.github/workflows/ci.yml` and the report-only
`.github/workflows/host-audit.yml`.

| Threat area | Automated check | Workflow job | Gate |
|---|---|---|---|
| Command injection / host command construction | custom Semgrep `honenode-rust-command-interpolated`; Clippy | security / rust | yes |
| SQL injection | custom Semgrep `honenode-rust-sql-format`; `sqlx` bind review and tests | security / rust | yes |
| Credential protection / weak hashes | custom Semgrep `honenode-rust-weak-password-hash`; Argon2/bcrypt tests | security / rust | yes |
| Secret leakage | Betterleaks, Trivy secret scanner, Semgrep `p/secrets` | security | yes |
| Vulnerable dependencies / supply chain | Cargo Audit, Trivy filesystem, Cargo Deny, Dependency Review; OSV report | security | yes |
| Container/image hardening | Hadolint, Trivy misconfiguration, distroless `Containerfile` | security / release | yes |
| Web headers, XSS, CSRF, information disclosure | ZAP baseline against the login surface, app CSP/security headers | dast | yes |
| Host hardening | OpenSCAP/Lynis container reports in the host-audit workflow | host-audit | report-only |
| End-to-end provisioning | Rocky bundle installer + full smoke on the built binaries | smoke-rocky | yes |

## Accepted advisories (0.8.8)

The shipped OCI image and the OS bundles are **precompiled**; users do not
compile. The acceptance below is therefore about runtime reachability in the
shipped artifacts, not about asking a user to build around a vulnerable crate.
Both advisories are ignored explicitly by `cargo audit`; `cargo deny` ignores
the SQLite advisory and does not encounter `rsa` because the `sqlx-mysql` path
is not compiled.

- **RUSTSEC-2026-0459** — `libsqlite3-sys` 0.30.1 via `sqlx`. The patched crate
  is 0.39 (rusqlite 0.41 / sqlx 0.9 lineage), but sqlx 0.9.1 still pins
  `libsqlite3-sys` 0.30.1. Rocky 10 provides Rust 1.97, so the toolchain is no
  longer the blocker; this is an upstream dependency gap. The app opens only
  its own registry database, in a root/app-owned directory; tenants have no
  host access and cannot supply a crafted database plus `-wal` pair. Fix paths
  for 0.8.9: wait for sqlx to adopt 0.39, vendor-patch `sqlx-sqlite`, or link a
  system SQLite and prove the version on each shipped runtime.
- **RUSTSEC-2023-0071** — `rsa` 0.9.10. It is pulled only by the optional
  `sqlx-mysql` crate, which this workspace does not enable or compile; the
  advisory is not reachable in the shipped binary. A tested sqlx 0.9.1 update
  removes it from `Cargo.lock` entirely (107 broker + 75 app tests passed
  unchanged), so it is a candidate for the 0.8.9 dependency pass rather than a
  reason to ship a different 0.8.8.

The image scan is gated at CRITICAL/HIGH (currently clean); the accepted
advisories are MEDIUM and appear in the dependency reports, not in the image
gate. There is no claim of SBOM signing, provenance, or a digest-pinned base
image yet.

## Notes

- Semgrep runs the FOSS CLI with the Semgrep registry packs plus
  `semgrep.yml` in this directory. `--severity ERROR --error` gates the job.
- Betterleaks (MIT, the maintained successor to Gitleaks by the same
  authors) scans the full git history with `.betterleaks.toml`; findings fail
  the job and are also uploaded as SARIF.
- Trivy runs a filesystem scan on the source tree and an image scan on the
  image built by the DAST job. Cargo Audit and OSV-Scanner cover the Rust
  lockfile; Trivy covers the image layers and the Containerfile misconfiguration.
- OpenSCAP and Lynis audit the Rocky container, not the GitHub runner, and now
  run only in the manual/weekly `host-audit.yml` workflow. Their reports are
  artifacts because the real target host is the SharpNode machine. The OpenSCAP
  datastream has no container profile; the host CIS profile is used as a tool
  smoke and many controls are correctly `notapplicable` in a container
  (no bootloader, kernel, partitioning or host services). This is not a host
  compliance result; a real host scan needs a Rocky VM or a self-hosted runner.
- ZAP scans the login/signup surface of the app image; the tenant pages need a
  live broker and are covered by the Rocky smoke instead.
- The custom rules intentionally avoid flagging `Command::new(program)` in the
  broker's centralized wrappers; the rule targets interpolation, which is the
  actual injection shape.
