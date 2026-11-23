//! Generic rootless containers. Only this module accepts tenant image references;
//! no tenant string is ever a host command, host environment, or mount source.
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::capacity::Reservation;
use crate::config::Config;
use crate::host::{self, Envelope, ServiceKind};
use crate::service_config::{zero_string, Secret};

const INVALID: &str = "invalid container specification (details redacted)";
const STORED_INVALID: &str = "container metadata or environment is missing, untrusted, or inconsistent";
const MAX_ENV_BYTES: usize = 16 * 1024;

fn default_user() -> u32 { 1000 }

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContainerSpec {
    pub image: String,
    #[serde(default)]
    pub environment: BTreeMap<String, Secret>,
    pub container_port: u16,
    #[serde(default = "default_user")]
    pub user: u32,
    #[serde(default)]
    pub volumes: Vec<VolumeSpec>,
    #[serde(default)]
    pub read_only: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct VolumeSpec {
    pub name: String,
    pub target: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ContainerInfo {
    pub name: String,
    pub image: String,
    pub host_port: u16,
    pub container_port: u16,
    pub user: u32,
    pub read_only: bool,
    pub volumes: Vec<VolumeSpec>,
    pub environment_keys: Vec<String>,
}

// No cleartext environment in metadata. The canonical unit also preserves the
// CPU/memory reservation when deletion removes the live Quadlet (a tombstone).
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Stored {
    version: u32,
    tenant: String,
    info: ContainerInfo,
    quadlet: String,
}

pub fn valid_registry(value: &str) -> bool {
    if value.len() > 253 || value.is_empty() { return false; }
    let (host, port) = value.split_once(':').map_or((value, None), |(h, p)| (h, Some(p)));
    if port.is_some_and(|p| p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) || p.parse::<u16>().map_or(true, |n| n == 0)) {
        return false;
    }
    (host.contains('.') || host == "localhost" || port.is_some())
        && host.split('.').all(|part| !part.is_empty() && part.len() <= 63
            && part.as_bytes()[0].is_ascii_alphanumeric()
            && part.as_bytes()[part.len()-1].is_ascii_alphanumeric()
            && part.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'))
}

fn image_registry(image: &str) -> Option<&str> {
    if image.len() > 512 || !image.bytes().all(|b| b.is_ascii_alphanumeric() || b"/._-:@".contains(&b)) {
        return None;
    }
    let (registry, reference) = image.split_once('/')?;
    if !valid_registry(registry) { return None; }
    let repository = if let Some((repo, digest)) = reference.split_once("@sha256:") {
        if digest.len() != 64 || !digest.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) { return None; }
        repo
    } else {
        let (repo, tag) = reference.rsplit_once(':')?;
        if tag.is_empty() || tag.len() > 128 || tag.starts_with(['-', '.'])
            || !tag.bytes().all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b)) { return None; }
        repo
    };
    if !repository.split('/').all(|part| !part.is_empty()
        && part.as_bytes()[0].is_ascii_alphanumeric()
        && part.as_bytes()[part.len()-1].is_ascii_alphanumeric()
        && part.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b))) { return None; }
    Some(registry)
}

fn env_key(key: &str) -> bool {
    !key.is_empty() && key.len() <= 64
        && (key.as_bytes()[0].is_ascii_alphabetic() || key.starts_with('_'))
        && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

fn identifier(value: &str) -> bool {
    !value.is_empty() && value.len() <= 32 && value.as_bytes()[0].is_ascii_lowercase()
        && value.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_-".contains(&b))
}

fn below(path: &str, ancestor: &str) -> bool {
    path == ancestor || path.strip_prefix(ancestor).is_some_and(|rest| rest.starts_with('/'))
}

fn target_valid(path: &str) -> bool {
    path.len() <= 256 && path.starts_with('/') && path != "/"
        && path[1..].split('/').all(|part| !part.is_empty() && part != "." && part != ".."
            && part.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)))
        // /var/run is often a symlink to /run. No engine sockets or control trees.
        && !["/proc", "/sys", "/dev", "/run", "/var/run"].iter().any(|root| below(path, root))
        && !path.ends_with(".sock")
}

pub fn validate_name(tenant: &str, unit: &str) -> Result<(), String> {
    if !crate::shape_is_valid(tenant) { return Err(INVALID.to_string()); }
    crate::validate_unit_name(tenant, unit).map_err(|_| INVALID.to_string())?;
    let name = &unit[tenant.len()+1..];
    if name.is_empty() || name.starts_with('-') || name.ends_with('-')
        || ["site", "web", "db", "database", "vault", "files", "storage", "git", "forgejo"].contains(&name) {
        return Err(INVALID.to_string());
    }
    Ok(())
}

