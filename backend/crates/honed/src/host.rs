use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use crate::capacity::{MemorySize, Reservation};
use crate::config::Config;
use crate::service_config::{ServiceConfiguration, ServiceOptions, StoragePlan, WriteMode};

const COMMAND_TIMEOUT: Duration = Duration::from_secs(20);

pub fn run(program: &str, args: &[&str]) -> Result<String, String> {
    let mut command = Command::new(program);
    command.args(args);
    let output = command_output(&mut command, program, args)?;
    command_result(program, args, output)
}

pub fn run_as(user: &str, program: &str, args: &[&str]) -> Result<String, String> {
    run_as_with_env(user, program, args, &[])
}

fn run_as_with_env(
    user: &str,
    program: &str,
    args: &[&str],
    environment: &[(&str, &str)],
) -> Result<String, String> {
    let output = run_as_output(user, program, args, environment)?;
    command_result(program, args, output)
}

fn run_as_output(
    user: &str,
    program: &str,
    args: &[&str],
    environment: &[(&str, &str)],
) -> Result<Output, String> {
    let uid = user_uid(user)?;
    let gid = user_gid(user)?;
    let groups = user_groups(user)?;
    let home = user_home(user)?;

    let mut command = Command::new(program);
    command
        .args(args)
        .env("XDG_RUNTIME_DIR", format!("/run/user/{uid}"))
        .env("HOME", &home)
        .current_dir(&home);
    for (key, value) in environment {
        command.env(key, value);
    }
    // SAFETY: NSS lookups and allocations happen before fork. The child only
    // invokes async-signal-safe privilege syscalls and constructs an OS error.
    unsafe {
        command.pre_exec(move || {
            if libc::setgroups(groups.len(), groups.as_ptr()) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::setgid(gid) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::setuid(uid) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command_output(&mut command, program, args)
}

fn command_result(program: &str, args: &[&str], output: Output) -> Result<String, String> {
    command_result_inner(program, args, output, true)
}

fn command_result_raw(program: &str, args: &[&str], output: Output) -> Result<String, String> {
    command_result_inner(program, args, output, false)
}

fn command_result_inner(
    program: &str,
    args: &[&str],
    output: Output,
    trim_stdout: bool,
) -> Result<String, String> {
    if output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        Ok(if trim_stdout {
            stdout.trim().to_string()
        } else {
            stdout.into_owned()
        })
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        Err(format!("{program} {}: {stderr}", args.join(" ")))
    }
}

fn command_output(command: &mut Command, program: &str, args: &[&str]) -> Result<Output, String> {
    command_output_bounded(command, COMMAND_TIMEOUT, 1024 * 1024, 128 * 1024)
        .map_err(|error| format!("{program} {}: {error}", args.join(" ")))
}

fn command_output_bounded(
    command: &mut Command,
    timeout: Duration,
    stdout_cap: usize,
    stderr_cap: usize,
) -> Result<Output, String> {
    let deadline = Instant::now() + timeout;
    command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut child = command.spawn().map_err(|error| error.to_string())?;
    let outcome = (|| {
        let mut stdout = child.stdout.take().ok_or("cannot capture stdout")?;
        let mut stderr = child.stderr.take().ok_or("cannot capture stderr")?;
        for fd in [stdout.as_raw_fd(), stderr.as_raw_fd()] {
            // SAFETY: both descriptors remain owned and open for this loop.
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
            {
                return Err(std::io::Error::last_os_error().to_string());
            }
        }
        let (mut stdout_bytes, mut stderr_bytes) = (Vec::new(), Vec::new());
        let (mut stdout_eof, mut stderr_eof) = (false, false);
        let mut status = None;
        loop {
            if Instant::now() >= deadline {
                return Err("command/output timed out".to_string());
            }
            if !stdout_eof {
                stdout_eof = read_pipe_chunk(&mut stdout, &mut stdout_bytes, stdout_cap, "stdout")?;
            }
            if !stderr_eof {
                stderr_eof = read_pipe_chunk(&mut stderr, &mut stderr_bytes, stderr_cap, "stderr")?;
            }
            if status.is_none() {
                status = child.try_wait().map_err(|error| error.to_string())?;
            }
            if stdout_eof && stderr_eof {
                if let Some(status) = status {
                    return Ok(Output {
                        status,
                        stdout: stdout_bytes,
                        stderr: stderr_bytes,
                    });
                }
            }
            let mut descriptors = [
                libc::pollfd {
                    fd: if stdout_eof { -1 } else { stdout.as_raw_fd() },
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: if stderr_eof { -1 } else { stderr.as_raw_fd() },
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            let wait_ms = deadline
                .saturating_duration_since(Instant::now())
                .as_millis()
                .min(20) as i32;
            // SAFETY: descriptors is a valid, initialized array for the call.
            if unsafe {
                libc::poll(
                    descriptors.as_mut_ptr(),
                    descriptors.len() as libc::nfds_t,
                    wait_ms,
                )
            } < 0
            {
                let error = std::io::Error::last_os_error();
                if error.kind() != std::io::ErrorKind::Interrupted {
                    return Err(error.to_string());
                }
            }
        }
    })();
    if outcome.is_err() {
        kill_command_group(&mut child);
    }
    outcome
}

fn read_pipe_chunk(
    pipe: &mut impl Read,
    bytes: &mut Vec<u8>,
    cap: usize,
    name: &str,
) -> Result<bool, String> {
    let mut chunk = [0_u8; 8192];
    match pipe.read(&mut chunk) {
        Ok(0) => Ok(true),
        Ok(count) => {
            if count > cap.saturating_sub(bytes.len()) {
                return Err(format!("{name} output exceeds {cap} byte limit"));
            }
            bytes.extend_from_slice(&chunk[..count]);
            Ok(false)
        }
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
            ) =>
        {
            Ok(false)
        }
        Err(error) => Err(format!("cannot read {name}: {error}")),
    }
}

fn kill_command_group(child: &mut Child) {
    // Kill descendants even when the leader exited but left inherited pipes.
    // Never block cleanup indefinitely on a process stuck in kernel I/O.
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
    let _ = child.kill();
    let deadline = Instant::now() + Duration::from_millis(100);
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => return,
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
        }
    }
    tracing::warn!(
        pid = child.id(),
        "killed command has not been reaped within cleanup deadline"
    );
}

pub fn user_uid(user: &str) -> Result<u32, String> {
    run("id", &["-u", user])?
        .parse()
        .map_err(|_| format!("cannot read the uid of {user}"))
}

pub fn user_gid(user: &str) -> Result<u32, String> {
    run("id", &["-g", user])?
        .parse()
        .map_err(|_| format!("cannot read the gid of {user}"))
}

fn user_groups(user: &str) -> Result<Vec<u32>, String> {
    run("id", &["-G", user])?
        .split_whitespace()
        .map(|group| {
            group
                .parse()
                .map_err(|_| format!("cannot read the supplementary groups of {user}"))
        })
        .collect()
}

pub fn user_home(user: &str) -> Result<String, String> {
    run("getent", &["passwd", user])?
        .split(':')
        .nth(5)
        .map(str::to_string)
        .ok_or_else(|| format!("cannot read the home of {user}"))
}

pub fn user_exists(user: &str) -> bool {
    run("id", &["-u", user]).is_ok()
}

pub fn ensure_subid(user: &str, file: &str, flag: &str, config: &Config) -> Result<(), String> {
    let count = config.subid_count;
    let mut start: u64 = config.subid_base;
    let contents = std::fs::read_to_string(file).unwrap_or_default();
    if contents
        .lines()
        .any(|line| line.split(':').next() == Some(user))
    {
        return Ok(());
    }

    let mut ranges: Vec<(u64, u64)> = contents
        .lines()
        .filter_map(|line| {
            let mut fields = line.split(':');
            fields.next()?;
            let start = fields.next()?.parse().ok()?;
            let count = fields.next()?.parse().ok()?;
            Some((start, count))
        })
        .collect();
    ranges.sort_unstable();

    for (range_start, range_count) in ranges {
        if start + count as u64 <= range_start {
            break;
        }
        if range_start + range_count > start {
            start = range_start + range_count;
        }
    }

    let range = format!("{start}-{}", start + count as u64 - 1);
    run("usermod", &[flag, &range, user])?;
    Ok(())
}

fn ensure_user_manager(
    user: &str,
    uid: u32,
    mut run: impl FnMut(&str, &[&str]) -> Result<String, String>,
) -> Result<(), String> {
    run("loginctl", &["enable-linger", user])?;
    // Enabling linger can return before the user manager is ready for daemon-reload.
    let unit = format!("user@{uid}.service");
    run("systemctl", &["start", &unit])?;
    Ok(())
}

pub fn tenant_create(config: &Config, slug: &str) -> Result<String, String> {
    // Account creation is repeatable; service secrets are handled separately with create-new semantics.
    crate::provisioning::ensure_tenant_root(config)?;
    let already_existed = user_exists(slug);

    if !already_existed {
        let comment = format!("SharpNode tenant {slug}");
        let mut args = vec![
            "--create-home",
            "--shell",
            "/sbin/nologin",
            "--comment",
            comment.as_str(),
        ];
        if run("getent", &["group", slug]).is_ok() {
            args.push("--gid");
            args.push(slug);
        }
        args.push(slug);
        run("useradd", &args)?;
    }

    // The tenant's user-manager journal is only readable by the
    // systemd-journal group; add the tenant so the panel's log view works.
    if run("getent", &["group", "systemd-journal"]).is_ok() {
        run("usermod", &["-aG", "systemd-journal", slug])?;
    }

    ensure_subid(slug, "/etc/subuid", "--add-subuids", config)?;
    ensure_subid(slug, "/etc/subgid", "--add-subgids", config)?;
    let uid = user_uid(slug)?;
    ensure_user_manager(slug, uid, run)?;

    let root = crate::provisioning::ensure_tenant_root(config)?;
    let gid = user_gid(slug)?;
    crate::provisioning::ensure_directory(&root, slug, uid, gid, 0o750)?;

    if already_existed {
        Ok(format!(
            "account {slug} (uid {uid}) already present, linger ensured"
        ))
    } else {
        Ok(format!(
            "account {slug} (uid {uid}) created, linger enabled"
        ))
    }
}

pub fn tenant_delete(_config: &Config, _slug: &str) -> Result<String, String> {
    Err("tenant.delete disabled: retention and independently authorized confirmation policy is not implemented".to_string())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ServiceKind {
    Web,
    Database,
    Storage,
    Git,
    Container,
}

impl ServiceKind {
    pub(crate) fn parse(value: &str) -> Result<Self, String> {
        match value {
            "web" => Ok(Self::Web),
            "database" => Ok(Self::Database),
            "storage" => Ok(Self::Storage),
            "git" => Ok(Self::Git),
            "container" => Ok(Self::Container),
            _ => Err(format!("unknown service kind {value:?}")),
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Web => "web",
            Self::Database => "database",
            Self::Storage => "storage",
            Self::Git => "git",
            Self::Container => "container",
        }
    }

    fn http_port(self, tenant_number: u32) -> Option<u32> {
        let base = match self {
            Self::Web => 18_000,
            Self::Storage => 19_000,
            Self::Git => 20_000,
            Self::Database | Self::Container => return None,
        };
        Some(base + tenant_number)
    }
}

pub struct Envelope {
    pub kind: ServiceKind,
    pub cpu_weight: i64,
    pub memory_high: MemorySize,
    pub memory_max: MemorySize,
    pub tasks_max: i64,
    pub cpu_quota_percent: u32,
    pub plan_code: Option<String>,
}

impl Envelope {
    pub fn new(
        kind: &str,
        cpu_weight: i64,
        memory_high: &str,
        memory_max: &str,
        tasks_max: i64,
    ) -> Result<Self, String> {
        let kind = ServiceKind::parse(kind)?;
        let memory_high = MemorySize::parse(memory_high)?;
        let memory_max = MemorySize::parse(memory_max)?;

        if !(1..=10_000).contains(&cpu_weight) {
            return Err("cpu_weight must be between 1 and 10000".to_string());
        }
        if !(1..=1_000_000).contains(&tasks_max) {
            return Err("tasks_max must be between 1 and 1000000".to_string());
        }
        if memory_high.as_bytes() == 0 || memory_max.as_bytes() == 0 {
            return Err("memory limits must be greater than zero".to_string());
        }
        if memory_high > memory_max {
            return Err("memory_high cannot exceed memory_max".to_string());
        }

        Ok(Self {
            kind,
            cpu_weight,
            memory_high,
            memory_max,
            tasks_max,
            cpu_quota_percent: 100,
            plan_code: None,
        })
    }
}

pub fn committed_reservations(config: &Config) -> Result<Vec<Reservation>, String> {
    let mut reservations = discover_reservations(&config.quadlet_users, validate_root_owned_entry)?;
    crate::containers::merge_reservations(config, &mut reservations)?;
    Ok(reservations)
}

fn discover_reservations(
    root: &Path,
    validate: fn(&Path, bool) -> Result<(), String>,
) -> Result<Vec<Reservation>, String> {
    // Authorization and NSS are deliberately absent: removed or unresolvable
    // tenants must continue reserving their CPU and memory after a restart.
    validate(root, true)?;
    let mut reservations = Vec::new();
    for directory in std::fs::read_dir(root).map_err(|error| error.to_string())? {
        let directory = directory.map_err(|error| error.to_string())?;
        let name = directory.file_name();
        let name = name.to_string_lossy();
        if name.parse::<u32>().is_err() {
            continue;
        }
        let directory = directory.path();
        validate(&directory, true)?;
        for entry in std::fs::read_dir(&directory).map_err(|error| error.to_string())? {
            let path = entry.map_err(|error| error.to_string())?.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("container") {
                continue;
            }
            validate(&path, false)?;
            let body = std::fs::read_to_string(&path)
                .map_err(|error| format!("{}: {error}", path.display()))?;
            let unit = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .ok_or("invalid Quadlet filename")?;
            if let Some(reservation) = discovered_reservation(unit, &body).map_err(|error| {
                format!("invalid committed reservation {}: {error}", path.display())
            })? {
                reservations.push(reservation);
            }
        }
    }
    Ok(reservations)
}

fn discovered_reservation(unit: &str, body: &str) -> Result<Option<Reservation>, String> {
    let legacy_tenant = unit
        .split_once('-')
        .map(|(tenant, _)| tenant)
        .filter(|tenant| crate::shape_is_valid(tenant));
    let marked = body.lines().any(|line| line.starts_with("# Honed"));
    if !marked && legacy_tenant.is_none() {
        return Ok(None);
    }
    let tenant = optional_unit_directive(body, "# HonedTenant=")?
        .or(legacy_tenant)
        .ok_or("marked broker unit has no resolvable tenant identity")?;
    if !crate::shape_is_valid(tenant) {
        return Err("invalid managed tenant identity".to_string());
    }
    crate::validate_unit_name(tenant, unit)?;
    // Markers and the reserved tNN-* legacy namespace distinguish managed
    // workload reservations from unrelated platform units in other UID dirs.
    parse_reservation(tenant, unit, body).map(Some)
}

pub(crate) fn parse_reservation(tenant: &str, unit: &str, body: &str) -> Result<Reservation, String> {
    let mut memory_max = None;
    for line in body.lines() {
        if let Some(value) = line.strip_prefix("MemoryMax=") {
            if memory_max.is_some() {
                return Err("duplicate MemoryMax directive".to_string());
            }
            memory_max = Some(MemorySize::parse(value)?);
        }
    }

    let memory_max = memory_max.ok_or_else(|| "missing MemoryMax directive".to_string())?;
    if memory_max.as_bytes() == 0 {
        return Err("MemoryMax must be greater than zero".to_string());
    }
    let plan_code = optional_unit_directive(body, "# HonedPlan=")?.map(str::to_string);
    if plan_code
        .as_deref()
        .is_some_and(|code| !crate::policy::valid_code(code))
    {
        return Err("invalid unit plan reservation".to_string());
    }
    let kind = optional_unit_directive(body, "# HonedKind=")?
        .map(ServiceKind::parse)
        .transpose()?;
    let cpu_quota_percent = optional_unit_directive(body, "CPUQuota=")?
        .map(|value| {
            let quota = value
                .strip_suffix('%')
                .ok_or_else(|| "CPUQuota must be an integer percent".to_string())?
                .parse::<u32>()
                .map_err(|_| "invalid unit CPUQuota".to_string())?;
            if quota == 0 {
                return Err("CPUQuota must be positive".to_string());
            }
            Ok(quota)
        })
        .transpose()?;
    Ok(Reservation {
        tenant: tenant.to_string(),
        unit: unit.to_string(),
        memory_max,
        plan_code,
        cpu_quota_percent,
        kind,
    })
}

fn optional_unit_directive<'a>(body: &'a str, prefix: &str) -> Result<Option<&'a str>, String> {
    let mut values = body.lines().filter_map(|line| line.strip_prefix(prefix));
    let value = values.next();
    if values.next().is_some() {
        return Err(format!("duplicate unit directive {prefix}"));
    }
    Ok(value)
}

