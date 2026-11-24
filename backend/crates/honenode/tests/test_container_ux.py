"""Offline markup and browser-script regressions; no service is provisioned."""
import json
import pathlib
import re
import shutil
import subprocess
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[1]
SCRIPT = ROOT.parents[1] / "static/hone.js"


class ContainerUXTests(unittest.TestCase):
    def test_only_self_hosted_external_script_and_unchanged_csp(self):
        base = (ROOT / "templates/base.html").read_text()
        self.assertEqual(re.findall(r"<script\b[^>]*>.*?</script>", base),
                         ['<script src="/static/hone.js" defer></script>'])
        middleware = (ROOT / "src/web/middleware.rs").read_text()
        self.assertIn("script-src 'self'", middleware)
        self.assertNotIn("unsafe-inline", middleware)
        self.assertNotIn("unsafe-eval", middleware)
        router = (ROOT / "src/web/mod.rs").read_text()
        self.assertIn('.nest_service("/static", static_files)', router)

    def test_generator_has_no_network_storage_or_dynamic_execution(self):
        script = SCRIPT.read_text()
        self.assertIn("crypto.getRandomValues", script)
        self.assertIn("new Uint8Array(16)", script)
        self.assertNotRegex(script, r"fetch\s*\(|XMLHttpRequest|WebSocket|sendBeacon|eval\s*\(|new Function|Math\.random|localStorage|sessionStorage|document\.cookie|\.submit\s*\(|requestSubmit|console\.")
        form = (ROOT / "templates/tenant/container_new.html").read_text()
        self.assertIn('type="button" id="generate-secrets" hidden', form)
        self.assertNotIn("{{ form.environment", form)
        self.assertIn('class="grid cols-2"', form)
        self.assertNotRegex(form, r"\son\w+=|<script")

    @unittest.skipUnless(shutil.which("node"), "Node.js unavailable for browser-script checks")
    def test_browser_generator_is_optional_placeholder_only_and_atomic(self):
        # Run the actual script with a minimal DOM and a real Web Crypto source.
        harness = r'''
const assert = require("node:assert/strict");
const vm = require("node:vm");
const { webcrypto } = require("node:crypto");
const script = SCRIPT;
function setup(value, crypto = webcrypto, maxLength = 16384, present = true) {
  const elements = {
    "generate-secrets": { hidden: true, disabled: false, addEventListener(type, fn) {
      assert.equal(type, "click"); this.click = fn;
    } },
    environment: { value, maxLength },
    "secret-status": { textContent: "" },
    "database_password": { value: "" },
    "database-password-status": { textContent: "" }
  };
  const generators = [{
    dataset: { generatePassword: "database_password", statusTarget: "database-password-status" },
    hidden: true,
    disabled: false,
    addEventListener(type, fn) { assert.equal(type, "click"); this.click = fn; }
  }];
  vm.runInNewContext(script, { crypto, document: {
    getElementById(id) { return present ? elements[id] : null; },
    querySelectorAll(selector) {
      assert.equal(selector, "[data-generate-password]");
      return present ? generators : [];
    }
  } });
  return { button: elements["generate-secrets"], env: elements.environment,
           status: elements["secret-status"], generator: generators[0],
           database: elements["database_password"],
           databaseStatus: elements["database-password-status"] };
}
setup("", webcrypto, 16384, false); // Other pages are unaffected.
const original = "APP_SECRET=GENERATE\nPASSWORD=GENERATE\nNONCE=GENERATE\nFORGEJO__security__SECRET=REEMPLAZA_CON_UN_SECRETO_ALEATORIO\nAPP_MODE=GENERATE\nAPP_PASSWORD=keep-this\nROOT_URL=http://127.0.0.1:30000/";
const form = setup(original);
assert.equal(form.env.value, original); // No automatic generation.
assert.equal(form.button.hidden, false);
form.button.click();
const lines = form.env.value.split("\n");
for (let i = 0; i < 4; i++) assert.match(lines[i].split("=")[1], /^[0-9a-f]{32}$/);
assert.equal(new Set(lines.slice(0, 4).map(line => line.split("=")[1])).size, 4);
assert.deepEqual(lines.slice(4), original.split("\n").slice(4));
assert.match(form.status.textContent, /4/);
const generated = form.env.value;
form.button.click();
assert.equal(form.env.value, generated); // Existing values are never overwritten.
assert.ok(!form.status.textContent.includes(lines[0].split("=")[1]));

const unavailable = setup("APP_SECRET=GENERATE", null);
assert.equal(unavailable.button.disabled, true);
assert.equal(unavailable.env.value, "APP_SECRET=GENERATE");
const failed = setup("APP_SECRET=GENERATE", { getRandomValues() { throw Error("unavailable"); } });
failed.button.click();
assert.equal(failed.env.value, "APP_SECRET=GENERATE");
const tooLong = setup("APP_SECRET=GENERATE", webcrypto, 20);
tooLong.button.click();
assert.equal(tooLong.env.value, "APP_SECRET=GENERATE");
const crlf = setup("PASSWORD=GENERATE\r\nNONCE=GENERATE");
crlf.button.click();
assert.match(crlf.env.value, /^PASSWORD=[0-9a-f]{32}\nNONCE=[0-9a-f]{32}$/);
const database = setup("");
assert.equal(database.generator.hidden, false);
database.generator.click();
assert.match(database.database.value, /^[0-9a-f]{32}$/);
assert.match(database.databaseStatus.textContent, /generada/);
'''.replace("const script = SCRIPT;", "const script = " + json.dumps(SCRIPT.read_text()) + ";")
        result = subprocess.run(["node", "-e", harness], capture_output=True,
                                text=True, timeout=15)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)


if __name__ == "__main__":
    unittest.main()