impl ContainerSpec {
    pub fn validate(&self, registries: &[String]) -> Result<(), String> {
        let mut total = 0;
        let mut names = BTreeSet::new();
        let valid = image_registry(&self.image).is_some_and(|registry| registries.iter().any(|r| r == registry))
            && self.container_port > 0 && (1..=65535).contains(&self.user)
            && self.environment.len() <= 32 && self.volumes.len() <= 8
            && self.environment.iter().all(|(key, value)| {
                total += key.len() + value.expose().len() + 2;
                env_key(key) && value.expose().len() <= 4096 && !value.expose().chars().any(char::is_control)
            })
            && total <= MAX_ENV_BYTES
            && self.volumes.iter().enumerate().all(|(i, volume)| {
                identifier(&volume.name) && names.insert(&volume.name) && target_valid(&volume.target)
                    && !self.volumes[..i].iter().any(|other| below(&other.target, &volume.target) || below(&volume.target, &other.target))
            });
        if valid { Ok(()) } else { Err(INVALID.to_string()) }
    }

    fn info(&self, unit: &str, port: u16) -> ContainerInfo {
        ContainerInfo { name: unit.to_string(), image: self.image.clone(), host_port: port,
            container_port: self.container_port, user: self.user, read_only: self.read_only,
            volumes: self.volumes.clone(), environment_keys: self.environment.keys().cloned().collect() }
    }

    fn environment_file(&self, port: u16) -> Result<String, String> {
        let port = port.to_string();
        let origin = format!("http://127.0.0.1:{port}");
        let mut total = 0;
        let mut body = String::new();
        for (key, value) in &self.environment {
            let mut expanded = value.expose()
                .replace("${HONE_HOST_PORT}", &port)
                .replace("${HONE_HTTP_ORIGIN}", &origin);
            total += key.len() + expanded.len() + 2;
            if expanded.len() > 4096 || total > MAX_ENV_BYTES {
                zero_string(&mut expanded);
                zero_string(&mut body);
                return Err(INVALID.to_string());
            }
            body.push_str(key); body.push('='); body.push_str(&expanded); body.push('\n');
            zero_string(&mut expanded);
        }
        Ok(body)
    }
}

fn metadata_dir(config: &Config) -> PathBuf { config.etc_hosting.join("containers") }
fn metadata_path(config: &Config, unit: &str) -> PathBuf { metadata_dir(config).join(format!("{unit}.json")) }
fn environment_path(config: &Config, unit: &str) -> PathBuf { metadata_dir(config).join(format!("{unit}.env")) }
fn volume_root(config: &Config, tenant: &str, unit: &str) -> PathBuf { config.tenant_data_root.join(tenant).join("containers").join(unit) }

pub fn exists(config: &Config, unit: &str) -> Result<bool, String> {
    let path = metadata_path(config, unit);
    match fs::symlink_metadata(&path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "container metadata stat failed");
            Err(STORED_INVALID.to_string())
        }
    }
}

fn read_private(path: &Path, gid: u32, mode: u32, cap: u64) -> Result<String, String> {
    host::validate_root_owned_path(path).map_err(|_| STORED_INVALID.to_string())?;
    let file = OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path).map_err(|_| STORED_INVALID.to_string())?;
    let m = file.metadata().map_err(|_| STORED_INVALID.to_string())?;
    if !m.is_file() || m.uid() != 0 || m.gid() != gid || m.mode() & 0o777 != mode || m.nlink() != 1 { return Err(STORED_INVALID.to_string()); }
    let mut body = String::new();
    file.take(cap+1).read_to_string(&mut body).map_err(|_| STORED_INVALID.to_string())?;
    if body.len() as u64 > cap { zero_string(&mut body); return Err(STORED_INVALID.to_string()); }
    Ok(body)
}

fn envelope(body: &str) -> Result<Envelope, String> {
    let get = |prefix| host::unit_directive(body, prefix);
    let mut env = Envelope::new("container", get("CPUWeight=")?.parse().map_err(|_| STORED_INVALID)?,
        get("MemoryHigh=")?, get("MemoryMax=")?, get("TasksMax=")?.parse().map_err(|_| STORED_INVALID)?)?;
    env.plan_code = Some(get("# HonedPlan=")?.to_string());
    env.cpu_quota_percent = get("CPUQuota=")?.strip_suffix('%').ok_or(STORED_INVALID)?.parse().map_err(|_| STORED_INVALID)?;
    Ok(env)
}

fn read_stored(config: &Config, unit: &str) -> Result<Stored, String> {
    let body = read_private(&metadata_path(config, unit), 0, 0o600, 32*1024)?;
    parse_stored(config, unit, &body)
}

fn parse_stored(config: &Config, unit: &str, body: &str) -> Result<Stored, String> {
    let stored: Stored = serde_json::from_str(body).map_err(|_| STORED_INVALID.to_string())?;
    validate_name(&stored.tenant, unit)?;
    if stored.version != 1 || stored.info.name != unit || !(30000..40000).contains(&stored.info.host_port) { return Err(STORED_INVALID.to_string()); }
    let info = &stored.info;
    let spec = ContainerSpec { image: info.image.clone(), environment: BTreeMap::new(), container_port: info.container_port,
        user: info.user, volumes: info.volumes.clone(), read_only: info.read_only };
    spec.validate(&[image_registry(&info.image).ok_or(STORED_INVALID)?.to_string()])?;
    if info.environment_keys.len() > 32 || info.environment_keys.iter().any(|k| !env_key(k))
        || info.environment_keys.windows(2).any(|w| w[0] >= w[1]) { return Err(STORED_INVALID.to_string()); }
    let env = envelope(&stored.quadlet)?;
    if stored.quadlet != render(config, &stored.tenant, info, &env)? { return Err(STORED_INVALID.to_string()); }
    Ok(stored)
}

