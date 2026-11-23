/* Isolated HTTP/browser integration. Broker reports are fixtures, never host evidence. */
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const net = require('node:net');
const crypto = require('node:crypto');
const { spawn } = require('node:child_process');
const assert = require('node:assert/strict');

async function main() {
  const root = path.resolve(__dirname, '../..');
  const { chromium } = require(path.join(root, 'backend/.build-tools/browser/node_modules/playwright'));
  const temporary = fs.mkdtempSync(path.join(os.tmpdir(), 'honenode-browser-'));
  fs.chmodSync(temporary, 0o700);
  const socket = path.join(temporary, 'broker.sock');
  const screenshots = path.join(root, 'packaging/.local-run/screenshots');
  fs.mkdirSync(screenshots, { recursive: true, mode: 0o700 });
  const calls = [];
  const states = new Map();
  const catalog = [{ plan_code: 'lab', memory_high: '128M', memory_max: '256M',
    cpu_quota_percent: 25, tasks_max: 128, max_services: 2, service_days: 30,
    allowed_kinds: ['web', 'database'], images: { web: 'docker.io/nginxinc/nginx-unprivileged:1.31.7-alpine', database: 'docker.io/library/postgres:17.11-alpine' } }];
  const server = net.createServer(connection => {
    let buffer = '';
    connection.on('data', chunk => {
      buffer += chunk.toString();
      if (!buffer.includes('\n')) return;
      const request = JSON.parse(buffer.slice(0, buffer.indexOf('\n')));
      calls.push(request);
      const response = { ok: true, op: request.op, request_id: request.request_id, version: 'browser-fixture' };
      if (request.op === 'catalog.list') response.catalog = catalog;
      if (request.op === 'service.start' || request.op === 'service.restart' || request.op === 'service.resume') states.set(request.service, 'active');
      if (request.op === 'service.stop' || request.op === 'service.suspend') states.set(request.service, 'inactive');
      if (request.op === 'service.status') Object.assign(response, { uid: 1091, unit: request.service, active_state: states.get(request.service) || 'inactive' });
      connection.end(JSON.stringify(response) + '\n');
    });
  });
  await new Promise(resolve => server.listen(socket, resolve));
  const password = crypto.randomBytes(24).toString('hex');
  const base = 'http://127.0.0.1:18089';
  const binary = process.env.HONE_TEST_BINARY || path.join(root, 'backend/target/x86_64-unknown-linux-musl/release/honenode');
  const child = spawn(binary, [], { cwd: root, stdio: ['ignore', 'pipe', 'pipe'], env: { ...process.env,
    HONE_BIND: '127.0.0.1:18089', HONE_REGISTRY: path.join(temporary, 'registry.db'),
    HONE_BROKER_SOCKET: socket, HONE_OPERATOR_ACCOUNT: '100000000008', HONE_OPERATOR_PASSWORD: password,
    HONE_SECURE_COOKIES: 'false', HONE_TENANTS: 't91,t92', HONE_TENANT_DATA: path.join(temporary, 'tenants'),
    HONE_THEME: 'sharpnode', HONE_STATIC_DIR: path.join(root, 'backend/static'), HONE_LOG_FORMAT: 'plain',
    HONE_SAMPLE_SECS: '3600', HONE_SWEEP_SECS: '3600', HONE_API_TOKENS: '' } });
  let output = '';
  child.stdout.on('data', chunk => { output += chunk.toString(); });
  child.stderr.on('data', chunk => { output += chunk.toString(); });
  let browser;
  try {
    let ready = false;
    for (let attempt = 0; attempt < 60; attempt++) {
      try { ready = (await fetch(base + '/healthz')).ok; } catch (_) {}
      if (ready) break;
      if (child.exitCode !== null) throw new Error('Application exited before health check: ' + output);
      await new Promise(resolve => setTimeout(resolve, 100));
    }
    assert(ready, 'temporary app did not start');
    browser = await chromium.launch({ headless: true });
    const context = await browser.newContext({ viewport: { width: 1440, height: 1000 } });
    const page = await context.newPage();
    const errors = [];
    page.on('pageerror', error => errors.push(error.message));
    async function login(page, account, secret) {
      await page.goto(base + '/login');
      await page.locator('#number').fill(account);
      await page.locator('#password').fill(secret);
      await Promise.all([page.waitForURL(url => !url.pathname.startsWith('/login')), page.getByRole('button', { name: 'Entrar', exact: true }).click()]);
    }
    await login(page, '100000000008', password);
    assert.equal(await page.locator('html').getAttribute('class'), 'theme-neutral');
    await page.screenshot({ path: path.join(screenshots, 'operator-desktop.png'), fullPage: true });
    await page.goto(base + '/operator/tenants');
    await page.locator('#name').fill('Estudio Horizonte · browser fixture');
    await page.getByRole('button', { name: 'Crear inquilino', exact: true }).click();
    await page.getByRole('heading', { name: 'Inquilino creado', exact: true }).waitFor();
    async function value(label) { return page.locator('dt').filter({ hasText: label }).locator('xpath=following-sibling::dd[1]').innerText(); }
    const tenantAccount = (await value('Número de cuenta')).trim();
    const tenantPassword = (await value('Contraseña')).trim();
    const tenantContext = await browser.newContext({ viewport: { width: 1440, height: 1000 } });
    const portal = await tenantContext.newPage();
    portal.on('pageerror', error => errors.push(error.message));
    await login(portal, tenantAccount, tenantPassword);
    assert.equal(await portal.locator('html').getAttribute('class'), 'theme-sharpnode');
    await portal.goto(base + '/portal/catalog');
    await portal.getByRole('heading', { name: 'Nuevo servicio', exact: true }).waitFor();
    await portal.screenshot({ path: path.join(screenshots, 'tenant-catalog-desktop.png'), fullPage: true });
    async function provision(kind) {
      await portal.goto(base + '/portal/catalog');
      await portal.locator('select[name="kind"]').selectOption(kind);
      await Promise.all([portal.waitForURL(/\/portal\/services\/\d+$/), portal.getByRole('button', { name: /Crear servicio/ }).click()]);
      assert((await portal.locator('main').innerText()).includes('active'), 'observed fixture state must be displayed');
    }
    await provision('web');
    await portal.getByRole('button', { name: 'Detener', exact: true }).click();
    await portal.getByRole('button', { name: 'Iniciar', exact: true }).waitFor();
    await portal.getByRole('button', { name: 'Iniciar', exact: true }).click();
    await portal.getByRole('button', { name: 'Detener', exact: true }).waitFor();
    await provision('database');
    await portal.goto(base + '/portal');
    await portal.screenshot({ path: path.join(screenshots, 'tenant-services-desktop.png'), fullPage: true });
    await portal.setViewportSize({ width: 390, height: 844 });
    await portal.screenshot({ path: path.join(screenshots, 'tenant-mobile.png'), fullPage: true });
    assert(await portal.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth), 'mobile layout overflows viewport');
    await portal.goto(base + '/portal/catalog');
    assert.equal(await portal.locator('form[action="/portal/services"]').count(), 0, 'quota exhaustion hides provisioning form');
    const denied = await tenantContext.request.get(base + '/operator');
    assert.equal(denied.status(), 403, 'tenant cannot enter operator surface');
    assert.equal(calls.filter(call => call.op === 'limits.set').length, 2);
    assert(calls.filter(call => call.op === 'limits.set').every(call => call.plan_code === 'lab' && call.cpu_weight === undefined && call.actor.kind === 'tenant'));
    assert.deepEqual(errors, []);
    console.log('PASS browser: neutral operator, SharpNode tenant, catalogue, two provisions, stop/start, quotas, authorization, desktop/mobile. Broker simulated; no host workload evidence.');
  } finally {
    if (browser) await browser.close();
    child.kill('SIGTERM');
    await new Promise(resolve => { if (child.exitCode !== null) resolve(); else { child.once('exit', resolve); setTimeout(() => { child.kill('SIGKILL'); resolve(); }, 4000).unref(); } });
    await new Promise(resolve => server.close(resolve));
    fs.rmSync(temporary, { recursive: true, force: true });
  }
}
main().catch(error => { console.error(error); process.exitCode = 1; });
