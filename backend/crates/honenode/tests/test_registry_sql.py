"""Run the production SQL against SQLite when a Rust toolchain is unavailable.

This complements (does not replace) cargo test. No query implementation is stubbed.
"""
import concurrent.futures
from contextlib import closing
import pathlib
import re
import sqlite3
import tempfile
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[1]
SOURCE = (ROOT / "src/registry.rs").read_text()
AUTH = (ROOT / "src/auth.rs").read_text()
LIFECYCLE = (ROOT / "src/lifecycle.rs").read_text()


def query(source, function, prefix):
    section = source.split(f"async fn {function}(", 1)[1].split("\npub ", 1)[0]
    return next(sql for sql in re.findall(r'"([^"\\]*(?:\\.[^"\\]*)*)"', section, re.DOTALL)
                if sql.startswith(prefix))


RESERVE = query(SOURCE, "reserve_service", "INSERT INTO services")
LIMIT = query(AUTH, "limit_bucket", "INSERT INTO login_limits")
CONSUME = query(AUTH, "consume_login_challenge", "DELETE FROM login_challenges")
PURGE = query(SOURCE, "services_purgeable", "SELECT id")
OBSERVE = query(SOURCE, "record_observation", "UPDATE services SET uid")
CLEAR_OBSERVATION = query(SOURCE, "clear_service_observation", "UPDATE services SET observed_state")
UID_QUERY = re.search(r'"(SELECT uid FROM services[^"\\]*)"', LIFECYCLE).group(1)


def seed(db, stop_before=None):
    db.execute("PRAGMA foreign_keys=ON")
    for migration in sorted((ROOT / "migrations").glob("*.sql")):
        if migration.name == stop_before:
            break
        db.executescript(migration.read_text())
    db.execute("INSERT INTO accounts (id, number, role, password_hash, created_at) VALUES (1, '000000000000', 'tenant', 'unused', '2026-01-01')")
    db.execute("INSERT INTO tenants (id, account_id, slug, name, created_at) VALUES (1, 1, 't01', 'test', '2026-01-01')")
    db.commit()


def reserve(db, unit="t01-web", *, tenant_id=1, kind="web", plan_id=3,
            max_services=1, ports="{}", volumes="{}"):
    return db.execute(RESERVE, (
        tenant_id, kind, plan_id, unit, None, ports, volumes,
        "2026-01-01", "2026-02-01", tenant_id, tenant_id, max_services,
        unit, ports, volumes,
    )).fetchone()


