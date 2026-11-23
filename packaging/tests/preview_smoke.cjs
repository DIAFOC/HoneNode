/* Real, existing OCI preview only. --restart explicitly restarts honenode-preview. */
const fs = require('node:fs');
const path = require('node:path');
const { execFileSync } = require('node:child_process');
const assert = require('node:assert/strict');

const root = path.resolve(__dirname, '../..');
const runDir = path.join(root, 'packaging/.local-run');
const base = 'http://127.0.0.1:8088';
const name = 'honenode-preview';
let stage = 'configuration';

function podman(args) {
  return execFileSync('podman', args, { encoding: 'utf8', timeout: 30000, stdio: ['ignore', 'pipe', 'pipe'] });
}

function privatePath(filename, directory, mode) {
  const info = fs.lstatSync(filename);
  assert(!info.isSymbolicLink() && info.uid === process.getuid(), 'state must belong to the invoking user, without symlinks');
  assert(directory ? info.isDirectory() : info.isFile(), 'unexpected state file type');
  assert.equal(info.mode & 0o777, mode, 'unsafe state permissions');
  return info;
}

async function main() {
  assert(process.argv.slice(2).every(arg => arg === '--restart'), 'only --restart is supported');
  assert.notEqual(process.getuid(), 0, 'use the ordinary preview owner, not root');
  privatePath(runDir, true, 0o700);
  const bootstrap = path.join(runDir, 'bootstrap.env');
  assert(privatePath(bootstrap, false, 0o600).size <= 8192, 'bootstrap exceeds 8 KiB');
  const savedBootstrap = fs.readFileSync(bootstrap, 'utf8');
  const credentials = Object.fromEntries(savedBootstrap.trim().split('\n').map(line => {
    const separator = line.indexOf('=');
    return [line.slice(0, separator), line.slice(separator + 1)];
  }));
  assert(/^[0-9]{12}$/.test(credentials.HONE_OPERATOR_ACCOUNT), 'missing operator account');
  assert(credentials.HONE_OPERATOR_PASSWORD, 'missing operator password');
  const database = path.join(runDir, 'data/registry/registry.db');
  const originalDatabase = fs.statSync(database);
  const info = JSON.parse(podman(['info', '--format', 'json']));
  assert.equal(info.host.security.rootless, true, 'preview must use rootless Podman');
  assert.equal(info.host.serviceIsRemote, false, 'preview must use local Podman');

  stage = 'container confinement';
  const container = JSON.parse(podman(['container', 'inspect', name]))[0];
  assert.equal(container.State.Status, 'running', 'start the existing preview before testing');
  assert.equal(container.Config.User, '65532:65532');
  assert.equal(container.HostConfig.ReadonlyRootfs, true);
  assert(container.HostConfig.SecurityOpt.includes('no-new-privileges'));
  assert.equal(container.HostConfig.Privileged, false);
  assert.notEqual(container.HostConfig.NetworkMode, 'host');
  assert.equal(container.HostConfig.Memory, 512 * 1024 * 1024);
  assert.equal(container.HostConfig.MemorySwap, container.HostConfig.Memory);
  assert.equal(container.HostConfig.NanoCpus, 1000000000);
  assert.equal(container.HostConfig.PidsLimit, 256);
  assert.deepEqual(container.HostConfig.PortBindings, { '8080/tcp': [{ HostIp: '127.0.0.1', HostPort: '8088' }] });
  assert.equal(container.Mounts.length, 1, 'app-only preview must not mount a host broker or other host paths');
  assert.equal(container.Mounts[0].Source, path.join(runDir, 'data'));
  assert.equal(container.Mounts[0].Destination, '/var/lib/honenode');
  assert(container.ProcessLabel.includes(':container_t:'), 'expected SELinux container confinement');
  assert.equal(execFileSync('getenforce', [], { encoding: 'utf8', timeout: 5000 }).trim(), 'Enforcing');

  const { chromium } = require(path.join(root, 'backend/.build-tools/browser/node_modules/playwright'));
  let browser;
  try {
    browser = await chromium.launch({ headless: true });
    const context = await browser.newContext({ viewport: { width: 1440, height: 1000 } });
    context.setDefaultTimeout(10000);
    const page = await context.newPage();
    const errors = [];
    page.on('pageerror', error => errors.push(error.name));
    async function health() {
      const response = await context.request.get(base + '/healthz', { timeout: 5000 });
      assert.equal(response.status(), 200);
      assert.equal((await response.text()).trim(), 'ok');
    }
    stage = 'HTTP and authentication';
    await health();
    for (const route of ['/api/v1/status', '/api/v1/broker']) {
      const response = await context.request.get(base + route);
      assert.equal(response.status(), 403, 'preview disables sensitive APIs with an empty token list');
    }
    const denied = await page.goto(base + '/operator');
    assert.equal(denied.status(), 401, 'operator requires authentication');
    const login = await page.goto(base + '/login');
    assert(login.headers()['content-security-policy'], 'CSP must be present');
    assert.equal(login.headers()['x-content-type-options'], 'nosniff');
    stage = 'saved-credential login';
    await page.locator('#number').fill(credentials.HONE_OPERATOR_ACCOUNT);
    await page.locator('#password').fill(credentials.HONE_OPERATOR_PASSWORD);
    await Promise.all([
      page.waitForURL(url => url.pathname.startsWith('/operator')),
      page.getByRole('button', { name: 'Entrar', exact: true }).click(),
    ]);
    for (const route of ['/operator', '/operator/tenants', '/operator/services', '/operator/jobs', '/operator/audit']) {
      stage = 'authenticated route ' + route;
      const response = await page.goto(base + route);
      assert.equal(response.status(), 200, 'operator route must render without a broker: ' + route);
      assert.equal(await page.locator('html').getAttribute('class'), 'theme-neutral');
    }
    const css = await context.request.get(base + '/static/hone.css');
    assert.equal(css.status(), 200);
    assert.equal(await css.text(), fs.readFileSync(path.join(root, 'backend/static/hone.css'), 'utf8'));
    stage = 'repeated health requests';
    for (let batch = 0; batch < 10; batch++) await Promise.all(Array.from({ length: 10 }, health));

    if (process.argv.includes('--restart')) {
      stage = 'explicit preview restart';
      podman(['restart', '--time', '10', name]);
      let ready = false;
      for (let attempt = 0; attempt < 30; attempt++) {
        try { await health(); ready = true; break; } catch (_) {}
        await new Promise(resolve => setTimeout(resolve, 250));
      }
      assert(ready, 'preview must become healthy after restart');
      const response = await page.goto(base + '/operator');
      assert.equal(response.status(), 200);
      assert.equal(new URL(page.url()).pathname, '/operator', 'existing session must survive restart');
      const currentDatabase = fs.statSync(database);
      assert.equal(currentDatabase.dev, originalDatabase.dev);
      assert.equal(currentDatabase.ino, originalDatabase.ino, 'persistent database must not be replaced');
      assert(fs.readFileSync(bootstrap, 'utf8') === savedBootstrap, 'restart must preserve bootstrap');
    }
    stage = 'browser assertions';
    assert.deepEqual(errors, [], 'no browser runtime errors');
    console.log('PASS real OCI preview: rootless/SELinux confinement, health, token gates, saved-credential login, neutral operator routes, current CSS, 100 concurrent-batch health requests' + (process.argv.includes('--restart') ? ', session/database/bootstrap persistence across restart.' : '.'));
    console.log('App-only evidence: no host broker mounted; live tenant provisioning is NOT tested. Credentials were not printed.');
  } finally {
    if (browser) await browser.close();
  }
}
main().catch(() => {
  // Browser error call logs may include filled credentials; never print them.
  console.error('FAIL real OCI preview at stage: ' + stage + '. Inspect the preview locally; secrets and browser call logs were withheld.');
  process.exitCode = 1;
});
