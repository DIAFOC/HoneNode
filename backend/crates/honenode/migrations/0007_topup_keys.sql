-- Top-up keys: prepaid plan time the operator generates and tenants redeem.
-- Only the SHA-256 of each code is stored; the full code is shown once at generation.
CREATE TABLE topup_keys (
    id                INTEGER PRIMARY KEY,
    code_hash         TEXT    NOT NULL UNIQUE,
    code_prefix       TEXT    NOT NULL,
    plan_code         TEXT    NOT NULL,
    days              INTEGER NOT NULL CHECK (days BETWEEN 1 AND 3650),
    created_by        INTEGER REFERENCES accounts(id) ON DELETE SET NULL,
    created_at        TEXT    NOT NULL,
    claimed_by_tenant INTEGER REFERENCES tenants(id) ON DELETE SET NULL,
    claimed_at        TEXT,
    notes             TEXT,
    CHECK (claimed_at IS NULL OR claimed_by_tenant IS NOT NULL)
);

CREATE INDEX topup_keys_claimed ON topup_keys(claimed_at);

-- The demo plans: the basic plan is a two-service 7-day trial; the pro plan
-- carries four services. These rows are display metadata; the live root
-- catalogue remains the authorization source.
UPDATE plans SET max_services = 2,
                 description = 'Plan básico: prueba de 7 días, hasta dos servicios'
  WHERE code = 'starter';
UPDATE plans SET max_services = 4,
                 description = 'Plan pro: hasta cuatro servicios, tiempo vía claves de recarga'
  WHERE code = 'pro';
