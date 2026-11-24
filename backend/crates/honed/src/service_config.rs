//! Typed service options shared with the app, plus the broker-owned FileBrowser
//! credential bootstrap. Secrets never appear in `Debug`, errors, audit logs,
//! responses, or command argv.

use std::fmt;
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::host::ServiceKind;

/// Image entrypoint override: skips the image's `tini -- /init.sh` wrapper,
/// which would copy a default config and could start without credentials.
pub const FILEBROWSER_ENTRYPOINT: &str = "/bin/filebrowser";
/// Fixed CLI flags. They take precedence over settings stored in the
/// FileBrowser database, so the admin UI cannot re-enable the command runner,
/// external symlink following, or no-auth mode.
pub const FILEBROWSER_EXEC: &str = "--config /config/settings.json --database /database/filebrowser.db --root /srv --port 8080 --address 0.0.0.0 --disableExec=true --followExternalSymlinks=false --noauth=false";
pub const FILEBROWSER_DATABASE: &str = "filebrowser.db";
pub const STORAGE_READ_ONLY_MARKER: &str = "# HonedStorageReadOnly=";

const BCRYPT_COST: u32 = 12;
const MAX_SETTINGS_BYTES: u64 = 4096;
const USERNAME_MAX: usize = 32;
const PASSWORD_MIN: usize = 12;
const PASSWORD_MAX: usize = 72;

/// A secret string: redacted in `Debug`, best-effort zeroed on drop.
#[derive(Default, Deserialize)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    pub(crate) fn from_string(value: String) -> Self { Self(value) }

    #[cfg(test)]
    pub fn new(value: &str) -> Self {
        Self(value.to_string())
    }

    pub(crate) fn expose(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("REDACTED")
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        zero_string(&mut self.0);
    }
}

