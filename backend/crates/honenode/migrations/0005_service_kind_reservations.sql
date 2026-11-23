-- Tombstones retain kind/port/volume ownership until broker-backed reclamation exists.
CREATE UNIQUE INDEX services_tenant_kind_reservation ON services(tenant_id, kind);
