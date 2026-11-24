#!/usr/bin/env python3
"""Build an explicitly OS-tagged HoneNode evaluation bundle.

The backend is shared; the binaries are not. A Fedora-built glibc binary can
require a newer loader than Rocky provides (and vice versa), so each bundle is
built on its target OS and records that target in manifest.json. There is no
pretence that one tarball is portable across both.

    packaging/os/fedora/build.sh
    packaging/os/rocky/build.sh

The lower-level entry point accepts already-built binaries:

    python3 packaging/os/build_os_bundle.py \
      --os fedora \
      --app-binary backend/target/release/honenode \
      --broker-binary backend/target/release/honed \
      --output packaging/os/dist/honenode-0.8.8-fedora-x86_64.tar.gz
"""

from __future__ import annotations

import argparse
import gzip
import hashlib
import io
import json
import os
import re
import subprocess
import sys
import tarfile
import tempfile
from collections.abc import Sequence
from pathlib import Path, PurePosixPath

REPO = Path(__file__).resolve().parents[2]
OS_DIR = Path(__file__).resolve().parent
SUPPORTED_OS = ("fedora", "rocky")
ARCH = "x86_64"
VERSION_RE = re.compile(
    r"(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)"
    r"(?:-[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?"
)


class BundleError(Exception):
    pass


def require_regular(path: Path, description: str) -> None:
    if path.is_symlink() or not path.is_file():
        raise BundleError(f"{description} must be a regular non-symlink file: {path}")


def elf_arch(path: Path, readelf: str = "readelf") -> str:
    require_regular(path, "ELF artifact")
    result = subprocess.run(
        [readelf, "-h", str(path)], text=True, capture_output=True, timeout=30
    )
    if result.returncode != 0:
        raise BundleError(f"readelf failed for {path}: {result.stderr.strip()}")
    match = re.search(r"Machine:\s+(.+)", result.stdout)
    if not match:
        raise BundleError(f"cannot read ELF machine for {path}")
    return match.group(1).strip()


def elf_interpreter(path: Path, readelf: str = "readelf") -> str | None:
    result = subprocess.run(
        [readelf, "-l", str(path)], text=True, capture_output=True, timeout=30
    )
    if result.returncode != 0:
        raise BundleError(f"readelf failed for {path}: {result.stderr.strip()}")
    match = re.search(r"Requesting program interpreter:\s*([^\]]+)", result.stdout)
    return match.group(1).strip() if match else None


def elf_facts(path: Path, readelf: str = "readelf") -> dict[str, str]:
    arch = elf_arch(path, readelf)
    if arch != "Advanced Micro Devices X86-64":
        raise BundleError(f"{path} is not x86_64 (readelf says {arch!r})")
    interpreter = elf_interpreter(path, readelf)
    return {
        "arch": ARCH,
        "libc": "glibc" if interpreter else "static",
        "interpreter": interpreter or "",
    }


def copy_static_tree(source: Path, destination: Path) -> list[str]:
    if source.is_symlink() or not source.is_dir():
        raise BundleError(f"static directory must be a real non-symlink directory: {source}")
    destination.mkdir(parents=True, exist_ok=True)
    files: list[str] = []
    for root, directories, names in os.walk(source, followlinks=False):
        root_path = Path(root)
        for name in sorted(directories):
            if (root_path / name).is_symlink():
                raise BundleError(f"static tree contains a symlink: {root_path / name}")
        for name in sorted(names):
            path = root_path / name
            if path.is_symlink() or not path.is_file():
                raise BundleError(f"static tree contains a non-regular file: {path}")
            relative = path.relative_to(source)
            target = destination / relative
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes(path.read_bytes())
            files.append(relative.as_posix())
    return sorted(files)


def tar_entry(name: str, *, mode: int, size: int = 0, directory: bool = False) -> tarfile.TarInfo:
    info = tarfile.TarInfo(name)
    info.mode = mode
    info.size = 0 if directory else size
    info.mtime = 0
    info.uid = info.gid = 0
    info.uname = info.gname = ""
    info.type = tarfile.DIRTYPE if directory else tarfile.REGTYPE
    return info


def write_archive(
    output: Path,
    root: str,
    payload: dict[str, bytes | Path],
    directories: set[str],
) -> None:
    entries: dict[str, str | None] = {root: None}
    for directory in directories:
        entries[f"{root}/{directory}"] = None
    for name in payload:
        parts = PurePosixPath(name).parts
        for length in range(1, len(parts)):
            entries[f"{root}/{'/'.join(parts[:length])}"] = None
        entries[f"{root}/{name}"] = name
    output.parent.mkdir(parents=True, exist_ok=True)
    descriptor, staged_name = tempfile.mkstemp(
        dir=output.parent, prefix=".honenode-os-bundle-", suffix=".tmp"
    )
    staged = Path(staged_name)
    try:
        with os.fdopen(descriptor, "wb") as raw:
            with gzip.GzipFile(
                filename="", mode="wb", fileobj=raw, compresslevel=9, mtime=0
            ) as compressed:
                with tarfile.open(
                    fileobj=compressed, mode="w", format=tarfile.PAX_FORMAT
                ) as archive:
                    for name in sorted(entries):
                        relative = entries[name]
                        if relative is None:
                            archive.addfile(tar_entry(name, mode=0o755, directory=True))
                            continue
                        value = payload[relative]
                        data = value if isinstance(value, bytes) else value.read_bytes()
                        mode = (
                            0o555
                            if relative in {"native/honenode", "broker/honed", "install.sh"}
                            else 0o644
                        )
                        archive.addfile(
                            tar_entry(name, mode=mode, size=len(data)),
                            io.BytesIO(data),
                        )
        os.chmod(staged, 0o644)
        os.replace(staged, output)
    finally:
        staged.unlink(missing_ok=True)


