-- No table references services; retain its outgoing FKs, observations and tombstones.
CREATE TABLE services_new (
    id INTEGER PRIMARY KEY,
    tenant_id INTEGER NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    kind TEXT NOT NULL CHECK (kind IN ('web', 'database', 'storage', 'git', 'container')),
    plan_id INTEGER NOT NULL REFERENCES plans(id),
    state TEXT NOT NULL DEFAULT 'pending' CHECK (state IN ('pending', 'running', 'stopped', 'suspended', 'deleted', 'failed')),
    observed_state TEXT,
    unit_name TEXT NOT NULL UNIQUE,
    uid INTEGER,
    ports_json TEXT NOT NULL DEFAULT '{}',
    volume_json TEXT NOT NULL DEFAULT '{}',
    created_at TEXT NOT NULL,
    expires_at TEXT,
    suspended_at TEXT,
    deleted_at TEXT,
    observed_at TEXT
);
INSERT INTO services_new SELECT id, tenant_id, kind, plan_id, state, observed_state,
    unit_name, uid, ports_json, volume_json, created_at, expires_at, suspended_at, deleted_at, observed_at FROM services;
DROP TABLE services;
ALTER TABLE services_new RENAME TO services;
CREATE INDEX services_tenant ON services(tenant_id);
CREATE INDEX services_expiry ON services(expires_at);
CREATE UNIQUE INDEX services_tenant_kind_reservation ON services(tenant_id, kind) WHERE kind != 'container';
