# Generic run-image workflow

No workload was deployed while implementing this feature. The following is a guide for the owner's later manual test, not a verified Forgejo deployment.

## Operator prerequisites

The live broker root policy must explicitly allow kind `container` on the selected plan and include the exact registry hostname (optionally port) in `allowed_registries`. Its default empty registry list denies every image. This is registry trust, not approval of an exact image, signature verification, or a guarantee that a repository/tag exists. Rootless Podman pulls at queued startup. Private registry credential onboarding is not provided by this form.

## Form

GET and POST `/portal/containers/new`: `csrf`, `name`, `plan_code`, `image`, `container_port`, `user` (1000 by default), `environment` (literal KEY=VALUE lines), `volumes` (managed name=/container/path lines), optional `read_only` checkbox. Plan limits are read-only. Duplicate fields or environment keys are rejected. Environment values are never re-rendered or stored in app SQL/jobs/audit; the broker owns private runtime environment storage. On validation/conflict errors the form retains the name, plan, image, internal port, UID, volumes and read-only selection, but clears **all** environment lines, including non-secret ones. Re-enter them before resubmitting. Malformed/duplicate form fields or invalid CSRF are rejected without reflecting submitted values.

The optional **Generar secretos marcados** button uses browser `crypto.getRandomValues` to replace explicit `GENERATE` (or `REEMPLAZA_CON_UN_SECRETO_ALEATORIO`) values on keys named `SECRET`, `PASSWORD`, `NONCE`, or ending in `_SECRET`, `_PASSWORD`, `_NONCE`. Each replacement is 16 random bytes encoded as 32 lowercase hexadecimal characters (128 bits); generation does not contact the server, save to browser storage, submit the form, overwrite actual values, or run automatically. For example, enter `APP_SECRET=GENERATE` and `NONCE=GENERATE`, then click the button. Only the normal form submission sends the resulting environment to the app/broker. A password manager remains an alternative if JavaScript/browser crypto is unavailable. Generation does not impose application-specific secret requirements.

Services reserve a unique immutable unit name before tenant/container creation. Failed creates retain the record and quota reservation for operator review; no automatic retry/reclamation is offered. Deleted unit names remain reserved. Multiple container instances can share a kind and image, subject to the selected plan's atomic tenant-wide quota.

## Later Forgejo rootless test

1. Authorize `codeberg.org` in root policy and a container-capable plan. Do not select the optional fixed Git template (its onboarding is separate).
2. Choose a non-reserved name such as `forge-one` (not `forgejo` or `git`). Enter a fully qualified **existing rootless Forgejo tag**, e.g. `codeberg.org/forgejo/forgejo:<verified-version>-rootless`; replace the version before submitting. Consult that version's upstream documentation for supported UID, persistence and configuration.
3. Set HTTP port `3000`, UID `1000`, and managed volume `data=/var/lib/gitea`. Leave read-only root off initially. The registry-verified rootless image keeps data and configuration below `/var/lib/gitea`; do not use the regular `/data` layout or an obsolete `/etc/gitea` mount in this guide. Verify against the chosen image version.
4. Supply application env lines as needed, e.g. `FORGEJO__server__HTTP_PORT=3000` and `FORGEJO__server__ROOT_URL=${HONE_HTTP_ORIGIN}/`. Use the optional placeholder generator or a password manager for required secrets. Shell quotes, dollar signs and command-looking values remain literal; do not use `export` or put multiline values in a variable. Configure SQLite or an external DB explicitly per upstream guidance; this flow does not create a companion database.
5. Submit once, then use Resumen → Actualizar estado and Registros. Rootless pull/start is queued; Pending/Failed controls stay blocked for operator review. The background reconciler observes activation; `active` is systemd state, not HTTP readiness or completed Forgejo setup.
6. With loopback service access configured, the panel links to the **broker-allocated** `http://127.0.0.1:PORT/`. That address is on the host: use an appropriate SSH tunnel from another machine. Default public mode does not manufacture a URL. No public hostname is claimed ready. Coordinate routing/TLS and Forgejo's external ROOT_URL **before** creation/onboarding.

### ROOT_URL and the allocated port

Broker-side reserved placeholders are expanded when it creates the environment
file, after allocating a host port:

```text
FORGEJO__server__ROOT_URL=${HONE_HTTP_ORIGIN}/
FORGEJO__server__DOMAIN=localhost
FORGEJO__server__HTTP_PORT=3000
FORGEJO__database__DB_TYPE=sqlite3
```

`${HONE_HTTP_ORIGIN}` becomes `http://127.0.0.1:ASSIGNED_PORT` with no trailing
slash; `${HONE_HOST_PORT}` becomes the decimal port. These are explicit broker
substitutions, not shell expansion. Embedded occurrences are accepted, unknown
placeholders and other dollar-prefixed text remain literal, and expanded size
limits are revalidated. No environment values or the expanded URL containing
that port are persisted as secrets in the app database.

The guide's verified 2026-11-11 multi-arch image reference is
`codeberg.org/forgejo/forgejo@sha256:5ef02fbcf22beef56b8c844952ffad58cfe177b6f98e46e6abb5df90b88dc8d3`
(17.0.1 rootless, also tagged 17-rootless). It was checked through the registry
manifest/config, not by launching it here. On this takeover's verified rootless
layout, use `UID=1000`, internal HTTP `3000`, one managed volume
`data=/var/lib/gitea`, and leave read-only root off. Data, configuration, git
repositories and SQLite live beneath that path. No companion database or SSH
port is created. Generic name `forge-one` is not the reserved fixed-template
`forgejo`.

There is still **no environment/image/volume editing after creation**. A later
move to public HTTPS needs an operator routing plan and Forgejo's application
configuration. The local URL is on the host; for an SSH tunnel expose the same
assigned local port or coordinate the public ROOT_URL beforehand.

## Boundaries

Existing optional templates/file-manager configuration remain available. Generic summary, safe live inspection, lifecycle controls and bounded logs share the service routes. Generic configuration is read-only; environment/volume/image editing after creation, custom commands, host mounts, privileged mode, extra TCP/SSH ports, automatic edge publication and terminal access are not provided. Forgejo's own setup/UI is outside this panel. Volumes have no enforced disk quota yet.
