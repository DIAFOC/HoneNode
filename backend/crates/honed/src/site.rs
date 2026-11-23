//! Tenant static-site content for the fixed nginx web template.
//!
//! The broker is the only writer. Every path component is resolved with
//! `openat(O_NOFOLLOW)`, so a tenant-created symlink can never redirect a
//! root-owned write outside `{tenant_data}/site`. Files are written to a
//! temporary sibling and renamed into place, then labelled for containers.

use std::ffi::CString;
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use crate::config::Config;
use crate::host::ServiceKind;

/// The panel sends text files; binary assets need a future upload path.
pub const MAX_SITE_FILE_BYTES: usize = 64 * 1024;
const MAX_PATH_BYTES: usize = 256;
const MAX_COMPONENTS: usize = 16;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SiteContent {
    pub path: String,
    pub content: String,
}

pub fn read(config: &Config, slug: &str, unit: &str, path: &str) -> Result<String, String> {
    require_web_unit(config, slug, unit)?;
    let components = validate_path(path)?;
    let root = site_root(config, slug, unit)?;
    let (dir, name) = open_parent(&root, &components, false, 0, 0)?;
    let mut file = open_file_at(&dir, name, libc::O_RDONLY, 0)?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("cannot stat site file: {error}"))?;
    if !metadata.is_file() {
        return Err("site path is not a regular file".to_string());
    }
    if metadata.len() > MAX_SITE_FILE_BYTES as u64 {
        return Err("site file is too large to display".to_string());
    }
    let mut content = String::new();
    Read::by_ref(&mut file)
        .take(MAX_SITE_FILE_BYTES as u64 + 1)
        .read_to_string(&mut content)
        .map_err(|_| "site file is not valid UTF-8 text".to_string())?;
    if content.len() > MAX_SITE_FILE_BYTES {
        return Err("site file is too large to display".to_string());
    }
    Ok(content)
}

pub fn write(
    config: &Config,
    slug: &str,
    unit: &str,
    path: &str,
    content: &str,
) -> Result<(), String> {
    require_web_unit(config, slug, unit)?;
    if content.len() > MAX_SITE_FILE_BYTES {
        return Err(format!(
            "site file exceeds the {MAX_SITE_FILE_BYTES}-byte text limit"
        ));
    }
    let components = validate_path(path)?;
    let root = site_root(config, slug, unit)?;
    let uid = crate::host::user_uid(slug)?;
    let gid = crate::host::user_gid(slug)?;
    let (dir, name) = open_parent(&root, &components, true, uid, gid)?;
    write_file_at(&dir, name, content.as_bytes(), uid, gid)
}

fn require_web_unit(config: &Config, slug: &str, unit: &str) -> Result<(), String> {
    let (_, body, reservation) = crate::host::committed_unit(config, slug, unit)?;
    let kind = reservation
        .kind
        .ok_or_else(|| "unit has no committed kind".to_string())?;
    if kind != ServiceKind::Web {
        return Err("site content is only available for web services".to_string());
    }
    if crate::host::unit_directive(&body, "# HonedKind=")? != "web" {
        return Err("unit kind marker does not match a web service".to_string());
    }
    Ok(())
}

fn site_root(config: &Config, slug: &str, unit: &str) -> Result<std::path::PathBuf, String> {
    let instance = crate::host::instance_from_unit(slug, ServiceKind::Web, unit)?;
    Ok(crate::provisioning::instance_data_root(
        config,
        slug,
        ServiceKind::Web,
        instance,
    )
    .join("site"))
}

fn validate_path(path: &str) -> Result<Vec<&str>, String> {
    if path.is_empty()
        || path.len() > MAX_PATH_BYTES
        || path.starts_with('/')
        || path.ends_with('/')
    {
        return Err("site path must be a relative file path".to_string());
    }
    let components: Vec<&str> = path.split('/').collect();
    if components.len() > MAX_COMPONENTS {
        return Err("site path has too many components".to_string());
    }
    for component in &components {
        if component.is_empty() || *component == "." || *component == ".." {
            return Err("site path contains an invalid component".to_string());
        }
        if !component
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
        {
            return Err("site path contains unsupported characters".to_string());
        }
    }
    Ok(components)
}

fn open_parent<'a>(
    root: &Path,
    components: &'a [&'a str],
    create: bool,
    uid: u32,
    gid: u32,
) -> Result<(File, &'a str), String> {
    let (name, dirs) = components
        .split_last()
        .ok_or_else(|| "site path is empty".to_string())?;
    let mut dir = open_root(root)?;
    for component in dirs {
        dir = open_child_dir(&dir, component, create, uid, gid)?;
    }
    Ok((dir, name))
}

