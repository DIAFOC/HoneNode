#!/usr/bin/env python3
"""Install the HoneNode app and broker from a Fedora/Rocky evaluation bundle.

Run from the extracted bundle root:

    sudo python3 install.py

Defaults are deliberately conservative: the app binds to loopback, the broker
runs on the host, the OS bundle's own binaries are used, and an operator policy
allowing t01..t06 is installed. Use --dry-run first; use --yes for unattended
installs. This installer does not download HoneNode code or build anything.
"""

from __future__ import annotations

import argparse
import grp
import hashlib
import json
import os
import platform
import pwd
import re
import shlex
import shutil
import stat
import subprocess
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path

BUNDLE = Path(__file__).resolve().parent
MANIFEST = BUNDLE / "manifest.json"
SUMS = BUNDLE / "SHA256SUMS"
PREFIX = Path("/usr/local/libexec/honenode")
ETC = Path("/etc/honenode")
RUN = Path("/run/honenode")
STATE = Path("/var/lib/honenode")
REGISTRY = STATE / "registry"
HOSTING = Path("/srv/hosting")
ETC_HOSTING = Path("/etc/hosting")
TENANT_RE = re.compile(r"t[0-9]{2}")
SERVICE_USER = "honenode"
SERVICE_GROUP = "honenode"
PACKAGES = (
    "podman",
    "passt",
    "shadow-utils",
    "systemd-udev",
    "util-linux",
    "policycoreutils",
    "libcap",
)


class InstallError(Exception):
    pass


def say(message: str) -> None:
    print(message, flush=True)


def run(argv: list[str], *, check: bool = True, capture: bool = True) -> subprocess.CompletedProcess[str]:
    try:
        result = subprocess.run(argv, text=True, capture_output=True if capture else False)
    except FileNotFoundError as error:
        raise InstallError(f"required command not found: {argv[0]}") from error
    if check and result.returncode != 0:
        detail = (result.stderr or result.stdout or "").strip()
        raise InstallError(f"{' '.join(argv)} failed: {detail}")
    return result


def read_os_release() -> dict[str, str]:
    values: dict[str, str] = {}
    try:
        lines = Path("/etc/os-release").read_text().splitlines()
    except OSError as error:
        raise InstallError(f"cannot read /etc/os-release: {error}") from error
    for line in lines:
        line = line.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        key, value = line.split("=", 1)
        try:
            parsed = shlex.split(value)
        except ValueError:
            continue
        if parsed:
            values[key] = parsed[0]
    return values


def detect_os() -> str:
    release = read_os_release()
    identifier = release.get("ID", "")
    like = release.get("ID_LIKE", "")
    if identifier in ("fedora", "rocky"):
        return identifier
    if "fedora" in like.split():
        return "fedora"
    if "rocky" in like.split() or "rhel" in like.split():
        return "rocky"
    raise InstallError(
        f"unsupported host: ID={identifier or '?'} ID_LIKE={like or '?'}; "
        "this bundle supports Fedora and Rocky Linux"
    )


def verify_bundle(os_name: str) -> dict:
    if not MANIFEST.is_file() or MANIFEST.is_symlink():
        raise InstallError(f"missing regular {MANIFEST.name}; run this from the extracted bundle root")
    try:
        manifest = json.loads(MANIFEST.read_text())
    except (OSError, json.JSONDecodeError) as error:
        raise InstallError(f"invalid manifest.json: {error}") from error
    if manifest.get("name") != "honenode-os-evaluation" or manifest.get("schema_version") != 1:
        raise InstallError("unexpected bundle manifest")
    if manifest.get("os") != os_name:
        raise InstallError(
            f"bundle is tagged for {manifest.get('os')!r}, host is {os_name!r}; "
            "use the matching OS bundle"
        )
    if manifest.get("arch") != "x86_64" or platform.machine() not in ("x86_64", "amd64"):
        raise InstallError("this bundle is x86_64-only")
    if not SUMS.is_file() or SUMS.is_symlink():
        raise InstallError("missing regular SHA256SUMS")
    for line in SUMS.read_text().splitlines():
        if not line.strip():
            continue
        try:
            digest, relative = line.split("  ", 1)
        except ValueError as error:
            raise InstallError(f"invalid SHA256SUMS line: {line!r}") from error
        path = BUNDLE / relative
        try:
            info = path.lstat()
        except OSError as error:
            raise InstallError(f"missing bundle file {relative}: {error}") from error
        if stat.S_ISLNK(info.st_mode) or not stat.S_ISREG(info.st_mode):
            raise InstallError(f"bundle payload must be a regular file: {relative}")
        actual = hashlib.sha256(path.read_bytes()).hexdigest()
        if actual != digest:
            raise InstallError(f"checksum mismatch for {relative}")
    for required in (
        "native/honenode",
        "broker/honed",
        "native/static",
        "examples/policy.selfservice.json",
    ):
        path = BUNDLE / required
        if not path.exists():
            raise InstallError(f"bundle is missing {required}")
    return manifest


