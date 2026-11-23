#!/usr/bin/env python3
"""Build a reproducible experimental HoneNode Rocky x86_64 evaluation bundle."""

from __future__ import annotations

import argparse
import gzip
import hashlib
import io
import json
import os
import re
import stat
import subprocess
import sys
import tarfile
import tempfile
from collections.abc import Sequence
from pathlib import Path, PurePosixPath

OCI_DIR = Path(__file__).resolve().parents[1] / "oci"
sys.path.insert(0, str(OCI_DIR))

from build_archive import (  # noqa: E402
    add_regular,
    architecture,
    build_archive as build_oci_archive,
    tar_entry,
)
from build_image import (  # noqa: E402
    BuildError,
    Runner,
    require_regular_file,
    verify_static_elf,
)

REPOSITORY_ROOT = Path(__file__).resolve().parents[2]
TARGET_ARCH = "x86_64"
OCI_ARCH = "amd64"
EXECUTABLES = {"native/honenode", "broker/honed"}
SOURCE_FILES = {
    "LICENSE": "LICENSE",
    "README.md": "packaging/release/README.md",
    "EVALUATION.md": "packaging/release/EVALUATION.md",
    "examples/policy.example.json": "backend/crates/honed/policy.example.json",
    "examples/systemd/honed.service": "packaging/rpm/honed.service",
    "examples/systemd/honenode.sysusers.conf": "packaging/rpm/honenode.sysusers.conf",
    "examples/systemd/honenode.tmpfiles.conf": "packaging/rpm/honenode.tmpfiles.conf",
}
VERSION_RE = re.compile(
    r"(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)"
    r"(?:-[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?"
)

APP_ENV_EXAMPLE = b"""# Generic app-only evaluation example; this file is not loaded automatically.
# Review every path. No operator credential or API token belongs in this example.
HONE_BIND=127.0.0.1:8080
HONE_SERVICE_ACCESS=loopback
HONE_STATIC_DIR=native/static
HONE_BROKER_SOCKET=/run/honenode/broker.sock
HONE_REGISTRY=/var/lib/honenode/registry/registry.db
HONE_TENANT_DATA=/srv/hosting
HONE_TENANTS=
# Only for loopback HTTP evaluation; use secure cookies behind reviewed TLS.
HONE_SECURE_COOKIES=false
HONE_LOG_FORMAT=json
HONE_THEME=sharpnode
"""


def json_bytes(value: object) -> bytes:
    return (json.dumps(value, sort_keys=True, indent=2) + "\n").encode("utf-8")


def quadlet_example(version: str) -> bytes:
    return f"""# App-only Quadlet example; review before installing or enabling anything.
# No broker socket is mounted, so this app cannot provision tenants.
# The named volume is runtime state and is not supplied by this bundle.
[Unit]
Description=HoneNode experimental app-only evaluation
After=network-online.target

[Container]
Image=localhost/honenode-app:{version}
ContainerName=honenode-app
PublishPort=127.0.0.1:8080:8080
Volume=honenode-eval-data:/var/lib/honenode
Environment=HONE_BIND=0.0.0.0:8080
Environment=HONE_SERVICE_ACCESS=public
Environment=HONE_SECURE_COOKIES=false
NoNewPrivileges=true

[Service]
Restart=no

[Install]
WantedBy=default.target
""".encode("utf-8")


def copy_open_regular(source_fd: int, destination: Path) -> None:
    with os.fdopen(source_fd, "rb") as source:
        if not stat.S_ISREG(os.fstat(source.fileno()).st_mode):
            raise BuildError(f"not a regular file: {destination}")
        with destination.open("xb") as target:
            for chunk in iter(lambda: source.read(1024 * 1024), b""):
                target.write(chunk)