def build_bundle(
    *,
    os_name: str,
    version: str,
    app_binary: Path,
    broker_binary: Path,
    static_dir: Path,
    output: Path,
    readelf: str = "readelf",
) -> Path:
    if os_name not in SUPPORTED_OS:
        raise BundleError(f"--os must be one of {', '.join(SUPPORTED_OS)}")
    if VERSION_RE.fullmatch(version) is None:
        raise BundleError(f"invalid version: {version!r}")
    if output.suffixes[-2:] != [".tar", ".gz"]:
        raise BundleError("output must end in .tar.gz")

    app = elf_facts(app_binary, readelf)
    broker = elf_facts(broker_binary, readelf)
    if app["arch"] != broker["arch"]:
        raise BundleError("app and broker architectures differ")

    with tempfile.TemporaryDirectory(prefix="honenode-os-bundle-") as temporary:
        work = Path(temporary)
        static = work / "static"
        static_files = copy_static_tree(static_dir, static)
        payload: dict[str, bytes | Path] = {
            "install.py": OS_DIR / "install.py",
            "install.sh": OS_DIR / "install.sh",
            "native/honenode": app_binary,
            "broker/honed": broker_binary,
            "examples/app.env.example": (OS_DIR / os_name / "app.env.example").read_bytes(),
            "examples/policy.example.json": (
                REPO / "backend/crates/honed/policy.example.json"
            ).read_bytes(),
            "examples/policy.selfservice.json": (
                REPO / "backend/crates/honed/policy.selfservice.example.json"
            ).read_bytes(),
            "examples/systemd/honed.service": (
                REPO / "packaging/rpm/honed.service"
            ).read_bytes(),
            "examples/systemd/honenode.sysusers.conf": (
                REPO / "packaging/rpm/honenode.sysusers.conf"
            ).read_bytes(),
            "examples/systemd/honenode.tmpfiles.conf": (
                REPO / "packaging/rpm/honenode.tmpfiles.conf"
            ).read_bytes(),
            "README.md": (OS_DIR / os_name / "README.md").read_bytes(),
        }
        for relative in static_files:
            payload[f"native/static/{relative}"] = static / relative

        artifacts: dict[str, dict[str, object]] = {}
        for name, value in payload.items():
            data = value if isinstance(value, bytes) else value.read_bytes()
            artifacts[name] = {
                "sha256": hashlib.sha256(data).hexdigest(),
                "size": len(data),
            }
        manifest = (
            json.dumps(
                {
                    "schema_version": 1,
                    "name": "honenode-os-evaluation",
                    "os": os_name,
                    "version": version,
                    "arch": ARCH,
                    "binaries": {"app": app, "broker": broker},
                    "experimental": True,
                    "artifacts": artifacts,
                },
                sort_keys=True,
                indent=2,
            )
            + "\n"
        ).encode()
        payload["manifest.json"] = manifest
        artifacts["manifest.json"] = {
            "sha256": hashlib.sha256(manifest).hexdigest(),
            "size": len(manifest),
        }
        sums = "".join(
            f"{artifacts[name]['sha256']}  {name}\n" for name in sorted(artifacts)
        ).encode()
        payload["SHA256SUMS"] = sums

        directories = {"native/static"}
        for relative in static_files:
            parts = PurePosixPath(relative).parts
            for length in range(1, len(parts)):
                directories.add(f"native/static/{'/'.join(parts[:length])}")
        write_archive(
            output,
            f"honenode-{version}-{os_name}-{ARCH}",
            payload,
            directories,
        )
    return output


def parse_args(argv: Sequence[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--os", dest="os_name", required=True, choices=SUPPORTED_OS)
    parser.add_argument("--version", required=True)
    parser.add_argument("--app-binary", required=True, type=Path)
    parser.add_argument("--broker-binary", required=True, type=Path)
    parser.add_argument("--static-dir", type=Path, default=REPO / "backend/static")
    parser.add_argument("--output", required=True, type=Path)
    return parser.parse_args(argv)


def main(argv: Sequence[str] | None = None) -> int:
    args = parse_args(argv)
    try:
        output = build_bundle(
            os_name=args.os_name,
            version=args.version,
            app_binary=args.app_binary,
            broker_binary=args.broker_binary,
            static_dir=args.static_dir,
            output=args.output,
        )
    except (BundleError, OSError, subprocess.SubprocessError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    print(f"built {output}; OS-tagged as {args.os_name}; nothing was installed or started")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
