//! Site custom fields (M10.1, ADR 0023) against a real PostgreSQL (see `common`): adding a field
//! (rules, column, indexes, version), disable and enable (data kept), the effective registry
//! (order, skipped bad rows), `migrate` giving back a missing column, and the registry holder
//! (poll, reload, snapshots, a swap under concurrent readers).

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{CUSTOMER, PERSON, TestDb, registry};
use forgebase_core::meta::Registry;
use forgebase_core::overlay::{OverlayError, parse_field_toml};
use forgebase_db::{
    DbError, RegistryHolder, SyncOptions, add_custom_field, effective_registry, list_custom_fields,
    meta_version, migrate, set_custom_field_enabled,
};

const LOCK: Duration = Duration::from_secs(5);

async fn site() -> (TestDb, Registry) {
    let db = TestDb::new().await;
    let base = registry(&[CUSTOMER, PERSON]);
    migrate(&db.pool, &base, &SyncOptions::default())
        .await
        .expect("migrate");
    (db, base)
}

fn field(text: &str) -> forgebase_core::meta::DocField {
    parse_field_toml(text).expect("valid field")
}

async fn add(
    db: &TestDb,
    base: &Registry,
    doctype: &str,
    text: &str,
) -> Result<forgebase_db::CustomField, DbError> {
    let mut client = db.pool.get().await.expect("connection");
    add_custom_field(&mut client, base, "t", doctype, field(text), "cli", LOCK).await
}

