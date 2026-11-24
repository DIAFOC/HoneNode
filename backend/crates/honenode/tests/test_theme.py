"""Static stylesheet/markup checks; Rust/Askama rendering is tested separately."""
import pathlib
import re
import unittest
from urllib.parse import urlsplit

ROOT = pathlib.Path(__file__).resolve().parents[1]
CSS = (ROOT.parents[1] / "static/hone.css").read_text()
TEMPLATES = ROOT / "templates"


def expanded_template(path):
    """Expand shared includes for static checks, without pretending to render Askama."""
    text = (TEMPLATES / path).read_text()
    return re.sub(r'{%\s*include "([^"]+)"\s*%}',
                  lambda match: expanded_template(match.group(1)), text)


def variables(selector):
    rule = re.search(re.escape(selector) + r"\s*\{([^}]+)\}", CSS)
    if rule is None:
        raise AssertionError(f"Missing CSS rule: {selector}")
    return dict(re.findall(r"(--[a-z0-9-]+):\s*(#[0-9a-fA-F]{6})", rule.group(1)))


def luminance(color):
    channels = [int(color[index:index + 2], 16) / 255 for index in (1, 3, 5)]
    linear = [channel / 12.92 if channel <= 0.04045 else ((channel + 0.055) / 1.055) ** 2.4
              for channel in channels]
    return sum(channel * weight for channel, weight in zip(linear, (0.2126, 0.7152, 0.0722)))


def contrast(left, right):
    bright, dark = sorted((luminance(left), luminance(right)), reverse=True)
    return (bright + 0.05) / (dark + 0.05)


