use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::host::ServiceKind;

const WEB_DIRECTORIES: &[(&str, u32)] = &[("site", 0o750)];
const DATABASE_DIRECTORIES: &[(&str, u32)] = &[("pgdata", 0o700)];
const STORAGE_DIRECTORIES: &[(&str, u32)] = &[("vault", 0o750), ("db", 0o700)];
const GIT_DIRECTORIES: &[(&str, u32)] = &[("gitea", 0o750), ("gitea-config", 0o700)];

pub fn ensure_tenant_root(config: &Config) -> Result<File, String> {
    ensure_root_directory(&config.tenant_data_root, 0o755)?;
    let root = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&config.tenant_data_root)
        .map_err(|error| format!("{}: {error}", config.tenant_data_root.display()))?;
    let metadata = root
        .metadata()
        .map_err(|error| format!("{}: {error}", config.tenant_data_root.display()))?;
    if !metadata.is_dir()
        || metadata.uid() != 0
        || metadata.gid() != 0
        || metadata.mode() & 0o777 != 0o755
    {
        return Err("tenant data root must be root-owned mode 0755".to_string());
    }
    Ok(root)
}

/// Suffix used for a fixed-template instance: `site`, `pgdata`, `vault`,
/// `gitea` for the first instance and `site-2`, `db-3`, ... afterwards.
pub(crate) fn instance_dir(kind: ServiceKind, instance: u32) -> String {
    let suffix = match kind {
        ServiceKind::Web => "site",
        ServiceKind::Database => "db",
        ServiceKind::Storage => "vault",
        ServiceKind::Git => "gitea",
        ServiceKind::Container => "container",
    };
    if instance == 1 {
        suffix.to_string()
    } else {
        format!("{suffix}-{instance}")
    }
}

/// Root that holds this instance's kind directory. Instance 1 keeps the legacy
/// `{tenant_data}/{slug}` layout; later instances live below `{slug}/{suffix}-n`.
pub(crate) fn instance_data_root(
    config: &Config,
    slug: &str,
    kind: ServiceKind,
    instance: u32,
) -> PathBuf {
    let root = config.tenant_data_root.join(slug);
    if instance == 1 {
        root
    } else {
        root.join(instance_dir(kind, instance))
    }
}

pub(crate) fn secret_file_name(slug: &str, service: &str, instance: u32) -> String {
    if instance == 1 {
        format!("{slug}-{service}.env")
    } else {
        format!("{slug}-{service}-{instance}.env")
    }
}

/// Creates the kind's directories and generated secrets. Storage credentials
/// are written separately by `service_config` after option validation.
pub fn setup_service(
    config: &Config,
    slug: &str,
    kind: ServiceKind,
    instance: u32,
    database_password: Option<&str>,
) -> Result<(), String> {
    if kind == ServiceKind::Container {
        return Err("use container.create".to_string());
    }
    if instance == 0 {
        return Err("invalid instance number".to_string());
    }
    // Git (Forgejo) is a first-class fixed template: directories and its secret
    // env file are created here like any other kind.
    let root = ensure_tenant_root(config)?;
    let uid = crate::host::user_uid(slug)?;
    let gid = crate::host::user_gid(slug)?;
    let tenant = ensure_directory(&root, slug, uid, gid, 0o750)?;
    let data = if instance == 1 {
        tenant
    } else {
        ensure_directory(&tenant, &instance_dir(kind, instance), uid, gid, 0o750)?
    };
    for (name, mode) in directories(kind) {
        ensure_directory(&data, name, uid, gid, *mode)?;
    }

    let data = instance_data_root(config, slug, kind, instance);

    match kind {
        ServiceKind::Web => ensure_initial_html(&data.join("site/index.html"), uid, gid),
        ServiceKind::Database => {
            ensure_secret_parent(config)?;
            let path = secret_path(config, slug, "db", instance);
            ensure_secret_file(&path, || database_body(slug, database_password), gid)?;
            Ok(())
        }
        ServiceKind::Git => {
            ensure_secret_parent(config)?;
            let path = secret_path(config, slug, "forgejo", instance);
            ensure_secret_file(
                &path,
                || {
                    let secret_key = random_hex(32)?;
                    let internal_token = random_hex(64)?;
                    Ok(forgejo_environment(&secret_key, &internal_token))
                },
                gid,
            )?;
            Ok(())
        }
        ServiceKind::Storage => ensure_secret_parent(config),
        ServiceKind::Container => Err("use container.create".to_string()),
    }
}

