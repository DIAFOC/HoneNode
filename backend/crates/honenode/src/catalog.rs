use crate::models::ServiceKind;

#[derive(Debug, Clone, Copy)]
pub struct Entry {
    pub kind: ServiceKind,
    pub label: &'static str,
    pub summary: &'static str,
    pub accent: &'static str,
    pub image: &'static str,
    pub needs_egress: bool,
    pub public_ports: &'static [&'static str],
    pub controls: &'static [&'static str],
}

const ENTRIES: [Entry; 5] = [
    Entry {
        kind: ServiceKind::Container,
        label: "Contenedor",
        summary: "Imagen de un registro autorizado, sin privilegios y con volúmenes gestionados.",
        accent: "#A3E635",
        image: "Referencia elegida por el inquilino",
        needs_egress: true,
        public_ports: &[],
        controls: &["Rootless, usuario no root", "Sin comandos ni montajes del host", "HTTP en loopback; publicación pública requiere configuración del operador"],
    },
    Entry {
        kind: ServiceKind::Web,
        label: "Sitio web",
        summary: "Contenido estático servido por nginx sin privilegios, con el contenido en el entorno del inquilino.",
        accent: "#A3E635",
        image: "nginxinc/nginx-unprivileged",
        needs_egress: false,
        public_ports: &[],
        controls: &[
            "Sistema de archivos raíz de solo lectura",
            "Contenido montado en modo lectura",
            "Sin capacidades, sin privilegios",
        ],
    },
    Entry {
        kind: ServiceKind::Database,
        label: "Base de datos",
        summary: "PostgreSQL relacional con publicación en loopback; acceso coordinado por el operador.",
        accent: "#3B82F6",
        image: "postgres:17-alpine",
        needs_egress: false,
        public_ports: &[],
        controls: &[
            "Publicación exclusiva en loopback",
            "Autenticación scram-sha-256",
            "Consola del broker aún no implementada",
        ],
    },
    Entry {
        kind: ServiceKind::Storage,
        label: "Bóveda de medios",
        summary: "File Browser sobre un volumen del inquilino. Dependencia archivada; sin cuota de disco ni inspección de archivos integradas.",
        accent: "#F4F4F5",
        image: "filebrowser/filebrowser",
        needs_egress: false,
        public_ports: &[],
        controls: &[
            "Cuota de disco aún no aplicada",
            "Inspección MIME no integrada",
            "Dependencia archivada: requiere decisión de seguridad antes de producción",
        ],
    },
    Entry {
        kind: ServiceKind::Git,
        label: "Forja git",
        summary: "Forgejo autogestionado con SQLite y SSH de git en un puerto aprobado.",
        accent: "#F97316",
        image: "codeberg.org/forgejo/forgejo",
        needs_egress: false,
        public_ports: &["22000+id"],
        controls: &[
            "Completa la página de instalación (cuenta admin) justo después de activar",
            "SSH git en puerto dedicado del inquilino",
            "Acciones de gestión registradas por HoneNode",
        ],
    },
];

pub fn all() -> &'static [Entry] {
    &ENTRIES
}

pub fn entry(kind: ServiceKind) -> &'static Entry {
    ENTRIES
        .iter()
        .find(|entry| entry.kind == kind)
        .unwrap_or(&ENTRIES[0])
}

pub fn label(kind: ServiceKind) -> &'static str {
    entry(kind).label
}

pub fn accent(kind: ServiceKind) -> &'static str {
    entry(kind).accent
}

pub fn unit_name(slug: &str, kind: ServiceKind) -> String {
    let suffix = match kind {
        ServiceKind::Web => "web",
        ServiceKind::Database => "db",
        ServiceKind::Storage => "vault",
        ServiceKind::Git => "forgejo",
        ServiceKind::Container => "container",
    };
    format!("{slug}-{suffix}")
}

pub fn tenant_number(slug: &str) -> Option<u32> {
    slug.strip_prefix('t')?.parse().ok()
}

pub fn ports(slug: &str, kind: ServiceKind) -> serde_json::Value {
    let Some(number) = tenant_number(slug) else {
        return serde_json::json!({});
    };
    match kind {
        ServiceKind::Container => serde_json::json!({}),
        ServiceKind::Web | ServiceKind::Storage => {
            serde_json::json!({ "web": 18000 + number })
        }
        ServiceKind::Database => serde_json::json!({ "database": 15430 + number }),
        ServiceKind::Git => serde_json::json!({
            "web": 18000 + number,
            "git_ssh": 22000 + number
        }),
    }
}

pub fn volume(root: &std::path::Path, slug: &str, kind: ServiceKind) -> serde_json::Value {
    let base = root.join(slug);
    let path = |name: &str| base.join(name).display().to_string();
    match kind {
        ServiceKind::Container => serde_json::json!({}),
        ServiceKind::Web => serde_json::json!({ "site": path("site") }),
        ServiceKind::Database => serde_json::json!({ "pgdata": path("pgdata") }),
        ServiceKind::Storage => serde_json::json!({
            "vault": path("vault"),
            "database": path("db")
        }),
        ServiceKind::Git => serde_json::json!({
            "gitea": path("gitea"),
            "config": path("gitea-config"),
            "pgdata": path("pgdata")
        }),
    }
}