fn all_stored(config: &Config) -> Result<Vec<Stored>, String> {
    let dir = metadata_dir(config);
    match fs::symlink_metadata(&dir) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(_) => return Err(STORED_INVALID.to_string()),
        Ok(_) => host::validate_root_owned_entry(&dir, true)?,
    }
    let mut records = Vec::new();
    let mut ports = BTreeSet::new();
    for entry in fs::read_dir(&dir).map_err(|_| STORED_INVALID)? {
        let path = entry.map_err(|_| STORED_INVALID)?.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") { continue; }
        let unit = path.file_stem().and_then(|s| s.to_str()).ok_or(STORED_INVALID)?;
        // A single stale tombstone (for example, metadata written by an older
        // render before its unit was committed) must not brick admission for
        // every other service. Skip it and keep the rest of the reservations.
        let stored = match read_stored(config, unit) {
            Ok(stored) => stored,
            Err(error) => {
                tracing::warn!(unit, %error, "ignoring inconsistent container metadata");
                continue;
            }
        };
        if !ports.insert(stored.info.host_port) { return Err(STORED_INVALID.to_string()); }
        records.push(stored);
    }
    Ok(records)
}

pub fn merge_reservations(config: &Config, reservations: &mut Vec<Reservation>) -> Result<(), String> {
    for stored in all_stored(config)? {
        // Metadata whose Quadlet unit no longer exists is an orphan left by a
        // removed or never-started service: it must not count toward admission
        // or the tenant's service quota.
        if !unit_present(config, &stored.tenant, &stored.info.name)? {
            continue;
        }
        merge_record(reservations, &stored)?;
    }
    // A generic unit without root metadata is never an admissible reservation.
    for r in reservations.iter().filter(|r| r.kind == Some(ServiceKind::Container)) {
        if !exists(config, &r.unit)? { return Err(STORED_INVALID.to_string()); }
    }
    Ok(())
}

fn unit_present(config: &Config, tenant: &str, unit: &str) -> Result<bool, String> {
    let uid = match host::user_uid(tenant) {
        Ok(uid) => uid,
        // A missing tenant account makes every record for it an orphan.
        Err(_) => return Ok(false),
    };
    let path = config
        .quadlet_users
        .join(uid.to_string())
        .join(format!("{unit}.container"));
    Ok(path.exists())
}

fn merge_record(reservations: &mut Vec<Reservation>, stored: &Stored) -> Result<(), String> {
    let record = host::parse_reservation(&stored.tenant, &stored.info.name, &stored.quadlet)?;
    if let Some(existing) = reservations.iter().find(|r| r.unit == record.unit) {
        if existing != &record { return Err(STORED_INVALID.to_string()); }
    } else { reservations.push(record); }
    Ok(())
}

fn choose_port(reserved: &BTreeSet<u16>, mut available: impl FnMut(u16) -> bool) -> Result<u16, String> {
    (30000..40000).find(|p| !reserved.contains(p) && available(*p))
        .ok_or_else(|| "no free container loopback ports".to_string())
}

fn allocate_port(config: &Config) -> Result<u16, String> {
    let mut reserved: BTreeSet<_> = all_stored(config)?.iter().map(|s| s.info.host_port).collect();
    // Include every root-committed publication, even a legacy/platform unit.
    host::validate_root_owned_entry(&config.quadlet_users, true)?;
    for dir in fs::read_dir(&config.quadlet_users).map_err(|_| STORED_INVALID)? {
        let dir = dir.map_err(|_| STORED_INVALID)?;
        if dir.file_name().to_string_lossy().parse::<u32>().is_err() { continue; }
        host::validate_root_owned_entry(&dir.path(), true)?;
        for file in fs::read_dir(dir.path()).map_err(|_| STORED_INVALID)? {
            let path = file.map_err(|_| STORED_INVALID)?.path();
            if path.extension().and_then(|s| s.to_str()) != Some("container") { continue; }
            host::validate_root_owned_path(&path)?;
            let body = fs::read_to_string(path).map_err(|_| STORED_INVALID)?;
            for line in body.lines().filter_map(|l| l.strip_prefix("PublishPort=")) {
                // Broker-managed units use a single numeric host port.
                let parts: Vec<_> = line.split(':').collect();
                if parts.len() != 3 { return Err("unrecognized committed port reservation".to_string()); }
                reserved.insert(parts[1].parse().map_err(|_| STORED_INVALID)?);
            }
        }
    }
    choose_port(&reserved, |port| std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).is_ok())
}