pub fn validate_service(
    config: &Config,
    slug: &str,
    kind: ServiceKind,
    instance: u32,
) -> Result<(), String> {
    if kind == ServiceKind::Container {
        return Err("generic container requires metadata validation".to_string());
    }
    if instance == 0 {
        return Err("invalid instance number".to_string());
    }
    // Git (Forgejo) is a first-class fixed template again. Its first-run install
    // page is reachable only through the tenant's loopback publication; the
    // tenant must complete the admin account immediately after activation.
    let _ = kind; // Web, Database, Storage and Git share the checks below.
    validate_tenant_root(config)?;
    let uid = crate::host::user_uid(slug)?;
    let gid = crate::host::user_gid(slug)?;
    let data = instance_data_root(config, slug, kind, instance);
    validate_directory(&data, uid, gid)?;
    for (name, _) in directories(kind) {
        validate_directory(&data.join(name), uid, gid)?;
    }
    match kind {
        ServiceKind::Database => {
            validate_secret_parents(config)?;
            validate_secret_file(&secret_path(config, slug, "db", instance), gid)
        }
        ServiceKind::Git => {
            validate_secret_parents(config)?;
            validate_secret_file(&secret_path(config, slug, "forgejo", instance), gid)
        }
        ServiceKind::Container => Err("generic container requires metadata validation".to_string()),
        ServiceKind::Web => validate_initial_html(&data.join("site/index.html"), uid),
        ServiceKind::Storage => {
            validate_secret_parents(config)?;
            crate::service_config::validate_storage(config, slug, uid, gid, instance).map(|_| ())
        }
    }
}

fn validate_tenant_root(config: &Config) -> Result<(), String> {
    crate::host::validate_root_owned_entry(&config.tenant_data_root, true)?;
    let metadata = std::fs::symlink_metadata(&config.tenant_data_root)
        .map_err(|error| format!("{}: {error}", config.tenant_data_root.display()))?;
    if metadata.mode() & 0o777 != 0o755 {
        return Err("tenant data root must be root-owned mode 0755 for container traversal".to_string());
    }
    Ok(())
}

fn validate_secret_parents(config: &Config) -> Result<(), String> {
    let secrets = config.etc_hosting.join("secrets");
    for (path, mode) in [(&config.etc_hosting, 0o755), (&secrets, 0o711)] {
        crate::host::validate_root_owned_entry(path, true)?;
        let metadata = std::fs::symlink_metadata(path)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        if metadata.mode() & 0o777 != mode {
            return Err(format!(
                "secret parent must have mode {mode:o} for service traversal: {}",
                path.display()
            ));
        }
    }
    Ok(())
}

fn directories(kind: ServiceKind) -> &'static [(&'static str, u32)] {
    match kind {
        ServiceKind::Web => WEB_DIRECTORIES,
        ServiceKind::Database => DATABASE_DIRECTORIES,
        ServiceKind::Storage => STORAGE_DIRECTORIES,
        ServiceKind::Git => GIT_DIRECTORIES,
        ServiceKind::Container => &[],
    }
}

fn secret_path(config: &Config, slug: &str, service: &str, instance: u32) -> PathBuf {
    config
        .etc_hosting
        .join("secrets")
        .join(secret_file_name(slug, service, instance))
}

pub(crate) fn ensure_secret_parent(config: &Config) -> Result<(), String> {
    ensure_root_directory(&config.etc_hosting, 0o755)?;
    ensure_root_directory(&config.etc_hosting.join("secrets"), 0o711)
}

pub(crate) fn ensure_root_directory(path: &Path, mode: u32) -> Result<(), String> {
    if !path.is_absolute() {
        return Err(format!("managed directory must be absolute: {}", path.display()));
    }
    reject_symlink_components(path)?;
    std::fs::create_dir_all(path).map_err(|error| format!("{}: {error}", path.display()))?;
    crate::host::validate_root_owned_entry(path, true)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .map_err(|error| format!("chmod {}: {error}", path.display()))?;
    crate::host::validate_root_owned_entry(path, true)
}

