-- Multiple fixed-template instances of the same kind are allowed. The plan's
-- service count, the host budgets and the unique unit/port/volume reservations
-- remain the limits; the old one-per-kind index would reject a second nginx or
-- PostgreSQL before those checks could run.
DROP INDEX IF EXISTS services_tenant_kind_reservation;