pub(crate) fn unit_directive<'a>(body: &'a str, prefix: &str) -> Result<&'a str, String> {
    optional_unit_directive(body, prefix)?.ok_or_else(|| format!("missing unit directive {prefix}"))
}

pub(crate) fn ensure_quadlet_dir(config: &Config, slug: &str) -> Result<PathBuf, String> {
    let uid = user_uid(slug)?;
    let dir = config.quadlet_users.join(uid.to_string());
    std::fs::create_dir_all(&dir).map_err(|error| format!("{}: {error}", dir.display()))?;
    std::os::unix::fs::chown(&dir, Some(0), Some(0))
        .map_err(|error| format!("chown {}: {error}", dir.display()))?;
    std::fs::set_permissions(&dir, std::os::unix::fs::PermissionsExt::from_mode(0o755))
        .map_err(|error| format!("chmod {}: {error}", dir.display()))?;
    Ok(dir)
}

/// Compatibility wrapper for option-less callers (web/database behaviour).
#[allow(dead_code)]
pub fn write_unit(
    config: &Config,
    slug: &str,
    unit: &str,
    envelope: &Envelope,
) -> Result<PathBuf, String> {
    write_unit_with_options(config, slug, unit, envelope, None, WriteMode::Provision, None)
        .map(|(path, _)| path)
}

/// Validates options before any host mutation, then provisions, renders,
/// verifies, and installs the unit. Storage credentials are created exclusively
/// on first provisioning and rolled back if this request fails to install.
pub fn write_unit_with_options(
    config: &Config,
    slug: &str,
    unit: &str,
    envelope: &Envelope,
    options: Option<&ServiceOptions>,
    mode: WriteMode,
    port: Option<u16>,
) -> Result<(PathBuf, ServiceConfiguration), String> {
    if envelope.plan_code.is_none() || envelope.cpu_quota_percent == 0 {
        return Err("broker policy plan and CPUQuota are required".to_string());
    }
    tracing::info!(unit, kind = envelope.kind.as_str(), "write_unit_with_options");
    if envelope.kind == ServiceKind::Container || crate::containers::exists(config, unit)? {
        return Err("generic containers require container.create and immutable names".to_string());
    }
    let instance = instance_from_unit(slug, envelope.kind, unit)?;
    crate::service_config::validate_options(config, envelope.kind, options, mode)?;
    ensure_limit_update_stopped(config, slug, unit)?;
    let existing_read_only = match existing_unit_body(config, slug, unit)? {
        Some(body)
            if optional_unit_directive(&body, "# HonedKind=")?
                == Some(ServiceKind::Storage.as_str()) =>
        {
            Some(crate::service_config::read_only_marker(
                &body,
                ServiceKind::Storage,
            )?)
        }
        _ => None,
    };
    if mode == WriteMode::Configure && existing_read_only.is_none() {
        return Err("service.configure requires an existing storage unit".to_string());
    }
    let storage = if envelope.kind == ServiceKind::Storage {
        Some(crate::service_config::plan_storage(
            config,
            slug,
            user_uid(slug)?,
            user_gid(slug)?,
            options,
            existing_read_only,
            mode,
            instance,
        )?)
    } else {
        None
    };

    let database_password = options
        .and_then(|options| options.database.as_ref())
        .map(|database| database.password.expose());
    crate::provisioning::setup_service(
        config,
        slug,
        envelope.kind,
        instance,
        database_password,
    )?;
    let created = match &storage {
        Some(StoragePlan::Create { username, hash, .. }) => {
            crate::service_config::create_settings(
                config,
                slug,
                user_gid(slug)?,
                username,
                hash,
                instance,
            )?;
            true
        }
        _ => false,
    };
    let read_only = storage.as_ref().is_some_and(StoragePlan::read_only);
    let installed = install_unit(config, slug, unit, envelope, read_only, port, instance);
    if installed.is_err() && created {
        crate::service_config::remove_created_settings(config, slug, instance);
    }
    let path = installed?;
    let configuration = ServiceConfiguration {
        kind: envelope.kind,
        image: config.image_for(envelope.kind).to_string(),
        username: storage.as_ref().map(|plan| plan.username().to_string()),
        read_only: storage.as_ref().map(StoragePlan::read_only),
    };
    Ok((path, configuration))
}

fn existing_unit_body(config: &Config, slug: &str, unit: &str) -> Result<Option<String>, String> {
    let uid = user_uid(slug)?;
    let path = config
        .quadlet_users
        .join(uid.to_string())
        .join(format!("{unit}.container"));
    match std::fs::symlink_metadata(&path) {
        Ok(_) => {
            let (_, path, _) = owned_unit(config, slug, unit)?;
            std::fs::read_to_string(&path)
                .map(Some)
                .map_err(|error| format!("{}: {error}", path.display()))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("{}: {error}", path.display())),
    }
}