impl Serialize for Secret {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

pub(crate) fn zero_string(value: &mut String) {
    // SAFETY: zero bytes are valid UTF-8; volatile writes discourage elision.
    unsafe {
        for byte in value.as_mut_vec().iter_mut() {
            std::ptr::write_volatile(byte, 0);
        }
    }
    value.clear();
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceOptions {
    #[serde(default)]
    pub image: Option<String>,
    #[serde(default)]
    pub file_manager: Option<FileManagerOptions>,
    #[serde(default)]
    pub database: Option<DatabaseOptions>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileManagerOptions {
    pub username: String,
    pub password: Secret,
    #[serde(default)]
    pub read_only: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DatabaseOptions {
    pub password: Secret,
}

/// Sanitized configuration returned by `limits.set`, `service.inspect`, and
/// `service.configure`. Never contains secrets, hashes, or host paths.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ServiceConfiguration {
    pub kind: ServiceKind,
    pub image: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_only: Option<bool>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WriteMode {
    /// `limits.set`: initial creation or same-unit re-render.
    Provision,
    /// `service.configure`: change mutable settings of an existing unit.
    Configure,
}

/// What a storage unit write must do after all checks pass.
#[derive(Debug)]
pub enum StoragePlan {
    Create {
        username: String,
        hash: Secret,
        read_only: bool,
    },
    Existing {
        username: String,
        read_only: bool,
    },
}

impl StoragePlan {
    pub fn read_only(&self) -> bool {
        match self {
            Self::Create { read_only, .. } | Self::Existing { read_only, .. } => *read_only,
        }
    }

    pub fn username(&self) -> &str {
        match self {
            Self::Create { username, .. } | Self::Existing { username, .. } => username,
        }
    }
}

pub fn validate_username(value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > USERNAME_MAX
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_.-".contains(&byte))
    {
        return Err(
            "invalid file_manager.username: expected 1-32 ASCII letters, digits, _ . -"
                .to_string(),
        );
    }
    Ok(())
}

pub fn validate_new_password(password: &Secret) -> Result<(), String> {
    let value = password.expose();
    if value.len() < PASSWORD_MIN
        || value.len() > PASSWORD_MAX
        || value.chars().any(char::is_control)
    {
        return Err(
            "invalid file_manager.password: expected 12-72 bytes without control characters"
                .to_string(),
        );
    }
    Ok(())
}

pub fn validate_database_password(password: &Secret) -> Result<(), String> {
    let value = password.expose();
    if value.len() < PASSWORD_MIN
        || value.len() > PASSWORD_MAX
        || value.chars().any(char::is_control)
    {
        return Err(
            "invalid options.database.password: expected 12-72 bytes without control characters"
                .to_string(),
        );
    }
    Ok(())
}

/// Pure checks performed before any host lookup or mutation.
pub fn validate_options(
    config: &Config,
    kind: ServiceKind,
    options: Option<&ServiceOptions>,
    mode: WriteMode,
) -> Result<(), String> {
    if mode == WriteMode::Configure && kind != ServiceKind::Storage {
        return Err("service.configure supports only storage file-manager settings".to_string());
    }
    let Some(options) = options else {
        if mode == WriteMode::Configure {
            return Err("service.configure requires options.file_manager".to_string());
        }
        return Ok(());
    };
    if let Some(image) = options.image.as_deref() {
        if image != config.image_for(kind) {
            return Err(
                "options.image must exactly match the broker-approved image for this kind"
                    .to_string(),
            );
        }
    }
    if options.database.is_some() && kind != ServiceKind::Database {
        return Err(format!(
            "options.database is not accepted for {} services",
            kind.as_str()
        ));
    }
    match (&options.file_manager, kind) {
        (Some(_), kind) if kind != ServiceKind::Storage => {
            return Err(format!(
                "options.file_manager is not accepted for {} services",
                kind.as_str()
            ))
        }
        (Some(file_manager), _) => {
            validate_username(&file_manager.username)?;
            if !file_manager.password.is_empty() {
                validate_new_password(&file_manager.password)?;
            }
        }
        (None, _) if mode == WriteMode::Configure => {
            return Err("service.configure requires options.file_manager".to_string())
        }
        (None, _) => {}
    }
    if let Some(database) = options.database.as_ref() {
        validate_database_password(&database.password)?;
    }
    Ok(())
}

/// Decides the storage bootstrap from trusted host state. Reads only; the
/// returned plan is applied by the unit writer after setup succeeds.
pub fn plan_storage(
    config: &Config,
    slug: &str,
    uid: u32,
    gid: u32,
    options: Option<&ServiceOptions>,
    existing_read_only: Option<bool>,
    mode: WriteMode,
    instance: u32,
) -> Result<StoragePlan, String> {
    let state = HostState {
        stored: read_settings(config, slug, gid, instance)?,
        database: database_state(config, slug, uid, instance)?,
        initialized: marker_exists(config, slug, instance)?,
    };
    decide_storage(
        &state,
        options.and_then(|options| options.file_manager.as_ref()),
        existing_read_only,
        mode,
        BCRYPT_COST,
    )
}

struct HostState {
    stored: Option<StoredFileManager>,
    database: DatabaseState,
    initialized: bool,
}

fn decide_storage(
    state: &HostState,
    requested: Option<&FileManagerOptions>,
    existing_read_only: Option<bool>,
    mode: WriteMode,
    cost: u32,
) -> Result<StoragePlan, String> {
    let (database, initialized) = (state.database, state.initialized);
    let Some(stored) = state.stored.as_ref() else {
        if existing_read_only.is_some() || mode == WriteMode::Configure {
            return Err(
                "storage file-manager configuration is missing; refusing to bootstrap new credentials for an existing service"
                    .to_string(),
            );
        }
        if database == DatabaseState::Present {
            return Err(
                "a FileBrowser database exists without broker configuration; restore the settings file or remove the stale database"
                    .to_string(),
            );
        }
        if initialized {
            return Err(
                "a FileBrowser initialization marker exists without broker configuration; remove the stale marker or restore the settings file"
                    .to_string(),
            );
        }
        let requested = requested.ok_or_else(|| {
            "initial storage creation requires options.file_manager".to_string()
        })?;
        validate_username(&requested.username)?;
        validate_new_password(&requested.password)?;
        let hash = hash_password(&requested.password, cost)?;
        return Ok(StoragePlan::Create {
            username: requested.username.clone(),
            hash,
            read_only: requested.read_only,
        });
    };

    if initialized && database == DatabaseState::Absent {
        return Err(missing_database_error());
    }
    let read_only = match (requested, existing_read_only, mode) {
        (Some(requested), current, mode) => {
            if requested.username != stored.username {
                return Err(
                    "file_manager.username cannot be changed after provisioning".to_string(),
                );
            }
            if !requested.password.is_empty() {
                return Err(
                    "file manager credentials are already provisioned; send an empty password (no reset or rotation through the broker)"
                        .to_string(),
                );
            }
            match (current, mode) {
                (Some(current), WriteMode::Provision) if current != requested.read_only => {
                    return Err(
                        "read_only changes require service.configure on the stopped service"
                            .to_string(),
                    );
                }
                (None, WriteMode::Configure) => {
                    return Err("service.configure requires an existing storage unit".to_string());
                }
                _ => requested.read_only,
            }
        }
        (None, Some(current), WriteMode::Provision) => current,
        (None, None, WriteMode::Provision) => {
            return Err("initial storage creation requires options.file_manager".to_string());
        }
        (None, _, WriteMode::Configure) => {
            return Err("service.configure requires options.file_manager".to_string());
        }
    };
    Ok(StoragePlan::Existing {
        username: stored.username.clone(),
        read_only,
    })
}

fn missing_database_error() -> String {
    "FileBrowser database is missing after initialization; refusing to re-bootstrap credentials (restore it or perform an operator reset)".to_string()
}

pub fn settings_path(config: &Config, slug: &str, instance: u32) -> PathBuf {
    let name = if instance == 1 {
        format!("{slug}-filebrowser.json")
    } else {
        format!("{slug}-filebrowser-{instance}.json")
    };
    config.etc_hosting.join("secrets").join(name)
}

fn marker_path(config: &Config, slug: &str, instance: u32) -> PathBuf {
    let name = if instance == 1 {
        format!("{slug}-filebrowser.initialized")
    } else {
        format!("{slug}-filebrowser-{instance}.initialized")
    };
    config.etc_hosting.join("secrets").join(name)
}

fn database_path(config: &Config, slug: &str, instance: u32) -> PathBuf {
    crate::provisioning::instance_data_root(config, slug, ServiceKind::Storage, instance)
        .join("db")
        .join(FILEBROWSER_DATABASE)
}

/// FileBrowser settings file. `password` is the bcrypt hash used only for
/// FileBrowser's first-run quick setup; it is ignored once the database exists.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Settings {
    username: String,
    password: Secret,
    #[serde(rename = "disableExec")]
    disable_exec: bool,
    #[serde(rename = "followExternalSymlinks")]
    follow_external_symlinks: bool,
    noauth: bool,
}

impl Settings {
    fn new(username: &str, hash: &Secret) -> Self {
        Self {
            username: username.to_string(),
            password: Secret(hash.expose().to_string()),
            disable_exec: true,
            follow_external_symlinks: false,
            noauth: false,
        }
    }

    fn validate(&self) -> Result<(), String> {
        validate_username(&self.username)
            .map_err(|_| "stored file-manager username is invalid".to_string())?;
        if !valid_bcrypt_hash(self.password.expose())
            || !self.disable_exec
            || self.follow_external_symlinks
            || self.noauth
        {
            return Err("stored file-manager configuration is invalid or unsafe".to_string());
        }
        Ok(())
    }
}

/// Validated, sanitized view of the stored settings (no hash).
#[derive(Debug, Eq, PartialEq)]
pub struct StoredFileManager {
    pub username: String,
}

fn valid_bcrypt_hash(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 60
        && (value.starts_with("$2a$") || value.starts_with("$2b$") || value.starts_with("$2y$"))
        && bytes[4].is_ascii_digit()
        && bytes[5].is_ascii_digit()
        && bytes[6] == b'$'
        && (4..=31).contains(&((bytes[4] - b'0') * 10 + (bytes[5] - b'0')))
        && bytes[7..]
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || b"./".contains(byte))
}

fn hash_password(password: &Secret, cost: u32) -> Result<Secret, String> {
    bcrypt::non_truncating_hash_with_result(password.expose().as_bytes(), cost)
        // FileBrowser (Go x/crypto/bcrypt) itself writes the $2a$ prefix.
        .map(|parts| Secret(parts.format_for_version(bcrypt::Version::TwoA)))
        .map_err(|_| "cannot hash the file manager password".to_string())
}

fn render_settings(username: &str, hash: &Secret) -> Result<Secret, String> {
    let settings = Settings::new(username, hash);
    settings.validate()?;
    let mut body = serde_json::to_string_pretty(&settings)
        .map_err(|_| "cannot render file-manager configuration".to_string())?;
    body.push('\n');
    Ok(Secret(body))
}

/// Reads the root-owned settings; `None` only when the file is absent.
pub fn read_settings(
    config: &Config,
    slug: &str,
    gid: u32,
    instance: u32,
) -> Result<Option<StoredFileManager>, String> {
    read_settings_at(&settings_path(config, slug, instance), 0, gid, true)
}

fn read_settings_at(
    path: &Path,
    owner_uid: u32,
    gid: u32,
    require_root_ancestry: bool,
) -> Result<Option<StoredFileManager>, String> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err("cannot inspect file-manager configuration".to_string()),
    }
    if require_root_ancestry {
        crate::host::validate_root_owned_entry(path, false)
            .map_err(|_| "file-manager configuration path is untrusted".to_string())?;
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|_| "cannot open file-manager configuration".to_string())?;
    let metadata = file
        .metadata()
        .map_err(|_| "cannot inspect file-manager configuration".to_string())?;
    if !metadata.is_file()
        || metadata.uid() != owner_uid
        || metadata.gid() != gid
        || metadata.mode() & 0o777 != 0o640
        || metadata.nlink() != 1
    {
        return Err(
            "file-manager configuration must be a regular root:<tenant-group> 0640 file"
                .to_string(),
        );
    }
    let mut body = String::new();
    file.take(MAX_SETTINGS_BYTES + 1)
        .read_to_string(&mut body)
        .map_err(|_| "cannot read file-manager configuration".to_string())?;
    let body = Secret(body);
    if body.0.len() as u64 > MAX_SETTINGS_BYTES {
        return Err("file-manager configuration is too large".to_string());
    }
    // Parse errors may quote values (including the hash); never forward them.
    let settings: Settings = serde_json::from_str(body.expose())
        .map_err(|_| "file-manager configuration is malformed".to_string())?;
    settings.validate()?;
    Ok(Some(StoredFileManager {
        username: settings.username.clone(),
    }))
}