pub(crate) fn ensure_directory(
    parent: &File,
    name: &str,
    uid: u32,
    gid: u32,
    mode: u32,
) -> Result<File, String> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') {
        return Err("managed directory name must be one path component".to_string());
    }
    let name = std::ffi::CString::new(name)
        .map_err(|_| "managed directory name contains a NUL byte".to_string())?;
    // SAFETY: parent remains open and name is a valid, single-component C string.
    if unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), mode) } != 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::AlreadyExists {
            return Err(format!("mkdir {}: {error}", name.to_string_lossy()));
        }
    }
    // Open the actual child without following a pre-existing or racing symlink.
    // All ownership and mode changes below apply only to this pinned descriptor.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(format!(
            "open managed directory {}: {}",
            name.to_string_lossy(),
            std::io::Error::last_os_error()
        ));
    }
    // SAFETY: fd was just returned by openat and is owned by this File.
    let directory = unsafe { File::from_raw_fd(fd) };
    // SAFETY: directory owns a valid descriptor throughout both calls.
    if unsafe { libc::fchown(fd, uid, gid) } != 0 {
        return Err(format!(
            "fchown managed directory {}: {}",
            name.to_string_lossy(),
            std::io::Error::last_os_error()
        ));
    }
    if unsafe { libc::fchmod(fd, mode) } != 0 {
        return Err(format!(
            "fchmod managed directory {}: {}",
            name.to_string_lossy(),
            std::io::Error::last_os_error()
        ));
    }
    let metadata = directory
        .metadata()
        .map_err(|error| format!("fstat managed directory: {error}"))?;
    if !metadata.is_dir()
        || metadata.uid() != uid
        || metadata.gid() != gid
        || metadata.mode() & 0o777 != mode
    {
        return Err("managed directory ownership or mode verification failed".to_string());
    }
    Ok(directory)
}

fn reject_symlink_components(path: &Path) -> Result<(), String> {
    for component in path.ancestors() {
        match std::fs::symlink_metadata(component) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(format!(
                    "refusing to traverse a symlink in managed path: {}",
                    component.display()
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("{}: {error}", component.display())),
        }
    }
    Ok(())
}

fn validate_directory(path: &Path, uid: u32, gid: u32) -> Result<(), String> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| format!("{}: {error}", path.display()))?;
    if !metadata.is_dir() || metadata.uid() != uid || metadata.gid() != gid {
        return Err(format!(
            "tenant data path must be a non-symlink directory owned by uid {uid} and gid {gid}: {}",
            path.display()
        ));
    }
    Ok(())
}

fn ensure_initial_html(path: &Path, uid: u32, gid: u32) -> Result<(), String> {
    let body = "<!doctype html>\n<html lang=\"en\"><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>SharpNode tenant</title><main><h1>SharpNode tenant site</h1><p>Your static web service is provisioned.</p></main></html>\n";
    match create_new(path, 0o644) {
        Ok(file) => {
            if let Err(error) = initialize_file(file, body.as_bytes(), uid, gid, 0o644) {
                let _ = std::fs::remove_file(path);
                return Err(error);
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            validate_initial_html(path, uid)
        }
        Err(error) => Err(format!("{}: {error}", path.display())),
    }
}

fn validate_initial_html(path: &Path, uid: u32) -> Result<(), String> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| format!("{}: {error}", path.display()))?;
    if !metadata.is_file() || metadata.uid() != uid {
        return Err(format!(
            "existing index.html must be a tenant-owned regular file: {}",
            path.display()
        ));
    }
    Ok(())
}

fn ensure_secret_file(
    path: &Path,
    body: impl FnOnce() -> Result<String, String>,
    service_gid: u32,
) -> Result<(), String> {
    ensure_secret_file_for_owner(path, body, 0, service_gid)
}