class ThemeTests(unittest.TestCase):
    def test_neutral_text_contrast(self):
        palette = variables(":root")
        for foreground in ("--text", "--muted", "--accent", "--gold", "--danger", "--ok"):
            for background in ("--bg-obsidian", "--surface", "--surface-2"):
                with self.subTest(foreground=foreground, background=background):
                    self.assertGreaterEqual(contrast(palette[foreground], palette[background]), 4.5)
        for background in ("--bg-obsidian", "--surface", "--surface-2"):
            with self.subTest(control_boundary=background):
                self.assertGreaterEqual(contrast(palette["--border"], palette[background]), 3.0)

    def test_dark_text_and_controls_contrast(self):
        palette = variables(".theme-sharpnode")
        for background in ("--bg-obsidian", "--surface", "--surface-2"):
            for foreground in ("--text", "--muted", "--accent", "--gold", "--danger", "--ok"):
                with self.subTest(foreground=foreground, background=background):
                    self.assertGreaterEqual(contrast(palette[foreground], palette[background]), 4.5)
            self.assertGreaterEqual(contrast(palette["--border"], palette[background]), 3.0)

    def test_dark_palette_and_kind_colors_are_scoped(self):
        self.assertRegex(CSS, r":root\s*\{\s*color-scheme:\s*light;")
        self.assertRegex(CSS, r"\.theme-sharpnode\s*\{\s*color-scheme:\s*dark;")
        self.assertEqual(variables(".theme-sharpnode")["--bg-obsidian"], "#0B0C0F")
        rules = re.findall(r"([^{}]+)\{[^{}]*--accent:[^{}]*\}", CSS)
        kind_selectors = [selector.strip() for rule in rules for selector in rule.split(',') if '.tone-' in selector]
        self.assertEqual(len(kind_selectors), 10)
        self.assertTrue(all('.theme-sharpnode' in selector for selector in kind_selectors))

    def test_theme_preserves_csp_and_post_csrf_fields(self):
        base = (ROOT / "templates/base.html").read_text()
        self.assertIn('class="{{ principal.theme_class }}"', base)
        forms = []
        for template in (ROOT / "templates").rglob("*.html"):
            text = expanded_template(template.relative_to(TEMPLATES))
            scripts = re.findall(r"<script\b[^>]*>.*?</script>", text, re.DOTALL | re.IGNORECASE)
            self.assertTrue(all(script == '<script src="/static/hone.js" defer></script>'
                                for script in scripts))
            self.assertNotRegex(text, r"<script(?!\s+src=)")
            self.assertNotRegex(text, r"\s(?:style|on\w+)\s*=")
            self.assertNotRegex(text, r'<(?:link|script|img|iframe)[^>]+(?:src|href)="https?://')
            self.assertNotIn("|safe", text)
            forms.extend(form for form in re.findall(r"<form\b.*?</form>", text, re.DOTALL)
                         if 'method="post"' in form)
        self.assertGreater(len(forms), 0)
        self.assertTrue(all('name="csrf"' in form for form in forms))
        self.assertNotRegex(CSS, r"@import|url\(|gradient\(|backdrop-filter|box-shadow")

    def test_tenant_default_and_neutral_role_boundary(self):
        config = (ROOT / "src/config.rs").read_text()
        layout = (ROOT / "src/web/layout.rs").read_text()
        self.assertIn('Theme::parse(&text("HONE_THEME", "sharpnode"))?', config)
        self.assertRegex(config, r"#\[default\]\s*SharpNode")
        function = layout.split("pub fn principal_view(", 1)[1].split("#[derive", 1)[0]
        self.assertIn("Some(principal) if !principal.role().is_operator() => theme", function)
        self.assertIn("_ => Theme::Neutral", function)
        self.assertIn("theme_class: theme.css_class()", function)
        self.assertIn("is_sharpnode: theme == Theme::SharpNode", function)

    def test_exact_provisioning_contract(self):
        for path, action in (
            ("tenant/catalog.html", "/portal/services"),
            ("operator/tenant_detail.html", "/operator/tenants/{{ tenant_id }}/services"),
        ):
            with self.subTest(path=path):
                text = expanded_template(path)
                forms = re.findall(r"<form\b.*?</form>", text, re.DOTALL)
                matching = [form for form in forms if f'action="{action}"' in form]
                self.assertEqual(len(matching), 1)
                form = matching[0]
                self.assertIn('method="post"', form)
                self.assertEqual(re.findall(r'\bname="([^"]+)"', form),
                                 ["csrf", "plan_code", "kind"])
                self.assertIn('name="plan_code" value="{{ entry.plan_code }}"', form)
                self.assertIn('name="kind" required', form)
                self.assertIn("entry.allowed_kinds", form)
                self.assertIn('value="" selected disabled', form)
                self.assertRegex(form, r'entry.reserved_kinds.contains\(kind\) %} disabled')
                self.assertIn("{% if entry.can_provision %}\n    <form", text)
                self.assertIn("{% if has_entries %}", text)

    def test_catalog_fields_match_actual_context(self):
        text = expanded_template("tenant/catalog.html")
        self.assertEqual(set(re.findall(r"\bentry\.(\w+)", text)), {
            "plan_code", "label", "memory_high", "memory_max", "cpu_quota_percent",
            "tasks_max", "max_services", "service_days", "allowed_kinds", "images",
            "reserved_kinds", "can_provision",
        })
        self.assertIn("{% for (kind, image) in entry.images %}", text)
        self.assertIn("{% when Some with (reference) %}", text)
        self.assertIn("{% when None %}", text)
        self.assertIn("{{ tenant_name }}", text)
        operator = expanded_template("operator/tenant_detail.html")
        for value in ("tenant_slug", "status_label", "status_class", "created", "plan.price"):
            self.assertIn("{{ " + value + " }}", operator)
        self.assertIn('include "partials/services_table.html"',
                      (TEMPLATES / "operator/tenant_detail.html").read_text())
        self.assertIn('include "partials/audit_table.html"',
                      (TEMPLATES / "operator/tenant_detail.html").read_text())

    def test_regular_upstreams_are_reference_links_only(self):
        text = expanded_template("partials/registries.html")
        urls = re.findall(r'href="(https://[^"]+)"', text)
        self.assertEqual(len(urls), 10)
        self.assertEqual({urlsplit(url).hostname for url in urls}, {
            "hub.docker.com", "nginx.org", "www.postgresql.org", "quay.io",
            "docs.quay.io", "codeberg.org", "forgejo.org", "github.com", "filebrowser.org",
        })
        self.assertIn("https://hub.docker.com/_/nginx", urls)
        self.assertIn("https://hub.docker.com/_/postgres", urls)
        self.assertIn("https://github.com/filebrowser/filebrowser", urls)
        # The approved catalogue is the primary destination; upstreams are secondary.
        self.assertIn('href="/portal/images"', text)
        self.assertNotRegex(text, r"<(?:form|input|button|script)\b")
        self.assertIn('id="registries"', text)

    def test_image_catalogue_deploys_only_approved_references(self):
        text = expanded_template("tenant/images.html")
        self.assertIn('href="{{ row.deploy_url }}"', text)
        self.assertIn("{% if row.can_deploy %}", text)
        self.assertIn('href="{{ row.registry_url }}"', text)
        self.assertIn('rel="noopener noreferrer"', text)
        self.assertIn("{{ row.reference }}", text)
        self.assertIn("{{ row.digest }}", text)
        self.assertNotIn("hub.docker.com/search", text)
        self.assertNotRegex(text, r"<(?:form|input|button|script)\b")
        self.assertIn('include "partials/registries.html"',
                      (TEMPLATES / "tenant/images.html").read_text())

    def test_deploy_wizard_fields_and_password_handling(self):
        text = expanded_template("tenant/deploy.html")
        forms = re.findall(r"<form\b.*?</form>", text, re.DOTALL)
        self.assertEqual(len(forms), 1)
        form = forms[0]
        self.assertIn('action="/portal/services"', form)
        for hidden in ("csrf", "plan_code", "kind", "image"):
            self.assertIn(f'type="hidden" name="{hidden}"', form)
        self.assertIn('name="image" value="{{ image }}"', form)
        self.assertIn('name="file_manager_username"', form)
        self.assertIn('name="file_manager_read_only"', form)
        password = re.search(r"<input[^>]+name=\"file_manager_password\"[^>]*>", form).group(0)
        self.assertIn('type="password"', password)
        self.assertNotIn("value=", password)
        self.assertIn('autocomplete="new-password"', password)
        database_password = re.search(
            r"<input[^>]+name=\"database_password\"[^>]*>", form
        ).group(0)
        self.assertIn('type="password"', database_password)
        self.assertNotIn("value=", database_password)
        self.assertIn('autocomplete="new-password"', database_password)
        self.assertIn('data-generate-password="database_password"', form)
        self.assertIn('data-generate-password="file_manager_password"', form)
        # Ports, paths, and resources are read-only broker-managed output.
        self.assertNotIn('name="ports"', form)
        self.assertNotIn('name="volumes"', form)
        self.assertNotIn('name="memory_max"', form)
        self.assertIn("{{ port.value }}", text)
        self.assertIn("{{ volume.value }}", text)

    def test_service_tabs_configuration_and_logs_contract(self):
        tabs = (TEMPLATES / "partials/service_tabs.html").read_text()
        for target, label in (("", "Resumen"), ("/configuration", "Configuración"), ("/logs", "Registros")):
            self.assertIn(f'href="/portal/services/{{{{ service_id }}}}{target}"', tabs)
            self.assertIn(label, tabs)
        for page in ("tenant/service_detail.html", "tenant/service_configuration.html", "tenant/service_logs.html"):
            self.assertIn('include "partials/service_tabs.html"', (TEMPLATES / page).read_text())
        configuration = expanded_template("tenant/service_configuration.html")
        forms = re.findall(r"<form\b.*?</form>", configuration, re.DOTALL)
        self.assertEqual(len(forms), 1)
        self.assertIn('action="/portal/services/{{ service_id }}/configuration"', forms[0])
        self.assertEqual(re.findall(r'\bname="([^"]+)"', forms[0]), ["csrf", "read_only"])
        self.assertIn("{{ username }}", configuration)
        self.assertNotIn('type="password"', configuration)
        self.assertNotIn('name="username"', configuration)
        self.assertNotIn('name="image"', configuration)
        logs = expanded_template("tenant/service_logs.html")
        self.assertIn('<pre class="log-view">{{ logs }}</pre>', logs)
        self.assertNotRegex(logs, r"<(?:form|input|button|script)\b")
        self.assertNotIn('name="lines"', logs)
        self.assertNotIn('name="unit"', logs)

    def test_responsive_shell_and_navigation(self):
        base = (TEMPLATES / "base.html").read_text()
        nav = (TEMPLATES / "partials/sidebar.html").read_text()
        for target in ("/portal", "/portal/images", "/portal/catalog", "/portal/audit", "/portal/images#registries"):
            self.assertIn(f'href="{target}"', nav)
        self.assertIn('aria-current="page"', nav)
        self.assertIn('class="sidebar"', base)
        self.assertIn('id="main" tabindex="-1"', base)
        self.assertIn("grid-template-columns: 14rem minmax(0, 1fr)", CSS)
        self.assertIn("@media (max-width: 760px)", CSS)
        self.assertIn("grid-template-columns: minmax(0, 1fr)", CSS)
        self.assertTrue(all(value.strip() == "0"
                            for value in re.findall(r"border-radius:([^;]+);", CSS)))
        dashboard = (TEMPLATES / "operator/dashboard.html").read_text()
        self.assertIn("{% block sidebar_summary %}", dashboard)
        self.assertIn("{% for stat in host_stats %}", dashboard)


if __name__ == "__main__":
    unittest.main()