fn install_unit(
    config: &Config,
    slug: &str,
    unit: &str,
    envelope: &Envelope,
    read_only: bool,
    port: Option<u16>,
    instance: u32,
) -> Result<PathBuf, String> {
    let dir = ensure_quadlet_dir(config, slug)?;

    let data = crate::provisioning::instance_data_root(config, slug, envelope.kind, instance);
    let body = render_unit_with(config, slug, unit, &data, envelope, read_only, port, instance)?;
    let path = dir.join(format!("{unit}.container"));
    let staging_dir = dir.join(".honed-staging");
    std::fs::create_dir_all(&staging_dir)
        .map_err(|error| format!("{}: {error}", staging_dir.display()))?;
    std::os::unix::fs::chown(&staging_dir, Some(0), Some(0))
        .map_err(|error| format!("chown {}: {error}", staging_dir.display()))?;
    std::fs::set_permissions(
        &staging_dir,
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .map_err(|error| format!("chmod {}: {error}", staging_dir.display()))?;
    let staging = staging_dir.join(format!("{unit}.container"));
    write_atomic(&staging, &body)?;
    let network = format!("snp-{slug}.network");
    if let Err(error) = std::fs::copy(dir.join(&network), staging_dir.join(&network)) {
        let _ = std::fs::remove_dir_all(&staging_dir);
        return Err(format!("cannot stage {network}: {error}"));
    }

    if let Err(error) = validate_unit(slug, &staging_dir, unit) {
        let _ = std::fs::remove_dir_all(&staging_dir);
        return Err(error);
    }
    if let Err(error) = std::fs::rename(&staging, &path) {
        let _ = std::fs::remove_dir_all(&staging_dir);
        return Err(format!("{}: {error}", path.display()));
    }
    let _ = std::fs::remove_dir_all(&staging_dir);
    Ok(path)
}

#[cfg(test)]
fn render_unit(
    config: &Config,
    slug: &str,
    unit: &str,
    data: &Path,
    envelope: &Envelope,
) -> Result<String, String> {
    render_unit_with(config, slug, unit, data, envelope, false, None, 1)
}

fn render_unit_with(
    config: &Config,
    slug: &str,
    unit: &str,
    data: &Path,
    envelope: &Envelope,
    storage_read_only: bool,
    port: Option<u16>,
    instance: u32,
) -> Result<String, String> {
    tenant_number(slug).ok_or_else(|| "invalid tenant_id".to_string())?;
    let ports = publish_ports(slug, envelope.kind, port, instance)?;
    let ports = format!("{}\n", ports.join("\n"));
    // Only storage carries this marker so web/database renders stay byte-identical.
    let markers = if envelope.kind == ServiceKind::Storage {
        format!(
            "{}{storage_read_only}\n",
            crate::service_config::STORAGE_READ_ONLY_MARKER
        )
    } else {
        String::new()
    };

    let (image, user, volumes, extra) = match envelope.kind {
        ServiceKind::Container => return Err("generic container requires committed metadata".to_string()),
        ServiceKind::Web => (
            config.image_nginx.as_str(),
            101,
            format!("Volume={}/site:/usr/share/nginx/html:ro,Z\n", data.display()),
            "ReadOnly=true\nTmpfs=/tmp:mode=1777\nTmpfs=/var/cache/nginx:mode=1777\n".to_string(),
        ),
        ServiceKind::Database => (
            config.image_postgres.as_str(),
            70,
            format!("Volume={}/pgdata:/var/lib/postgresql/data:Z\n", data.display()),
            format!(
                "EnvironmentFile={}/secrets/{}\nHealthCmd=pg_isready -U postgres\n",
                config.etc_hosting.display(),
                crate::provisioning::secret_file_name(slug, "db", instance)
            ),
        ),
        ServiceKind::Storage => (
            config.image_filebrowser.as_str(),
            1000,
            // The settings file (bcrypt hash) is a dedicated per-tenant file,
            // mounted read-only and never under the tenant-visible /srv tree.
            // No `:Z`: the broker labels it for containers itself, because the
            // tenant's rootless Podman cannot relabel a root-owned file.
            format!(
                "Volume={data}/vault:/srv:{vault}\n\
                 Volume={data}/db:/database:Z\n\
                 Volume={settings}:/config/settings.json:ro\n",
                data = data.display(),
                vault = if storage_read_only { "ro,Z" } else { "Z" },
                settings = crate::service_config::settings_path(config, slug, instance).display(),
            ),
            format!(
                "Entrypoint={}\n\
                 Exec={}\n\
                 DropCapability=all\n\
                 ReadOnly=true\n\
                 Tmpfs=/tmp:mode=1777\n",
                crate::service_config::FILEBROWSER_ENTRYPOINT,
                crate::service_config::FILEBROWSER_EXEC,
            ),
        ),
        ServiceKind::Git => {
            // Forgejo embeds its own URL in links and warns when the browser
            // reaches it through a different origin. Derive it from the same
            // access mode the panel publishes, so the loopback lab and public
            // subdomains both match.
            let http_port = git_http_port(slug, port, instance)?;
            let (domain, root_url) = match config.service_access {
                crate::config::ServiceAccess::Loopback => (
                    "127.0.0.1".to_string(),
                    format!("http://127.0.0.1:{http_port}/"),
                ),
                crate::config::ServiceAccess::Public => (
                    format!("{slug}.{}", config.tenant_domain),
                    format!("https://{slug}.{}/", config.tenant_domain),
                ),
            };
            (
                config.image_forgejo.as_str(),
                1000,
                format!(
                    "Volume={}/gitea:/var/lib/gitea:Z\nVolume={}/gitea-config:/etc/gitea:Z\n",
                    data.display(),
                    data.display()
                ),
                format!(
                    "EnvironmentFile={}/secrets/{}\n\
                     Environment=FORGEJO__server__DOMAIN={domain}\n\
                     Environment=FORGEJO__server__ROOT_URL={root_url}\n\
                     Environment=FORGEJO__server__SSH_PORT={}\n\
                     Environment=FORGEJO__server__SSH_LISTEN_PORT=2222\n\
                     Environment=FORGEJO__server__START_SSH_SERVER=true\n\
                     Environment=FORGEJO__security__INSTALL_LOCK=true\n",
                    config.etc_hosting.display(),
                    crate::provisioning::secret_file_name(slug, "forgejo", instance),
                    http_port + 2_000,
                    domain = domain,
                    root_url = root_url
                ),
            )
        }
    };

    Ok(format!(
        "# HonedTenant={slug}\n\
         # HonedPlan={plan_code}\n\
         # HonedKind={kind}\n\
         {markers}\
         [Unit]\n\
         Description=SharpNode tenant {slug} — {kind} ({unit})\n\n\
         [Container]\n\
         ContainerName={unit}\n\
         Image={image}\n\
         Network=snp-{slug}.network\n\
         {ports}\
         UserNS=keep-id:uid={user},gid={user}\n\
         User={user}\n\
         {volumes}\
         {extra}\
         NoNewPrivileges=true\n\
         PidsLimit={tasks_max}\n\
         LogDriver=journald\n\n\
         [Service]\n\
         Restart=always\n\
         MemoryHigh={memory_high}\n\
         MemoryMax={memory_max}\n\
         CPUWeight={cpu_weight}\n\
         CPUQuota={cpu_quota_percent}%\n\
         TasksMax={tasks_max}\n\
         IOWeight=100\n\n\
         [Install]\n\
         WantedBy=default.target\n",
        kind = envelope.kind.as_str(),
        plan_code = envelope.plan_code.as_deref().unwrap_or(""),
        cpu_quota_percent = envelope.cpu_quota_percent,
        memory_high = envelope.memory_high,
        memory_max = envelope.memory_max,
        cpu_weight = envelope.cpu_weight,
        tasks_max = envelope.tasks_max,
    ))
}

fn unit_suffix(kind: ServiceKind) -> &'static str {
    match kind {
        ServiceKind::Web => "site",
        ServiceKind::Database => "db",
        ServiceKind::Storage => "vault",
        ServiceKind::Git => "git",
        ServiceKind::Container => "container",
    }
}

/// Instance 1 keeps the legacy `{tenant}-{suffix}` unit name; later instances
/// append `-2`, `-3`, ... so the name, port and data directory stay unique.
pub(crate) fn instance_from_unit(slug: &str, kind: ServiceKind, unit: &str) -> Result<u32, String> {
    let prefix = format!("{slug}-{}", unit_suffix(kind));
    if unit == prefix {
        return Ok(1);
    }
    let rest = unit
        .strip_prefix(&format!("{prefix}-"))
        .ok_or_else(|| "unit name does not match its kind".to_string())?;
    let instance: u32 = rest
        .parse()
        .map_err(|_| "invalid instance suffix".to_string())?;
    if !(2..=99).contains(&instance) {
        return Err("invalid instance suffix".to_string());
    }
    Ok(instance)
}

/// First published loopback host port in a committed unit. Git publishes HTTP
/// first and SSH second; every other kind publishes one host port.
fn published_port(body: &str) -> Result<Option<u16>, String> {
    for line in body.lines() {
        if let Some(rest) = line.strip_prefix("PublishPort=127.0.0.1:") {
            if let Some(port) = rest
                .split(':')
                .next()
                .and_then(|value| value.parse::<u16>().ok())
            {
                return Ok(Some(port));
            }
        }
    }
    Ok(None)
}

fn publish_ports(
    slug: &str,
    kind: ServiceKind,
    port: Option<u16>,
    instance: u32,
) -> Result<Vec<String>, String> {
    let number = tenant_number(slug).ok_or_else(|| "invalid tenant_id".to_string())?;
    let offset = instance.saturating_sub(1) * 100;
    let default = |base: u32| base + number + offset;
    match kind {
        ServiceKind::Container => Err("generic container requires an allocated port".to_string()),
        ServiceKind::Database => Ok(vec![format!(
            "PublishPort=127.0.0.1:{}:5432",
            port.map(u32::from).unwrap_or_else(|| default(15_430))
        )]),
        ServiceKind::Git => {
            let http = port.map(u32::from).unwrap_or_else(|| default(20_000));
            Ok(vec![
                format!("PublishPort=127.0.0.1:{http}:3000"),
                format!("PublishPort=127.0.0.1:{}:2222", http + 2_000),
            ])
        }
        ServiceKind::Web | ServiceKind::Storage => Ok(vec![format!(
            "PublishPort=127.0.0.1:{}:{}",
            port.map(u32::from).unwrap_or_else(|| default(if kind == ServiceKind::Web {
                18_000
            } else {
                19_000
            })),
            8080
        )]),
    }
}

fn git_http_port(slug: &str, port: Option<u16>, instance: u32) -> Result<u32, String> {
    let number = tenant_number(slug).ok_or_else(|| "invalid tenant_id".to_string())?;
    Ok(port
        .map(u32::from)
        .unwrap_or_else(|| 20_000 + number + instance.saturating_sub(1) * 100))
}

fn validate_unit_layout(
    config: &Config,
    slug: &str,
    unit: &str,
    body: &str,
    envelope: &Envelope,
) -> Result<(), String> {
    if envelope.kind == ServiceKind::Container {
        return crate::containers::validate_committed(config, slug, unit, body, envelope).map(|_| ());
    }
    validate_unit_directives(config, slug, unit, body, envelope)?;
    let instance = instance_from_unit(slug, envelope.kind, unit)?;
    crate::provisioning::validate_service(config, slug, envelope.kind, instance)
}

fn validate_unit_directives(
    config: &Config,
    slug: &str,
    unit: &str,
    body: &str,
    envelope: &Envelope,
) -> Result<(), String> {
    let instance = instance_from_unit(slug, envelope.kind, unit)?;
    let data = crate::provisioning::instance_data_root(config, slug, envelope.kind, instance);
    let read_only = crate::service_config::read_only_marker(body, envelope.kind)?;
    let port = published_port(body)?;
    let expected = render_unit_with(config, slug, unit, &data, envelope, read_only, port, instance)?;
    if envelope.kind == ServiceKind::Storage {
        // Storage is new: every unit must be exactly the current safe render
        // (approved image, entrypoint, fixed flags, capabilities, mounts).
        if body != expected {
            return Err(
                "storage unit layout is stale or unsafe; stop it and rerun limits.set to migrate"
                    .to_string(),
            );
        }
        return Ok(());
    }
    // Web/database keep prefix checks so existing live units remain valid;
    // broker renders never contain Entrypoint/Exec/DropCapability for them.
    for prefix in [
        "Volume=",
        "PublishPort=",
        "EnvironmentFile=",
        "Environment=",
        "UserNS=",
        "User=",
        "Entrypoint=",
        "Exec=",
        "DropCapability=",
    ] {
        let expected: Vec<_> = expected
            .lines()
            .filter(|line| line.starts_with(prefix))
            .collect();
        let actual: Vec<_> = body
            .lines()
            .filter(|line| line.starts_with(prefix))
            .collect();
        if actual != expected {
            return Err(format!(
                "unit {prefix} layout is stale or unsafe; stop it and rerun limits.set to migrate"
            ));
        }
    }
    Ok(())
}