/// Exclusive creation; an existing file is never overwritten or rotated.
pub fn create_settings(
    config: &Config,
    slug: &str,
    gid: u32,
    username: &str,
    hash: &Secret,
    instance: u32,
) -> Result<PathBuf, String> {
    crate::provisioning::ensure_secret_parent(config)?;
    let path = settings_path(config, slug, instance);
    create_settings_at(&path, 0, gid, username, hash)?;
    Ok(path)
}

fn create_settings_at(
    path: &Path,
    owner_uid: u32,
    gid: u32,
    username: &str,
    hash: &Secret,
) -> Result<(), String> {
    let body = render_settings(username, hash)?;
    let file = crate::provisioning::create_new(path, 0o640).map_err(|error| {
        if error.kind() == std::io::ErrorKind::AlreadyExists {
            "file-manager configuration already exists; refusing to overwrite".to_string()
        } else {
            "cannot create file-manager configuration".to_string()
        }
    })?;
    if let Err(error) =
        crate::provisioning::initialize_file(file, body.expose().as_bytes(), owner_uid, gid, 0o640)
    {
        let _ = std::fs::remove_file(path);
        return Err(error);
    }
    // Rootless Podman cannot relabel a root-owned file, so the broker labels it
    // once here and the Quadlet mounts it read-only without `:Z`. Best effort:
    // on hosts without SELinux the xattr namespace does not exist, and the
    // container start reports any real labeling problem.
    if let Err(error) = label_for_container(path) {
        tracing::warn!(path = %path.display(), %error, "file-manager configuration is not SELinux-labeled");
    }
    Ok(())
}

