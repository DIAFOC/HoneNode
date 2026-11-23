#!/usr/bin/env python3
"""Build an experimental HoneNode PoC image from prebuilt artifacts."""

from __future__ import annotations

import argparse
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
import uuid
from collections.abc import Callable, Sequence

APP_UID = 65532
APP_GID = 65532
DEFAULT_IMAGE = "localhost/honenode-app:poc"


class BuildError(RuntimeError):
    pass


Runner = Callable[..., subprocess.CompletedProcess[str]]


def run(
    argv: Sequence[str], *, runner: Runner = subprocess.run
) -> subprocess.CompletedProcess[str]:
    result = runner(
        list(argv),
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        check=False,
    )
    if result.returncode != 0:
        detail = result.stderr.strip() or result.stdout.strip() or "no diagnostic output"
        raise BuildError(f"command failed ({' '.join(argv)}): {detail}")
    return result


def require_regular_file(path: Path, description: str) -> None:
    if path.is_symlink() or not path.is_file():
        raise BuildError(f"{description} must be a regular, non-symlink file: {path}")


def verify_static_elf(
    binary: Path, *, readelf: str = "readelf", runner: Runner = subprocess.run
) -> None:
    """Reject dynamic dependencies; this does not identify the libc or build target."""
    require_regular_file(binary, "honenode binary")
    header = run([readelf, "-hW", str(binary)], runner=runner).stdout
    if not re.search(r"^ELF Header:", header, re.MULTILINE):
        raise BuildError(f"not an ELF binary: {binary}")

    program_headers = run([readelf, "-lW", str(binary)], runner=runner).stdout
    if re.search(r"\bINTERP\b|Requesting program interpreter", program_headers):
        raise BuildError(f"honenode has a dynamic program interpreter: {binary}")

    dynamic = run([readelf, "-dW", str(binary)], runner=runner).stdout
    if re.search(r"\(NEEDED\)|\bNEEDED\b", dynamic):
        raise BuildError(f"honenode has DT_NEEDED shared-library dependencies: {binary}")


def build_image(
    *,
    binary: Path,
    css: Path,
    image: str,
    buildah: str = "buildah",
    readelf: str = "readelf",
    runner: Runner = subprocess.run,
) -> str:
    verify_static_elf(binary, readelf=readelf, runner=runner)
    require_regular_file(css, "static CSS")
    if css.stat().st_size == 0:
        raise BuildError(f"static CSS is empty: {css}")
    if not image.strip():
        raise BuildError("image reference must not be empty")

    container = f"honenode-poc-{os.getpid()}-{uuid.uuid4().hex}"
    try:
        with tempfile.TemporaryDirectory(prefix="honenode-scratch-") as temporary:
            empty = Path(temporary) / "empty"
            empty.mkdir()

            run(
                [buildah, "from", "--name", container, "scratch"],
                runner=runner,
            )
            run(
                [
                    buildah,
                    "copy",
                    "--chown",
                    "0:0",
                    "--chmod",
                    "0755",
                    container,
                    str(binary),
                    "/usr/local/bin/honenode",
                ],
                runner=runner,
            )
            run(
                [
                    buildah,
                    "copy",
                    "--chown",
                    "0:0",
                    "--chmod",
                    "0444",
                    container,
                    str(css),
                    "/app/static/hone.css",
                ],
                runner=runner,
            )
            run(
                [
                    buildah,
                    "copy",
                    "--chown",
                    f"{APP_UID}:{APP_GID}",
                    "--chmod",
                    "0700",
                    container,
                    str(empty),
                    "/var/lib/honenode/registry",
                ],
                runner=runner,
            )
            run(
                [
                    buildah,
                    "copy",
                    "--chown",
                    "0:0",
                    "--chmod",
                    "0755",
                    container,
                    str(empty),
                    "/run/honenode",
                ],
                runner=runner,
            )
            run(
                [
                    buildah,
                    "config",
                    "--user",
                    f"{APP_UID}:{APP_GID}",
                    "--workingdir",
                    "/app",
                    "--entrypoint",
                    '["/usr/local/bin/honenode"]',
                    "--stop-signal",
                    "SIGTERM",
                    "--env",
                    "HONE_BIND=0.0.0.0:8080",
                    "--env",
                    "HONE_BROKER_SOCKET=/run/honenode/broker.sock",
                    "--env",
                    "HONE_REGISTRY=/var/lib/honenode/registry/registry.db",
                    "--env",
                    "HONE_STATIC_DIR=/app/static",
                    "--env",
                    "HONE_TENANTS=",
                    "--env",
                    "HONE_SECURE_COOKIES=true",
                    "--env",
                    "HONE_LOG_FORMAT=json",
                    "--env",
                    "HONE_THEME=neutral",
                    "--port",
                    "8080/tcp",
                    "--label",
                    "org.opencontainers.image.title=HoneNode PoC",
                    "--label",
                    "org.opencontainers.image.description=Experimental app image; not a product release",
                    container,
                ],
                runner=runner,
            )
            run(
                [buildah, "commit", "--format", "oci", container, image],
                runner=runner,
            )
    finally:
        handling_failure = sys.exc_info()[0] is not None
        try:
            run([buildah, "rm", container], runner=runner)
        except (BuildError, OSError) as cleanup_error:
            if not handling_failure:
                raise
            print(f"warning: working-container cleanup failed: {cleanup_error}", file=sys.stderr)

    return image


def parse_args(argv: Sequence[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description=(
            "Build an experimental OCI image with Buildah from scratch. "
            "Inputs are not compiled and no base image is pulled."
        )
    )
    parser.add_argument("--binary", required=True, type=Path, help="prebuilt static honenode ELF")
    parser.add_argument("--css", required=True, type=Path, help="prebuilt/static hone.css")
    parser.add_argument("--image", default=DEFAULT_IMAGE, help=f"image reference (default: {DEFAULT_IMAGE})")
    return parser.parse_args(argv)


def main(argv: Sequence[str] | None = None) -> int:
    args = parse_args(argv)
    missing = [tool for tool in ("buildah", "readelf") if shutil.which(tool) is None]
    if missing:
        print(f"error: required tools not found: {', '.join(missing)}", file=sys.stderr)
        return 2
    try:
        image = build_image(binary=args.binary, css=args.css, image=args.image)
    except (BuildError, OSError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    print(f"built experimental PoC image (not a release): {image}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