pub(crate) fn validate_unit(slug: &str, staging_dir: &Path, unit: &str) -> Result<(), String> {
    let staging_dir_text = staging_dir
        .to_str()
        .ok_or_else(|| "unit staging path is not valid UTF-8".to_string())?;
    let generated = run_as_with_env(
        slug,
        "/usr/libexec/podman/quadlet",
        &["-user", "-dryrun"],
        &[("QUADLET_UNIT_DIRS", staging_dir_text)],
    )?;
    let generated = parse_generated_units(&generated)?;
    let expected = format!("{unit}.service");
    if !generated.iter().any(|(name, _)| name == &expected) {
        return Err(format!("Quadlet did not generate {expected}"));
    }

    let mut generated_paths = Vec::new();
    for (name, body) in generated {
        let path = staging_dir.join(name);
        write_atomic(&path, &body)?;
        generated_paths.push(path);
    }
    let generated_paths = generated_paths
        .iter()
        .map(|path| {
            path.to_str()
                .map(str::to_string)
                .ok_or_else(|| "generated unit path is not valid UTF-8".to_string())
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut args = vec!["verify", "--user"];
    args.extend(generated_paths.iter().map(String::as_str));
    // The verification manager that systemd-analyze spawns rebinds the user bus
    // sockets under XDG_RUNTIME_DIR and leaves them dead when it exits (observed
    // on Rocky 10 / systemd 257), which then breaks every later `systemctl
    // --user` call for the tenant. Verification is static analysis: give it a
    // throwaway runtime directory so it can never touch the live manager.
    let verify_runtime = verify_runtime_dir(slug, unit)?;
    let verification = run_as_with_env(
        slug,
        "systemd-analyze",
        &args,
        &[("XDG_RUNTIME_DIR", verify_runtime.to_string_lossy().as_ref())],
    );
    let _ = std::fs::remove_dir_all(&verify_runtime);
    for path in generated_paths {
        let _ = std::fs::remove_file(path);
    }
    verification?;
    Ok(())
}

/// A private, tenant-owned scratch directory for the verification manager.
fn verify_runtime_dir(slug: &str, unit: &str) -> Result<std::path::PathBuf, String> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    let dir = std::env::temp_dir().join(format!(
        "honed-verify-{}-{}-{}",
        slug,
        unit,
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)
        .map_err(|error| format!("{}: {error}", dir.display()))?;
    std::os::unix::fs::chown(&dir, Some(user_uid(slug)?), Some(user_gid(slug)?))
        .map_err(|error| format!("chown {}: {error}", dir.display()))?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
        .map_err(|error| format!("chmod {}: {error}", dir.display()))?;
    Ok(dir)
}

fn parse_generated_units(output: &str) -> Result<Vec<(String, String)>, String> {
    let mut units = Vec::new();
    let mut current: Option<(String, String)> = None;

    for line in output.lines() {
        if let Some(name) = line
            .strip_prefix("---")
            .and_then(|line| line.strip_suffix("---"))
        {
            if let Some(unit) = current.take() {
                units.push(unit);
            }
            if name.is_empty()
                || !name.ends_with(".service")
                || !name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"._@:-".contains(&byte))
            {
                return Err(format!("invalid generated unit name {name:?}"));
            }
            current = Some((name.to_string(), String::new()));
        } else if let Some((_, body)) = current.as_mut() {
            body.push_str(line);
            body.push('\n');
        } else if !line.trim().is_empty() {
            return Err("unexpected Quadlet dry-run output".to_string());
        }
    }
    if let Some(unit) = current {
        units.push(unit);
    }
    if units.is_empty() {
        return Err("Quadlet generated no units".to_string());
    }
    Ok(units)
}

pub fn write_network(config: &Config, slug: &str) -> Result<(), String> {
    let dir = ensure_quadlet_dir(config, slug)?;

    let body = format!(
        "[Unit]\n\
         Description=SharpNode tenant {slug} network (snp-{slug}) — internal, no egress\n\n\
         [Network]\n\
         NetworkName=snp-{slug}\n\
         Internal=true\n"
    );
    write_atomic(&dir.join(format!("snp-{slug}.network")), &body)
}

pub(crate) fn write_atomic(path: &Path, body: &str) -> Result<(), String> {
    let temporary = path.with_extension("tmp");
    std::fs::write(&temporary, body)
        .map_err(|error| format!("{}: {error}", temporary.display()))?;
    std::fs::set_permissions(
        &temporary,
        std::os::unix::fs::PermissionsExt::from_mode(0o644),
    )
    .map_err(|error| format!("chmod {}: {error}", temporary.display()))?;
    std::fs::rename(&temporary, path).map_err(|error| format!("{}: {error}", path.display()))?;
    Ok(())
}

pub fn daemon_reload(slug: &str) -> Result<String, String> {
    run_as(slug, "systemctl", &["--user", "daemon-reload"])
}

pub fn stop_owned(config: &Config, slug: &str, unit: &str) -> Result<String, String> {
    // Emergency stop must not depend on mutable generic env files or volumes.
    stop_owned_with(config, slug, unit, owned_unit_identity, unit_state, unit_action)
}

/// Best-effort stop for deletion: a unit that is not loaded is already stopped,
/// so deleting a service whose provisioning never wrote a unit still succeeds.
pub fn stop_if_loaded(slug: &str, unit: &str) -> Result<(), String> {
    match unit_action(slug, unit, "stop") {
        Ok(_) => Ok(()),
        Err(error) if error.contains("not loaded") || error.contains("not found") => Ok(()),
        Err(error) => Err(error),
    }
}

fn stop_owned_with(
    config: &Config,
    slug: &str,
    unit: &str,
    identity: impl FnOnce(&Config, &str, &str) -> Result<(u32, PathBuf, Reservation), String>,
    state: impl FnOnce(&str, &str) -> Result<SystemdUnit, String>,
    action: impl FnOnce(&str, &str, &str) -> Result<String, String>,
) -> Result<String, String> {
    let (uid, path, _) = identity(config, slug, unit)?;
    validate_systemd_source(&state(slug, unit)?, uid, &path)?;
    action(slug, unit, "stop")
}

pub fn unit_action(slug: &str, unit: &str, action: &str) -> Result<String, String> {
    unit_action_with(slug, unit, action, run_as)
}

fn unit_action_with(
    slug: &str,
    unit: &str,
    action: &str,
    mut run_as: impl FnMut(&str, &str, &[&str]) -> Result<String, String>,
) -> Result<String, String> {
    let target = format!("{unit}.service");
    let mut args = vec!["--user"];
    // Queue activation without waiting for cold image pulls; stops must finish
    // before deletion or capacity can safely proceed.
    if matches!(action, "start" | "restart") {
        args.push("--no-block");
    }
    args.extend([action, &target]);
    run_as(slug, "systemctl", &args)?;
    Ok(format!("{target} {action}"))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ActiveState {
    Active,
    Reloading,
    Inactive,
    Failed,
    Activating,
    Deactivating,
    Maintenance,
    Refreshing,
}

impl ActiveState {
    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "active" => Ok(Self::Active),
            "reloading" => Ok(Self::Reloading),
            "inactive" => Ok(Self::Inactive),
            "failed" => Ok(Self::Failed),
            "activating" => Ok(Self::Activating),
            "deactivating" => Ok(Self::Deactivating),
            "maintenance" => Ok(Self::Maintenance),
            "refreshing" => Ok(Self::Refreshing),
            _ => Err(format!("unsupported systemd ActiveState {value:?}")),
        }
    }
}

pub struct ServiceStatus {
    pub uid: u32,
    pub unit: String,
    pub active_state: ActiveState,
}

struct SystemdUnit {
    id: String,
    load_state: String,
    active_state: ActiveState,
    source_path: PathBuf,
    fragment_path: PathBuf,
    job: String,
    memory_max: Option<u64>,
    cpu_quota_per_sec_usec: Option<u64>,
}

fn unit_state(slug: &str, unit: &str) -> Result<SystemdUnit, String> {
    let target = format!("{unit}.service");
    let args = [
        "--user", "show", "--no-pager", "--all",
        "--property=Id,LoadState,ActiveState,SourcePath,FragmentPath,Job,MemoryMax,CPUQuotaPerSecUSec",
        &target,
    ];
    let output = run_as_output(slug, "systemctl", &args, &[])?;
    let state = parse_systemd_unit(&String::from_utf8_lossy(&output.stdout), unit)?;
    // A fully described, absent unit is safe for initial creation even if this
    // systemctl version reports not-found with a nonzero process exit status.
    if !output.status.success() && state.load_state != "not-found" {
        return Err(format!(
            "systemctl show failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(state)
}

fn parse_systemd_unit(output: &str, unit: &str) -> Result<SystemdUnit, String> {
    let mut properties = std::collections::HashMap::new();
    for line in output.lines() {
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| "malformed systemd show output".to_string())?;
        if properties.insert(key, value).is_some() {
            return Err(format!("duplicate systemd property {key}"));
        }
    }
    let get = |key| {
        properties
            .get(key)
            .copied()
            .ok_or_else(|| format!("missing systemd property {key}"))
    };
    let id = get("Id")?;
    if id != format!("{unit}.service") {
        return Err("systemd returned a different unit or alias".to_string());
    }
    let load_state = get("LoadState")?;
    let memory_max_text = match properties.get("MemoryMax").copied() {
        Some(value) => value,
        None if load_state == "not-found" => "infinity",
        None => return Err("missing systemd property MemoryMax".to_string()),
    };
    let memory_max = match memory_max_text {
        "infinity" => None,
        value => Some(
            value
                .parse::<u64>()
                .map_err(|_| "invalid systemd MemoryMax".to_string())?,
        ),
    };
    Ok(SystemdUnit {
        id: id.to_string(),
        load_state: get("LoadState")?.to_string(),
        active_state: ActiveState::parse(get("ActiveState")?)?,
        source_path: PathBuf::from(get("SourcePath")?),
        fragment_path: PathBuf::from(get("FragmentPath")?),
        job: get("Job")?.to_string(),
        memory_max,
        cpu_quota_per_sec_usec: properties
            .get("CPUQuotaPerSecUSec")
            .map(|value| parse_duration_usec(value))
            .transpose()?
            .flatten(),
    })
}

fn parse_duration_usec(value: &str) -> Result<Option<u64>, String> {
    if value == "infinity" {
        return Ok(None);
    }
    let mut total = 0_u64;
    if value.is_empty() {
        return Err("empty systemd CPU quota duration".to_string());
    }
    for component in value.split_whitespace() {
        let end = component
            .bytes()
            .take_while(|byte| byte.is_ascii_digit() || *byte == b'.')
            .count();
        let multiplier = match &component[end..] {
            "us" => 1_u64,
            "ms" => 1000,
            "s" => 1_000_000,
            "min" => 60_000_000,
            "h" => 3_600_000_000,
            _ => return Err("unsupported systemd CPU quota duration".to_string()),
        };
        let (whole, fraction) = component[..end]
            .split_once('.')
            .unwrap_or((&component[..end], ""));
        if fraction.len() > 6 || !fraction.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err("invalid CPU quota duration fraction".to_string());
        }
        let whole = whole
            .parse::<u64>()
            .map_err(|_| "invalid CPU quota duration".to_string())?;
        let fraction = if fraction.is_empty() {
            0
        } else {
            let numerator = fraction
                .parse::<u64>()
                .map_err(|_| "invalid CPU quota duration fraction".to_string())?
                .checked_mul(multiplier)
                .ok_or_else(|| "CPU quota duration overflow".to_string())?;
            let denominator = 10_u64.pow(fraction.len() as u32);
            if numerator % denominator != 0 {
                return Err("sub-microsecond CPU quota duration".to_string());
            }
            numerator / denominator
        };
        let component_usec = whole
            .checked_mul(multiplier)
            .and_then(|value| value.checked_add(fraction))
            .ok_or_else(|| "CPU quota duration overflow".to_string())?;
        total = total
            .checked_add(component_usec)
            .ok_or_else(|| "CPU quota duration overflow".to_string())?;
    }
    Ok(Some(total))
}

pub(crate) fn validate_root_owned_path(path: &Path) -> Result<(), String> {
    validate_root_owned_entry(path, false)
}

pub(crate) fn validate_root_owned_entry(path: &Path, directory: bool) -> Result<(), String> {
    if !path.is_absolute() {
        return Err("broker Quadlet path must be absolute".to_string());
    }
    for ancestor in path.ancestors() {
        let metadata = std::fs::symlink_metadata(ancestor)
            .map_err(|error| format!("{}: {error}", ancestor.display()))?;
        let regular = if ancestor == path && !directory {
            metadata.is_file()
        } else {
            metadata.is_dir()
        };
        if !regular || metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
            return Err(format!(
                "untrusted broker unit path: {}",
                ancestor.display()
            ));
        }
    }
    Ok(())
}

fn owned_unit_identity(
    config: &Config,
    slug: &str,
    unit: &str,
) -> Result<(u32, PathBuf, Reservation), String> {
    crate::validate_unit_name(slug, unit)?;
    let uid = user_uid(slug)?;
    let path = config
        .quadlet_users
        .join(uid.to_string())
        .join(format!("{unit}.container"));
    validate_root_owned_path(&path)?;
    let body =
        std::fs::read_to_string(&path).map_err(|error| format!("{}: {error}", path.display()))?;
    let reservation = validate_unit_identity(slug, unit, &body)?;
    Ok((uid, path, reservation))
}