/// `system_u:object_r:container_file_t:s0` (no MCS categories) is readable by
/// every container domain, which is exactly what a shared read-only config needs.
fn label_for_container(path: &Path) -> Result<(), String> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| "settings path contains NUL".to_string())?;
    let name = b"security.selinux\0";
    let value = b"system_u:object_r:container_file_t:s0\0";
    let result = unsafe {
        libc::lsetxattr(
            path.as_ptr(),
            name.as_ptr() as *const libc::c_char,
            value.as_ptr() as *const libc::c_void,
            value.len(),
            0,
        )
    };
    if result == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::EOPNOTSUPP) {
        return Ok(());
    }
    Err(format!("cannot label file-manager configuration: {error}"))
}

/// Removes settings created by the current failed request only.
pub fn remove_created_settings(config: &Config, slug: &str, instance: u32) {
    let _ = std::fs::remove_file(settings_path(config, slug, instance));
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DatabaseState {
    Absent,
    Present,
}

fn database_state(config: &Config, slug: &str, uid: u32, instance: u32) -> Result<DatabaseState, String> {
    database_state_at(&database_path(config, slug, instance), uid)
}

fn database_state_at(path: &Path, uid: u32) -> Result<DatabaseState, String> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() && metadata.uid() == uid => Ok(DatabaseState::Present),
        Ok(_) => Err("FileBrowser database must be a tenant-owned regular file".to_string()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(DatabaseState::Absent),
        Err(_) => Err("cannot inspect the FileBrowser database".to_string()),
    }
}

