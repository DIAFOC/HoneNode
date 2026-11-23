//! Generic wire contract and preflight validation. Root policy remains authoritative.
use std::collections::{BTreeMap, BTreeSet};
use serde::{Deserialize, Serialize};
use crate::{error::{AppError, AppResult}, service_options::Secret};

const INVALID: &str = "Especificación no válida: revisa nombre, referencia con registro y etiqueta/digest, usuario no root, variables únicas y destinos de volúmenes. Los valores secretos se han borrado.";
fn invalid() -> AppError { AppError::BadRequest(INVALID.into()) }

#[derive(Debug, Serialize)]
pub struct ContainerSpec {
    pub image: String,
    pub environment: BTreeMap<String, Secret>,
    pub container_port: u16,
    pub user: u32,
    pub volumes: Vec<VolumeSpec>,
    pub read_only: bool,
}
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct VolumeSpec { pub name: String, pub target: String }
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

pub fn valid_registry(value: &str) -> bool {
    if value.len() > 253 || value.is_empty() { return false; }
    let (host, port) = value.split_once(':').map_or((value, None), |(h,p)| (h,Some(p)));
    if port.is_some_and(|p| p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) || p.parse::<u16>().map_or(true, |n| n == 0)) { return false; }
    (host.contains('.') || host == "localhost" || port.is_some()) && host.split('.').all(|part| !part.is_empty() && part.len() <= 63
        && part.as_bytes()[0].is_ascii_alphanumeric() && part.as_bytes()[part.len()-1].is_ascii_alphanumeric()
        && part.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'))
}
fn image_registry(image: &str) -> Option<&str> {
    if image.len() > 512 || !image.bytes().all(|b| b.is_ascii_alphanumeric() || b"/._-:@".contains(&b)) { return None; }
    let (registry, reference) = image.split_once('/')?;
    if !valid_registry(registry) { return None; }
    let repository = if let Some((repo,digest)) = reference.split_once("@sha256:") {
        if digest.len() != 64 || !digest.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) { return None; }
        repo
    } else {
        let (repo,tag) = reference.rsplit_once(':')?;
        if tag.is_empty() || tag.len() > 128 || tag.starts_with(['-', '.']) || !tag.bytes().all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b)) { return None; }
        repo
    };
    if !repository.split('/').all(|p| !p.is_empty() && p.as_bytes()[0].is_ascii_alphanumeric() && p.as_bytes()[p.len()-1].is_ascii_alphanumeric()
        && p.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b))) { return None; }
    Some(registry)
}
fn env_key(key: &str) -> bool {
    !key.is_empty() && key.len() <= 64 && (key.as_bytes()[0].is_ascii_alphabetic() || key.starts_with('_'))
        && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}
fn below(path: &str, ancestor: &str) -> bool { path == ancestor || path.strip_prefix(ancestor).is_some_and(|r| r.starts_with('/')) }
fn target_valid(path: &str) -> bool {
    path.len() <= 256 && path.starts_with('/') && path != "/" && path[1..].split('/').all(|p| !p.is_empty() && p != "." && p != ".."
        && p.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)))
        && !["/proc", "/sys", "/dev", "/run", "/var/run"].iter().any(|r| below(path,r)) && !path.ends_with(".sock")
}
pub fn unit_name(slug: &str, name: &str) -> AppResult<String> {
    let unit = format!("{slug}-{name}");
    if unit.len() > 64 || name.is_empty() || name.starts_with('-') || name.ends_with('-')
        || !name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        || ["site", "web", "db", "database", "vault", "files", "storage", "git", "forgejo"].contains(&name) { return Err(invalid()); }
    Ok(unit)
}
fn port_like(key: &str) -> bool {
    key == "PORT" || key.ends_with("_PORT") || key.ends_with("_PORT_NUMBER")
}