fn owned_unit(
    config: &Config,
    slug: &str,
    unit: &str,
) -> Result<(u32, PathBuf, Reservation), String> {
    let (uid, path, reservation) = owned_unit_identity(config, slug, unit)?;
    if reservation.kind == Some(ServiceKind::Container) {
        let body = std::fs::read_to_string(&path)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        crate::containers::validate_provenance(config, slug, unit, &body)?;
    } else if crate::containers::exists(config, unit)? {
        return Err("reserved generic container identity cannot change kind".to_string());
    }
    Ok((uid, path, reservation))
}

fn validate_unit_identity(slug: &str, unit: &str, body: &str) -> Result<Reservation, String> {
    crate::validate_unit_name(slug, unit)?;
    let reservation = parse_reservation(slug, unit, body)?;
    let names: Vec<_> = body
        .lines()
        .filter_map(|line| line.strip_prefix("ContainerName="))
        .collect();
    if names != [unit] {
        return Err("Quadlet ContainerName does not match the requested unit".to_string());
    }
    Ok(reservation)
}

fn validate_systemd_source(state: &SystemdUnit, uid: u32, path: &Path) -> Result<(), String> {
    let expected_fragment =
        PathBuf::from(format!("/run/user/{uid}/systemd/generator/{}", state.id));
    if state.load_state != "loaded"
        || state.source_path != path
        || state.fragment_path != expected_fragment
    {
        return Err("systemd unit is not generated from the broker-owned Quadlet".to_string());
    }
    Ok(())
}

/// Root-trusted committed unit bound to the requesting tenant by its marker.
pub fn committed_unit(
    config: &Config,
    slug: &str,
    unit: &str,
) -> Result<(u32, String, Reservation), String> {
    let (uid, path, reservation) = owned_unit(config, slug, unit)?;
    let body =
        std::fs::read_to_string(&path).map_err(|error| format!("{}: {error}", path.display()))?;
    if optional_unit_directive(&body, "# HonedTenant=")? != Some(slug) {
        return Err("unit is not bound to the requesting tenant".to_string());
    }
    if reservation.kind.is_none() || reservation.plan_code.is_none() {
        return Err("unit has no committed broker kind/plan; stop and migrate it".to_string());
    }
    Ok((uid, body, reservation))
}

/// Sanitized configuration of the committed unit; works while inactive.
pub fn inspect_unit(
    config: &Config,
    slug: &str,
    unit: &str,
) -> Result<ServiceConfiguration, String> {
    let (uid, body, reservation) = committed_unit(config, slug, unit)?;
    let kind = reservation
        .kind
        .ok_or_else(|| "unit has no committed kind".to_string())?;
    let image = unit_directive(&body, "Image=")?.to_string();
    let read_only = crate::service_config::read_only_marker(&body, kind)?;
    if kind != ServiceKind::Storage {
        return Ok(ServiceConfiguration {
            kind,
            image,
            username: None,
            read_only: None,
        });
    }
    let instance = instance_from_unit(slug, kind, unit)?;
    let stored = crate::service_config::read_settings(config, slug, user_gid(slug)?, instance)?
        .ok_or_else(|| "storage file-manager configuration is missing".to_string())?;
    crate::service_config::observe_database(config, slug, uid, instance)?;
    Ok(ServiceConfiguration {
        kind,
        image,
        username: Some(stored.username),
        read_only: Some(read_only),
    })
}

/// Host port already committed in a unit, used when `service.configure`
/// re-renders an existing service without resending its allocation.
pub fn published_port_for(config: &Config, slug: &str, unit: &str) -> Result<Option<u16>, String> {
    let (_, body, _) = committed_unit(config, slug, unit)?;
    published_port(&body)
}

pub fn service_status(config: &Config, slug: &str, unit: &str) -> Result<ServiceStatus, String> {
    let (uid, path, reservation) = owned_unit(config, slug, unit)?;
    let state = unit_state(slug, unit)?;
    validate_systemd_source(&state, uid, &path)?;
    if let Some(kind) = reservation.kind.filter(|kind| *kind == ServiceKind::Storage) {
        // Status polling after first start is the earliest chance to record
        // that FileBrowser created its database; failures surface on activation.
        let instance = instance_from_unit(slug, kind, unit)?;
        if let Err(error) = crate::service_config::observe_database(config, slug, uid, instance) {
            tracing::warn!(tenant_id = slug, unit, error = %error, "storage database observation failed");
        }
    }
    Ok(ServiceStatus {
        uid,
        unit: unit.to_string(),
        active_state: state.active_state,
    })
}

fn require_stopped(state: &SystemdUnit) -> Result<(), String> {
    // A failed unit is not running: reconfiguring a crashed service must be
    // possible without a separate reset-failed step.
    if !matches!(
        state.active_state,
        ActiveState::Inactive | ActiveState::Failed
    ) || !(state.job.is_empty() || state.job == "0")
    {
        return Err(
            "limits.set/service.configure require an inactive service with no pending job; stop the service first"
                .to_string(),
        );
    }
    Ok(())
}

pub(crate) fn ensure_limit_update_stopped(config: &Config, slug: &str, unit: &str) -> Result<(), String> {
    crate::validate_unit_name(slug, unit)?;
    let uid = user_uid(slug)?;
    let path = config
        .quadlet_users
        .join(uid.to_string())
        .join(format!("{unit}.container"));
    let exists = match std::fs::symlink_metadata(&path) {
        Ok(_) => {
            owned_unit(config, slug, unit)?;
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(format!("{}: {error}", path.display())),
    };
    let state = unit_state(slug, unit)?;
    validate_limit_update_state(&state, uid, &path, exists)
}

fn validate_limit_update_state(
    state: &SystemdUnit,
    uid: u32,
    path: &Path,
    exists: bool,
) -> Result<(), String> {
    require_stopped(state)?;
    if state.load_state == "not-found"
        && state.source_path.as_os_str().is_empty()
        && state.fragment_path.as_os_str().is_empty()
    {
        return Ok(());
    }
    if !exists {
        return Err("refusing to replace a service without a broker-owned Quadlet".to_string());
    }
    validate_systemd_source(state, uid, path)
}

pub fn activate_unit(
    config: &Config,
    slug: &str,
    unit: &str,
    action: &str,
) -> Result<String, String> {
    if !matches!(action, "start" | "restart") {
        return Err("invalid service activation action".to_string());
    }
    let (uid, path, reservation) = owned_unit(config, slug, unit)?;
    let body =
        std::fs::read_to_string(&path).map_err(|error| format!("{}: {error}", path.display()))?;
    let envelope = crate::policy::load(config)?.validate_activation(&reservation, &body)?;
    validate_unit_layout(config, slug, unit, &body, &envelope)?;
    // Reload even after an earlier failed limits.set reload: the cached service
    // definition must not start with an older, larger MemoryMax reservation.
    daemon_reload(slug)?;
    let state = unit_state(slug, unit)?;
    validate_activation(&state, uid, &path, &reservation)?;
    unit_action(slug, unit, action)
}

fn validate_activation(
    state: &SystemdUnit,
    uid: u32,
    path: &Path,
    reservation: &Reservation,
) -> Result<(), String> {
    validate_systemd_source(state, uid, path)?;
    if state.memory_max != Some(reservation.memory_max.as_bytes()) {
        return Err("systemd MemoryMax does not match the committed reservation".to_string());
    }
    let expected_quota = reservation
        .cpu_quota_percent
        .map(|quota| u64::from(quota) * 10_000);
    if state.cpu_quota_per_sec_usec != expected_quota {
        return Err("systemd CPU quota does not match the committed reservation".to_string());
    }
    Ok(())
}

pub fn logs(config: &Config, slug: &str, unit: &str, lines: i64) -> Result<String, String> {
    let (uid, path, _) = owned_unit(config, slug, unit)?;
    validate_systemd_source(&unit_state(slug, unit)?, uid, &path)?;
    let target = format!("{unit}.service");
    let lines = lines.clamp(1, 2000).to_string();
    let _ = config;
    let args = [
        "--user-unit",
        &target,
        "--lines",
        &lines,
        "--no-pager",
        "--output",
        "short-iso",
    ];
    run_as_output(slug, "journalctl", &args, &[])
        .and_then(|output| command_result_raw("journalctl", &args, output))
        .map(|output| {
            if output.is_empty() {
                format!("no output for {target} (uid {uid})")
            } else {
                output
            }
        })
}

pub fn edge_reload(
    config: &Config,
    slug: &str,
    unit: &str,
    hostname: &str,
    upstream: &str,
) -> Result<String, String> {
    let (_, path, reservation) = owned_unit(config, slug, unit)?;
    let body = std::fs::read_to_string(&path).map_err(|error| error.to_string())?;
    let envelope = crate::policy::load(config)?.validate_activation(&reservation, &body)?;
    validate_unit_layout(config, slug, unit, &body, &envelope)?;
    if envelope.kind == ServiceKind::Container {
        let info = crate::containers::inspect(config, slug, unit)?;
        let expected_hostname = format!("{unit}.{}", config.tenant_domain);
        let expected_upstream = format!("127.0.0.1:{}", info.host_port);
        if !valid_hostname(hostname) || hostname != expected_hostname || upstream != expected_upstream {
            return Err("generic edge endpoint does not match committed container".to_string());
        }
    } else {
        validate_edge_request(config, slug, unit, envelope.kind, hostname, upstream)?;
    }
    service_status(config, slug, unit)?;

    let sites = config.caddy_dir.join("sites");
    std::fs::create_dir_all(&sites).map_err(|error| format!("{}: {error}", sites.display()))?;

    let body = format!(
        "{hostname} {{\n\
         \tencode zstd gzip\n\
         \treverse_proxy {upstream}\n\
         \theader {{\n\
         \t\tStrict-Transport-Security \"max-age=31536000; includeSubDomains\"\n\
         \t\tX-Content-Type-Options \"nosniff\"\n\
         \t\tX-Frame-Options \"DENY\"\n\
         \t\tReferrer-Policy \"strict-origin-when-cross-origin\"\n\
         \t\t-Server\n\
         \t}}\n\
         \tlog {{\n\
         \t\toutput file /var/log/caddy/{hostname}-access.log {{\n\
         \t\t\troll_size 50MiB\n\
         \t\t\troll_keep 10\n\
         \t\t}}\n\
         \t\tformat json\n\
         \t}}\n\
         }}\n"
    );

    let caddyfile = std::fs::read_to_string(config.caddy_dir.join("Caddyfile"))
        .map_err(|error| format!("cannot read the Caddyfile: {error}"))?;
    let site_path = sites.join(format!("{hostname}.caddy"));
    let previous = match std::fs::read_to_string(&site_path) {
        Ok(previous) => Some(previous),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(format!("{}: {error}", site_path.display())),
    };
    write_atomic(&site_path, &body)?;

    if let Err(error) = post_caddyfile(&config.caddy_admin, &caddyfile) {
        let rollback = match previous {
            Some(previous) => write_atomic(&site_path, &previous),
            None => std::fs::remove_file(&site_path)
                .map_err(|rollback_error| format!("{}: {rollback_error}", site_path.display())),
        };
        if let Err(rollback_error) = rollback {
            return Err(format!(
                "{error}; edge site rollback failed: {rollback_error}"
            ));
        }
        return Err(error);
    }
    Ok(format!(
        "vhost {hostname} -> {upstream} published and the edge reloaded"
    ))
}

fn validate_edge_request(
    config: &Config,
    slug: &str,
    unit: &str,
    kind: ServiceKind,
    hostname: &str,
    upstream: &str,
) -> Result<(), String> {
    crate::validate_unit_name(slug, unit)?;
    if kind == ServiceKind::Database {
        return Err("database services cannot publish HTTP vhosts".to_string());
    }
    if !valid_hostname(hostname) || !valid_upstream(upstream) {
        return Err("invalid hostname or upstream".to_string());
    }

    let number = tenant_number(slug).ok_or_else(|| "invalid tenant_id".to_string())?;
    if !config.tenants.iter().any(|tenant| tenant == slug) {
        return Err("hostname tenant is outside the node allowlist".to_string());
    }
    let expected_hostname = format!("{slug}.{}", config.tenant_domain);
    let expected_upstream = format!(
        "127.0.0.1:{}",
        kind.http_port(number)
            .ok_or_else(|| "database services cannot publish HTTP vhosts".to_string())?
    );
    if hostname != expected_hostname {
        return Err("hostname is outside the tenant's assigned vhost".to_string());
    }
    if upstream != expected_upstream {
        return Err("upstream is not the tenant's assigned loopback port for this service kind".to_string());
    }
    Ok(())
}

fn tenant_number(slug: &str) -> Option<u32> {
    if slug.len() == 3
        && slug.starts_with('t')
        && slug[1..].bytes().all(|byte| byte.is_ascii_digit())
    {
        slug[1..].parse().ok()
    } else {
        None
    }
}

fn valid_hostname(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 253
        && value.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
                && label
                    .as_bytes()
                    .first()
                    .is_some_and(u8::is_ascii_alphanumeric)
                && label
                    .as_bytes()
                    .last()
                    .is_some_and(u8::is_ascii_alphanumeric)
        })
}