pub(crate) fn render(config: &Config, tenant: &str, info: &ContainerInfo, env: &Envelope) -> Result<String, String> {
    validate_name(tenant, &info.name)?;
    for path in [&config.etc_hosting, &config.tenant_data_root] {
        if !path.is_absolute() || !path.to_str().is_some_and(|s| s.bytes().all(|b| b.is_ascii_alphanumeric() || b"/._-".contains(&b)) && !s.split('/').any(|p| p == ".." || p == ".")) { return Err("unsafe configured container path".to_string()); }
    }
    if env.kind != ServiceKind::Container || env.cpu_quota_percent == 0 || !env.plan_code.as_deref().is_some_and(crate::policy::valid_code) { return Err(STORED_INVALID.to_string()); }
    let volumes: String = info.volumes.iter().map(|v| format!("Volume={}/{}:{}:Z\n", volume_root(config, tenant, &info.name).display(), v.name, v.target)).collect();
    // Node and other runtimes expect a writable /tmp even when the image root
    // is read-only; the tmpfs is private to the container and never persisted.
    let tmpfs = if info.read_only { "Tmpfs=/tmp:mode=1777\n" } else { "" };
    Ok(format!("# HonedTenant={tenant}\n# HonedPlan={plan}\n# HonedKind=container\n\
        [Unit]\nDescription=SharpNode container {unit}\n\n\
        [Container]\nContainerName={unit}\nImage={image}\nPull=missing\nNetwork=snp-{tenant}.network\n\
        PublishPort=127.0.0.1:{host_port}:{port}\nUserNS=keep-id:uid={user},gid={user}\nUser={user}\n\
        EnvironmentFile={envfile}\n{volumes}ReadOnly={read_only}\n{tmpfs}DropCapability=all\nNoNewPrivileges=true\n\
        PidsLimit={tasks}\nLogDriver=journald\n\n[Service]\nRestart=always\nMemoryHigh={high}\nMemoryMax={max}\n\
        CPUWeight={weight}\nCPUQuota={cpu}%\nTasksMax={tasks}\nIOWeight=100\n\n[Install]\nWantedBy=default.target\n",
        plan=env.plan_code.as_deref().unwrap_or(""), unit=info.name, image=info.image, host_port=info.host_port,
        port=info.container_port, user=info.user, envfile=environment_path(config, &info.name).display(), read_only=info.read_only,
        tasks=env.tasks_max, high=env.memory_high, max=env.memory_max, weight=env.cpu_weight, cpu=env.cpu_quota_percent))
}

// Walk data directories using pinned descriptors: a tenant symlink must never
// redirect root's mkdir/chown/chmod operations outside the managed data tree.
fn child_dir(parent: &File, name: &str, uid: u32, gid: u32, create: bool) -> Result<File, String> {
    let name = std::ffi::CString::new(name).map_err(|_| STORED_INVALID)?;
    if create {
        let result = unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o750) };
        if result != 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::AlreadyExists { return Err(STORED_INVALID.to_string()); }
    }
    let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC) };
    if fd < 0 { return Err(STORED_INVALID.to_string()); }
    let file = unsafe { File::from_raw_fd(fd) };
    let m = file.metadata().map_err(|_| STORED_INVALID)?;
    if create && m.uid() == 0 {
        if unsafe { libc::fchown(fd, uid, gid) } != 0 || unsafe { libc::fchmod(fd, if uid == 0 { 0o755 } else { 0o750 }) } != 0 { return Err(STORED_INVALID.to_string()); }
    }
    let m = file.metadata().map_err(|_| STORED_INVALID)?;
    if m.uid() != uid || m.gid() != gid || m.mode() & 0o022 != 0 { return Err(STORED_INVALID.to_string()); }
    Ok(file)
}

fn volumes(config: &Config, tenant: &str, info: &ContainerInfo, create: bool) -> Result<(), String> {
    host::validate_root_owned_entry(&config.tenant_data_root, true)?;
    let root = OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW).open(&config.tenant_data_root).map_err(|_| STORED_INVALID)?;
    let uid = host::user_uid(tenant)?;
    let gid = host::user_gid(tenant)?;
    if uid == 0 || gid == 0 { return Err("generic containers require a non-root tenant account and group".to_string()); }
    let tenant_dir = child_dir(&root, tenant, uid, gid, false)?;
    let containers = child_dir(&tenant_dir, "containers", 0, 0, create)?;
    let unit = child_dir(&containers, &info.name, 0, 0, create)?;
    for v in &info.volumes { child_dir(&unit, &v.name, uid, gid, create)?; }
    Ok(())
}

fn read_environment(config: &Config, tenant: &str, info: &ContainerInfo) -> Result<BTreeMap<String, Secret>, String> {
    let path = environment_path(config, &info.name);
    for ancestor in path.parent().ok_or(STORED_INVALID)?.ancestors() {
        if fs::symlink_metadata(ancestor).map_err(|_| STORED_INVALID)?.mode() & 0o001 == 0 { return Err(STORED_INVALID.to_string()); }
    }
    let mut body = read_private(&path, host::user_gid(tenant)?, 0o640, MAX_ENV_BYTES as u64)?;
    let mut environment = BTreeMap::new();
    let result = (|| {
        for line in body.lines() {
            let (key, value) = line.split_once('=').ok_or(STORED_INVALID)?;
            if environment.insert(key.to_string(), Secret::from_string(value.to_string())).is_some() { return Err(STORED_INVALID.to_string()); }
        }
        if environment.keys().cloned().collect::<Vec<_>>() != info.environment_keys { return Err(STORED_INVALID.to_string()); }
        Ok(())
    })();
    zero_string(&mut body);
    result?;
    Ok(environment)
}

