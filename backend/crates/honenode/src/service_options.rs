//! Typed service options mirrored from the broker contract (`honed::service_config`).
//!
//! The clear password exists only long enough to cross the broker socket: it is
//! redacted in `Debug`, zeroed on drop, and never reaches jobs, audit rows, logs,
//! templates, or the app SQLite registry.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::error::{AppError, AppResult};
use crate::models::ServiceKind;

const USERNAME_MAX: usize = 32;
const PASSWORD_MIN: usize = 12;
const PASSWORD_MAX: usize = 72;

/// A secret string: redacted in `Debug`, best-effort zeroed on drop.
#[derive(Default, Deserialize, Serialize)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
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
        // SAFETY: zero bytes are valid UTF-8; volatile writes discourage elision.
        unsafe {
            for byte in self.0.as_mut_vec().iter_mut() {
                std::ptr::write_volatile(byte, 0);
            }
        }
        self.0.clear();
    }
}

/// `options` of `limits.set` / `service.configure`. Unknown fields are refused so
/// a tampered form cannot smuggle host directives to the broker.
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_manager: Option<FileManagerOptions>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FileManagerOptions {
    pub username: String,
    pub password: Secret,
    #[serde(default)]
    pub read_only: bool,
}

impl ServiceOptions {
    /// Non-secret metadata for jobs and audit rows. There is deliberately no
    /// password or hash in this projection.
    pub fn audit_summary(&self) -> serde_json::Value {
        serde_json::json!({
            "image": self.image,
            "file_manager": self.file_manager.as_ref().map(|file_manager| {
                serde_json::json!({
                    "username": file_manager.username,
                    "read_only": file_manager.read_only,
                })
            }),
        })
    }
}

/// Sanitized `configuration` of `limits.set`, `service.inspect`, and
/// `service.configure`. Unknown fields fail closed: no secrets or host paths.
#[derive(Debug, Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceConfiguration {
    pub kind: String,
    pub image: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_only: Option<bool>,
}

impl ServiceConfiguration {
    /// Binds the broker answer to the service the tenant actually owns.
    pub fn validate(&self, expected: ServiceKind) -> AppResult<()> {
        let kind = ServiceKind::parse(&self.kind)
            .filter(|kind| *kind == expected)
            .ok_or_else(|| AppError::Broker("configuration kind mismatch".to_string()))?;
        if crate::images::ImageReference::parse(&self.image).is_none() {
            return Err(AppError::Broker("configuration image invalid".to_string()));
        }
        match kind {
            ServiceKind::Storage => {
                let username = self.username.as_deref().ok_or_else(|| {
                    AppError::Broker("storage configuration missing username".to_string())
                })?;
                validate_username(username).map_err(|_| {
                    AppError::Broker("storage configuration username invalid".to_string())
                })?;
                if self.read_only.is_none() {
                    return Err(AppError::Broker(
                        "storage configuration missing read_only".to_string(),
                    ));
                }
            }
            _ => {
                if self.username.is_some() || self.read_only.is_some() {
                    return Err(AppError::Broker(
                        "unexpected file-manager fields in configuration".to_string(),
                    ));
                }
            }
        }
        Ok(())
    }
}

pub fn validate_username(value: &str) -> AppResult<()> {
    if value.is_empty()
        || value.len() > USERNAME_MAX
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_.-".contains(&byte))
    {
        return Err(AppError::BadRequest(
            "usuario del gestor de archivos: 1-32 caracteres ASCII (letras, dígitos, _ . -)"
                .to_string(),
        ));
    }
    Ok(())
}

pub fn validate_new_password(password: &Secret) -> AppResult<()> {
    let value = password.expose();
    if value.len() < PASSWORD_MIN
        || value.len() > PASSWORD_MAX
        || value.chars().any(char::is_control)
    {
        return Err(AppError::BadRequest(
            "la contraseña del gestor de archivos debe tener entre 12 y 72 bytes, sin caracteres de control"
                .to_string(),
        ));
    }
    Ok(())
}