def parse_tenants(value: str) -> list[str]:
    tenants = [item.strip() for item in value.split(",") if item.strip()]
    if not tenants:
        raise InstallError("--tenants must contain at least one tenant id")
    if len(set(tenants)) != len(tenants):
        raise InstallError("--tenants contains duplicates")
    for tenant in tenants:
        if not TENANT_RE.fullmatch(tenant):
            raise InstallError(f"tenant {tenant!r} must look like t01, t02, ...")
    return tenants


def ensure_loopback(bind: str) -> str:
    host, separator, port = bind.rpartition(":")
    if not separator or not port.isdigit() or not 1 <= int(port) <= 65535:
        raise InstallError("--bind must be 127.0.0.1:PORT")
    if host not in ("127.0.0.1", "localhost", "[::1]", "::1"):
        raise InstallError("the installer only permits a loopback panel bind; use a reverse proxy for remote access")
    return bind


def missing_commands() -> list[str]:
    required = ("systemctl", "dnf", "podman", "useradd", "groupadd", "setcap", "restorecon", "newuidmap")
    return [name for name in required if shutil.which(name) is None]


def install_dependencies() -> None:
    say("installing host dependencies with dnf ...")
    run(["/usr/bin/dnf", "install", "-y", *PACKAGES], capture=False)
    setcap = shutil.which("setcap") or "/usr/sbin/setcap"
    for name, capability in (("newuidmap", "cap_setuid+ep"), ("newgidmap", "cap_setgid+ep")):
        path = Path("/usr/bin") / name
        if path.exists() and Path(setcap).exists():
            run([setcap, capability, str(path)], check=False)
            run(["/usr/bin/chmod", "u+s", str(path)], check=False)


def ensure_account() -> None:
    try:
        grp.getgrnam(SERVICE_GROUP)
    except KeyError:
        run(["/usr/sbin/groupadd", "--system", SERVICE_GROUP])
    try:
        pwd.getpwnam(SERVICE_USER)
    except KeyError:
        run(
            [
                "/usr/sbin/useradd",
                "--system",
                "--gid",
                SERVICE_GROUP,
                "--home-dir",
                str(STATE),
                "--no-create-home",
                "--shell",
                "/sbin/nologin",
                SERVICE_USER,
            ]
        )
    else:
        if pwd.getpwnam(SERVICE_USER).pw_gid != grp.getgrnam(SERVICE_GROUP).gr_gid:
            run(["/usr/sbin/usermod", "-aG", SERVICE_GROUP, SERVICE_USER], check=False)


def ensure_directories() -> None:
    for path, mode, group in (
        (STATE, 0o750, SERVICE_GROUP),
        (REGISTRY, 0o750, SERVICE_GROUP),
        (ETC, 0o750, SERVICE_GROUP),
        (RUN, 0o750, SERVICE_GROUP),
        (ETC_HOSTING, 0o750, SERVICE_GROUP),
        (ETC_HOSTING / "caddy", 0o750, SERVICE_GROUP),
        (Path("/etc/containers/systemd/users"), 0o755, None),
        (HOSTING, 0o750, SERVICE_GROUP),
    ):
        path.mkdir(parents=True, exist_ok=True)
        os.chmod(path, mode)
        if group:
            gid = grp.getgrnam(group).gr_gid
            os.chown(path, 0, gid)
    os.chown(REGISTRY, pwd.getpwnam(SERVICE_USER).pw_uid, grp.getgrnam(SERVICE_GROUP).gr_gid)


def copy_regular(source: Path, destination: Path, mode: int) -> None:
    info = source.lstat()
    if stat.S_ISLNK(info.st_mode) or not stat.S_ISREG(info.st_mode):
        raise InstallError(f"refusing non-regular source {source}")
    destination.parent.mkdir(parents=True, exist_ok=True)
    descriptor = os.open(destination, os.O_WRONLY | os.O_CREAT | os.O_TRUNC | os.O_NOFOLLOW, mode)
    with os.fdopen(descriptor, "wb") as target:
        target.write(source.read_bytes())
        target.flush()
        os.fsync(target.fileno())
    os.chmod(destination, mode)


