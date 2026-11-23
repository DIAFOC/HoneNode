# HoneNode themes and UI

The UI is role-aware. Set `HONE_THEME` in the application's environment and restart:

| Surface | Unset / `sharpnode` | `neutral` |
|---|---|---|
| Authenticated operator | Neutral HoneNode | Neutral HoneNode |
| Authenticated tenant | SharpNode, obsidian and gold | Neutral HoneNode |
| Anonymous login, including failed login | Neutral HoneNode | Neutral HoneNode |

`SharpNode` is the `Theme` enum default and `HONE_THEME` defaults to `sharpnode`. `principal_view` enforces neutral for operators even when passed `Theme::SharpNode`, and for roleless login (`None`). Unknown environment values fail configuration parsing. The setting is a deployment-level tenant appearance preference, not a permission or a browser preference. Routes, cookies, domains, broker policies and service allocation are unchanged.

```text
HONE_THEME=neutral
```

Use this override for a neutral tenant portal. `HONE_THEME=sharpnode` explicitly selects the default tenant branding. Deploy the application and `backend/static/hone.css` together.

## Layout

The operator console uses beige/gray/white surfaces, a compact side navigation and bordered data panels. The dashboard retains the full host metrics and shows the same supplied `host_stats` as a sidebar summary; other pages do not invent host telemetry. The tenant console uses SharpNode branding by default, with navigation for services, new service, activity and upstream image registries. Neutral tenants retain that same navigation and layout.

A proportional two-column shell keeps the navigation narrow and the content flexible, with a bounded reading width. On small screens the navigation moves above the content, plan panels stack, and wide tables scroll independently. Panels and controls have sharp corners: no gradients, glass, shadows, external fonts or decorative rounded cards. Status labels accompany semantic colors, keyboard focus remains visible, and a skip link targets the main content.

Both themes use the single local stylesheet. Askama writes an allowlisted `theme-neutral` or `theme-sharpnode` class on the root element. Dark colors and service-kind accents are scoped to `theme-sharpnode`. There is no JavaScript, inline style, remote stylesheet, font, CDN or CSP exception.

## Provisioning UI contract

- `GET /portal/catalog` renders `tenant/catalog.html` using only `title`, `accent_class`, `principal`, `csrf`, `tenant_name`, `entries: Vec<CatalogEntry>` and `has_entries`.
- Each entry uses `plan_code`, `label`, `memory_high`, `memory_max`, `cpu_quota_percent`, `tasks_max`, `max_services`, `service_days`, `allowed_kinds`, `images`, `reserved_kinds` and `can_provision`.
- `images` is a `BTreeMap<String, Option<String>>`: present references are escaped text, and missing references are explicitly labeled. They are not editable or executable links.
- Tenant forms post to `/portal/services`; operator tenant-detail forms post to `/operator/tenants/{id}/services`.
- Both forms submit **only** `csrf`, `plan_code` and `kind`. The plan is a hidden fixed value, the kind is an allowlisted required selection, and duration is informational, not a `days` input. There is no `plan` field.
- Reserved kinds remain visible but disabled. An entry with `can_provision == false` has explanatory text rather than a provisioning form. Empty catalogs have a useful empty state. The backend still revalidates availability; UI state is not authorization.
- Shared partials keep plan details and form fields consistent. The operator page retains identity, services, activity and a separate read-only local commercial reference; local pricing metadata does not override broker policy.

The registry section links to regular upstream image listings and documentation for Docker Hub nginx/PostgreSQL, Quay and Forgejo. These are ordinary outbound reference links, not automatic downloads, imports, arbitrary image entry or shell access.

## Validation

Toolchain-free UI checks, from the workspace root:

```sh
python3 -B backend/crates/honenode/tests/test_theme.py -v
```

These are static template/source and palette-contrast checks, not rendered Askama or browser tests. They cover exact provisioning field names, shared partial references, reserved/unavailable states, registry links, theme-selection guards and local/CSP-safe assets. Coordinate Rust/Askama compilation and `cargo test -p honenode` with the parent build owner. Browser checks at mobile and desktop widths, zoom, keyboard and assistive-technology validation remain necessary. No packaging files or other Rust modules are changed by this UI work.
