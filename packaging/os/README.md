# OS-tagged evaluation bundles

The Rust backend is shared. The binaries are not portable by assumption: a
Fedora-built glibc binary can require a newer loader than Rocky provides, and a
Rocky-built binary can miss libraries a newer Fedora expects. This directory
builds one explicit artifact per target OS and records the target in
`manifest.json` (`"os": "fedora"` or `"os": "rocky"`).

## Build

```sh
# On Fedora, with the native toolchain:
bash packaging/os/fedora/build.sh

# From any host with podman; boots Rocky 10 and builds there:
bash packaging/os/rocky/build.sh
```

Both write to `packaging/os/dist/`:

```text
honenode-0.8.8-fedora-x86_64.tar.gz
honenode-0.8.8-rocky-x86_64.tar.gz
```

Do not install the Fedora bundle on Rocky or the Rocky bundle on Fedora. The
manifest tells you which one you have; the `README.md` inside each bundle has
the target-specific quickstart.

## Contents

Each bundle contains the app (`native/honenode`), the broker
(`broker/honed`), the app's static assets, policy/environment/systemd examples,
`README.md`, `manifest.json` and `SHA256SUMS`.

The Fedora bundle's README describes its contents and build. The Rocky
bundle's README documents the precompiled binaries, the manual host-integration
boundary and the repeatable smoke test. The host deployment scripts themselves
(the Fedora lab wizard and reset helper) live in the SharpNode host repository,
outside this app source tree.

## Verification

`packaging/os/fedora/build.sh` runs the Rust and Python suites before writing
the bundle. `packaging/os/rocky/build.sh` builds inside the same Rocky 10
container as `packaging/rocky/run_smoke_podman.sh`; after building, run
`bash packaging/rocky/run_smoke_podman.sh --prebuilt` to exercise those exact
binaries. CI compiles the Rocky binaries once and runs the full smoke with
`--prebuilt`, rather than rebuilding inside the smoke.

`python3 -m unittest discover -s packaging/tests` covers the bundle builder,
including the OS tag and the rejection of an unknown target.