def install_payload() -> None:
    PREFIX.mkdir(parents=True, exist_ok=True)
    os.chmod(PREFIX, 0o755)
    copy_regular(BUNDLE / "native/honenode", PREFIX / "honenode", 0o755)
    copy_regular(BUNDLE / "broker/honed", PREFIX / "honed", 0o755)
    static_src = BUNDLE / "native/static"
    static_dst = PREFIX / "static"
    if static_dst.exists():
        shutil.rmtree(static_dst)
    for root, directories, names in os.walk(static_src, followlinks=False):
        root_path = Path(root)
        relative = root_path.relative_to(static_src)
        target_root = static_dst / relative
        target_root.mkdir(parents=True, exist_ok=True)
        os.chmod(target_root, 0o755)
        for name in directories:
            if (root_path / name).is_symlink():
                raise InstallError(f"static tree contains a symlink: {root_path / name}")
        for name in names:
            source = root_path / name
            if source.is_symlink() or not source.is_file():
                raise InstallError(f"static tree contains a non-regular file: {source}")
            copy_regular(source, target_root / name, 0o644)


def write_file(path: Path, content: str, mode: int = 0o644, group: str | None = None) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(f".{path.name}.tmp")
    temporary.write_text(content)
    os.chmod(temporary, mode)
    if group:
        os.chown(temporary, 0, grp.getgrnam(group).gr_gid)
    os.replace(temporary, path)


def install_config(tenants: list[str], bind: str, domain: str, reset_policy: bool) -> None:
    tenant_value = ",".join(tenants)
    policy = ETC / "policy.json"
    if reset_policy or not policy.exists():
        write_file(policy, (BUNDLE / "examples/policy.selfservice.json").read_text(), 0o644)
    write_file(
        ETC / "honed.env",
        "# Generated by install.py; review before exposing anything beyond loopback.\n"
        f"HONE_TENANTS={tenant_value}\n"
        f"HONE_TENANT_DOMAIN={domain}\n"
        "# HONE_IMAGE_NGINX=...\n# HONE_IMAGE_POSTGRES=...\n# HONE_SUBID_BASE=100000\n",
        0o640,
        SERVICE_GROUP,
    )
    write_file(
        ETC / "app.env",
        f"HONE_BIND={bind}\nHONE_TENANTS={tenant_value}\n",
        0o640,
        SERVICE_GROUP,
    )


def broker_unit(tenants: list[str], domain: str) -> str:
    tenant_value = ",".join(tenants)
    return f"""# Generated by install.py; review before editing.
[Unit]
Description=HoneNode host broker
After=local-fs.target

[Service]
Type=simple
User=root
Group={SERVICE_GROUP}
Environment=PATH=/usr/sbin:/usr/bin:/sbin:/bin
Environment=HONE_BROKER_SOCKET=/run/honenode/broker.sock
Environment=HONE_POLICY_FILE=/etc/honenode/policy.json
Environment=HONE_TENANTS={tenant_value}
Environment=HONE_SUBID_COUNT=65536
Environment=HONE_QUADLET_USERS=/etc/containers/systemd/users
Environment=HONE_TENANT_DATA=/srv/hosting
Environment=HONE_ETC=/etc/hosting
Environment=HONE_CADDY_DIR=/etc/hosting/caddy
Environment=HONE_CADDY_ADMIN=127.0.0.1:2019
Environment=HONE_TENANT_DOMAIN={domain}
Environment=HONE_SERVICE_ACCESS=loopback
EnvironmentFile=-/etc/honenode/honed.env
ExecStart={PREFIX}/honed
RuntimeDirectory=honenode
RuntimeDirectoryMode=0750
UMask=0027
Restart=on-failure
RestartSec=2
NoNewPrivileges=true

[Install]
WantedBy=multi-user.target
"""


