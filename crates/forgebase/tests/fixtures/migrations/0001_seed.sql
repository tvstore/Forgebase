-- Fixture of crates/forgebase/tests/apps.rs: an app data migration embedded with `migrations!`.
CREATE TABLE fixture_log (entry text NOT NULL);
INSERT INTO fixture_log (entry) VALUES ('seeded');