class RegistrySQLTests(unittest.TestCase):
    def setUp(self):
        self.db = sqlite3.connect(":memory:")
        seed(self.db)

    def tearDown(self):
        self.db.close()

    def test_multiple_instances_are_allowed_and_deleted_rows_free_reservations(self):
        self.assertIsNotNone(reserve(self.db, max_services=4))
        self.assertIsNotNone(reserve(self.db, "t01-web-other", max_services=4))
        self.db.execute("UPDATE services SET state='deleted', deleted_at='2026-01-02'")
        self.assertIsNotNone(reserve(self.db, "t01-web-new", max_services=4))
        self.assertIsNotNone(reserve(self.db, "t01-db", kind="database", max_services=4))

    def test_inactive_tenant_and_zero_quota_rejected(self):
        for status in ("grace", "suspended", "purged"):
            with self.subTest(status=status):
                self.db.execute("UPDATE tenants SET status=?", (status,))
                self.assertIsNone(reserve(self.db))
        self.db.execute("UPDATE tenants SET status='active'")
        self.assertIsNone(reserve(self.db, max_services=0))
        self.assertIsNone(reserve(self.db, tenant_id=99))
        self.assertEqual(self.db.execute("SELECT COUNT(*) FROM services").fetchone()[0], 0)

    def test_concurrent_writers_cannot_both_reserve(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = pathlib.Path(tmp) / "registry.db"
            with closing(sqlite3.connect(path)) as db, db:
                seed(db)
                db.execute("PRAGMA journal_mode=WAL")
            def attempt(_):
                with closing(sqlite3.connect(path, timeout=5)) as db, db:
                    return reserve(db, max_services=4) is not None
            with concurrent.futures.ThreadPoolExecutor(max_workers=8) as executor:
                self.assertEqual(sum(executor.map(attempt, range(8))), 1)

    def test_concurrent_distinct_kinds_cannot_exceed_quota(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = pathlib.Path(tmp) / "registry.db"
            with closing(sqlite3.connect(path)) as db, db:
                seed(db)
                db.execute("PRAGMA journal_mode=WAL")
            def attempt(kind):
                with closing(sqlite3.connect(path, timeout=5)) as db, db:
                    return reserve(db, f"t01-{kind}", kind=kind, max_services=1) is not None
            with concurrent.futures.ThreadPoolExecutor(max_workers=4) as executor:
                self.assertEqual(sum(executor.map(attempt, ("web", "database", "storage", "git"))), 1)
            with closing(sqlite3.connect(path)) as db:
                self.assertEqual(db.execute("SELECT COUNT(*) FROM services").fetchone()[0], 1)

    def test_explicit_quota_counts_all_plans_and_failed_services(self):
        self.assertIsNotNone(reserve(self.db, plan_id=3, max_services=4))
        self.assertIsNone(reserve(self.db, "t01-db", kind="database", plan_id=1, max_services=1))
        self.assertIsNotNone(reserve(self.db, "t01-db", kind="database", plan_id=1, max_services=2))
        self.db.execute("UPDATE services SET state='failed' WHERE kind='database'")
        self.assertIsNone(reserve(self.db, "t01-vault", kind="storage", plan_id=3, max_services=2))
        self.assertEqual(self.db.execute("SELECT COUNT(*) FROM services").fetchone()[0], 2)

    def test_deleted_rows_free_quota_and_kind(self):
        for deletion in ("state='deleted'", "deleted_at='2026-01-02'",
                         "state='deleted', deleted_at='2026-01-02'"):
            with self.subTest(deletion=deletion):
                self.db.execute("DELETE FROM services")
                self.assertIsNotNone(reserve(self.db, max_services=2))
                self.db.execute(f"UPDATE services SET {deletion}")
                self.assertIsNotNone(reserve(self.db, "t01-web-new", max_services=2))
                self.assertIsNotNone(reserve(self.db, "t01-db", kind="database", max_services=2))

    def test_kind_and_quota_are_scoped_to_tenant(self):
        self.db.execute("INSERT INTO accounts (id, number, role, password_hash, created_at) VALUES (2, '000000000018', 'tenant', 'unused', '2026-01-01')")
        self.db.execute("INSERT INTO tenants (id, account_id, slug, name, created_at) VALUES (2, 2, 't02', 'other', '2026-01-01')")
        self.assertIsNotNone(reserve(self.db))
        self.assertIsNotNone(reserve(self.db, "t02-web", tenant_id=2))
        self.assertIsNone(reserve(self.db, "t02-db", tenant_id=2, kind="database"))
        self.assertEqual(self.db.execute("SELECT COUNT(*) FROM services").fetchone()[0], 2)

    def test_port_and_volume_values_are_reserved_only_while_live(self):
        self.db.execute("INSERT INTO accounts (id, number, role, password_hash, created_at) VALUES (2, '000000000018', 'tenant', 'unused', '2026-01-01')")
        self.db.execute("INSERT INTO tenants (id, account_id, slug, name, created_at) VALUES (2, 2, 't02', 'other', '2026-01-01')")
        self.assertIsNotNone(reserve(
            self.db, ports='{"http":18001,"admin":18002}',
            volumes='{"data":"/srv/test/t01/data","cache":"/srv/test/t01/cache"}',
        ))
        # While live, overlapping ports and volumes are refused.
        for fields in ({"ports": '{"other":18002}'},
                       {"volumes": '{"other":"/srv/test/t01/cache"}'}):
            with self.subTest(fields=fields):
                self.assertIsNone(reserve(
                    self.db, "t02-db", tenant_id=2, kind="database", **fields,
                ))
        # A deleted service releases them for reallocation.
        self.db.execute("UPDATE services SET state='deleted', deleted_at='2026-01-02'")
        for fields in ({"ports": '{"other":18002}'},
                       {"volumes": '{"other":"/srv/test/t01/cache"}'}):
            with self.subTest(deleted=True, fields=fields):
                self.assertIsNotNone(reserve(
                    self.db, "t02-db", tenant_id=2, kind="database", **fields,
                ))
                self.db.execute("DELETE FROM services WHERE tenant_id=2")
        self.assertIsNotNone(reserve(
            self.db, "t02-db", tenant_id=2, kind="database",
            ports='{"http":18003}', volumes='{"data":"/srv/test/t02/data"}',
        ))
        self.assertEqual(self.db.execute("SELECT COUNT(*) FROM services").fetchone()[0], 2)

    def test_generic_instances_quota_and_unit_tombstones(self):
        self.assertIsNotNone(reserve(self.db, "t01-one", kind="container", max_services=2))
        self.assertIsNotNone(reserve(self.db, "t01-two", kind="container", max_services=2))
        self.assertIsNone(reserve(self.db, "t01-three", kind="container", max_services=2))
        # set_service_state renames the tombstone so the original unit name is
        # free again; mirror that here.
        self.db.execute("UPDATE services SET state='deleted', deleted_at='2026-01-02', "
                        "unit_name=unit_name || '-deleted-' || id WHERE unit_name='t01-one'")
        self.assertIsNotNone(reserve(self.db, "t01-one", kind="container", max_services=2))
        self.assertIsNone(reserve(self.db, "t01-three", kind="container", max_services=2))
        self.assertEqual(self.db.execute("PRAGMA foreign_key_check").fetchall(), [])

    def test_container_migration_preserves_all_columns_and_indexes(self):
        with closing(sqlite3.connect(":memory:")) as old:
            seed(old, stop_before="0006_generic_containers.sql")
            reserve(old)
            old.execute("UPDATE services SET observed_state='active', observed_at='2026-01-03', uid=1001")
            before = old.execute("SELECT * FROM services").fetchall()
            old.executescript((ROOT / "migrations/0006_generic_containers.sql").read_text())
            self.assertEqual(old.execute("SELECT * FROM services").fetchall(), before)
            self.assertEqual(old.execute("PRAGMA foreign_key_check").fetchall(), [])
            indexes = {row[1] for row in old.execute("PRAGMA index_list(services)")}
            self.assertTrue({"services_tenant", "services_expiry", "services_tenant_kind_reservation"} <= indexes)

    def test_login_limit_atomic_increment_and_window_reset(self):
        for expected in range(1, 12):
            self.assertEqual(self.db.execute(LIMIT, ("account:test", 1)).fetchone()[0], expected)
        self.assertEqual(self.db.execute(LIMIT, ("account:test", 2)).fetchone()[0], 1)

    def test_login_challenge_consumed_once_and_expired_rejected(self):
        self.db.execute("INSERT INTO login_challenges VALUES ('valid', 200)")
        self.assertEqual(self.db.execute(CONSUME, ("valid", 100)).rowcount, 1)
        self.assertEqual(self.db.execute(CONSUME, ("valid", 100)).rowcount, 0)
        self.db.execute("INSERT INTO login_challenges VALUES ('expired', 100)")
        self.assertEqual(self.db.execute(CONSUME, ("expired", 100)).rowcount, 0)

    def test_observation_migration_resets_unverified_report_not_policy_or_uid(self):
        legacy = sqlite3.connect(":memory:")
        try:
            seed(legacy, stop_before="0004_service_observations.sql")
            reserve(legacy)
            legacy.execute("UPDATE services SET state='running', observed_state='running', uid=1001")
            legacy.executescript((ROOT / "migrations/0004_service_observations.sql").read_text())
            self.assertEqual(legacy.execute("SELECT state, uid, observed_state, observed_at FROM services").fetchone(),
                             ("running", 1001, None, None))
        finally:
            legacy.close()

    def test_observation_only_promotes_pending_on_explicit_active(self):
        service_id = reserve(self.db)[0]
        for active in ("active", "reloading", "inactive", "failed", "activating", "deactivating", "maintenance", "refreshing"):
            self.db.execute("UPDATE services SET state='pending'")
            changed = self.db.execute(OBSERVE, (1001, active, "2026-01-03", active, service_id, 1, "t01-web"))
            self.assertEqual(changed.rowcount, 1)
            saved = self.db.execute("SELECT state, uid, observed_state, observed_at FROM services").fetchone()
            self.assertEqual(saved, ("running" if active == "active" else "pending", 1001, active, "2026-01-03"))
        # Suspended and stopped are intents and survive an active report; a
        # failed service that reports active is running again.
        for policy in ("suspended", "stopped"):
            self.db.execute("UPDATE services SET state=?", (policy,))
            self.db.execute(OBSERVE, (1001, "active", "2026-01-04", "active", service_id, 1, "t01-web"))
            self.assertEqual(self.db.execute("SELECT state FROM services").fetchone()[0], policy)
        self.db.execute("UPDATE services SET state='failed'")
        self.db.execute(OBSERVE, (1001, "active", "2026-01-04", "active", service_id, 1, "t01-web"))
        self.assertEqual(self.db.execute("SELECT state FROM services").fetchone()[0], "running")

    def test_telemetry_requires_valid_recent_observation_and_live_reservation(self):
        reserve(self.db)
        args = (1, "2026-01-02", "2026-01-04")
        self.db.execute("UPDATE services SET uid=1001, observed_state='active', observed_at='2026-01-03'")
        self.assertEqual(self.db.execute(UID_QUERY, args).fetchone(), (1001,))
        for uid in (-1, 0, 4294967295, 4294967296):
            self.db.execute("UPDATE services SET uid=?", (uid,))
            self.assertIsNone(self.db.execute(UID_QUERY, args).fetchone())
        self.db.execute("UPDATE services SET uid=1001")
        for observed_at in (None, "2026-01-01", "2026-01-05"):
            self.db.execute("UPDATE services SET observed_at=?", (observed_at,))
            self.assertIsNone(self.db.execute(UID_QUERY, args).fetchone())
        self.db.execute("UPDATE services SET observed_at='2026-01-03', observed_state=NULL")
        self.assertIsNone(self.db.execute(UID_QUERY, args).fetchone())
        self.db.execute("UPDATE services SET observed_state='active', state='deleted', deleted_at='2026-01-02'")
        self.assertIsNone(self.db.execute(UID_QUERY, args).fetchone())

    def test_observation_identity_and_tombstone_guards(self):
        service_id = reserve(self.db)[0]
        for tenant_id, unit in ((2, "t01-web"), (1, "t02-web")):
            result = self.db.execute(OBSERVE, (1001, "active", "2026-01-03", "active", service_id, tenant_id, unit))
            self.assertEqual(result.rowcount, 0)
        self.db.execute("UPDATE services SET state='deleted', deleted_at='2026-01-02'")
        self.assertEqual(self.db.execute(OBSERVE, (1001, "active", "2026-01-03", "active", service_id, 1, "t01-web")).rowcount, 0)
        self.db.execute(CLEAR_OBSERVATION, (service_id, 1, "t01-web"))
        self.assertEqual(self.db.execute("SELECT state, observed_state, observed_at FROM services").fetchone(), ("deleted", None, None))

    def test_service_selects_include_observation_timestamp(self):
        service_id = reserve(self.db)[0]
        functions = [("service_by_id", (service_id,)), ("services_expired_before", ("2026-03-01",)),
                     ("service_view", (service_id,)), ("services_of_tenant", (1,)), ("all_services", ())]
        for function, args in functions:
            sql = query(SOURCE, function, "SELECT")
            cursor = self.db.execute(sql, args)
            self.assertIn("observed_at", [column[0] for column in cursor.description], function)
            self.assertEqual(len(cursor.fetchall()), 1, function)

    def test_renewed_suspended_service_is_not_purged(self):
        reserve(self.db)
        self.db.execute("UPDATE services SET state='suspended', suspended_at='2026-01-02', expires_at='2026-05-01'")
        self.assertEqual(self.db.execute(PURGE, ("2026-03-01", "2026-03-01")).fetchall(), [])
        self.db.execute("UPDATE services SET expires_at='2026-02-01'")
        self.assertEqual(len(self.db.execute(PURGE, ("2026-03-01", "2026-03-01")).fetchall()), 1)


if __name__ == "__main__":
    unittest.main()