fn marker_exists(config: &Config, slug: &str, instance: u32) -> Result<bool, String> {
    marker_exists_at(&marker_path(config, slug, instance), 0, 0, true)
}

fn marker_exists_at(
    path: &Path,
    owner_uid: u32,
    owner_gid: u32,
    require_root_ancestry: bool,
) -> Result<bool, String> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            if require_root_ancestry {
                crate::host::validate_root_owned_entry(path, false)
                    .map_err(|_| "file-manager initialization marker is untrusted".to_string())?;
            }
            if !metadata.is_file()
                || metadata.uid() != owner_uid
                || metadata.gid() != owner_gid
                || metadata.mode() & 0o777 != 0o600
            {
                return Err("file-manager initialization marker must be root-owned 0600".to_string());
            }
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(_) => Err("cannot inspect the file-manager initialization marker".to_string()),
    }
}

/// Records that FileBrowser has created its database, so a later disappearance
/// fails closed instead of silently re-running first-run setup. An absent
/// database without a marker is the pending first start.
pub fn observe_database(config: &Config, slug: &str, uid: u32, instance: u32) -> Result<DatabaseState, String> {
    observe_database_at(
        &database_path(config, slug, instance),
        &marker_path(config, slug, instance),
        uid,
        0,
        0,
        true,
    )
}

fn observe_database_at(
    database: &Path,
    marker: &Path,
    uid: u32,
    owner_uid: u32,
    owner_gid: u32,
    require_root_ancestry: bool,
) -> Result<DatabaseState, String> {
    let state = database_state_at(database, uid)?;
    let initialized = marker_exists_at(marker, owner_uid, owner_gid, require_root_ancestry)?;
    match (state, initialized) {
        (DatabaseState::Absent, true) => Err(missing_database_error()),
        (DatabaseState::Present, false) => {
            match crate::provisioning::create_new(marker, 0o600) {
                Ok(file) => {
                    crate::provisioning::initialize_file(file, b"", owner_uid, owner_gid, 0o600)
                }
                    .map_err(|_| "cannot record file-manager initialization".to_string())
                    .map(|()| state),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(state),
                Err(_) => Err("cannot record file-manager initialization".to_string()),
            }
        }
        _ => Ok(state),
    }
}

/// Activation-time validation of the stored configuration and database state.
pub fn validate_storage(
    config: &Config,
    slug: &str,
    uid: u32,
    gid: u32,
    instance: u32,
) -> Result<String, String> {
    let stored = read_settings(config, slug, gid, instance)?
        .ok_or_else(|| "storage file-manager configuration is missing".to_string())?;
    observe_database(config, slug, uid, instance)?;
    Ok(stored.username)
}

