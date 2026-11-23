#!/usr/bin/env python3
"""Assemble a scratch OCI archive without Docker, Buildah, or third-party Python."""

from __future__ import annotations

import argparse
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
from pathlib import Path

from build_image import (
    APP_GID,
    APP_UID,
    DEFAULT_IMAGE,
    BuildError,
    Runner,
    run,
    verify_static_elf,
)

OCI = "application/vnd.oci.image."


def json_bytes(value: object) -> bytes:
    return json.dumps(value, sort_keys=True, separators=(",", ":")).encode("utf-8")


def tar_entry(name: str, *, mode: int, size: int = 0, directory: bool = False,
              uid: int = 0, gid: int = 0) -> tarfile.TarInfo:
    info = tarfile.TarInfo(name)
    info.mode, info.size, info.uid, info.gid = mode, size, uid, gid
    info.mtime = 0
    info.type = tarfile.DIRTYPE if directory else tarfile.REGTYPE
    return info


def add_regular(archive: tarfile.TarFile, name: str, fd: int, mode: int) -> None:
    with os.fdopen(fd, "rb") as source:
        metadata = os.fstat(source.fileno())
        if not stat.S_ISREG(metadata.st_mode):
            raise BuildError(f"not a regular file: {name}")
        archive.addfile(tar_entry(name, mode=mode, size=metadata.st_size), source)


