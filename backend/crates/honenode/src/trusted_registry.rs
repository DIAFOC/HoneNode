//! Curated trusted-registry index shown to tenants for peace of mind.
//!
//! This is documentation, not authorization: provisioning still validates every
//! image against the live root catalogue (`allowed_registries` + approved
//! references). Links point at the upstream registry page of each image and are
//! never used to import, search or pull anything.

pub struct TrustedImage {
    /// Human label of the workload.
    pub name: &'static str,
    /// What the tenant would use it for.
    pub purpose: &'static str,
    /// Fully qualified reference the platform deploys today (or the maintained
    /// upstream reference for reference-only entries).
    pub reference: &'static str,
    /// Upstream registry page of the image.
    pub registry_url: &'static str,
    /// Platform service kind it maps to, or `tool` for reference-only entries.
    pub kind: &'static str,
    /// True for the four demo services of the hosting presentation.
    pub demo: bool,
    /// Constraints worth reading before deploying (egress, archived upstream…).
    pub note: &'static str,
}

/// Verified against the upstream publishers on 2026-11-17.
pub const ENTRIES: &[TrustedImage] = &[
    TrustedImage {
        name: "Blog de desarrollador",
        purpose: "Sitio estático servido por nginx sin privilegios",
        reference: "docker.io/nginxinc/nginx-unprivileged:1.27-alpine",
        registry_url: "https://hub.docker.com/r/nginxinc/nginx-unprivileged",
        kind: "web",
        demo: true,
        note: "Imagen aprobada de la plantilla web; el contenido vive en el volumen del inquilino.",
    },
    TrustedImage {
        name: "Base de datos de la tienda",
        purpose: "PostgreSQL para el comercio (modelo 3FN básico)",
        reference: "docker.io/library/postgres:17-alpine",
        registry_url: "https://hub.docker.com/_/postgres",
        kind: "database",
        demo: true,
        note: "Publicación exclusiva en loopback; la consola la media el operador.",
    },
    TrustedImage {
        name: "Bóveda de archivos",
        purpose: "File Browser para imágenes y medios",
        reference: "docker.io/filebrowser/filebrowser:v2.63.23",
        registry_url: "https://hub.docker.com/r/filebrowser/filebrowser",
        kind: "storage",
        demo: true,
        note: "Upstream archivado: exige decisión de seguridad del operador antes de producción.",
    },
    TrustedImage {
        name: "Forja git",
        purpose: "Forgejo para el repositorio de código misceláneo",
        reference: "codeberg.org/forgejo/forgejo:17-rootless",
        registry_url: "https://codeberg.org/forgejo/-/packages/container/forgejo",
        kind: "git",
        demo: true,
        note: "SSH de git en el puerto dedicado del inquilino (excepción aprobada).",
    },
    TrustedImage {
        name: "whoami",
        purpose: "Eco HTTP para probar publicación y rutas",
        reference: "docker.io/traefik/whoami:v1.12.0",
        registry_url: "https://hub.docker.com/r/traefik/whoami",
        kind: "tool",
        demo: false,
        note: "Referencia de prueba; ya demostrada en el laboratorio.",
    },
    TrustedImage {
        name: "MariaDB",
        purpose: "Base de datos compatible con MySQL",
        reference: "docker.io/library/mariadb:11",
        registry_url: "https://hub.docker.com/_/mariadb",
        kind: "database",
        demo: false,
        note: "Imagen oficial de Docker Library; fija la etiqueta menor antes de producción.",
    },
    TrustedImage {
        name: "Bruno CLI",
        purpose: "Ejecutar colecciones de API (brunoapi)",
        reference: "docker.io/usebruno/cli:latest",
        registry_url: "https://hub.docker.com/r/usebruno/cli",
        kind: "tool",
        demo: false,
        note: "Editor verificado de Docker Hub; corre como UID 1000. Ejecuta la colección y termina: no es un servicio residente, y para alcanzar un objetivo necesita que este sea alcanzable desde su red.",
    },
    TrustedImage {
        name: "OWASP ZAP",
        purpose: "DAST contra objetivos autorizados",
        reference: "ghcr.io/zaproxy/zaproxy:stable",
        registry_url: "https://github.com/zaproxy/zaproxy/pkgs/container/zaproxy",
        kind: "tool",
        demo: false,
        note: "Mantenimiento activo del equipo ZAP. Requiere alcance al objetivo: dentro del modelo sin egreso solo escanea lo alcanzable desde su propia red.",
    },
    TrustedImage {
        name: "CoreDNS",
        purpose: "DNS autoritativo para una zona propia",
        reference: "docker.io/coredns/coredns",
        registry_url: "https://hub.docker.com/_/coredns",
        kind: "dns",
        demo: false,
        note: "Viable como servidor autoritativo (solo entrada). La recursión requiere egreso, y un puerto 53/UDP público contradice el modelo de borde: expón DNS-over-HTTPS a través de Caddy o mantenlo en loopback.",
    },
    TrustedImage {
        name: "2FAuth",
        purpose: "Resolver TOTP de 2FA autoalojado",
        reference: "docker.io/2fauth/2fauth",
        registry_url: "https://hub.docker.com/r/2fauth/2fauth",
        kind: "totp",
        demo: false,
        note: "Web app solo de entrada, UID 1000, datos en SQLite: encaja con el modelo sin egreso.",
    },
];
