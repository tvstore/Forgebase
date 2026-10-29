# Migrations

SQL data migrations, applied once per database by `migrate`, after the DocType schema sync.
Add one when you must change or backfill data, not to create tables for DocTypes (the sync does
that). Name files `NNNN_what.sql` and list them in `src/lib.rs`:

    .migrations(forgebase::migrations![(1, "backfill", "migrations/0001_backfill.sql")])

Versions only increase. Never edit or remove a migration that was applied: it is refused (its
checksum is verified); add a new one instead.