impl ContainerSpec {
    pub fn from_text(image: String, port: u16, user: u32, environment: &Secret, volumes: &str, read_only: bool) -> AppResult<Self> {
        let mut env = BTreeMap::new();
        for line in environment.expose().lines().filter(|l| !l.is_empty()) {
            let (key, value) = line.split_once('=').ok_or_else(|| AppError::BadRequest(
                "cada variable debe tener el formato KEY=VALUE, una por línea".into(),
            ))?;
            if !env_key(key) {
                return Err(AppError::BadRequest(format!(
                    "nombre de variable inválido: {key:?}; usa letras, números y _"
                )));
            }
            if value.chars().any(char::is_control) {
                return Err(AppError::BadRequest(format!(
                    "la variable {key} no puede contener saltos de línea ni caracteres de control"
                )));
            }
            if env.insert(key.to_string(), Secret::new(value)).is_some() {
                return Err(AppError::BadRequest(format!("variable duplicada: {key}")));
            }
            // A URL or a path in PORT / *_PORT / *_PORT_NUMBER makes Node listen
            // on a Unix socket (EROFS on a read-only root) and makes Go look up
            // an unknown TCP service. Catch it before the container starts.
            if port_like(key) && value.parse::<u16>().map_or(true, |port| port == 0) {
                return Err(AppError::BadRequest(format!(
                    "{key} debe ser un número de puerto (1-65535), no una URL ni una ruta"
                )));
            }
        }
        let mut mounts = Vec::new();
        for line in volumes.lines().filter(|l| !l.is_empty()) {
            let (name, target) = line.split_once('=').ok_or_else(|| AppError::BadRequest(
                "cada volumen debe tener el formato nombre=/destino, uno por línea".into(),
            ))?;
            mounts.push(VolumeSpec { name: name.into(), target: target.into() });
        }
        Ok(Self { image, environment: env, container_port: port, user, volumes: mounts, read_only })
    }
    pub fn validate(&self, registries: &[String]) -> AppResult<()> {
        let registry = image_registry(&self.image).ok_or_else(|| AppError::BadRequest(
            "la imagen debe incluir un registro, un repositorio y una etiqueta o digest".into(),
        ))?;
        if !registries.iter().any(|allowed| allowed == registry) {
            return Err(AppError::BadRequest(format!(
                "el registro {registry} no está permitido por la política"
            )));
        }
        if self.container_port == 0 {
            return Err(AppError::BadRequest("el puerto interno debe ser 1-65535".into()));
        }
        if !(1..=65535).contains(&self.user) {
            return Err(AppError::BadRequest("el UID debe ser 1-65535".into()));
        }
        if self.environment.len() > 32 {
            return Err(AppError::BadRequest("máximo 32 variables de entorno".into()));
        }
        if self.volumes.len() > 8 {
            return Err(AppError::BadRequest("máximo 8 volúmenes".into()));
        }
        let mut total = 0;
        for (key, value) in &self.environment {
            total += key.len() + value.expose().len() + 2;
            if !env_key(key) {
                return Err(AppError::BadRequest(format!("nombre de variable inválido: {key:?}")));
            }
            if value.expose().len() > 4096 || value.expose().chars().any(char::is_control) {
                return Err(AppError::BadRequest(format!("valor inválido para {key}")));
            }
        }
        if total > 16384 {
            return Err(AppError::BadRequest("las variables de entorno superan 16 KiB".into()));
        }
        let mut names = BTreeSet::new();
        for (index, volume) in self.volumes.iter().enumerate() {
            if volume.name.is_empty()
                || volume.name.len() > 32
                || !volume.name.as_bytes()[0].is_ascii_lowercase()
                || !volume.name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_-".contains(&b))
            {
                return Err(AppError::BadRequest(format!(
                    "nombre de volumen inválido: {:?}; usa minúsculas, números, _ y -", volume.name
                )));
            }
            if !names.insert(&volume.name) {
                return Err(AppError::BadRequest(format!("volumen duplicado: {}", volume.name)));
            }
            if !target_valid(&volume.target) {
                return Err(AppError::BadRequest(format!(
                    "destino de volumen inválido: {:?}; usa una ruta absoluta fuera de /proc, /sys, /dev y /run", volume.target
                )));
            }
            if self.volumes[..index]
                .iter()
                .any(|other| below(&other.target, &volume.target) || below(&volume.target, &other.target))
            {
                return Err(AppError::BadRequest("los volúmenes no pueden solaparse".into()));
            }
        }
        Ok(())
    }
    pub fn summary(&self) -> serde_json::Value {
        serde_json::json!({"image":self.image,"environment_keys":self.environment.keys().collect::<Vec<_>>(),"volume_names":self.volumes.iter().map(|v| &v.name).collect::<Vec<_>>()})
    }
}
impl ContainerInfo {
    pub fn validate(&self, unit: &str) -> AppResult<()> {
        if self.name != unit || self.host_port == 0 || self.environment_keys.len() > 32 || !self.environment_keys.iter().all(|k| env_key(k)) { return Err(AppError::Broker("invalid safe container info".into())); }
        let spec = ContainerSpec {image:self.image.clone(),environment:BTreeMap::new(),container_port:self.container_port,user:self.user,volumes:self.volumes.clone(),read_only:self.read_only};
        spec.validate(&[image_registry(&self.image).unwrap_or_default().to_string()]).map_err(|_| AppError::Broker("invalid safe container info".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn port_like_environment_values_must_be_numeric() {
        let parse = |env: &str| {
            ContainerSpec::from_text(
                "approved.example/app:1".into(),
                3000,
                1000,
                &Secret::new(env),
                "",
                false,
            )
        };
        assert!(parse("PORT=3000\nNODE_ENV=production").is_ok());
        assert!(parse("WHOAMI_PORT_NUMBER=8080").is_ok());
        assert!(parse("FORGEJO__server__HTTP_PORT=3000").is_ok());
        assert!(parse("ROOT_URL=http://127.0.0.1:3000/").is_ok());
        for env in [
            "PORT=http://127.0.0.1:3000",
            "PORT=/tmp/app.sock",
            "WHOAMI_PORT_NUMBER=abc",
            "PORT=0",
        ] {
            let error = parse(env).unwrap_err();
            assert!(error.to_string().contains("número de puerto"), "{env}");
        }
    }

    #[test]
    fn literals_duplicates_injection_and_mount_escape() {
        let parse = |env:&str,vol:&str| ContainerSpec::from_text("approved.example/random:1".into(),8080,1000,&Secret::new(env),vol,false);
        let spec = parse("KEY=$(literal) 'pass phrase'", "data=/data").unwrap();
        assert!(spec.validate(&["approved.example".into()]).is_ok());
        assert!(!format!("{spec:?}").contains("pass phrase"));
        assert!(spec.validate(&[]).is_err());
        for env in ["KEY=one\nKEY=two", "KEY=one\rINJECT=x", "KEY=one\nINJECT", "KEY=one\0"] { assert!(parse(env,"").is_err()); }
        for vol in ["data=/a/../b", "data=/run/socket", "data=/data\ndata=/other", "a=/data\nb=/data/sub"] { assert!(parse("",vol).unwrap().validate(&["approved.example".into()]).is_err()); }
        assert!(parse("A=one\r\nB=two", "data=/data\r\ncache=/cache").unwrap().validate(&["approved.example".into()]).is_ok());
        assert!(unit_name("t01","forgejo").is_err());
        assert!(unit_name("t01","forge-one").is_ok());
    }
}