#[tokio::test]
async fn adding_a_field_stores_it_adds_the_column_and_bumps_the_version() {
    let (db, base) = site().await;
    let client = db.pool.get().await.unwrap();
    assert_eq!(meta_version(&**client).await.unwrap(), 0);
    drop(client);

    let stored = add(
        &db,
        &base,
        "customer",
        "name = \"custom_vip\"\ntype = \"check\"\n",
    )
    .await
    .expect("added");
    assert_eq!(
        (stored.doctype.as_str(), stored.name.as_str()),
        ("customer", "custom_vip")
    );
    assert!(stored.enabled);
    assert_eq!(stored.modified_by, "cli");
    assert_eq!(stored.type_name(), "check");
    assert_eq!(stored.label(), "Custom Vip");

    let columns = db.columns("doc_customer").await;
    assert!(
        columns.contains(&("custom_vip".to_owned(), "boolean/YES".to_owned())),
        "{columns:?}"
    );
    let client = db.pool.get().await.unwrap();
    assert_eq!(meta_version(&**client).await.unwrap(), 1);
    let listed = list_custom_fields(&**client, Some("customer"))
        .await
        .unwrap();
    assert_eq!(listed, vec![stored]);
    assert!(
        list_custom_fields(&**client, Some("person"))
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn unique_and_link_fields_get_their_keys_and_indexes() {
    let (db, base) = site().await;
    add(
        &db,
        &base,
        "customer",
        "name = \"custom_code\"\ntype = \"data\"\nunique = true\n",
    )
    .await
    .expect("unique");
    add(
        &db,
        &base,
        "customer",
        "name = \"custom_owner\"\ntype = \"link\"\ntarget = \"person\"\n",
    )
    .await
    .expect("link");
    assert_eq!(
        db.rows(
            "SELECT count(*)::text FROM pg_indexes WHERE tablename = 'doc_customer' \
             AND indexdef LIKE '%UNIQUE%' AND indexdef LIKE '%custom_code%'"
        )
        .await,
        vec![vec!["1".to_owned()]]
    );
    assert_eq!(
        db.rows(
            "SELECT count(*)::text FROM pg_constraint c JOIN pg_attribute a ON a.attrelid = c.conrelid AND a.attnum = c.conkey[1] \
             WHERE c.contype = 'f' AND c.conrelid = 'doc_customer'::regclass AND a.attname = 'custom_owner'"
        )
        .await,
        vec![vec!["1".to_owned()]]
    );
    assert_eq!(
        db.rows(
            "SELECT count(*)::text FROM pg_indexes WHERE tablename = 'doc_customer' AND indexdef LIKE '%custom_owner%'"
        )
        .await,
        vec![vec!["1".to_owned()]]
    );
}

#[tokio::test]
async fn the_rules_are_enforced_and_nothing_is_written_on_a_refusal() {
    let (db, base) = site().await;
    let cases: Vec<(&str, &str, fn(&DbError) -> bool)> = vec![
        ("customer", "name = \"vip\"\ntype = \"check\"\n", |e| {
            matches!(e, DbError::CustomField(OverlayError::MissingPrefix(_)))
        }),
        (
            "customer",
            "name = \"custom_a\"\ntype = \"check\"\nrequired = true\n",
            |e| matches!(e, DbError::CustomField(OverlayError::Required(_))),
        ),
        (
            "customer",
            "name = \"custom_a\"\ntype = \"check\"\nread_only = true\n",
            |e| matches!(e, DbError::CustomField(OverlayError::ReadOnly(_))),
        ),
        (
            "customer",
            "name = \"custom_a\"\ntype = \"table\"\ntarget = \"person\"\n",
            |e| matches!(e, DbError::CustomField(OverlayError::TableField(_))),
        ),
        (
            "customer",
            "name = \"custom_a\"\ntype = \"link\"\ntarget = \"ghost\"\n",
            |e| matches!(e, DbError::CustomField(OverlayError::Invalid(_))),
        ),
        ("customer", "name = \"email\"\ntype = \"check\"\n", |e| {
            matches!(e, DbError::CustomField(OverlayError::MissingPrefix(_)))
        }),
        ("ghost", "name = \"custom_a\"\ntype = \"check\"\n", |e| {
            matches!(e, DbError::CustomField(OverlayError::UnknownDocType(_)))
        }),
    ];
    for (doctype, text, expected) in cases {
        let error = add(&db, &base, doctype, text).await.expect_err(text);
        assert!(expected(&error), "{text}: {error}");
    }
    let client = db.pool.get().await.unwrap();
    assert!(
        list_custom_fields(&**client, None)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(meta_version(&**client).await.unwrap(), 0);
}

#[tokio::test]
async fn a_duplicate_is_refused_also_when_the_field_is_disabled() {
    let (db, base) = site().await;
    add(
        &db,
        &base,
        "customer",
        "name = \"custom_vip\"\ntype = \"check\"\n",
    )
    .await
    .expect("added");
    let again = add(
        &db,
        &base,
        "customer",
        "name = \"custom_vip\"\ntype = \"int\"\n",
    )
    .await;
    assert!(
        matches!(again, Err(DbError::CustomFieldExists { .. })),
        "{again:?}"
    );
    let mut client = db.pool.get().await.unwrap();
    set_custom_field_enabled(&mut client, "customer", "custom_vip", false, "cli", LOCK)
        .await
        .expect("disabled");
    drop(client);
    let again = add(
        &db,
        &base,
        "customer",
        "name = \"custom_vip\"\ntype = \"int\"\n",
    )
    .await;
    assert!(
        matches!(again, Err(DbError::CustomFieldExists { .. })),
        "{again:?}"
    );
}

#[tokio::test]
async fn adding_to_a_doctype_whose_table_does_not_exist_is_refused() {
    let db = TestDb::migrated().await;
    let base = registry(&[CUSTOMER]);
    let error = add(
        &db,
        &base,
        "customer",
        "name = \"custom_a\"\ntype = \"check\"\n",
    )
    .await
    .expect_err("no table");
    assert!(
        matches!(error, DbError::DocTypeNotMigrated { .. }),
        "{error}"
    );
    let client = db.pool.get().await.unwrap();
    assert!(
        list_custom_fields(&**client, None)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn disable_hides_the_field_and_keeps_the_column_and_the_data() {
    let (db, base) = site().await;
    add(
        &db,
        &base,
        "customer",
        "name = \"custom_vip\"\ntype = \"check\"\n",
    )
    .await
    .expect("added");
    db.exec("INSERT INTO doc_customer (id, name, owner, created_at, modified_at, modified_by, email, custom_vip) \
             VALUES (gen_random_uuid(), 'c1', 'x', now(), now(), 'x', 'a@b.c', true)")
        .await;

    let mut client = db.pool.get().await.unwrap();
    let disabled =
        set_custom_field_enabled(&mut client, "customer", "custom_vip", false, "ann", LOCK)
            .await
            .expect("disabled");
    assert!(!disabled.enabled);
    assert_eq!(disabled.modified_by, "ann");
    assert!(disabled.modified_at >= disabled.created_at);
    assert_eq!(meta_version(&**client).await.unwrap(), 2);

    let on = effective_registry(&**client, &base, "t", false)
        .await
        .unwrap()
        .registry;
    assert!(on.get("customer").unwrap().field("custom_vip").is_none());
    let all = effective_registry(&**client, &base, "t", true)
        .await
        .unwrap()
        .registry;
    assert!(all.get("customer").unwrap().field("custom_vip").is_some());
    assert_eq!(
        db.rows("SELECT custom_vip::text FROM doc_customer").await,
        vec![vec!["true".to_owned()]]
    );

    let enabled =
        set_custom_field_enabled(&mut client, "customer", "custom_vip", true, "ann", LOCK)
            .await
            .expect("enabled");
    assert!(enabled.enabled);
    assert_eq!(meta_version(&**client).await.unwrap(), 3);
    let on = effective_registry(&**client, &base, "t", false)
        .await
        .unwrap()
        .registry;
    assert!(on.get("customer").unwrap().field("custom_vip").is_some());

    let missing =
        set_custom_field_enabled(&mut client, "customer", "custom_nope", true, "ann", LOCK).await;
    assert!(
        matches!(missing, Err(DbError::CustomFieldNotFound { .. })),
        "{missing:?}"
    );
    assert_eq!(meta_version(&**client).await.unwrap(), 3);
}

#[tokio::test]
async fn the_effective_registry_appends_in_creation_order_per_doctype() {
    let (db, base) = site().await;
    for (doctype, text) in [
        ("customer", "name = \"custom_z\"\ntype = \"int\"\n"),
        ("person", "name = \"custom_p\"\ntype = \"int\"\n"),
        ("customer", "name = \"custom_a\"\ntype = \"int\"\n"),
    ] {
        add(&db, &base, doctype, text).await.expect("added");
    }
    let client = db.pool.get().await.unwrap();
    let effective = effective_registry(&**client, &base, "t", false)
        .await
        .unwrap();
    assert_eq!(effective.version, 3);
    assert!(effective.warnings.is_empty());
    let customer = effective.registry.get("customer").unwrap();
    let names: Vec<_> = customer.fields.iter().map(|f| f.name.as_str()).collect();
    let n = names.len();
    assert_eq!(&names[n - 2..], ["custom_z", "custom_a"]);
    // The base registry is not touched.
    assert!(base.get("customer").unwrap().field("custom_z").is_none());
}

#[tokio::test]
async fn a_stored_field_that_no_longer_validates_is_skipped_with_a_warning() {
    let (db, base) = site().await;
    add(
        &db,
        &base,
        "customer",
        "name = \"custom_ok\"\ntype = \"check\"\n",
    )
    .await
    .expect("added");
    for (doctype, name, definition) in [
        // Its Link target left the apps.
        (
            "customer",
            "custom_gone",
            r#"{"name":"custom_gone","type":"link","target":"ghost"}"#,
        ),
        // Not a field definition at all.
        (
            "customer",
            "custom_junk",
            r#"{"name":"custom_junk","type":"nope"}"#,
        ),
        // Its DocType left the apps.
        (
            "vanished",
            "custom_x",
            r#"{"name":"custom_x","type":"check"}"#,
        ),
        // Violates a rule of M10.1 (written around the framework).
        (
            "customer",
            "custom_req",
            r#"{"name":"custom_req","type":"check","required":true}"#,
        ),
        // The definition names another field than the row.
        (
            "customer",
            "custom_mismatch",
            r#"{"name":"custom_other","type":"check"}"#,
        ),
    ] {
        db.exec(&format!(
            "INSERT INTO fb_custom_field (doctype, name, definition, modified_by) \
             VALUES ('{doctype}', '{name}', '{definition}', 'sql')"
        ))
        .await;
    }
    let client = db.pool.get().await.unwrap();
    let effective = effective_registry(&**client, &base, "shop", false)
        .await
        .unwrap();
    assert_eq!(effective.warnings.len(), 5, "{:?}", effective.warnings);
    for warning in &effective.warnings {
        assert!(
            warning.starts_with("site `shop`: custom field `custom_"),
            "{warning}"
        );
    }
    let customer = effective.registry.get("customer").unwrap();
    assert!(customer.field("custom_ok").is_some());
    for gone in [
        "custom_gone",
        "custom_junk",
        "custom_req",
        "custom_mismatch",
        "custom_other",
    ] {
        assert!(customer.field(gone).is_none(), "{gone}");
    }
}

#[tokio::test]
async fn migrate_gives_back_a_missing_column_enabled_or_disabled() {
    let (db, base) = site().await;
    add(
        &db,
        &base,
        "customer",
        "name = \"custom_a\"\ntype = \"int\"\n",
    )
    .await
    .unwrap();
    add(
        &db,
        &base,
        "customer",
        "name = \"custom_b\"\ntype = \"data\"\nunique = true\n",
    )
    .await
    .unwrap();
    let mut client = db.pool.get().await.unwrap();
    set_custom_field_enabled(&mut client, "customer", "custom_b", false, "cli", LOCK)
        .await
        .unwrap();
    drop(client);
    db.exec("ALTER TABLE doc_customer DROP COLUMN custom_a, DROP COLUMN custom_b")
        .await;

    let report = migrate(&db.pool, &base, &SyncOptions::default())
        .await
        .expect("migrate");
    let columns: Vec<String> = db
        .columns("doc_customer")
        .await
        .into_iter()
        .map(|c| c.0)
        .collect();
    assert!(
        columns.contains(&"custom_a".to_owned()) && columns.contains(&"custom_b".to_owned()),
        "{columns:?}"
    );
    assert!(
        report
            .sync
            .expect("sync")
            .steps
            .iter()
            .any(|s| s.to_string().contains("custom_a"))
    );
    // The unique index of the disabled field is back too.
    assert_eq!(
        db.rows("SELECT count(*)::text FROM pg_indexes WHERE tablename = 'doc_customer' AND indexdef LIKE '%UNIQUE%custom_b%'").await,
        vec![vec!["1".to_owned()]]
    );
    // Idempotent: nothing left to do.
    let again = migrate(&db.pool, &base, &SyncOptions::default())
        .await
        .unwrap();
    assert!(again.sync.expect("sync").steps.is_empty());
    // A field that does not validate is skipped, and migrate still works.
    db.exec("INSERT INTO fb_custom_field (doctype, name, definition, modified_by) \
             VALUES ('customer', 'custom_bad', '{\"name\":\"custom_bad\",\"type\":\"link\",\"target\":\"ghost\"}', 'sql')").await;
    migrate(&db.pool, &base, &SyncOptions::default())
        .await
        .expect("migrate with a bad row");
}

#[tokio::test]
async fn the_holder_serves_a_snapshot_and_rebuilds_on_a_version_change() {
    let (db, base) = site().await;
    let base = Arc::new(base);
    let holder = RegistryHolder::load("t", Arc::clone(&base), &db.pool)
        .await
        .expect("load");
    let before = holder.snapshot();
    assert!(
        before
            .get("customer")
            .unwrap()
            .field("custom_vip")
            .is_none()
    );
    assert!(!holder.poll(&db.pool).await.unwrap(), "nothing changed");

    add(
        &db,
        &base,
        "customer",
        "name = \"custom_vip\"\ntype = \"check\"\n",
    )
    .await
    .unwrap();
    // The old snapshot is untouched; a new one sees the field after the poll.
    assert!(
        holder
            .snapshot()
            .get("customer")
            .unwrap()
            .field("custom_vip")
            .is_none()
    );
    assert!(holder.poll(&db.pool).await.unwrap(), "version changed");
    assert!(
        holder
            .snapshot()
            .get("customer")
            .unwrap()
            .field("custom_vip")
            .is_some()
    );
    assert!(
        before
            .get("customer")
            .unwrap()
            .field("custom_vip")
            .is_none()
    );
    assert!(!holder.poll(&db.pool).await.unwrap());

    let mut client = db.pool.get().await.unwrap();
    set_custom_field_enabled(&mut client, "customer", "custom_vip", false, "cli", LOCK)
        .await
        .unwrap();
    drop(client);
    assert_eq!(holder.reload(&db.pool).await.unwrap(), 2);
    assert!(
        holder
            .snapshot()
            .get("customer")
            .unwrap()
            .field("custom_vip")
            .is_none()
    );
    assert!(Arc::ptr_eq(holder.base(), &base));
}

#[tokio::test]
async fn a_failed_poll_keeps_the_current_registry() {
    let (db, base) = site().await;
    let holder = RegistryHolder::load("t", Arc::new(base), &db.pool)
        .await
        .expect("load");
    let before = holder.snapshot();
    db.exec("DROP TABLE fb_meta_version").await;
    assert!(holder.poll(&db.pool).await.is_err());
    assert!(Arc::ptr_eq(&before, &holder.snapshot()));
}

#[tokio::test]
async fn swapping_under_concurrent_readers_never_shows_a_half_built_registry() {
    let (db, base) = site().await;
    let base = Arc::new(base);
    let holder = Arc::new(
        RegistryHolder::load("t", Arc::clone(&base), &db.pool)
            .await
            .unwrap(),
    );
    add(
        &db,
        &base,
        "customer",
        "name = \"custom_a\"\ntype = \"int\"\n",
    )
    .await
    .unwrap();
    add(
        &db,
        &base,
        "customer",
        "name = \"custom_b\"\ntype = \"int\"\n",
    )
    .await
    .unwrap();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut readers = Vec::new();
    for _ in 0..8 {
        let (holder, stop) = (Arc::clone(&holder), Arc::clone(&stop));
        readers.push(tokio::spawn(async move {
            let mut seen = 0_u64;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                let snapshot = holder.snapshot();
                // A consistent registry: the fields of one rebuild, in order, whatever the version.
                let names: Vec<&str> = snapshot
                    .get("customer")
                    .unwrap()
                    .fields
                    .iter()
                    .map(|f| f.name.as_str())
                    .collect();
                let custom: Vec<&str> = names
                    .iter()
                    .copied()
                    .filter(|n| n.starts_with("custom_"))
                    .collect();
                assert!(
                    ["", "custom_a", "custom_a,custom_b", "custom_b"]
                        .contains(&custom.join(",").as_str()),
                    "{names:?}"
                );
                seen += 1;
                tokio::task::yield_now().await;
            }
            seen
        }));
    }
    for round in 0..20 {
        let mut client = db.pool.get().await.unwrap();
        set_custom_field_enabled(
            &mut client,
            "customer",
            "custom_a",
            round % 2 == 0,
            "cli",
            LOCK,
        )
        .await
        .unwrap();
        drop(client);
        holder.reload(&db.pool).await.unwrap();
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    for reader in readers {
        assert!(reader.await.unwrap() > 0);
    }
}