pub fn read_only_marker(body: &str, kind: ServiceKind) -> Result<bool, String> {
    let mut values = body
        .lines()
        .filter_map(|line| line.strip_prefix(STORAGE_READ_ONLY_MARKER));
    let value = values.next();
    if values.next().is_some() {
        return Err("duplicate storage read-only marker".to_string());
    }
    match (kind, value) {
        (ServiceKind::Storage, Some("true")) => Ok(true),
        (ServiceKind::Storage, Some("false")) => Ok(false),
        (ServiceKind::Storage, _) => {
            Err("storage unit has no valid read-only marker; stop it and rerun limits.set".to_string())
        }
        (_, None) => Ok(false),
        (_, Some(_)) => Err("non-storage unit carries a storage marker".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQUENCE: AtomicU64 = AtomicU64::new(0);

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "honed-service-config-{}-{}",
                std::process::id(),
                SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn ids() -> (u32, u32) {
        // SAFETY: these getters have no preconditions.
        unsafe { (libc::getuid(), libc::getgid()) }
    }

    fn options(value: serde_json::Value) -> ServiceOptions {
        serde_json::from_value(value).unwrap()
    }

    fn test_config() -> Config {
        Config {
            image_filebrowser: "example/filebrowser:1".to_string(),
            image_nginx: "example/nginx:1".to_string(),
            image_postgres: "example/postgres:1".to_string(),
            ..Config::from_env()
        }
    }

    #[test]
    fn options_schema_is_strict_and_passwords_are_redacted() {
        let parsed = options(serde_json::json!({
            "image": "example/filebrowser:1",
            "file_manager": {"username": "alice", "password": "correct horse battery"}
        }));
        let file_manager = parsed.file_manager.as_ref().unwrap();
        assert!(!file_manager.read_only);
        let debug = format!("{parsed:?}");
        assert!(debug.contains("REDACTED"));
        assert!(!debug.contains("correct horse"));
        let database = options(serde_json::json!({
            "image": "example/postgres:1",
            "database": {"password": "correct horse battery"}
        }));
        let debug = format!("{database:?}");
        assert!(debug.contains("REDACTED"));
        assert!(!debug.contains("correct horse"));
        for bad in [
            serde_json::json!({"imagex": "a"}),
            serde_json::json!({"file_manager": {"username": "a", "password": "b", "admin": true}}),
            serde_json::json!({"file_manager": {"username": "a"}}),
            serde_json::json!({"file_manager": {"username": "a", "password": 5}}),
        ] {
            assert!(serde_json::from_value::<ServiceOptions>(bad).is_err());
        }
    }

    #[test]
    fn username_and_password_bounds_are_enforced() {
        for good in ["a", "Alice_01", "x.y-z", &"u".repeat(32)] {
            assert!(validate_username(good).is_ok(), "{good}");
        }
        for bad in ["", "has space", "ünï", "a/b", "a\nb", "a:b", &"u".repeat(33)] {
            assert!(validate_username(bad).is_err(), "{bad:?}");
        }
        for good in ["123456789012", &"p".repeat(72), "pässwörd-123", "with spaces ok"] {
            assert!(validate_new_password(&Secret::new(good)).is_ok());
        }
        for bad in [
            "short",
            "12345678901",
            &"p".repeat(73),
            "tab\tinside-pass",
            "newline\ninside",
            "del\u{7f}character",
            "c1\u{85}control-chars",
        ] {
            let error = validate_new_password(&Secret::new(bad)).unwrap_err();
            assert!(!error.contains(bad));
        }
    }

    #[test]
    fn options_are_bound_to_kind_and_approved_image() {
        let config = test_config();
        let storage = options(serde_json::json!({
            "image": "example/filebrowser:1",
            "file_manager": {"username": "alice", "password": "correct horse battery"}
        }));
        assert!(validate_options(&config, ServiceKind::Storage, Some(&storage), WriteMode::Provision).is_ok());
        for kind in [ServiceKind::Web, ServiceKind::Database, ServiceKind::Git] {
            assert!(validate_options(&config, kind, Some(&storage), WriteMode::Provision).is_err());
            assert!(validate_options(&config, kind, None, WriteMode::Configure).is_err());
        }
        for image in ["example/filebrowser:2", "docker.io/evil/filebrowser:1", "example/nginx:1", ""] {
            let other = options(serde_json::json!({"image": image}));
            assert!(validate_options(&config, ServiceKind::Storage, Some(&other), WriteMode::Provision).is_err());
        }
        let web = options(serde_json::json!({"image": "example/nginx:1"}));
        assert!(validate_options(&config, ServiceKind::Web, Some(&web), WriteMode::Provision).is_ok());
        assert!(validate_options(&config, ServiceKind::Web, None, WriteMode::Provision).is_ok());
        assert!(validate_options(&config, ServiceKind::Storage, None, WriteMode::Configure).is_err());
        let database = options(serde_json::json!({
            "image": "example/postgres:1",
            "database": {"password": "correct horse battery"}
        }));
        assert!(validate_options(&config, ServiceKind::Database, Some(&database), WriteMode::Provision).is_ok());
        assert!(validate_options(&config, ServiceKind::Web, Some(&database), WriteMode::Provision).is_err());
        assert!(validate_options(&config, ServiceKind::Storage, Some(&database), WriteMode::Provision).is_err());
        let short = options(serde_json::json!({
            "database": {"password": "short"}
        }));
        assert!(validate_options(&config, ServiceKind::Database, Some(&short), WriteMode::Provision).is_err());
        let bad_user = options(serde_json::json!({
            "file_manager": {"username": "bad user", "password": ""}
        }));
        assert!(validate_options(&config, ServiceKind::Storage, Some(&bad_user), WriteMode::Configure).is_err());
    }

    #[test]
    fn hashing_uses_bcrypt_and_settings_never_echo_secrets() {
        let password = Secret::new("correct horse battery");
        let hash = hash_password(&password, 4).unwrap();
        assert!(hash.expose().starts_with("$2a$04$"));
        assert!(valid_bcrypt_hash(hash.expose()));
        assert!(bcrypt::verify("correct horse battery", hash.expose()).unwrap());
        assert!(!hash.expose().contains("correct horse"));
        assert_eq!(format!("{hash:?}"), "REDACTED");
        let body = render_settings("alice", &hash).unwrap();
        let value: serde_json::Value = serde_json::from_str(body.expose()).unwrap();
        assert_eq!(value["username"], "alice");
        assert_eq!(value["disableExec"], true);
        assert_eq!(value["followExternalSymlinks"], false);
        assert_eq!(value["noauth"], false);
        assert!(!body.expose().contains("correct horse"));
        for invalid in ["plaintext", "$2a$04$short", &format!("$1${}", "a".repeat(56))] {
            assert!(!valid_bcrypt_hash(invalid));
        }
    }

    #[test]
    fn settings_are_create_exclusive_and_strictly_validated() {
        let fixture = Fixture::new();
        let (uid, gid) = ids();
        let path = fixture.0.join("t01-filebrowser.json");
        let hash = hash_password(&Secret::new("correct horse battery"), 4).unwrap();
        assert_eq!(read_settings_at(&path, uid, gid, false).unwrap(), None);
        create_settings_at(&path, uid, gid, "alice", &hash).unwrap();
        let original = std::fs::read(&path).unwrap();
        assert_eq!(
            read_settings_at(&path, uid, gid, false).unwrap(),
            Some(StoredFileManager { username: "alice".to_string() })
        );
        let other = hash_password(&Secret::new("another password!"), 4).unwrap();
        let error = create_settings_at(&path, uid, gid, "mallory", &other).unwrap_err();
        assert!(error.contains("refusing to overwrite"));
        assert_eq!(std::fs::read(&path).unwrap(), original);

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_settings_at(&path, uid, gid, false).is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();

        let unsafe_body = String::from_utf8(original.clone())
            .unwrap()
            .replace("\"disableExec\": true", "\"disableExec\": false");
        std::fs::write(&path, &unsafe_body).unwrap();
        let error = read_settings_at(&path, uid, gid, false).unwrap_err();
        assert!(!error.contains("$2a$"));
        std::fs::write(&path, "{\"username\":\"alice\",\"password\":\"$2a$leak\"").unwrap();
        let error = read_settings_at(&path, uid, gid, false).unwrap_err();
        assert!(!error.contains("leak"));

        let link = fixture.0.join("link.json");
        std::fs::write(&path, &original).unwrap();
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(read_settings_at(&link, uid, gid, false).is_err());
    }

    #[test]
    fn database_marker_distinguishes_first_start_from_lost_database() {
        let fixture = Fixture::new();
        let (uid, gid) = ids();
        let database = fixture.0.join("filebrowser.db");
        let marker = fixture.0.join("t01-filebrowser.initialized");
        assert_eq!(
            observe_database_at(&database, &marker, uid, uid, gid, false).unwrap(),
            DatabaseState::Absent
        );
        assert!(!marker.exists());
        std::fs::write(&database, b"db").unwrap();
        assert_eq!(
            observe_database_at(&database, &marker, uid, uid, gid, false).unwrap(),
            DatabaseState::Present
        );
        assert_eq!(
            std::fs::symlink_metadata(&marker).unwrap().mode() & 0o777,
            0o600
        );
        std::fs::remove_file(&database).unwrap();
        assert!(observe_database_at(&database, &marker, uid, uid, gid, false)
            .unwrap_err()
            .contains("missing after initialization"));
        std::os::unix::fs::symlink("/etc/passwd", &database).unwrap();
        assert!(observe_database_at(&database, &marker, uid, uid, gid, false).is_err());
    }

    #[test]
    fn read_only_marker_is_required_only_for_storage() {
        assert_eq!(read_only_marker("# HonedStorageReadOnly=true\n", ServiceKind::Storage), Ok(true));
        assert_eq!(read_only_marker("# HonedStorageReadOnly=false\n", ServiceKind::Storage), Ok(false));
        assert!(read_only_marker("", ServiceKind::Storage).is_err());
        assert!(read_only_marker("# HonedStorageReadOnly=yes\n", ServiceKind::Storage).is_err());
        assert!(read_only_marker(
            "# HonedStorageReadOnly=true\n# HonedStorageReadOnly=false\n",
            ServiceKind::Storage
        )
        .is_err());
        assert_eq!(read_only_marker("", ServiceKind::Web), Ok(false));
        assert!(read_only_marker("# HonedStorageReadOnly=false\n", ServiceKind::Web).is_err());
    }

    #[test]
    fn storage_plan_never_resets_credentials_or_changes_username() {
        let fresh = HostState {
            stored: None,
            database: DatabaseState::Absent,
            initialized: false,
        };
        let provisioned = HostState {
            stored: Some(StoredFileManager {
                username: "alice".to_string(),
            }),
            database: DatabaseState::Absent,
            initialized: false,
        };
        let decide = |state: &HostState,
                      value: serde_json::Value,
                      existing: Option<bool>,
                      mode: WriteMode| {
            let options = options(value);
            decide_storage(state, options.file_manager.as_ref(), existing, mode, 4)
        };
        let new = serde_json::json!({"file_manager":{"username":"alice","password":"correct horse battery","read_only":true}});
        let created = decide(&fresh, new.clone(), None, WriteMode::Provision).unwrap();
        let StoragePlan::Create { hash, read_only, .. } = &created else {
            panic!("expected creation")
        };
        assert!(*read_only);
        assert!(bcrypt::verify("correct horse battery", hash.expose()).unwrap());
        assert!(!format!("{created:?}").contains(hash.expose()));
        assert!(decide(&fresh, serde_json::json!({}), None, WriteMode::Provision).is_err());
        let empty = serde_json::json!({"file_manager":{"username":"alice","password":""}});
        assert!(decide(&fresh, empty, None, WriteMode::Provision).is_err());
        // A unit without its settings or a stray database never re-bootstraps.
        assert!(decide(&fresh, new.clone(), Some(false), WriteMode::Provision).is_err());
        assert!(decide(&fresh, new.clone(), None, WriteMode::Configure).is_err());
        for (database, initialized) in [(DatabaseState::Present, false), (DatabaseState::Absent, true)] {
            let stray = HostState { stored: None, database, initialized };
            assert!(decide(&stray, new.clone(), None, WriteMode::Provision).is_err());
        }

        assert!(decide(&provisioned, new.clone(), None, WriteMode::Provision)
            .unwrap_err()
            .contains("empty password"));
        let rename = serde_json::json!({"file_manager":{"username":"bob","password":""}});
        assert!(decide(&provisioned, rename, Some(false), WriteMode::Configure)
            .unwrap_err()
            .contains("username"));
        let toggle = serde_json::json!({"file_manager":{"username":"alice","password":"","read_only":true}});
        let configured = decide(&provisioned, toggle.clone(), Some(false), WriteMode::Configure).unwrap();
        assert!(matches!(configured, StoragePlan::Existing { read_only: true, .. }));
        assert!(decide(&provisioned, toggle.clone(), Some(false), WriteMode::Provision)
            .unwrap_err()
            .contains("service.configure"));
        assert!(decide(&provisioned, toggle.clone(), None, WriteMode::Configure).is_err());
        assert!(decide(&provisioned, serde_json::json!({}), Some(false), WriteMode::Configure).is_err());
        let keep = decide(&provisioned, serde_json::json!({}), Some(true), WriteMode::Provision).unwrap();
        assert!(matches!(keep, StoragePlan::Existing { read_only: true, .. }));
        // Recreating a deleted unit reuses retained credentials, never new ones.
        let recreate = serde_json::json!({"file_manager":{"username":"alice","password":""}});
        assert!(decide(&provisioned, recreate, None, WriteMode::Provision).is_ok());
        let lost = HostState {
            stored: Some(StoredFileManager { username: "alice".to_string() }),
            database: DatabaseState::Absent,
            initialized: true,
        };
        assert!(decide(&lost, toggle, Some(false), WriteMode::Configure)
            .unwrap_err()
            .contains("missing after initialization"));
    }
}