fn valid_upstream(value: &str) -> bool {
    let Some((host, port)) = value.rsplit_once(':') else {
        return false;
    };
    let port_ok = port.parse::<u16>().map(|port| port > 0).unwrap_or(false);
    host == "127.0.0.1" && port_ok
}

fn post_caddyfile(admin: &str, body: &str) -> Result<String, String> {
    use std::io::{Read, Write};
    use std::net::TcpStream;

    let address = admin
        .parse::<std::net::SocketAddr>()
        .map_err(|error| format!("invalid caddy admin address {admin:?}: {error}"))?;
    let mut stream = TcpStream::connect_timeout(&address, std::time::Duration::from_secs(10))
        .map_err(|error| format!("caddy admin {admin} unreachable: {error}"))?;
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(10)))
        .ok();
    stream
        .set_write_timeout(Some(std::time::Duration::from_secs(10)))
        .ok();

    let request = format!(
        "POST /load HTTP/1.1\r\nHost: {admin}\r\nContent-Type: text/caddyfile\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );

    stream
        .write_all(request.as_bytes())
        .map_err(|error| format!("caddy admin write failed: {error}"))?;

    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .map_err(|error| format!("caddy admin read failed: {error}"))?;

    let status = response.lines().next().unwrap_or("").to_string();
    if status.contains(" 200") {
        Ok(status)
    } else {
        Err(format!("caddy refused the configuration: {status}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_manager_start_follows_linger_and_uses_exact_resolved_uid() {
        for uid in [1001, 4242] {
            let mut calls = Vec::new();
            ensure_user_manager("t91", uid, |program, args| {
                calls.push((program.to_string(), args.join(" ")));
                Ok(String::new())
            })
            .unwrap();
            assert_eq!(
                calls,
                vec![
                    ("loginctl".to_string(), "enable-linger t91".to_string()),
                    ("systemctl".to_string(), format!("start user@{uid}.service")),
                ]
            );
        }
    }

    #[test]
    fn user_manager_errors_propagate_and_prevent_continuation() {
        for failed_program in ["loginctl", "systemctl"] {
            let mut calls = Vec::new();
            let mut continued = false;
            let error = format!("{failed_program} failed");
            let result = (|| {
                ensure_user_manager("t91", 1001, |program, _args| {
                    calls.push(program.to_string());
                    if program == failed_program {
                        Err(error.clone())
                    } else {
                        Ok(String::new())
                    }
                })?;
                continued = true;
                Ok(())
            })();
            assert_eq!(result, Err(error));
            assert!(!continued);
            let expected = if failed_program == "loginctl" {
                vec!["loginctl"]
            } else {
                vec!["loginctl", "systemctl"]
            };
            assert_eq!(calls, expected);
        }
    }

    #[test]
    fn unit_actions_queue_activation_but_wait_for_stop() {
        for (action, expected) in [
            (
                "start",
                vec!["--user", "--no-block", "start", "t91-db.service"],
            ),
            (
                "restart",
                vec!["--user", "--no-block", "restart", "t91-db.service"],
            ),
            ("stop", vec!["--user", "stop", "t91-db.service"]),
        ] {
            let mut calls = 0;
            let result = unit_action_with("t91", "t91-db", action, |user, program, args| {
                calls += 1;
                assert_eq!(user, "t91");
                assert_eq!(program, "systemctl");
                assert_eq!(args, expected);
                Ok(String::new())
            });
            assert_eq!(calls, 1);
            assert_eq!(result, Ok(format!("t91-db.service {action}")));
        }
    }

    #[test]
    fn stop_uses_root_identity_and_generated_source_without_generic_runtime_files() {
        let config = config();
        let path = PathBuf::from("/etc/containers/systemd/users/1001/t01-site.container");
        let body = "# HonedTenant=t01\n# HonedKind=container\n# HonedPlan=generic\nContainerName=t01-site\nMemoryMax=384M\nCPUQuota=25%\n";
        let mut stopped = false;
        let result = stop_owned_with(
            &config,
            "t01",
            "t01-site",
            |_, _, _| {
                Ok((
                    1001,
                    path.clone(),
                    validate_unit_identity("t01", "t01-site", body)?,
                ))
            },
            |_, _| Ok(observed_unit("active")),
            |_, _, action| {
                assert_eq!(action, "stop");
                stopped = true;
                Ok("stopped".to_string())
            },
        );
        assert_eq!(result.as_deref(), Ok("stopped"));
        assert!(stopped);

        let result = stop_owned_with(
            &config,
            "t01",
            "t01-site",
            |_, _, _| {
                Ok((
                    1001,
                    path.clone(),
                    validate_unit_identity("t01", "t01-site", body)?,
                ))
            },
            |_, _| {
                let mut state = observed_unit("active");
                state.fragment_path =
                    PathBuf::from("/home/t01/.config/systemd/user/t01-site.service");
                Ok(state)
            },
            |_, _, _| panic!("an unprovenanced unit must not be stopped"),
        );
        assert!(result.is_err());
    }

    #[test]
    fn unit_action_errors_propagate_for_activation_and_stop() {
        for action in ["start", "restart", "stop"] {
            let error = format!("systemctl {action} failed");
            let result =
                unit_action_with("t91", "t91-db", action, |_, _, _| Err(error.clone()));
            assert_eq!(result, Err(error));
        }
    }

    fn config() -> Config {
        Config {
            socket: PathBuf::from("/run/honenode/broker.sock"),
            policy_file: PathBuf::from("/etc/honenode/policy.json"),
            tenants: vec!["t01".to_string(), "t02".to_string()],
            quadlet_users: PathBuf::from("/etc/containers/systemd/users"),
            tenant_data_root: PathBuf::from("/srv/hosting"),
            etc_hosting: PathBuf::from("/etc/hosting"),
            caddy_dir: PathBuf::from("/etc/hosting/caddy"),
            caddy_admin: "127.0.0.1:2019".to_string(),
            tenant_domain: "tenants.example".to_string(),
            service_access: crate::config::ServiceAccess::Public,
            subid_count: 65_536,
            subid_base: 100_000,
            admission_overcommit_percent: 200,
            host_memory_total: None,
            host_memory_reserve: None,
            tenant_memory_max: None,
            image_nginx: "example/nginx:1".to_string(),
            image_postgres: "example/postgres:1".to_string(),
            image_filebrowser: "example/filebrowser:1".to_string(),
            image_forgejo: "example/forgejo:1".to_string(),
        }
    }

    fn observed_unit(active_state: &str) -> SystemdUnit {
        parse_systemd_unit(
            &format!(
                "Id=t01-site.service\nLoadState=loaded\nActiveState={active_state}\nSourcePath=/etc/containers/systemd/users/1001/t01-site.container\nFragmentPath=/run/user/1001/systemd/generator/t01-site.service\nJob=0\nMemoryMax=402653184\n"
            ),
            "t01-site",
        )
        .unwrap()
    }

    #[test]
    fn status_parses_actual_inactive_failed_and_transitional_states() {
        for (text, expected) in [
            ("active", ActiveState::Active),
            ("inactive", ActiveState::Inactive),
            ("failed", ActiveState::Failed),
            ("activating", ActiveState::Activating),
            ("deactivating", ActiveState::Deactivating),
            ("reloading", ActiveState::Reloading),
            ("maintenance", ActiveState::Maintenance),
            ("refreshing", ActiveState::Refreshing),
        ] {
            assert_eq!(observed_unit(text).active_state, expected);
        }
        assert_eq!(observed_unit("failed").memory_max, Some(402653184));
        assert!(ActiveState::parse("unknown").is_err());
    }

    #[test]
    fn status_parser_rejects_missing_duplicate_and_aliased_properties() {
        assert!(parse_systemd_unit("ActiveState=active\n", "t01-site").is_err());
        assert!(
            parse_systemd_unit("Id=t01-site.service\nId=t01-site.service\n", "t01-site").is_err()
        );
        assert!(parse_systemd_unit("Id=t02-site.service\n", "t01-site").is_err());
    }

    #[test]
    fn status_and_activation_require_broker_quadlet_provenance() {
        let path = Path::new("/etc/containers/systemd/users/1001/t01-site.container");
        let mut state = observed_unit("inactive");
        assert!(validate_systemd_source(&state, 1001, path).is_ok());
        assert!(validate_systemd_source(&state, 1002, path).is_err());
        state.source_path =
            PathBuf::from("/home/t01/.config/containers/systemd/t01-site.container");
        assert!(validate_systemd_source(&state, 1001, path).is_err());
        state = observed_unit("inactive");
        state.fragment_path = PathBuf::from("/home/t01/.config/systemd/user/t01-site.service");
        assert!(validate_systemd_source(&state, 1001, path).is_err());
    }

    #[test]
    fn activation_refuses_stale_or_unlimited_cached_memory() {
        let path = Path::new("/etc/containers/systemd/users/1001/t01-site.container");
        let reservation = parse_reservation("t01", "t01-site", "MemoryMax=384M\n").unwrap();
        let mut state = observed_unit("inactive");
        assert!(validate_activation(&state, 1001, path, &reservation).is_ok());
        state.memory_max = Some(1024 * 1024 * 1024);
        assert!(validate_activation(&state, 1001, path, &reservation).is_err());
        state.memory_max = None;
        assert!(validate_activation(&state, 1001, path, &reservation).is_err());
    }

    #[test]
    fn limits_updates_require_a_stopped_unit_without_a_pending_job() {
        let path = Path::new("/etc/containers/systemd/users/1001/t01-site.container");
        // A failed unit is not running: reconfiguring a crashed service must
        // not require a separate reset-failed step.
        for state in ["inactive", "failed"] {
            assert!(validate_limit_update_state(&observed_unit(state), 1001, path, true).is_ok());
        }
        for state in [
            "active",
            "activating",
            "deactivating",
            "reloading",
            "maintenance",
            "refreshing",
        ] {
            assert!(validate_limit_update_state(&observed_unit(state), 1001, path, true).is_err());
        }
        let mut pending = observed_unit("inactive");
        pending.job = "42".to_string();
        assert!(validate_limit_update_state(&pending, 1001, path, true).is_err());
    }

    #[test]
    fn initial_limits_allow_not_found_but_never_overwrite_an_unowned_unit() {
        let path = Path::new("/etc/containers/systemd/users/1001/t01-site.container");
        assert!(
            validate_limit_update_state(&observed_unit("inactive"), 1001, path, false).is_err()
        );
        let mut missing = parse_systemd_unit(
            "Id=t01-site.service\nLoadState=not-found\nActiveState=inactive\nSourcePath=\nFragmentPath=\nJob=0\n",
            "t01-site",
        ).unwrap();
        assert!(validate_limit_update_state(&missing, 1001, path, false).is_ok());
        missing.active_state = ActiveState::Active;
        assert!(validate_limit_update_state(&missing, 1001, path, false).is_err());
    }

    #[test]
    fn owned_unit_identity_requires_matching_container_and_reservation() {
        let body = "[Container]\nContainerName=t01-site\n[Service]\nMemoryMax=384M\n";
        assert!(validate_unit_identity("t01", "t01-site", body).is_ok());
        assert!(validate_unit_identity("t02", "t01-site", body).is_err());
        assert!(validate_unit_identity("t01", "t01-other", body).is_err());
        assert!(validate_unit_identity("t01", "t01-site", "ContainerName=t01-site\n").is_err());
        assert!(validate_unit_identity(
            "t01",
            "t01-site",
            &format!("{body}ContainerName=t01-site\n")
        )
        .is_err());
    }

    #[test]
    fn cpu_and_plan_reservations_are_strict_and_rendered_as_hard_limits() {
        let body = "MemoryMax=384M\nCPUQuota=25%\n# HonedPlan=starter\n";
        let reservation = parse_reservation("t01", "t01-site", body).unwrap();
        assert_eq!(reservation.cpu_quota_percent, Some(25));
        assert_eq!(reservation.plan_code.as_deref(), Some("starter"));
        for bad in [
            "CPUQuota=0%",
            "CPUQuota=infinity",
            "CPUQuota=25.5%",
            "CPUQuota=4294967296%",
            "CPUQuota=25%\nCPUQuota=50%",
            "# HonedPlan=bad/name",
            "# HonedKind=shell",
        ] {
            assert!(
                parse_reservation("t01", "t01-site", &format!("MemoryMax=384M\n{bad}\n")).is_err()
            );
        }
        let mut envelope = Envelope::new("web", 100, "256M", "384M", 128).unwrap();
        envelope.plan_code = Some("starter".to_string());
        envelope.cpu_quota_percent = 25;
        let rendered = render_unit(
            &config(),
            "t01",
            "t01-site",
            Path::new("/srv/hosting/t01"),
            &envelope,
        )
        .unwrap();
        assert!(rendered.contains("CPUQuota=25%\n"));
        assert!(rendered.contains("# HonedPlan=starter\n# HonedKind=web\n"));
    }

    #[test]
    fn observed_cpu_quota_must_match_plan_reservation() {
        assert_eq!(parse_duration_usec("250ms").unwrap(), Some(250000));
        assert_eq!(parse_duration_usec("1.5s").unwrap(), Some(1500000));
        assert_eq!(parse_duration_usec("1min 2s").unwrap(), Some(62000000));
        assert_eq!(parse_duration_usec("infinity").unwrap(), None);
        assert!(parse_duration_usec("unlimited").is_err());
        let mut state = observed_unit("inactive");
        let reservation =
            parse_reservation("t01", "t01-site", "MemoryMax=384M\nCPUQuota=25%\n").unwrap();
        let path = Path::new("/etc/containers/systemd/users/1001/t01-site.container");
        state.cpu_quota_per_sec_usec = Some(250000);
        assert!(validate_activation(&state, 1001, path, &reservation).is_ok());
        state.cpu_quota_per_sec_usec = None;
        assert!(validate_activation(&state, 1001, path, &reservation).is_err());
        state.cpu_quota_per_sec_usec = Some(1000000);
        assert!(validate_activation(&state, 1001, path, &reservation).is_err());

        let legacy = parse_reservation("t01", "t01-site", "MemoryMax=384M\n").unwrap();
        state.cpu_quota_per_sec_usec = None;
        assert!(validate_activation(&state, 1001, path, &legacy).is_ok());
        state.cpu_quota_per_sec_usec = Some(250000);
        assert!(validate_activation(&state, 1001, path, &legacy).is_err());
    }

    #[test]
    fn envelope_rejects_unknown_kinds_and_unsafe_or_invalid_limits() {
        assert!(Envelope::new("unknown", 100, "256M", "384M", 128).is_err());
        assert!(Envelope::new("web", 0, "256M", "384M", 128).is_err());
        assert!(Envelope::new("web", 10_001, "256M", "384M", 128).is_err());
        assert!(Envelope::new("web", 100, "256M", "384M", 0).is_err());
        assert!(Envelope::new("web", 100, "1G", "384M", 128).is_err());
        assert!(Envelope::new("web", 100, "256M\nCPUQuota=100%", "384M", 128).is_err());
        assert!(Envelope::new("web", 100, "256M", "0", 128).is_err());
    }

    #[test]
    fn renders_only_typed_kind_and_canonical_numeric_limits() {
        let envelope = Envelope::new("web", 100, "256M", "384M", 128).unwrap();
        let body = render_unit(
            &config(),
            "t01",
            "t01-site",
            Path::new("/srv/hosting/t01"),
            &envelope,
        )
        .unwrap();
        assert!(body.contains("Image=example/nginx:1\n"));
        assert!(body.contains("MemoryHigh=268435456\n"));
        assert!(body.contains("MemoryMax=402653184\n"));
        assert!(body.contains("CPUWeight=100\n"));
        assert!(body.contains("TasksMax=128\n"));
        assert!(body.contains("PidsLimit=128\n"));
        assert!(body.contains("PublishPort=127.0.0.1:18001:8080\n"));
    }

    #[test]
    fn forgejo_root_url_matches_the_published_access_mode() {
        let mut config = config();
        config.service_access = crate::config::ServiceAccess::Loopback;
        let envelope = Envelope::new("git", 100, "256M", "384M", 128).unwrap();
        let body = render_unit(
            &config,
            "t01",
            "t01-git",
            Path::new("/srv/hosting/t01"),
            &envelope,
        )
        .unwrap();
        assert!(body.contains("Environment=FORGEJO__server__DOMAIN=127.0.0.1\n"));
        assert!(body.contains(
            "Environment=FORGEJO__server__ROOT_URL=http://127.0.0.1:20001/\n"
        ));

        config.service_access = crate::config::ServiceAccess::Public;
        let body = render_unit(
            &config,
            "t01",
            "t01-git",
            Path::new("/srv/hosting/t01"),
            &envelope,
        )
        .unwrap();
        assert!(body.contains(
            "Environment=FORGEJO__server__DOMAIN=t01.tenants.example\n"
        ));
        assert!(body.contains(
            "Environment=FORGEJO__server__ROOT_URL=https://t01.tenants.example/\n"
        ));
    }

    #[test]
    fn instance_suffixes_are_parsed_from_unit_names() {
        assert_eq!(
            instance_from_unit("t01", ServiceKind::Web, "t01-site").unwrap(),
            1
        );
        assert_eq!(
            instance_from_unit("t01", ServiceKind::Web, "t01-site-2").unwrap(),
            2
        );
        assert_eq!(
            instance_from_unit("t01", ServiceKind::Database, "t01-db-3").unwrap(),
            3
        );
        for invalid in ["t01-site-1", "t01-db", "t01-site-x", "t01-site-100"] {
            assert!(instance_from_unit("t01", ServiceKind::Web, invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn every_kind_uses_its_own_loopback_http_port_and_git_ssh_port() {
        assert_eq!(
            publish_ports("t02", ServiceKind::Database, None, 1).unwrap(),
            ["PublishPort=127.0.0.1:15432:5432"]
        );
        assert_eq!(
            publish_ports("t03", ServiceKind::Storage, None, 1).unwrap(),
            ["PublishPort=127.0.0.1:19003:8080"]
        );
        assert_eq!(
            publish_ports("t04", ServiceKind::Git, None, 1).unwrap(),
            [
                "PublishPort=127.0.0.1:20004:3000",
                "PublishPort=127.0.0.1:22004:2222"
            ]
        );
        // A second and third instance shift the whole range by 100 per instance.
        assert_eq!(
            publish_ports("t02", ServiceKind::Web, None, 2).unwrap(),
            ["PublishPort=127.0.0.1:18102:8080"]
        );
        assert_eq!(
            publish_ports("t02", ServiceKind::Database, None, 3).unwrap(),
            ["PublishPort=127.0.0.1:15632:5432"]
        );
        assert_eq!(
            publish_ports("t02", ServiceKind::Git, None, 2).unwrap(),
            [
                "PublishPort=127.0.0.1:20102:3000",
                "PublishPort=127.0.0.1:22102:2222"
            ]
        );
        assert_eq!(
            publish_ports("t02", ServiceKind::Web, Some(18999), 1).unwrap(),
            ["PublishPort=127.0.0.1:18999:8080"]
        );
        for (kind, user, volume) in [
            (ServiceKind::Web, 101, "/site:/usr/share/nginx/html:ro,Z"),
            (ServiceKind::Database, 70, "/pgdata:/var/lib/postgresql/data:Z"),
            (ServiceKind::Git, 1000, "/gitea:/var/lib/gitea:Z"),
        ] {
            let envelope = Envelope::new(kind.as_str(), 100, "256M", "384M", 128).unwrap();
            let unit = format!("t01-{}", unit_suffix(kind));
            let body = render_unit(
                &config(),
                "t01",
                &unit,
                Path::new("/srv/hosting/t01"),
                &envelope,
            )
            .unwrap();
            assert!(body.contains(&format!("UserNS=keep-id:uid={user},gid={user}\nUser={user}\n")));
            assert!(body.contains(volume));
            assert!(validate_unit_directives(
                &config(),
                "t01",
                &unit,
                &body,
                &envelope
            )
            .is_ok());
            let stale = body.replacen("PublishPort=127.0.0.1:", "PublishPort=127.0.0.2:", 1);
            assert!(validate_unit_directives(
                &config(),
                "t01",
                &unit,
                &stale,
                &envelope
            )
            .is_err());
        }
    }

    #[test]
    fn secret_mounts_follow_the_configured_hone_etc() {
        let mut config = config();
        config.etc_hosting = PathBuf::from("/srv/reference/etc");
        for (kind, secret) in [
            (ServiceKind::Database, "t01-db.env"),
            (ServiceKind::Git, "t01-forgejo.env"),
        ] {
            let envelope = Envelope::new(kind.as_str(), 100, "256M", "384M", 128).unwrap();
            let body = render_unit(
                &config,
                "t01",
                "t01-service",
                Path::new("/srv/hosting/t01"),
                &envelope,
            )
            .unwrap();
            assert!(body.contains(&format!(
                "EnvironmentFile=/srv/reference/etc/secrets/{secret}\n"
            )));
        }
    }

    fn storage_envelope() -> Envelope {
        let mut envelope = Envelope::new("storage", 100, "512M", "768M", 256).unwrap();
        envelope.plan_code = Some("vault".to_string());
        envelope.cpu_quota_percent = 50;
        envelope
    }

    #[test]
    fn storage_unit_uses_filebrowser_entrypoint_fixed_flags_and_safe_mounts() {
        let config = config();
        let envelope = storage_envelope();
        let body = render_unit_with(
            &config,
            "t01",
            "t01-vault",
            Path::new("/srv/hosting/t01"),
            &envelope,
            false,
            None,
            1,
        )
        .unwrap();
        assert!(body.contains("# HonedKind=storage\n# HonedStorageReadOnly=false\n"));
        assert!(body.contains("Image=example/filebrowser:1\n"));
        assert!(body.contains("Entrypoint=/bin/filebrowser\n"));
        assert!(body.contains(&format!("Exec={}\n", crate::service_config::FILEBROWSER_EXEC)));
        assert!(body.contains("--disableExec=true"));
        assert!(body.contains("--followExternalSymlinks=false"));
        assert!(body.contains("--noauth=false"));
        assert!(body.contains("--database /database/filebrowser.db"));
        assert!(body.contains("DropCapability=all\n"));
        assert!(body.contains("UserNS=keep-id:uid=1000,gid=1000\nUser=1000\n"));
        assert!(body.contains("Volume=/srv/hosting/t01/vault:/srv:Z\n"));
        assert!(body.contains("Volume=/srv/hosting/t01/db:/database:Z\n"));
        assert!(body.contains(
            "Volume=/etc/hosting/secrets/t01-filebrowser.json:/config/settings.json:ro\n"
        ));
        assert!(body.contains("PublishPort=127.0.0.1:19001:8080\n"));
        // No credentials, hashes, or plaintext-password CLI bootstrap.
        assert!(!body.contains("users add"));
        assert!(!body.contains("$2a$"));
        assert!(!body.contains("FB_PASSWORD"));
        assert!(!body.contains("/srv:/srv"));
        assert!(validate_unit_directives(&config, "t01", "t01-vault", &body, &envelope).is_ok());

        let read_only = render_unit_with(
            &config,
            "t01",
            "t01-vault",
            Path::new("/srv/hosting/t01"),
            &envelope,
            true,
            None,
            1,
        )
        .unwrap();
        assert!(read_only.contains("# HonedStorageReadOnly=true\n"));
        assert!(read_only.contains("Volume=/srv/hosting/t01/vault:/srv:ro,Z\n"));
        assert!(read_only.contains("Volume=/srv/hosting/t01/db:/database:Z\n"));
        assert!(validate_unit_directives(&config, "t01", "t01-vault", &read_only, &envelope).is_ok());

        for (name, tampered) in [
            ("command runner re-enabled", body.replace("--disableExec=true", "--disableExec=false")),
            ("external symlinks", body.replace("--followExternalSymlinks=false", "--followExternalSymlinks=true")),
            ("noauth", body.replace("--noauth=false", "--noauth=true")),
            ("arbitrary image", body.replace("example/filebrowser:1", "docker.io/evil:latest")),
            ("default entrypoint", body.replace("Entrypoint=/bin/filebrowser", "Entrypoint=/init.sh")),
            ("capabilities", body.replace("DropCapability=all", "DropCapability=chown")),
            ("writable config", body.replace("/config/settings.json:ro", "/config/settings.json:Z")),
            ("extra mount", format!("{body}Volume=/etc:/host-etc:Z\n")),
            ("read-only mismatch", body.replace("# HonedStorageReadOnly=false", "# HonedStorageReadOnly=true")),
            ("missing marker", body.replace("# HonedStorageReadOnly=false\n", "")),
            ("legacy storage layout", body.replace("Entrypoint=/bin/filebrowser\n", "").replace("DropCapability=all\n", "")),
        ] {
            assert!(
                validate_unit_directives(&config, "t01", "t01-vault", &tampered, &envelope).is_err(),
                "{name} must be refused"
            );
        }
    }

    #[test]
    fn web_and_database_layouts_stay_unchanged_but_reject_injected_exec_or_capabilities() {
        let config = config();
        for kind in [ServiceKind::Web, ServiceKind::Database] {
            let envelope = Envelope::new(kind.as_str(), 100, "256M", "384M", 128).unwrap();
            let unit = format!("t01-{}", unit_suffix(kind));
            let body = render_unit(
                &config,
                "t01",
                &unit,
                Path::new("/srv/hosting/t01"),
                &envelope,
            )
            .unwrap();
            for absent in ["Entrypoint=", "Exec=", "DropCapability=", "HonedStorageReadOnly"] {
                assert!(!body.contains(absent), "{} must not render {absent}", kind.as_str());
            }
            assert!(validate_unit_directives(&config, "t01", &unit, &body, &envelope).is_ok());
            for injected in ["Exec=--noauth=true\n", "Entrypoint=/bin/sh\n", "DropCapability=none\n"] {
                let tampered = body.replace("NoNewPrivileges=true\n", injected);
                assert!(validate_unit_directives(
                    &config,
                    "t01",
                    &unit,
                    &tampered,
                    &envelope
                )
                .is_err());
            }
        }
    }

    #[test]
    fn quadlet_dry_run_parser_requires_safe_native_service_units() {
        let output = "---snp-t01-network.service---\n[Service]\nType=oneshot\n\n---t01-site.service---\n[Service]\nExecStart=/usr/bin/podman run t01-site\n";
        let units = parse_generated_units(output).unwrap();
        assert_eq!(units.len(), 2);
        assert_eq!(units[1].0, "t01-site.service");
        assert!(units[1].1.contains("ExecStart=/usr/bin/podman"));
        assert!(parse_generated_units("no generated unit").is_err());
        assert!(parse_generated_units("---../escape.service---\n[Service]\n").is_err());
    }

    #[test]
    fn committed_unit_parser_fails_closed() {
        let parsed = parse_reservation("t01", "t01-site", "[Service]\nMemoryMax=384M\n").unwrap();
        assert_eq!(parsed.memory_max.as_bytes(), 384 * 1024 * 1024);
        assert!(parse_reservation("t01", "t01-site", "[Service]\n").is_err());
        assert!(parse_reservation("t01", "t01-site", "MemoryMax=384M\nMemoryMax=1G\n").is_err());
        assert!(parse_reservation("t01", "t01-site", "MemoryMax=infinity\n").is_err());
        assert!(parse_reservation("t01", "t01-site", "MemoryMax=0\n").is_err());
    }

    #[test]
    fn edge_publishing_is_bound_to_request_tenant_owned_service_and_exact_endpoint() {
        let config = config();
        for (kind, upstream) in [
            (ServiceKind::Web, "127.0.0.1:18001"),
            (ServiceKind::Storage, "127.0.0.1:19001"),
            (ServiceKind::Git, "127.0.0.1:20001"),
        ] {
            assert!(validate_edge_request(
                &config,
                "t01",
                "t01-site",
                kind,
                "t01.tenants.example",
                upstream
            )
            .is_ok());
            assert!(validate_edge_request(
                &config,
                "t01",
                "t01-site",
                kind,
                "t01.tenants.example",
                "127.0.0.1:18001"
            )
            .is_err() == (kind != ServiceKind::Web));
        }
        for (tenant, unit, kind, hostname, upstream) in [
            (
                "t01",
                "t01-site",
                ServiceKind::Database,
                "t01.tenants.example",
                "127.0.0.1:18001",
            ),
            (
                "t01",
                "t02-site",
                ServiceKind::Web,
                "t02.tenants.example",
                "127.0.0.1:18002",
            ),
            (
                "t01",
                "t01-site",
                ServiceKind::Web,
                "t02.tenants.example",
                "127.0.0.1:18002",
            ),
            (
                "t01",
                "t01-site",
                ServiceKind::Web,
                "t01.tenants.example",
                "127.0.0.1:18002",
            ),
            (
                "t01",
                "t01-site",
                ServiceKind::Web,
                "t01.tenants.example",
                "10.0.0.1:18001",
            ),
            (
                "t01",
                "t01-site",
                ServiceKind::Web,
                "t01.evil.example",
                "127.0.0.1:18001",
            ),
        ] {
            assert!(
                validate_edge_request(&config, tenant, unit, kind, hostname, upstream).is_err()
            );
        }
    }

    #[test]
    fn bounded_command_captures_both_streams_and_exact_cap() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "printf abcd; printf err >&2"]);
        let output = command_output_bounded(&mut command, Duration::from_secs(2), 4, 3).unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"abcd");
        assert_eq!(output.stderr, b"err");
    }

    #[test]
    fn journal_command_result_preserves_trailing_whitespace_for_redaction() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "printf 'secret \\n\\t'"]);
        let output = command_output_bounded(&mut command, Duration::from_secs(2), 1024, 1024)
            .unwrap();
        assert_eq!(
            command_result_raw("journalctl", &[], output).unwrap(),
            "secret \n\t"
        );
    }

    #[test]
    fn inherited_pipes_timeout_even_when_direct_child_has_exited() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "sleep 5 & exit 0"]);
        let started = Instant::now();
        let error = command_output_bounded(&mut command, Duration::from_millis(150), 1024, 1024)
            .unwrap_err();
        assert!(error.contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn oversized_stdout_and_stderr_are_killed_without_reader_joins() {
        for (script, stream) in [
            ("while :; do printf 0123456789abcdef; done", "stdout"),
            ("while :; do printf 0123456789abcdef >&2; done", "stderr"),
        ] {
            let mut command = Command::new("/bin/sh");
            command.args(["-c", script]);
            let error = command_output_bounded(&mut command, Duration::from_secs(2), 1024, 1024)
                .unwrap_err();
            assert!(error.contains(stream) && error.contains("limit"), "{error}");
        }
    }

    #[test]
    fn discovery_classifies_managed_units_without_authorization_or_nss() {
        let body = "# HonedTenant=t02\n# HonedPlan=standard\n# HonedKind=database\nMemoryMax=1G\nCPUQuota=75%\n";
        let record = discovered_reservation("t02-db", body).unwrap().unwrap();
        assert_eq!(record.tenant, "t02");
        assert_eq!(record.cpu_quota_percent, Some(75));
        assert_eq!(record.kind, Some(ServiceKind::Database));
        assert!(
            discovered_reservation("honenode-app", "[Container]\nImage=app\n")
                .unwrap()
                .is_none()
        );
        assert!(discovered_reservation("renamed", "# HonedPlan=standard\nMemoryMax=1G\n").is_err());
        assert!(discovered_reservation("t02-db", &body.replace("t02", "t01")).is_err());
        assert!(discovered_reservation("t02-db", "# HonedTenant=t02\n").is_err());
        assert!(discovered_reservation("t02-db", "MemoryMax=1G\n")
            .unwrap()
            .unwrap()
            .plan_code
            .is_none());
    }

    #[test]
    fn discovery_walks_removed_and_unresolved_uid_directories_and_ignores_platform_units() {
        static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        struct Fixture(PathBuf);
        impl Drop for Fixture {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        // Exercise the real directory traversal; only root ownership checks are
        // replaced so this fixture also runs under an unprivileged test runner.
        fn fixture_trust(path: &Path, directory: bool) -> Result<(), String> {
            let metadata = std::fs::symlink_metadata(path).map_err(|error| error.to_string())?;
            if (directory && metadata.is_dir()) || (!directory && metadata.is_file()) {
                Ok(())
            } else {
                Err("not a regular trusted fixture entry".to_string())
            }
        }
        let fixture = Fixture(std::env::temp_dir().join(format!(
            "honed-discovery-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        )));
        for uid in ["4000000001", "4000000002"] {
            std::fs::create_dir_all(fixture.0.join(uid)).unwrap();
        }
        std::fs::write(
            fixture.0.join("4000000001/t02-db.container"),
            "# HonedTenant=t02\n# HonedPlan=standard\nMemoryMax=1G\nCPUQuota=75%\n",
        )
        .unwrap();
        std::fs::write(
            fixture.0.join("4000000002/platform.container"),
            "[Container]\nImage=platform\n",
        )
        .unwrap();
        let records = discover_reservations(&fixture.0, fixture_trust).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].tenant, "t02");
        std::fs::write(
            fixture.0.join("4000000001/t02-db.container"),
            "# HonedTenant=t02\n",
        )
        .unwrap();
        assert!(discover_reservations(&fixture.0, fixture_trust).is_err());
    }
    #[test]
    fn generic_units_use_the_same_systemd_provenance_cached_limits_and_stopped_gates() {
        let path = Path::new("/etc/containers/systemd/users/1001/t01-custom.container");
        let body = "# HonedTenant=t01\n# HonedKind=container\n# HonedPlan=pro\nContainerName=t01-custom\nMemoryMax=384M\nCPUQuota=25%\n";
        let record = validate_unit_identity("t01", "t01-custom", body).unwrap();
        assert_eq!(record.kind, Some(ServiceKind::Container));
        let mut state = observed_unit("inactive");
        state.id = "t01-custom.service".into();
        state.source_path = path.into();
        state.fragment_path = "/run/user/1001/systemd/generator/t01-custom.service".into();
        state.cpu_quota_per_sec_usec = Some(250000);
        validate_activation(&state, 1001, path, &record).unwrap();
        validate_limit_update_state(&state, 1001, path, true).unwrap();
        state.job = "42".into();
        assert!(validate_limit_update_state(&state, 1001, path, true).is_err());
        state.fragment_path = "/home/t01/.config/systemd/user/t01-custom.service".into();
        assert!(validate_activation(&state, 1001, path, &record).is_err());
    }

}