def copy_regular(source: Path, destination: Path, description: str) -> None:
    require_regular_file(source, description)
    source_fd = os.open(source, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    copy_open_regular(source_fd, destination)


def read_regular(path: Path, description: str) -> bytes:
    require_regular_file(path, description)
    source_fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(source_fd, "rb") as source:
        if not stat.S_ISREG(os.fstat(source.fileno()).st_mode):
            raise BuildError(f"{description} is not a regular file: {path}")
        return source.read()


def copy_static_tree(source: Path, destination: Path) -> tuple[list[str], set[str]]:
    """Snapshot regular static files without following links; return relative paths."""
    if source.is_symlink() or not source.is_dir():
        raise BuildError(f"static directory must be a real, non-symlink directory: {source}")
    destination.mkdir()
    files: list[str] = []
    directories: set[str] = set()

    def visit(source_fd: int, target_dir: Path, relative_dir: str) -> None:
        for name in sorted(os.listdir(source_fd)):
            if "\n" in name or "\r" in name:
                raise BuildError(f"static tree contains an unsupported filename: {name!r}")
            metadata = os.stat(name, dir_fd=source_fd, follow_symlinks=False)
            relative = f"{relative_dir}/{name}" if relative_dir else name
            target = target_dir / name
            if stat.S_ISDIR(metadata.st_mode):
                child_fd = os.open(
                    name,
                    os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW,
                    dir_fd=source_fd,
                )
                try:
                    target.mkdir()
                    directories.add(relative)
                    visit(child_fd, target, relative)
                finally:
                    os.close(child_fd)
            elif stat.S_ISREG(metadata.st_mode):
                child_fd = os.open(
                    name,
                    os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK,
                    dir_fd=source_fd,
                )
                copy_open_regular(child_fd, target)
                files.append(relative)
            else:
                raise BuildError(f"static tree contains a symlink or special file: {relative}")

    root_fd = os.open(source, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
    try:
        visit(root_fd, destination, "")
    finally:
        os.close(root_fd)
    return sorted(files), directories


def validate_output(output: Path, protected: Sequence[Path], static_dir: Path) -> None:
    if output.is_symlink():
        raise BuildError(f"output must not be a symlink: {output}")
    if output.exists() and not output.is_file():
        raise BuildError(f"output must be a regular file when it exists: {output}")
    if not output.parent.is_dir():
        raise BuildError(f"output parent directory must already exist: {output.parent}")
    resolved_output = output.resolve()
    for path in protected:
        if resolved_output == path.resolve():
            raise BuildError(f"output must not overwrite an input: {output}")
    if resolved_output.is_relative_to(static_dir.resolve()):
        raise BuildError("output must be outside the static directory")


def sha256_path(path: Path) -> tuple[str, int]:
    digest = hashlib.sha256()
    size = 0
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
            size += len(chunk)
    return digest.hexdigest(), size


def payload_digest(value: Path | bytes) -> tuple[str, int]:
    if isinstance(value, bytes):
        return hashlib.sha256(value).hexdigest(), len(value)
    return sha256_path(value)


def add_payload_file(archive: tarfile.TarFile, name: str, value: Path | bytes, mode: int) -> None:
    if isinstance(value, bytes):
        archive.addfile(tar_entry(name, mode=mode, size=len(value)), io.BytesIO(value))
        return
    source_fd = os.open(value, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    add_regular(archive, name, source_fd, mode)


def write_deterministic_archive(
    output: Path,
    root_name: str,
    payload: dict[str, Path | bytes],
    explicit_directories: set[str],
) -> None:
    """Write sorted regular files/directories with gzip mtime zero, then rename."""
    directories = set(explicit_directories)
    for name in payload:
        parts = PurePosixPath(name).parts
        for length in range(1, len(parts)):
            directories.add("/".join(parts[:length]))

    entries: dict[str, str | None] = {root_name: None}
    for directory in directories:
        entries[f"{root_name}/{directory}"] = None
    for name in payload:
        entries[f"{root_name}/{name}"] = name

    descriptor, staged_name = tempfile.mkstemp(
        dir=output.parent, prefix=".honenode-release-", suffix=".tmp"
    )
    staged = Path(staged_name)
    try:
        with os.fdopen(descriptor, "wb") as raw:
            with gzip.GzipFile(
                filename="", mode="wb", fileobj=raw, compresslevel=9, mtime=0
            ) as compressed:
                with tarfile.open(fileobj=compressed, mode="w", format=tarfile.PAX_FORMAT) as archive:
                    for archive_name in sorted(entries):
                        relative = entries[archive_name]
                        if relative is None:
                            info = tar_entry(archive_name, mode=0o755, directory=True)
                            info.uname = info.gname = ""
                            archive.addfile(info)
                            continue
                        mode = 0o555 if relative in EXECUTABLES else 0o644
                        add_payload_file(archive, archive_name, payload[relative], mode)
        os.chmod(staged, 0o644)
        os.replace(staged, output)
    finally:
        staged.unlink(missing_ok=True)


def build_release(
    *,
    app_binary: Path,
    broker_binary: Path,
    static_dir: Path,
    output: Path,
    version: str,
    source_root: Path = REPOSITORY_ROOT,
    readelf: str = "readelf",
    runner: Runner = subprocess.run,
) -> Path:
    if VERSION_RE.fullmatch(version) is None:
        raise BuildError(f"version must be a numeric semantic version without build metadata: {version!r}")

    sources = {destination: source_root / source for destination, source in SOURCE_FILES.items()}
    for description, source in sources.items():
        require_regular_file(source, f"bundle source {description}")
    validate_output(output, [app_binary, broker_binary, *sources.values()], static_dir)

    with tempfile.TemporaryDirectory(prefix="honenode-release-work-") as temporary:
        work = Path(temporary)
        staged_app = work / "honenode"
        staged_broker = work / "honed"
        staged_static = work / "static"
        copy_regular(app_binary, staged_app, "app binary")
        copy_regular(broker_binary, staged_broker, "broker binary")
        static_files, static_directories = copy_static_tree(static_dir, staged_static)

        verify_static_elf(staged_app, readelf=readelf, runner=runner)
        verify_static_elf(staged_broker, readelf=readelf, runner=runner)
        app_arch = architecture(staged_app, readelf=readelf, runner=runner)
        broker_arch = architecture(staged_broker, readelf=readelf, runner=runner)
        if app_arch != broker_arch:
            raise BuildError(
                f"app and broker ELF architectures differ: {app_arch} versus {broker_arch}"
            )
        if app_arch != OCI_ARCH:
            raise BuildError(
                f"Rocky {TARGET_ARCH} bundle requires two {OCI_ARCH} ELFs, got {app_arch}"
            )

        oci_archive = work / "honenode-app.oci.tar"
        build_oci_archive(
            binary=staged_app,
            static_dir=staged_static,
            output=oci_archive,
            image=f"localhost/honenode-app:{version}",
            readelf=readelf,
            runner=runner,
        )

        payload: dict[str, Path | bytes] = {
            "oci/honenode-app.oci.tar": oci_archive,
            "native/honenode": staged_app,
            "broker/honed": staged_broker,
            "examples/app.env.example": APP_ENV_EXAMPLE,
            "examples/quadlet/honenode.container": quadlet_example(version),
        }
        for destination, source in sources.items():
            payload[destination] = read_regular(source, f"bundle source {destination}")
        for relative in static_files:
            payload[f"native/static/{relative}"] = staged_static / relative
        explicit_directories = {"native/static"}
        explicit_directories.update(
            f"native/static/{relative}" for relative in static_directories
        )

        artifact_digests: dict[str, str] = {}
        artifacts = {}
        for name in sorted(payload):
            digest, size = payload_digest(payload[name])
            artifact_digests[name] = digest
            artifacts[name] = {"sha256": digest, "size": size}
        manifest = json_bytes({
            "schema_version": 1,
            "name": "honenode-evaluation",
            "version": version,
            "arch": TARGET_ARCH,
            "oci_arch": OCI_ARCH,
            "experimental": True,
            "artifacts": artifacts,
        })
        payload["manifest.json"] = manifest
        artifact_digests["manifest.json"] = hashlib.sha256(manifest).hexdigest()
        sums = "".join(
            f"{artifact_digests[name]}  {name}\n" for name in sorted(artifact_digests)
        )
        payload["SHA256SUMS"] = sums.encode("utf-8")

        write_deterministic_archive(
            output,
            f"honenode-{version}-{TARGET_ARCH}",
            payload,
            explicit_directories,
        )
    return output


def parse_args(argv: Sequence[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--app-binary", required=True, type=Path, help="prebuilt static honenode ELF")
    parser.add_argument("--broker-binary", required=True, type=Path, help="prebuilt static honed ELF")
    parser.add_argument("--static-dir", type=Path, default=Path("backend/static"))
    parser.add_argument("--output", required=True, type=Path, help="output tar.gz; parent must exist")
    parser.add_argument("--version", required=True, help="evaluation version, for example 0.8.0")
    return parser.parse_args(argv)


def main(argv: Sequence[str] | None = None) -> int:
    args = parse_args(argv)
    try:
        build_release(
            app_binary=args.app_binary,
            broker_binary=args.broker_binary,
            static_dir=args.static_dir,
            output=args.output,
            version=args.version,
        )
    except (BuildError, OSError, tarfile.TarError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    print(
        f"built experimental Rocky {TARGET_ARCH} evaluation bundle: {args.output}; "
        "nothing was loaded, installed, started, or provisioned"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
