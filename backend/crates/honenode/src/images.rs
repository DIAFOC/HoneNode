//! Approved registry image references from the live root catalogue.
//!
//! References are parsed, never concatenated into URLs blindly: an upstream
//! registry link is derived only for known registry hosts, and it is always a
//! secondary documentation link — never an image import or a registry search.

/// One approved image reference (`registry/repository[:tag][@digest]`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageReference {
    pub reference: String,
    pub registry: String,
    pub repository: String,
    pub tag: Option<String>,
    pub digest: Option<String>,
}

impl ImageReference {
    pub fn parse(reference: &str) -> Option<Self> {
        if reference.is_empty()
            || reference.len() > 255
            || !reference
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._/:@+-".contains(&byte))
        {
            return None;
        }
        let (name, digest) = match reference.split_once('@') {
            Some((name, digest)) => {
                let hex = digest.strip_prefix("sha256:")?;
                if hex.len() != 64 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                    return None;
                }
                (name, Some(digest.to_string()))
            }
            None => (reference, None),
        };
        if name.is_empty() || name.contains('@') {
            return None;
        }
        let (name, tag) = match name.rfind(':') {
            Some(index) if index > name.rfind('/').unwrap_or(0) => {
                let (repository, tag) = name.split_at(index);
                let tag = &tag[1..];
                if tag.is_empty() {
                    return None;
                }
                (repository, Some(tag.to_string()))
            }
            _ => (name, None),
        };
        let components: Vec<&str> = name.split('/').collect();
        if components
            .iter()
            .any(|part| part.is_empty() || *part == "." || *part == "..")
        {
            return None;
        }
        let (registry, repository) = match components.as_slice() {
            [single] => ("docker.io".to_string(), (*single).to_string()),
            [first, rest @ ..]
                if first.contains('.') || first.contains(':') || *first == "localhost" =>
            {
                (first.to_string(), rest.join("/"))
            }
            _ => ("docker.io".to_string(), components.join("/")),
        };
        if repository.is_empty() {
            return None;
        }
        Some(Self {
            reference: reference.to_string(),
            registry,
            repository,
            tag,
            digest,
        })
    }

    /// Human-readable `registry/repository` without tag or digest.
    pub fn repository_display(&self) -> String {
        format!("{}/{}", self.registry, self.repository)
    }

    /// Documentation page of the exact upstream repository, if the registry is
    /// known. Unknown registries get no link instead of a fabricated search.
    pub fn registry_url(&self) -> Option<String> {
        let repository = self.repository.as_str();
        match self.registry.as_str() {
            "docker.io" | "index.docker.io" | "registry-1.docker.io" => {
                if repository.contains('/') {
                    Some(format!("https://hub.docker.com/r/{repository}"))
                } else {
                    Some(format!("https://hub.docker.com/_/{repository}"))
                }
            }
            "quay.io" => Some(format!("https://quay.io/repository/{repository}")),
            "codeberg.org" => {
                let (owner, package) = repository.split_once('/')?;
                Some(format!(
                    "https://codeberg.org/{owner}/-/packages/container/{package}"
                ))
            }
            "ghcr.io" => {
                let (owner, package) = repository.split_once('/')?;
                Some(format!(
                    "https://github.com/{owner}/{package}/pkgs/container/{package}"
                ))
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approved_references_split_into_repository_tag_and_digest() {
        let image = ImageReference::parse("docker.io/library/postgres:17-alpine").unwrap();
        assert_eq!(image.registry, "docker.io");
        assert_eq!(image.repository, "library/postgres");
        assert_eq!(image.tag.as_deref(), Some("17-alpine"));
        assert_eq!(image.digest, None);
        assert_eq!(
            image.registry_url().as_deref(),
            Some("https://hub.docker.com/r/library/postgres")
        );
        let image = ImageReference::parse("nginxinc/nginx-unprivileged:1.27-alpine").unwrap();
        assert_eq!(image.registry, "docker.io");
        assert_eq!(image.repository, "nginxinc/nginx-unprivileged");
        assert_eq!(
            image.registry_url().as_deref(),
            Some("https://hub.docker.com/r/nginxinc/nginx-unprivileged")
        );
        let image = ImageReference::parse("postgres:17-alpine").unwrap();
        assert_eq!(image.repository_display(), "docker.io/postgres");
        assert_eq!(
            image.registry_url().as_deref(),
            Some("https://hub.docker.com/_/postgres")
        );
        let image = ImageReference::parse("codeberg.org/forgejo/forgejo:17-rootless").unwrap();
        assert_eq!(
            image.registry_url().as_deref(),
            Some("https://codeberg.org/forgejo/-/packages/container/forgejo")
        );
        let digest = format!("quay.io/org/app@sha256:{}", "a".repeat(64));
        let image = ImageReference::parse(&digest).unwrap();
        assert_eq!(image.tag, None);
        assert_eq!(image.digest.as_deref(), Some(&digest["quay.io/org/app@".len()..]));
        assert_eq!(
            image.registry_url().as_deref(),
            Some("https://quay.io/repository/org/app")
        );
        let image = ImageReference::parse("registry.example.test:5000/team/app:2").unwrap();
        assert_eq!(image.registry, "registry.example.test:5000");
        assert_eq!(image.repository, "team/app");
        assert_eq!(image.tag.as_deref(), Some("2"));
        assert_eq!(image.registry_url(), None);
    }

    #[test]
    fn unsafe_or_malformed_references_are_rejected_not_linked() {
        for reference in [
            "",
            "evil\"><script>alert(1)</script>",
            "https://hub.docker.com/_/nginx",
            "docker.io/library/postgres:17 alpine",
            "docker.io//postgres",
            "/postgres",
            "postgres/",
            "docker.io/../postgres",
            "docker.io/library/postgres:",
            "docker.io/library/postgres@sha256:short",
            "docker.io/library/postgres@md5:aaaa",
            "image\nExecStart=/bin/sh",
            "image with spaces",
            &"a".repeat(256),
        ] {
            assert!(ImageReference::parse(reference).is_none(), "{reference:?}");
        }
    }
}