def app_unit(tenants: list[str], bind: str, domain: str) -> str:
    tenant_value = ",".join(tenants)
    return f"""# Generated by install.py; review before editing.
[Unit]
Description=HoneNode management plane
After=honed.service
Wants=honed.service

[Service]
Type=simple
User={SERVICE_USER}
Group={SERVICE_GROUP}
Environment=HONE_BIND={bind}
Environment=HONE_BROKER_SOCKET=/run/honenode/broker.sock
Environment=HONE_REGISTRY=/var/lib/honenode/registry/registry.db
Environment=HONE_STATIC_DIR={PREFIX}/static
Environment=HONE_TENANTS={tenant_value}
Environment=HONE_TENANT_DATA=/srv/hosting
Environment=HONE_PANEL_DOMAIN=localhost
Environment=HONE_TENANT_DOMAIN={domain}
Environment=HONE_SECURE_COOKIES=false
Environment=HONE_API_TOKENS=
Environment=HONE_THEME=sharpnode
Environment=HONE_SERVICE_ACCESS=loopback
Environment=HONE_LOG_FORMAT=json
EnvironmentFile=-/etc/honenode/app.env
ExecStart={PREFIX}/honenode
Restart=on-failure
RestartSec=2
NoNewPrivileges=true
PrivateTmp=true
ProtectSystem=full
ProtectHome=true
ReadWritePaths=/var/lib/honenode

[Install]
WantedBy=multi-user.target
"""


def install_units(tenants: list[str], bind: str, domain: str) -> None:
    write_file(Path("/etc/systemd/system/honed.service"), broker_unit(tenants, domain), 0o644)
    write_file(Path("/etc/systemd/system/honenode.service"), app_unit(tenants, bind, domain), 0o644)


def selinux_relabel() -> None:
    if shutil.which("restorecon") is None:
        return
    if shutil.which("getenforce") and run(["/usr/sbin/getenforce"], check=False).stdout.strip() == "Enforcing":
        run(["/usr/sbin/restorecon", "-RF", str(PREFIX), str(ETC), str(RUN), str(STATE), str(ETC_HOSTING), str(HOSTING)], check=False)


def wait_for(path_or_url: str, timeout: int, *, url: bool = False) -> bool:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if url:
            try:
                with urllib.request.urlopen(path_or_url, timeout=2) as response:
                    if 200 <= response.status < 300:
                        return True
            except (urllib.error.URLError, OSError):
                pass
        elif Path(path_or_url).exists():
            return True
        time.sleep(1)
    return False


def start_services(bind: str) -> None:
    run(["/usr/bin/systemctl", "daemon-reload"])
    run(["/usr/bin/systemctl", "enable", "honed.service"])
    run(["/usr/bin/systemctl", "restart", "honed.service"])
    if not wait_for("/run/honenode/broker.sock", 20):
        raise InstallError("broker socket did not appear; inspect: journalctl -u honed.service -n 80 --no-pager")
    run(["/usr/bin/systemctl", "enable", "honenode.service"])
    run(["/usr/bin/systemctl", "restart", "honenode.service"])
    if not wait_for(f"http://{bind}/healthz", 30, url=True):
        raise InstallError("app health endpoint did not answer; inspect: journalctl -u honenode.service -n 80 --no-pager")


def show_credentials(keep: bool) -> None:
    path = REGISTRY / "bootstrap-credentials.txt"
    if not path.exists():
        say("operator already exists; no new bootstrap credentials were generated")
        return
    values: dict[str, str] = {}
    for line in path.read_text().splitlines():
        if "=" in line and not line.startswith("#"):
            key, value = line.split("=", 1)
            values[key] = value
    account = values.get("HONE_OPERATOR_ACCOUNT", "")
    password = values.get("HONE_OPERATOR_PASSWORD", "")
    if not account or not password:
        raise InstallError(f"cannot parse {path}")
    say("")
    say("=" * 64)
    say("  OPERATOR CREDENTIALS — shown now; save them before continuing")
    say(f"  account:  {account}")
    say(f"  password: {password}")
    say("=" * 64)
    if not keep:
        path.unlink()
        say("removed the static bootstrap file; these credentials will not be shown again")


def uninstall(*, purge: bool) -> None:
    for unit in ("honenode.service", "honed.service"):
        run(["/usr/bin/systemctl", "disable", "--now", unit], check=False)
    for unit in (Path("/etc/systemd/system/honenode.service"), Path("/etc/systemd/system/honed.service")):
        if unit.exists():
            unit.unlink()
    run(["/usr/bin/systemctl", "daemon-reload"], check=False)
    if PREFIX.exists():
        shutil.rmtree(PREFIX)
    if purge:
        for path in (ETC, STATE, RUN, ETC_HOSTING, HOSTING):
            if path.exists():
                shutil.rmtree(path)
        for name in (SERVICE_USER,):
            run(["/usr/sbin/userdel", name], check=False)
        run(["/usr/sbin/groupdel", SERVICE_GROUP], check=False)
        say("purged binaries, configuration, state and hosting data")
    else:
        say("removed binaries and units; configuration, state and tenant data were preserved")