fn open_root(root: &Path) -> Result<File, String> {
    let c_path = cstring(root.as_os_str().as_bytes())?;
    let fd = unsafe {
        libc::open(
            c_path.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(format!(
            "{}: {}",
            root.display(),
            std::io::Error::last_os_error()
        ));
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn open_child_dir(
    parent: &File,
    name: &str,
    create: bool,
    uid: u32,
    gid: u32,
) -> Result<File, String> {
    let c_name = cstring(name.as_bytes())?;
    if create {
        let result = unsafe { libc::mkdirat(parent.as_raw_fd(), c_name.as_ptr(), 0o755) };
        if result != 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::AlreadyExists {
                return Err(format!("cannot create site directory {name}: {error}"));
            }
        }
    }
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            c_name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(format!(
            "cannot open site directory {name}: {}",
            std::io::Error::last_os_error()
        ));
    }
    let dir = unsafe { File::from_raw_fd(fd) };
    if create {
        // The tenant owns its site tree; the broker only writes through it.
        if unsafe { libc::fchown(fd, uid, gid) } != 0 {
            return Err(format!(
                "cannot set site directory ownership: {}",
                std::io::Error::last_os_error()
            ));
        }
        if unsafe { libc::fchmod(fd, 0o755) } != 0 {
            return Err(format!(
                "cannot set site directory mode: {}",
                std::io::Error::last_os_error()
            ));
        }
        label_for_container(&dir)?;
    }
    Ok(dir)
}

fn open_file_at(
    dir: &File,
    name: &str,
    flags: libc::c_int,
    mode: libc::mode_t,
) -> Result<File, String> {
    let c_name = cstring(name.as_bytes())?;
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            c_name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            mode,
        )
    };
    if fd < 0 {
        return Err(format!(
            "cannot open site file {name}: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn write_file_at(
    dir: &File,
    name: &str,
    content: &[u8],
    uid: u32,
    gid: u32,
) -> Result<(), String> {
    let c_name = cstring(name.as_bytes())?;
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let temp_name = format!(".{name}.honed-{}-{nonce}", std::process::id());
    let c_temp = cstring(temp_name.as_bytes())?;
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            c_temp.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    if fd < 0 {
        return Err(format!(
            "cannot create site file: {}",
            std::io::Error::last_os_error()
        ));
    }
    let mut file = unsafe { File::from_raw_fd(fd) };
    let result = (|| {
        file.write_all(content)
            .map_err(|error| format!("cannot write site file: {error}"))?;
        if unsafe { libc::fchown(file.as_raw_fd(), uid, gid) } != 0 {
            return Err(format!(
                "cannot set site file ownership: {}",
                std::io::Error::last_os_error()
            ));
        }
        if unsafe { libc::fchmod(file.as_raw_fd(), 0o644) } != 0 {
            return Err(format!(
                "cannot set site file mode: {}",
                std::io::Error::last_os_error()
            ));
        }
        label_for_container(&file)?;
        file.sync_all()
            .map_err(|error| format!("cannot sync site file: {error}"))?;
        Ok(())
    })();
    if let Err(error) = result {
        let _ = unsafe { libc::unlinkat(dir.as_raw_fd(), c_temp.as_ptr(), 0) };
        return Err(error);
    }
    drop(file);
    if unsafe {
        libc::renameat(
            dir.as_raw_fd(),
            c_temp.as_ptr(),
            dir.as_raw_fd(),
            c_name.as_ptr(),
        )
    } != 0
    {
        let error = std::io::Error::last_os_error();
        let _ = unsafe { libc::unlinkat(dir.as_raw_fd(), c_temp.as_ptr(), 0) };
        return Err(format!("cannot replace site file: {error}"));
    }
    Ok(())
}

/// `system_u:object_r:container_file_t:s0` is readable by every container
/// domain, which is what the read-only nginx mount needs.
fn label_for_container(file: &File) -> Result<(), String> {
    let name = b"security.selinux\0";
    let value = b"system_u:object_r:container_file_t:s0\0";
    let result = unsafe {
        libc::fsetxattr(
            file.as_raw_fd(),
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
    Err(format!("cannot label site content for containers: {error}"))
}

fn cstring(bytes: &[u8]) -> Result<CString, String> {
    CString::new(bytes).map_err(|_| "site path contains NUL".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_are_relative_bounded_and_symlink_free_by_construction() {
        for valid in ["index.html", "css/site.css", "assets/logo.svg", ".well-known/x"] {
            assert!(validate_path(valid).is_ok(), "{valid}");
        }
        for invalid in [
            "",
            "/etc/passwd",
            "../escape",
            "a/../b",
            "a//b",
            "a/",
            "a\\b",
            "a b",
            "a\0b",
            "a/b/c/d/e/f/g/h/i/j/k/l/m/n/o/p/q",
        ] {
            assert!(validate_path(invalid).is_err(), "{invalid:?}");
        }
    }

    #[test]
    fn second_web_instance_uses_its_own_site_root() {
        let config = Config::from_env();
        let first = site_root(&config, "t91", "t91-site").unwrap();
        let second = site_root(&config, "t91", "t91-site-2").unwrap();
        assert!(first.ends_with("t91/site"));
        assert!(second.ends_with("t91/site-2/site"));
        assert_ne!(first, second);
        assert!(site_root(&config, "t91", "t91-db").is_err());
    }

    #[test]
    fn site_content_is_bounded_text() {
        let content = SiteContent {
            path: "index.html".to_string(),
            content: "<h1>hola</h1>".to_string(),
        };
        let encoded = serde_json::to_string(&content).unwrap();
        assert!(encoded.contains("index.html"));
        assert!(serde_json::from_str::<SiteContent>(&encoded).is_ok());
        assert!(serde_json::from_str::<SiteContent>(r#"{"path":"a","content":"b","extra":1}"#).is_err());
    }
}