fn ensure_secret_file_for_owner(
    path: &Path,
    body: impl FnOnce() -> Result<String, String>,
    owner_uid: u32,
    service_gid: u32,
) -> Result<(), String> {
    match create_new(path, 0o640) {
        Ok(file) => {
            let body = match body() {
                Ok(body) => body,
                Err(error) => {
                    let _ = std::fs::remove_file(path);
                    return Err(error);
                }
            };
            if let Err(error) = initialize_file(file, body.as_bytes(), owner_uid, service_gid, 0o640)
            {
                let _ = std::fs::remove_file(path);
                return Err(error);
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            validate_secret_file_for_owner(path, owner_uid, service_gid)
        }
        Err(error) => Err(format!("{}: {error}", path.display())),
    }
}

fn validate_secret_file(path: &Path, service_gid: u32) -> Result<(), String> {
    validate_secret_file_for_owner(path, 0, service_gid)
}

fn validate_secret_file_for_owner(
    path: &Path,
    owner_uid: u32,
    service_gid: u32,
) -> Result<(), String> {
    if owner_uid == 0 {
        crate::host::validate_root_owned_entry(path, false)?;
    }
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| format!("{}: {error}", path.display()))?;
    if !metadata.is_file()
        || metadata.uid() != owner_uid
        || metadata.gid() != service_gid
        || metadata.mode() & 0o777 != 0o640
    {
        return Err(format!(
            "secret must have its required owner, service-group read access, and mode 0640: {}",
            path.display()
        ));
    }
    Ok(())
}

pub(crate) fn create_new(path: &Path, mode: u32) -> std::io::Result<File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
}

pub(crate) fn initialize_file(
    mut file: File,
    body: &[u8],
    uid: u32,
    gid: u32,
    mode: u32,
) -> Result<(), String> {
    file.write_all(body)
        .map_err(|error| format!("cannot initialize provisioned file: {error}"))?;
    // SAFETY: fd remains open and owned by file for both calls.
    if unsafe { libc::fchown(file.as_raw_fd(), uid, gid) } != 0 {
        return Err(format!(
            "cannot set provisioned file ownership: {}",
            std::io::Error::last_os_error()
        ));
    }
    // SAFETY: fd remains open and owned by file.
    if unsafe { libc::fchmod(file.as_raw_fd(), mode) } != 0 {
        return Err(format!(
            "cannot set provisioned file mode: {}",
            std::io::Error::last_os_error()
        ));
    }
    file.sync_all()
        .map_err(|error| format!("cannot sync provisioned file: {error}"))
}

fn database_environment(slug: &str, password: &str) -> String {
    format!("POSTGRES_USER=postgres\nPOSTGRES_DB={slug}\nPOSTGRES_PASSWORD={password}\n")
}

fn database_body(slug: &str, password: Option<&str>) -> Result<String, String> {
    let password = match password {
        Some(password) if password.is_empty() => {
            return Err("database password must not be empty".to_string())
        }
        Some(password) => password.to_string(),
        None => random_hex(32)?,
    };
    Ok(database_environment(slug, &password))
}

fn forgejo_environment(secret_key: &str, internal_token: &str) -> String {
    format!(
        "FORGEJO__security__SECRET_KEY={secret_key}\nFORGEJO__security__INTERNAL_TOKEN={internal_token}\n"
    )
}