/// HTML checkboxes send `on` when checked and nothing otherwise; anything else
/// is a tampered field, not a truthy value.
pub fn checkbox(value: Option<&str>) -> AppResult<bool> {
    match value {
        None => Ok(false),
        Some("on") | Some("true") | Some("1") => Ok(true),
        Some(_) => Err(AppError::BadRequest(
            "valor de casilla no válido".to_string(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options() -> ServiceOptions {
        ServiceOptions {
            image: Some("docker.io/filebrowser/filebrowser:v2.63.23".to_string()),
            file_manager: Some(FileManagerOptions {
                username: "alice".to_string(),
                password: Secret::new("correct horse battery"),
                read_only: true,
            }),
        }
    }

    #[test]
    fn secrets_are_redacted_in_debug_and_absent_from_audit_summaries() {
        let options = options();
        let debug = format!("{options:?}");
        assert!(debug.contains("REDACTED"));
        assert!(!debug.contains("correct horse"));
        let summary = options.audit_summary().to_string();
        assert!(summary.contains("alice"));
        assert!(summary.contains("v2.63.23"));
        for forbidden in ["password", "correct horse", "hash", "bcrypt"] {
            assert!(!summary.contains(forbidden), "summary leaked {forbidden}");
        }
        // Only the broker request serializes the secret, never the audit projection.
        let wire = serde_json::to_value(&options).unwrap();
        assert_eq!(wire["file_manager"]["password"], "correct horse battery");
        assert_eq!(wire["file_manager"]["read_only"], true);
        assert!(ServiceOptions::default().audit_summary()["file_manager"].is_null());
    }

    #[test]
    fn option_schema_is_strict_and_bounds_match_the_broker() {
        assert!(serde_json::from_value::<ServiceOptions>(serde_json::json!({
            "image": "x", "privileged": true
        }))
        .is_err());
        assert!(serde_json::from_value::<FileManagerOptions>(serde_json::json!({
            "username": "alice", "password": "x", "shell": "/bin/sh"
        }))
        .is_err());
        for valid in ["a", "alice", "a_b-c.d", &"x".repeat(32)] {
            assert!(validate_username(valid).is_ok(), "{valid}");
        }
        for invalid in ["", "has space", "a/b", "é", "a\nb", &"x".repeat(33), "root!"] {
            assert!(validate_username(invalid).is_err(), "{invalid:?}");
        }
        for valid in ["x".repeat(12), "x".repeat(72), "áéíóú contraseña".to_string()] {
            assert!(validate_new_password(&Secret::new(valid)).is_ok());
        }
        for invalid in [
            "x".repeat(11),
            "x".repeat(73),
            "twelve chars\n".to_string(),
            "twelve chars\u{7}".to_string(),
        ] {
            assert!(validate_new_password(&Secret::new(invalid)).is_err());
        }
        // The bound is bytes, not chars: 36 two-byte characters exceed 72 bytes.
        assert!(validate_new_password(&Secret::new("á".repeat(36))).is_ok());
        assert!(validate_new_password(&Secret::new("á".repeat(37))).is_err());
        assert_eq!(checkbox(None).unwrap(), false);
        assert_eq!(checkbox(Some("on")).unwrap(), true);
        assert!(checkbox(Some("yes please")).is_err());
    }

    #[test]
    fn configuration_is_sanitized_and_bound_to_the_owned_kind() {
        let storage: ServiceConfiguration = serde_json::from_value(serde_json::json!({
            "kind": "storage", "image": "docker.io/filebrowser/filebrowser:v2.63.23",
            "username": "alice", "read_only": false
        }))
        .unwrap();
        storage.validate(ServiceKind::Storage).unwrap();
        assert!(storage.validate(ServiceKind::Web).is_err());
        assert!(serde_json::from_value::<ServiceConfiguration>(serde_json::json!({
            "kind": "storage", "image": "docker.io/filebrowser/filebrowser:v2.63.23",
            "username": "alice", "read_only": false, "password": "leak"
        }))
        .is_err());
        let web: ServiceConfiguration = serde_json::from_value(serde_json::json!({
            "kind": "web", "image": "docker.io/nginxinc/nginx-unprivileged:1.27-alpine"
        }))
        .unwrap();
        web.validate(ServiceKind::Web).unwrap();
        let text = serde_json::to_string(&web).unwrap();
        assert!(!text.contains("username") && !text.contains("read_only"));
        let mut hostile = storage.clone();
        hostile.image = "evil\"><script>".to_string();
        assert!(hostile.validate(ServiceKind::Storage).is_err());
        let mut web_with_user = web;
        web_with_user.username = Some("alice".to_string());
        assert!(web_with_user.validate(ServiceKind::Web).is_err());
    }
}
