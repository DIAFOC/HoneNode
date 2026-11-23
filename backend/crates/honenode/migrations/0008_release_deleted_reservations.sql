-- One live service per fixed-template kind, but a deleted service frees its
-- kind so the tenant can re-provision it. The partial index keeps the
-- database-level guarantee for live rows only (generic containers stay exempt);
-- the reservation SQL applies the same predicate to unit names, ports and
-- volumes, and set_service_state renames the tombstoned unit name so the
-- original name is free again.
DROP INDEX IF EXISTS services_tenant_kind_reservation;
CREATE UNIQUE INDEX services_tenant_kind_reservation
    ON services(tenant_id, kind)
    WHERE kind != 'container' AND deleted_at IS NULL AND state != 'deleted';
