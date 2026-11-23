CREATE TABLE accounts (
    id            INTEGER PRIMARY KEY,
    number        TEXT    NOT NULL UNIQUE,
    role          TEXT    NOT NULL CHECK (role IN ('operator', 'tenant', 'support')),
    password_hash TEXT    NOT NULL,
    created_at    TEXT    NOT NULL,
    last_login_at TEXT,
    disabled_at   TEXT,
    notes         TEXT
);

CREATE TABLE tenants (
    id           INTEGER PRIMARY KEY,
    account_id   INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    slug         TEXT    NOT NULL UNIQUE,
    name         TEXT    NOT NULL,
    status       TEXT    NOT NULL DEFAULT 'active'
                         CHECK (status IN ('active', 'grace', 'suspended', 'purged')),
    created_at   TEXT    NOT NULL,
    suspended_at TEXT,
    purge_after  TEXT
);

CREATE UNIQUE INDEX tenants_account ON tenants(account_id);

CREATE TABLE plans (
    id            INTEGER PRIMARY KEY,
    code          TEXT    NOT NULL UNIQUE,
    monthly_price REAL    NOT NULL,
    cpu_weight    INTEGER NOT NULL,
    memory_high   TEXT    NOT NULL,
    memory_max    TEXT    NOT NULL,
    tasks_max     INTEGER NOT NULL,
    disk_gb       INTEGER NOT NULL,
    bandwidth_gb  INTEGER NOT NULL,
    max_services  INTEGER NOT NULL,
    description   TEXT    NOT NULL
);

CREATE TABLE services (
    id             INTEGER PRIMARY KEY,
    tenant_id      INTEGER NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    kind           TEXT    NOT NULL CHECK (kind IN ('web', 'database', 'storage', 'git')),
    plan_id        INTEGER NOT NULL REFERENCES plans(id),
    state          TEXT    NOT NULL DEFAULT 'pending'
                           CHECK (state IN ('pending', 'running', 'stopped', 'suspended', 'deleted', 'failed')),
    observed_state TEXT,
    unit_name      TEXT    NOT NULL,
    uid            INTEGER,
    ports_json     TEXT    NOT NULL DEFAULT '{}',
    volume_json    TEXT    NOT NULL DEFAULT '{}',
    created_at     TEXT    NOT NULL,
    expires_at     TEXT,
    suspended_at   TEXT,
    deleted_at     TEXT
);

CREATE INDEX services_tenant ON services(tenant_id);
CREATE INDEX services_expiry ON services(expires_at);

CREATE TABLE subscriptions (
    id           INTEGER PRIMARY KEY,
    tenant_id    INTEGER NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    plan_id      INTEGER NOT NULL REFERENCES plans(id),
    started_at   TEXT    NOT NULL,
    paid_through TEXT,
    auto_renew   INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE jobs (
    id           INTEGER PRIMARY KEY,
    tenant_id    INTEGER REFERENCES tenants(id) ON DELETE SET NULL,
    action       TEXT    NOT NULL,
    payload_json TEXT    NOT NULL DEFAULT '{}',
    state        TEXT    NOT NULL DEFAULT 'queued'
                         CHECK (state IN ('queued', 'running', 'done', 'failed')),
    requested_by INTEGER REFERENCES accounts(id) ON DELETE SET NULL,
    started_at   TEXT,
    finished_at  TEXT,
    error        TEXT
);

CREATE INDEX jobs_state ON jobs(state);

CREATE TABLE audit_events (
    id            INTEGER PRIMARY KEY,
    ts            TEXT NOT NULL,
    request_id    TEXT NOT NULL,
    tenant_id     INTEGER REFERENCES tenants(id) ON DELETE SET NULL,
    actor_type    TEXT NOT NULL,
    actor_id      TEXT,
    action        TEXT NOT NULL,
    resource_type TEXT,
    resource_id   TEXT,
    outcome       TEXT NOT NULL,
    reason        TEXT,
    src_ip        TEXT,
    user_agent    TEXT
);

CREATE INDEX audit_events_ts ON audit_events(ts DESC);
CREATE INDEX audit_events_tenant ON audit_events(tenant_id);

CREATE TABLE resource_samples (
    id         INTEGER PRIMARY KEY,
    tenant_id  INTEGER NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    ts         TEXT    NOT NULL,
    cpu_pct    REAL,
    mem_bytes  INTEGER,
    disk_bytes INTEGER,
    io_bytes   INTEGER
);

CREATE INDEX resource_samples_tenant_ts ON resource_samples(tenant_id, ts DESC);

CREATE TABLE sessions (
    id           TEXT    PRIMARY KEY,
    account_id   INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    csrf_token   TEXT    NOT NULL,
    created_at   TEXT    NOT NULL,
    expires_at   TEXT    NOT NULL,
    last_seen_at TEXT    NOT NULL,
    src_ip       TEXT,
    user_agent   TEXT
);

CREATE INDEX sessions_account ON sessions(account_id);
CREATE INDEX sessions_expiry ON sessions(expires_at);