def add_static_tree(archive: tarfile.TarFile, root: Path) -> None:
    # Descriptor-relative traversal avoids following a symlink swapped in after lstat.
    def visit(fd: int, destination: str) -> None:
        archive.addfile(tar_entry(destination, mode=0o555, directory=True))
        for name in sorted(os.listdir(fd)):
            metadata = os.stat(name, dir_fd=fd, follow_symlinks=False)
            target = f"{destination}/{name}"
            if stat.S_ISDIR(metadata.st_mode):
                child = os.open(name, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=fd)
                try:
                    visit(child, target)
                finally:
                    os.close(child)
            elif stat.S_ISREG(metadata.st_mode):
                source = os.open(name, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK, dir_fd=fd)
                add_regular(archive, target, source, 0o444)
            else:
                raise BuildError(f"static tree contains a symlink or special file: {target}")

    fd = os.open(root, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
    try:
        visit(fd, "app/static")
    finally:
        os.close(fd)


def architecture(binary: Path, *, readelf: str, runner: Runner) -> str:
    header = run([readelf, "-hW", str(binary)], runner=runner).stdout
    machine = re.search(r"^\s*Machine:\s*(.+)$", header, re.MULTILINE)
    supported = {"Advanced Micro Devices X86-64": "amd64", "AArch64": "arm64"}
    if machine is None or machine[1].strip() not in supported:
        raise BuildError("unsupported ELF architecture (supported: x86-64 and AArch64)")
    return supported[machine[1].strip()]


def descriptor(path: Path, media_type: str) -> dict:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return {"mediaType": media_type, "digest": f"sha256:{digest.hexdigest()}",
            "size": path.stat().st_size}


def build_archive(*, binary: Path, static_dir: Path, output: Path,
                  image: str = DEFAULT_IMAGE, readelf: str = "readelf",
                  runner: Runner = subprocess.run) -> Path:
    verify_static_elf(binary, readelf=readelf, runner=runner)
    arch = architecture(binary, readelf=readelf, runner=runner)
    if not re.fullmatch(r"localhost/[a-z0-9]+(?:[._/-][a-z0-9]+)*:[A-Za-z0-9_][A-Za-z0-9_.-]*", image):
        raise BuildError("use a tagged local image reference, e.g. localhost/honenode-app:poc")
    if output.is_symlink() or output.resolve() == binary.resolve():
        raise BuildError("output must not be a symlink or the input binary")
    if output.resolve().is_relative_to(static_dir.resolve()):
        raise BuildError("output must be outside the static directory")

    with tempfile.TemporaryDirectory(prefix="honenode-oci-") as temporary:
        root = Path(temporary)
        layer = root / "layer.tar"
        with tarfile.open(layer, "w", format=tarfile.PAX_FORMAT) as archive:
            for name in ("usr", "usr/local", "usr/local/bin", "app", "var", "var/lib"):
                archive.addfile(tar_entry(name, mode=0o755, directory=True))
            for name in ("var/lib/honenode", "var/lib/honenode/registry", "var/lib/honenode/tenants"):
                archive.addfile(tar_entry(name, mode=0o700, directory=True, uid=APP_UID, gid=APP_GID))
            archive.addfile(tar_entry("run", mode=0o755, directory=True))
            archive.addfile(tar_entry("run/honenode", mode=0o700, directory=True, uid=APP_UID, gid=APP_GID))
            archive.addfile(tar_entry("tmp", mode=0o1777, directory=True))
            fd = os.open(binary, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
            add_regular(archive, "usr/local/bin/honenode", fd, 0o555)
            add_static_tree(archive, static_dir)

        layer_desc = descriptor(layer, OCI + "layer.v1.tar")
        config = root / "config.json"
        config.write_bytes(json_bytes({
            "architecture": arch, "os": "linux",
            "config": {
                "User": f"{APP_UID}:{APP_GID}", "WorkingDir": "/app",
                "Entrypoint": ["/usr/local/bin/honenode"], "StopSignal": "SIGTERM",
                "Env": ["HONE_BIND=0.0.0.0:8080", "HONE_STATIC_DIR=/app/static",
                        "HONE_BROKER_SOCKET=/run/honenode/broker.sock",
                        "HONE_REGISTRY=/var/lib/honenode/registry/registry.db",
                        "HONE_TENANT_DATA=/var/lib/honenode/tenants", "HONE_TENANTS=",
                        "HONE_SECURE_COOKIES=true", "HONE_LOG_FORMAT=json", "HONE_THEME=sharpnode"],
                "ExposedPorts": {"8080/tcp": {}}, "Volumes": {"/var/lib/honenode": {}},
                "Labels": {"org.opencontainers.image.title": "HoneNode app preview",
                           "org.opencontainers.image.description": "Experimental; not a product release"},
            },
            "rootfs": {"type": "layers", "diff_ids": [layer_desc["digest"]]},
        }))
        config_desc = descriptor(config, OCI + "config.v1+json")
        manifest = root / "manifest.json"
        manifest.write_bytes(json_bytes({"schemaVersion": 2, "mediaType": OCI + "manifest.v1+json",
                                         "config": config_desc, "layers": [layer_desc]}))
        manifest_desc = descriptor(manifest, OCI + "manifest.v1+json")
        manifest_desc["annotations"] = {"org.opencontainers.image.ref.name": image}
        manifest_desc["platform"] = {"architecture": arch, "os": "linux"}
        index = json_bytes({"schemaVersion": 2, "mediaType": OCI + "index.v1+json",
                            "manifests": [manifest_desc]})
        # Replace only after the entire archive is complete; leave old output on failure.
        with tempfile.NamedTemporaryFile(dir=output.parent, prefix=".honenode-", delete=False) as target:
            staged = Path(target.name)
        try:
            with tarfile.open(staged, "w", format=tarfile.PAX_FORMAT) as archive:
                for name, data in (("oci-layout", json_bytes({"imageLayoutVersion": "1.0.0"})),
                                   ("index.json", index)):
                    archive.addfile(tar_entry(name, mode=0o644, size=len(data)), io.BytesIO(data))
                for path, desc in ((layer, layer_desc), (config, config_desc), (manifest, manifest_desc)):
                    with path.open("rb") as source:
                        archive.addfile(tar_entry("blobs/sha256/" + desc["digest"].split(":")[1],
                                                  mode=0o644, size=desc["size"]), source)
            os.replace(staged, output)
        finally:
            staged.unlink(missing_ok=True)
    return output


def parse_args(argv: Sequence[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True, type=Path, help="prebuilt static Linux honenode ELF")
    parser.add_argument("--static-dir", type=Path, default=Path("backend/static"))
    parser.add_argument("--output", required=True, type=Path, help="OCI archive; parent directory must exist")
    parser.add_argument("--image", default=DEFAULT_IMAGE)
    return parser.parse_args(argv)


def main(argv: Sequence[str] | None = None) -> int:
    args = parse_args(argv)
    try:
        build_archive(binary=args.binary, static_dir=args.static_dir, output=args.output, image=args.image)
    except (BuildError, OSError, tarfile.TarError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    print(f"built OCI app-preview archive: {args.output} ({args.image}); not loaded or started")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