fn random_hex(bytes: usize) -> Result<String, String> {
    let mut random = vec![0_u8; bytes];
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut random))
        .map_err(|error| format!("cannot read operating-system randomness: {error}"))?;
    let mut encoded = String::with_capacity(bytes * 2);
    for byte in random {
        use std::fmt::Write as _;
        write!(&mut encoded, "{byte:02x}").map_err(|_| "cannot encode secret".to_string())?;
    }
    Ok(encoded)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQUENCE: AtomicU64 = AtomicU64::new(0);

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "honed-provisioning-{}-{}",
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

    #[test]
    fn initial_html_is_seeded_once_without_overwriting_tenant_content() {
        // SAFETY: these getters have no preconditions.
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
        let fixture = Fixture::new();
        let path = fixture.0.join("index.html");
        ensure_initial_html(&path, uid, gid).unwrap();
        assert!(std::fs::read_to_string(&path)
            .unwrap()
            .contains("static web service is provisioned"));
        std::fs::write(&path, "tenant controlled\n").unwrap();
        ensure_initial_html(&path, uid, gid).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "tenant controlled\n");
        let link = fixture.0.join("index-link.html");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(ensure_initial_html(&link, uid, gid).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "tenant controlled\n");
    }

    #[test]
    fn existing_secret_is_validated_and_never_rotated() {
        // Production passes uid 0. Injecting the current uid keeps this fixture
        // unprivileged while exercising the same create-new and validation path.
        let fixture = Fixture::new();
        let path = fixture.0.join("db.env");
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
        ensure_secret_file_for_owner(
            &path,
            || Ok("POSTGRES_PASSWORD=first\n".to_string()),
            uid,
            gid,
        )
        .unwrap();
        ensure_secret_file_for_owner(
            &path,
            || panic!("an existing secret must not be regenerated"),
            uid,
            gid,
        )
        .unwrap();
        let metadata = std::fs::symlink_metadata(&path).unwrap();
        assert_eq!(metadata.mode() & 0o777, 0o640);
        assert_eq!(metadata.uid(), uid);
        assert_eq!(metadata.gid(), gid);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "POSTGRES_PASSWORD=first\n"
        );
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(ensure_secret_file_for_owner(
            &path,
            || Ok("POSTGRES_PASSWORD=rotated\n".to_string()),
            uid,
            gid,
        )
        .is_err());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "POSTGRES_PASSWORD=first\n"
        );
    }

    #[test]
    fn fresh_database_secret_has_all_required_credentials() {
        let body = database_environment("t01", &"a".repeat(64));
        assert_eq!(
            body,
            format!(
                "POSTGRES_USER=postgres\nPOSTGRES_DB=t01\nPOSTGRES_PASSWORD={}\n",
                "a".repeat(64)
            )
        );
        assert!(body.lines().all(|line| line.contains('=')));
    }

    #[test]
    fn database_body_prefers_the_tenant_password_and_falls_back_to_a_generated_one() {
        let chosen = database_body("t01", Some("chosen database password")).unwrap();
        assert_eq!(
            chosen,
            "POSTGRES_USER=postgres\nPOSTGRES_DB=t01\nPOSTGRES_PASSWORD=chosen database password\n"
        );
        let generated = database_body("t01", None).unwrap();
        let password = generated
            .lines()
            .find_map(|line| line.strip_prefix("POSTGRES_PASSWORD="))
            .unwrap();
        assert_eq!(password.len(), 64);
        assert!(password.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert!(database_body("t01", Some("")).is_err());
    }

    #[test]
    fn legacy_directories_reject_existing_symlink_leaves_without_touching_targets() {
        let fixture = Fixture::new();
        let root_path = fixture.0.join("root");
        let outside = fixture.0.join("outside");
        std::fs::create_dir(&root_path).unwrap();
        std::fs::create_dir(&outside).unwrap();
        std::fs::set_permissions(&outside, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::os::unix::fs::symlink(&outside, root_path.join("t01")).unwrap();
        let root = File::open(&root_path).unwrap();
        // SAFETY: these getters have no preconditions.
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
        assert!(ensure_directory(&root, "t01", uid, gid, 0o750).is_err());
        assert_eq!(
            std::fs::symlink_metadata(&outside).unwrap().mode() & 0o777,
            0o700
        );
        assert!(std::fs::symlink_metadata(root_path.join("t01"))
            .unwrap()
            .file_type()
            .is_symlink());
    }

    #[test]
    fn legacy_children_stay_in_pinned_tenant_and_validate_private_gid() {
        let fixture = Fixture::new();
        let root_path = fixture.0.join("root");
        let outside = fixture.0.join("outside");
        std::fs::create_dir(&root_path).unwrap();
        std::fs::create_dir(&outside).unwrap();
        let root = File::open(&root_path).unwrap();
        // SAFETY: these getters have no preconditions.
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
        let tenant = ensure_directory(&root, "t01", uid, gid, 0o750).unwrap();
        ensure_directory(&tenant, "site", uid, gid, 0o750).unwrap();
        let data = root_path.join("t01");
        validate_directory(&data, uid, gid).unwrap();
        assert!(validate_directory(&data, uid, gid.wrapping_add(1)).is_err());

        let moved = fixture.0.join("moved-tenant");
        std::fs::rename(&data, &moved).unwrap();
        std::os::unix::fs::symlink(&outside, &data).unwrap();
        ensure_directory(&tenant, "vault", uid, gid, 0o700).unwrap();
        assert!(moved.join("vault").is_dir());
        assert!(!outside.join("vault").exists());
        assert_eq!(
            std::fs::symlink_metadata(moved.join("vault"))
                .unwrap()
                .mode()
                & 0o777,
            0o700
        );
    }

    #[test]
    fn generic_containers_fail_before_account_lookup_or_unit_success() {
        let error =
            setup_service(&Config::from_env(), "t01", ServiceKind::Container, 1, None).unwrap_err();
        assert!(error.contains("container.create"));
        assert!(validate_service(&Config::from_env(), "t01", ServiceKind::Container, 1).is_err());
    }
}