pub fn validate_provenance(config: &Config, tenant: &str, unit: &str, body: &str) -> Result<ContainerInfo, String> {
    let stored = read_stored(config, unit)
        .map_err(|_| "container metadata is missing or untrusted".to_string())?;
    if stored.tenant != tenant || stored.quadlet != body {
        return Err("container metadata does not match the committed unit".to_string());
    }
    let environment = read_environment(config, tenant, &stored.info)
        .map_err(|_| "container environment is missing or untrusted".to_string())?;
    let spec = ContainerSpec { image: stored.info.image.clone(), environment,
        container_port: stored.info.container_port, user: stored.info.user, volumes: stored.info.volumes.clone(), read_only: stored.info.read_only };
    spec.validate(&[image_registry(&spec.image).ok_or(STORED_INVALID)?.to_string()])
        .map_err(|_| "container specification is invalid".to_string())?;
    volumes(config, tenant, &stored.info, false)
        .map_err(|_| "container volume is missing or unsafe".to_string())?;
    Ok(stored.info)
}

pub fn validate_committed(config: &Config, tenant: &str, unit: &str, body: &str, env: &Envelope) -> Result<ContainerInfo, String> {
    let info = validate_provenance(config, tenant, unit, body)?;
    let policy = crate::policy::load(config)?;
    if !image_registry(&info.image).is_some_and(|r| policy.allowed_registries.iter().any(|allowed| allowed == r)) {
        return Err("container image registry is not allowed by policy".to_string());
    }
    if render(config, tenant, &info, env)? != body {
        return Err("container unit does not match its committed metadata".to_string());
    }
    Ok(info)
}

pub fn inspect(config: &Config, tenant: &str, unit: &str) -> Result<ContainerInfo, String> {
    let (_, body, reservation) = host::committed_unit(config, tenant, unit)?;
    if reservation.kind != Some(ServiceKind::Container) { return Err("not a generic container".to_string()); }
    let env = crate::policy::load(config)?.validate_activation(&reservation, &body)?;
    validate_committed(config, tenant, unit, &body, &env)
}

pub fn create(config: &Config, tenant: &str, unit: &str, spec: &ContainerSpec, env: &Envelope) -> Result<ContainerInfo, String> {
    validate_name(tenant, unit)?;
    spec.validate(&crate::policy::load(config)?.allowed_registries)?;
    if exists(config, unit)? { return Err("container name is permanently reserved; choose a new name".to_string()); }
    host::ensure_limit_update_stopped(config, tenant, unit)?;
    let dir = host::ensure_quadlet_dir(config, tenant)?;
    let path = dir.join(format!("{unit}.container"));
    if fs::symlink_metadata(&path).is_ok() { return Err("service name already committed".to_string()); }
    let info = spec.info(unit, allocate_port(config)?);
    let body = render(config, tenant, &info, env)?;
    crate::provisioning::ensure_root_directory(&config.etc_hosting, 0o755)?;
    crate::provisioning::ensure_root_directory(&metadata_dir(config), 0o711)?;
    volumes(config, tenant, &info, true)?;
    let stage = dir.join(".honed-container-staging");
    crate::provisioning::ensure_root_directory(&stage, 0o755)?;
    let mut environment = spec.environment_file(info.host_port)?;
    let result = (|| {
        let staged = stage.join(format!("{unit}.container"));
        host::write_atomic(&staged, &body)?;
        let network = dir.join(format!("snp-{tenant}.network"));
        host::validate_root_owned_path(&network)?;
        fs::copy(&network, stage.join(format!("snp-{tenant}.network"))).map_err(|_| STORED_INVALID)?;
        host::validate_unit(tenant, &stage, unit).map_err(|_| "container unit validation failed (details redacted)".to_string())?;
        // Commit a durable tombstone first. Any crash/failure after this point
        // intentionally consumes the name, port, CPU and memory until review.
        let stored = Stored { version: 1, tenant: tenant.to_string(), info: info.clone(), quadlet: body.clone() };
        let metadata = serde_json::to_vec(&stored).map_err(|_| STORED_INVALID)?;
        let file = crate::provisioning::create_new(&metadata_path(config, unit), 0o600).map_err(|_| STORED_INVALID)?;
        crate::provisioning::initialize_file(file, &metadata, 0, 0, 0o600)?;
        File::open(metadata_dir(config)).and_then(|f| f.sync_all()).map_err(|_| STORED_INVALID)?;
        let file = crate::provisioning::create_new(&environment_path(config, unit), 0o640).map_err(|_| STORED_INVALID)?;
        crate::provisioning::initialize_file(file, environment.as_bytes(), 0, host::user_gid(tenant)?, 0o640)?;
        File::open(metadata_dir(config))
            .and_then(|f| f.sync_all())
            .map_err(|_| STORED_INVALID)?;
        validate_committed(config, tenant, unit, &body, env)?;
        fs::rename(staged, &path).map_err(|_| STORED_INVALID)?;
        File::open(&dir).and_then(|f| f.sync_all()).map_err(|_| STORED_INVALID)?;
        host::activate_unit(config, tenant, unit, "start")?;
        Ok(info)
    })();
    zero_string(&mut environment);
    let _ = fs::remove_dir_all(&stage);
    result
}