def plan(os_name: str, manifest: dict, tenants: list[str], bind: str, domain: str, prefix: Path) -> None:
    say(f"HoneNode {manifest.get('version')} · {os_name} bundle")
    say(f"install prefix: {prefix}")
    say(f"service user:   {SERVICE_USER} (system group {SERVICE_GROUP})")
    say(f"state:          {STATE} (registry owned by {SERVICE_USER})")
    say(f"panel:          http://{bind} (loopback only, non-secure cookies)")
    say(f"tenants:        {','.join(tenants)}")
    say(f"tenant domain:  {domain}")
    say("packages:       " + (", ".join(PACKAGES)))
    say("units:          honed.service, honenode.service")


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description="Install HoneNode from an OS-tagged evaluation bundle.")
    parser.add_argument("--yes", action="store_true", help="do not prompt")
    parser.add_argument("--dry-run", action="store_true", help="verify and print the plan only")
    parser.add_argument("--force", action="store_true", help="overwrite binaries/units but preserve state")
    parser.add_argument("--reset-policy", action="store_true", help="replace /etc/honenode/policy.json")
    parser.add_argument("--keep-bootstrap", action="store_true", help="keep the mode-0600 credentials file")
    parser.add_argument("--install-deps", action="store_true", help="install missing dnf dependencies")
    parser.add_argument("--no-start", action="store_true", help="install files/units but do not start services")
    parser.add_argument("--tenants", default="t01,t02,t03,t04,t05,t06")
    parser.add_argument("--bind", default="127.0.0.1:8088")
    parser.add_argument("--tenant-domain", default="tenants.honenode.lab")
    parser.add_argument("--uninstall", action="store_true")
    parser.add_argument("--purge", action="store_true", help="with --uninstall: remove config, state and tenant data")
    args = parser.parse_args(argv)

    os_name = detect_os()
    manifest = verify_bundle(os_name)
    tenants = parse_tenants(args.tenants)
    bind = ensure_loopback(args.bind)
    if not re.fullmatch(r"[A-Za-z0-9.-]+", args.tenant_domain):
        raise InstallError("--tenant-domain must be a plain DNS name")
    if os.geteuid() != 0 and not args.dry_run and not args.uninstall:
        raise InstallError("run the installer as root (sudo python3 install.py)")
    if args.uninstall:
        if os.geteuid() != 0:
            raise InstallError("run --uninstall as root")
        uninstall(purge=args.purge)
        return 0

    missing = missing_commands()
    plan(os_name, manifest, tenants, bind, args.tenant_domain, PREFIX)
    if missing:
        say("missing commands: " + ", ".join(missing))
    if args.dry_run:
        if missing and not args.install_deps:
            say("dry-run: add --install-deps or install the missing packages first")
        say("dry-run complete; nothing was written or started")
        return 0

    if Path("/etc/systemd/system/honenode.service").exists() and not args.force:
        raise InstallError(
            "an install already exists; use --force to replace binaries/units (state is preserved) "
            "or --uninstall to remove it"
        )
    if not args.yes:
        answer = input("Proceed with the install? [y/N] ").strip().lower()
        if answer not in ("y", "yes"):
            say("aborted")
            return 1
    if missing:
        if args.install_deps:
            install_dependencies()
        else:
            raise InstallError("install missing packages, or rerun with --install-deps")

    ensure_account()
    ensure_directories()
    install_payload()
    install_config(tenants, bind, args.tenant_domain, args.reset_policy)
    install_units(tenants, bind, args.tenant_domain)
    selinux_relabel()
    if args.no_start:
        say("install files written; services were not started (--no-start)")
        return 0
    start_services(bind)
    show_credentials(args.keep_bootstrap)
    say("")
    say(f"open http://{bind}/ and create the first tenant from /signup")
    say("the broker and app are enabled at boot; inspect with:")
    say("  systemctl status honed.service honenode.service")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main(sys.argv[1:]))
    except InstallError as error:
        print(f"error: {error}", file=sys.stderr)
        raise SystemExit(1)