pub fn redact_logs(config: &Config, tenant: &str, info: &ContainerInfo, output: String) -> Result<String, String> {
    let environment = read_environment(config, tenant, info)?;
    Ok(redact_values(&environment, output))
}

fn redact_values(environment: &BTreeMap<String, Secret>, mut output: String) -> String {
    // Longest first avoids partial substitution of a shared-prefix secret.
    let mut values: Vec<_> = environment.values().map(Secret::expose).filter(|v| !v.is_empty()).collect();
    values.sort_by_key(|v| std::cmp::Reverse(v.len()));
    for value in values { output = output.replace(value, "[REDACTED]"); }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn spec() -> ContainerSpec {
        serde_json::from_value(json!({
            "image":"codeberg.org/forgejo/forgejo:17-rootless", "container_port":3000,
            "environment":{"ADMIN_PASSWORD":"unique-secret-value", "LD_PRELOAD":"/app/lib.so", "CONTAINER_HOST":"ignored-by-host"},
            "volumes":[{"name":"data","target":"/var/lib/gitea"},{"name":"config","target":"/etc/gitea"}]
        })).unwrap()
    }

    fn config() -> Config {
        Config { etc_hosting: "/etc/hosting".into(), tenant_data_root: "/srv/hosting".into(), ..Config::from_env() }
    }

    fn env() -> Envelope {
        let mut env = Envelope::new("container", 100, "256M", "384M", 128).unwrap();
        env.plan_code = Some("generic".to_string());
        env.cpu_quota_percent = 25;
        env
    }

    fn stored() -> Stored {
        let info = spec().info("t01-forge-one", 30000);
        Stored { version: 1, tenant: "t01".to_string(), quadlet: render(&config(), "t01", &info, &env()).unwrap(), info }
    }

    #[test]
    fn arbitrary_images_require_explicit_registry_and_tag_or_digest() {
        let mut spec = spec();
        assert!(spec.validate(&[]).is_err());
        for image in ["codeberg.org/forgejo/forgejo:17-rootless", "codeberg.org/another/project:1.2"] {
            spec.image = image.to_string();
            assert!(spec.validate(&["codeberg.org".to_string()]).is_ok());
        }
        // The local demo blog is preloaded as localhost/... and Pull=missing
        // finds it; the policy must still allow the localhost registry.
        spec.image = "localhost/honenode/seclab:1.0".to_string();
        assert!(spec.validate(&["localhost".to_string()]).is_ok());
        assert!(spec.validate(&["docker.io".to_string()]).is_err());
        spec.image = format!("registry.example:5000/team/image@sha256:{}", "a".repeat(64));
        assert!(spec.validate(&["registry.example:5000".to_string()]).is_ok());
        for image in ["nginx", "nginx:latest", "library/nginx:latest", "https://codeberg.org/a:1", "codeberg.org/a", "codeberg.org/u:p@a:1", "u:p@codeberg.org/a:1", "evilcodeberg.org/a:1", "codeberg.org.evil/a:1", "codeberg.org/a:1 --privileged", "codeberg.org/a:1\nExec=/bin/sh", "codeberg.org/a:1%h", "codeberg.org/../a:1", "codeberg.org/a@sha256:abc", "codeberg.org/a:1@sha256:abc"] {
            spec.image = image.to_string();
            let error = spec.validate(&["codeberg.org".to_string()]).unwrap_err();
            assert_eq!(error, INVALID, "{image}");
        }
        for registry in ["codeberg.org/path", "u:p@codeberg.org", "https://codeberg.org", "codeberg.org:0", "codeberg.org:65536", "codeberg.org\n", "-bad.example", "codeberg.org:"] { assert!(!valid_registry(registry)); }
        spec.image = format!("codeberg.org/a:{}", "x".repeat(513));
        assert!(spec.validate(&["codeberg.org".to_string()]).is_err());
    }

    #[test]
    fn environment_is_container_only_bounded_literal_and_redacted() {
        let mut spec = spec();
        assert!(spec.validate(&["codeberg.org".into()]).is_ok());
        assert_eq!(spec.user, 1000);
        assert!(!format!("{spec:?}").contains("unique-secret-value"));
        assert!(format!("{spec:?}").contains("REDACTED"));
        spec.environment.insert("LITERAL".into(), Secret::new("a=$HOME # 'quoted' \\\" = b"));
        assert!(spec.environment_file(30001).unwrap().contains("LITERAL=a=$HOME # 'quoted' \\\" = b\n"));
        for key in ["", "9KEY", "bad-key", "A=B", "é", &"A".repeat(65)] {
            spec.environment.insert(key.into(), Secret::new("sensitive"));
            assert_eq!(spec.validate(&["codeberg.org".into()]).unwrap_err(), INVALID);
            spec.environment.remove(key);
        }
        for value in ["line\nbreak".to_string(), "nul\0byte".to_string(), "x".repeat(4097)] {
            spec.environment.insert("BAD".into(), Secret::new(&value));
            assert!(spec.validate(&["codeberg.org".into()]).is_err());
        }
        spec.environment.clear();
        for i in 0..33 { spec.environment.insert(format!("K{i}"), Secret::new("v")); }
        assert!(spec.validate(&["codeberg.org".into()]).is_err());
        spec.environment.clear();
        for i in 0..4 { spec.environment.insert(format!("K{i}"), Secret::new(&"x".repeat(4096))); }
        assert!(spec.validate(&["codeberg.org".into()]).is_err());
        let environment = BTreeMap::from([("A".into(), Secret::new("secret")), ("B".into(), Secret::new("secret-long"))]);
        assert_eq!(redact_values(&environment, "secret-long secret".into()), "[REDACTED] [REDACTED]");
        assert_eq!(
            redact_values(&environment, "secret \n\t".into()),
            "[REDACTED] \n\t"
        );
    }

    #[test]
    fn environment_expands_only_reserved_port_placeholders_and_rechecks_limits() {
        let mut spec = spec();
        spec.environment.clear();
        spec.environment.insert("ROOT_URL".into(), Secret::new("${HONE_HTTP_ORIGIN}/"));
        spec.environment.insert("EXACT_PORT".into(), Secret::new("${HONE_HOST_PORT}"));
        spec.environment.insert("EMBEDDED".into(), Secret::new("port=${HONE_HOST_PORT};origin=${HONE_HTTP_ORIGIN}/"));
        spec.environment.insert("LITERAL".into(), Secret::new("$HOME ${HOME} $HONE_HOST_PORT ${HONE_UNKNOWN}"));
        let environment = spec.environment_file(30001).unwrap();
        assert!(environment.contains("ROOT_URL=http://127.0.0.1:30001/\n"));
        assert!(environment.contains("EXACT_PORT=30001\n"));
        assert!(environment.contains("EMBEDDED=port=30001;origin=http://127.0.0.1:30001/\n"));
        assert!(environment.contains("LITERAL=$HOME ${HOME} $HONE_HOST_PORT ${HONE_UNKNOWN}\n"));

        spec.environment.clear();
        let placeholder = "${HONE_HTTP_ORIGIN}";
        let raw = format!("{}{}", "x".repeat(4096 - placeholder.len()), placeholder);
        spec.environment.insert("TOO_LONG".into(), Secret::new(&raw));
        assert!(spec.validate(&["codeberg.org".into()]).is_ok());
        assert_eq!(spec.environment_file(30001).unwrap_err(), INVALID);

        spec.environment.clear();
        let raw = format!("{}{}", placeholder.repeat(32), "x".repeat(4000 - 32 * placeholder.len()));
        for i in 0..4 { spec.environment.insert(format!("K{i}"), Secret::new(&raw)); }
        assert!(spec.validate(&["codeberg.org".into()]).is_ok());
        assert_eq!(spec.environment_file(30001).unwrap_err(), INVALID);
    }

    #[test]
    fn volumes_reject_control_trees_traversal_sockets_duplicates_and_overlap() {
        let mut spec = spec();
        for target in ["/", "relative", "/a/../b", "/a/./b", "/a//b", "/a/", "/proc", "/proc/x", "/sys", "/dev", "/run", "/run/user/1000/podman/podman.sock", "/var/run", "/var/run/docker.sock", "/tmp/docker.sock", "/a:b", "/a%h", "/a\nb"] {
            spec.volumes = vec![VolumeSpec { name: "data".into(), target: target.into() }];
            assert!(spec.validate(&["codeberg.org".into()]).is_err(), "{target}");
        }
        for name in ["../x", "", "Upper", "-bad", &"x".repeat(33)] {
            spec.volumes = vec![VolumeSpec { name: name.into(), target: "/data".into() }];
            assert!(spec.validate(&["codeberg.org".into()]).is_err());
        }
        for targets in [["/data", "/data"], ["/data", "/data/child"], ["/data/child", "/data"]] {
            spec.volumes = targets.iter().enumerate().map(|(i,t)| VolumeSpec { name: format!("v{i}"), target: t.to_string() }).collect();
            assert!(spec.validate(&["codeberg.org".into()]).is_err());
        }
        spec.volumes = vec![VolumeSpec {name:"data".into(), target:"/data".into()}, VolumeSpec {name:"data".into(), target:"/other".into()}];
        assert!(spec.validate(&["codeberg.org".into()]).is_err());
        spec.volumes = (0..9).map(|i| VolumeSpec {name:format!("v{i}"),target:format!("/data{i}")}).collect();
        assert!(spec.validate(&["codeberg.org".into()]).is_err());
        spec.volumes.pop();
        assert!(spec.validate(&["codeberg.org".into()]).is_ok());
    }

    #[test]
    fn names_and_users_do_not_collide_with_templates_or_allow_root() {
        for unit in ["t01-site", "t01-web", "t01-db", "t01-vault", "t01-git", "t01-forgejo", "t02-app", "t01-", "t01--app"] { assert!(validate_name("t01", unit).is_err()); }
        assert!(validate_name("t01", "t01-forge-one").is_ok());
        let mut spec = spec();
        for user in [0, 65536, u32::MAX] { spec.user = user; assert!(spec.validate(&["codeberg.org".into()]).is_err()); }
        spec.user = 65535;
        assert!(spec.validate(&["codeberg.org".into()]).is_ok());
        spec.container_port = 0;
        assert!(spec.validate(&["codeberg.org".into()]).is_err());
    }

    #[test]
    fn render_preserves_image_startup_and_only_broker_paths_and_safe_envelope() {
        let stored = stored();
        let body = &stored.quadlet;
        for directive in ["Pull=missing\n", "Network=snp-t01.network\n", "PublishPort=127.0.0.1:30000:3000\n", "UserNS=keep-id:uid=1000,gid=1000\nUser=1000\n", "EnvironmentFile=/etc/hosting/containers/t01-forge-one.env\n", "Volume=/srv/hosting/t01/containers/t01-forge-one/data:/var/lib/gitea:Z\n", "DropCapability=all\nNoNewPrivileges=true\n", "CPUQuota=25%\n", "MemoryMax=402653184\n"] { assert!(body.contains(directive), "{directive}"); }
        for absent in ["Entrypoint=", "Exec=", "Environment=", "unique-secret-value", "CONTAINER_HOST", "PodmanArgs=", "Privileged=", "AddDevice=", "Network=host"] { assert!(!body.contains(absent)); }
        let value = serde_json::to_value(&stored.info).unwrap();
        assert_eq!(value.as_object().unwrap().len(), 8);
        assert_eq!(value["environment_keys"], json!(["ADMIN_PASSWORD", "CONTAINER_HOST", "LD_PRELOAD"]));
        let metadata = serde_json::to_string(&stored).unwrap();
        assert!(!metadata.contains("unique-secret-value"));
        assert!(!value.to_string().contains("/srv/hosting"));
        assert!(!value.to_string().contains("/etc/hosting"));
        assert!(parse_stored(&config(), &stored.info.name, &metadata).is_ok());
        for tampered in [body.replace("DropCapability=all", "DropCapability=none"), format!("{body}Volume=/etc:/host:Z\n"), body.replace("User=1000", "User=0"), body.replace("Image=codeberg.org/forgejo/forgejo:17-rootless", "Image=evil.example/a:1"), body.replace("MemoryMax=402653184", "MemoryMax=infinity")] {
            let mut altered: serde_json::Value = serde_json::from_str(&metadata).unwrap();
            altered["quadlet"] = json!(tampered);
            assert!(parse_stored(&config(), &stored.info.name, &altered.to_string()).is_err());
        }
    }

    #[test]
    fn tombstones_keep_ports_and_capacity_and_do_not_double_count_live_units() {
        let stored = stored();
        let mut records = vec![];
        merge_record(&mut records, &stored).unwrap();
        assert_eq!(records.len(), 1); // No live Quadlet exists, still reserved.
        let size = crate::capacity::MemorySize::parse;
        let budget = crate::capacity::CapacityPolicy::new(size("2G").unwrap(), size("1G").unwrap(), size("512M").unwrap()).unwrap();
        assert!(crate::capacity::admit(budget, &records, "t01", "t01-new", size("384M").unwrap()).is_err());
        merge_record(&mut records, &stored).unwrap();
        assert_eq!(records.len(), 1);
        let reserved = BTreeSet::from([stored.info.host_port]);
        assert_eq!(choose_port(&reserved, |p| p != 30001).unwrap(), 30002);
        assert!(choose_port(&(30000..40000).collect(), |_| true).is_err());
        records[0].cpu_quota_percent = Some(99);
        assert!(merge_record(&mut records, &stored).is_err());
    }

    #[test]
    fn quadlet_dryrun_keeps_envfile_out_of_host_environment_and_default_entrypoint() {
        let executable = Path::new("/usr/libexec/podman/quadlet");
        if !executable.is_file() { return; }
        let path = std::env::temp_dir().join(format!("honed-generic-dryrun-{}", std::process::id()));
        fs::create_dir(&path).unwrap();
        struct Cleanup(PathBuf);
        impl Drop for Cleanup { fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); } }
        let _cleanup = Cleanup(path.clone());
        fs::write(path.join("t01-forge-one.container"), stored().quadlet).unwrap();
        fs::write(path.join("snp-t01.network"), "[Network]\nNetworkName=snp-t01\nInternal=true\n").unwrap();
        let output = std::process::Command::new(executable).args(["-user", "-dryrun"]).env("QUADLET_UNIT_DIRS", &path).output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let output = String::from_utf8(output.stdout).unwrap();
        assert!(output.contains("--pull missing"), "{output}");
        assert!(output.contains("--env-file /etc/hosting/containers/t01-forge-one.env"));
        assert!(output.contains("--userns keep-id:uid=1000,gid=1000"));
        assert!(output.contains("--cap-drop all"));
        assert!(output.contains("--publish 127.0.0.1:30000:3000"));
        assert!(!output.contains("--entrypoint"));
        assert!(!output.contains("unique-secret-value"));
    }
}
