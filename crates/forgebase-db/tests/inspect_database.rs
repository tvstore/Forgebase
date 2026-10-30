//! `inspect_database` (M7.3) against a real PostgreSQL: the server version and the emptiness
//! check that `site restore` relies on.

mod common;

use common::TestDb;
use forgebase_db::inspect_database;

#[tokio::test]
async fn a_new_database_is_empty_and_anything_in_it_makes_it_not_empty() {
    let db = TestDb::new().await;
    let info = inspect_database(&db.url()).await.expect("inspect");
    assert!(info.is_empty);
    assert!(info.server_major() >= 16, "{info:?}");

    // Each of these alone makes the database "not empty"; each is undone before the next.
    let cases = [
        (
            "a table in public",
            "CREATE TABLE public.t (a int)",
            "DROP TABLE public.t",
        ),
        (
            "a sequence",
            "CREATE SEQUENCE public.s",
            "DROP SEQUENCE public.s",
        ),
        (
            "a view",
            "CREATE VIEW public.v AS SELECT 1 AS a",
            "DROP VIEW public.v",
        ),
        ("a schema", "CREATE SCHEMA extra", "DROP SCHEMA extra"),
        (
            "a table in another schema",
            "CREATE SCHEMA extra; CREATE TABLE extra.t (a int)",
            "DROP SCHEMA extra CASCADE",
        ),
        (
            "a function",
            "CREATE FUNCTION public.f() RETURNS int LANGUAGE sql AS 'SELECT 1'",
            "DROP FUNCTION public.f()",
        ),
        (
            "an enum type",
            "CREATE TYPE public.e AS ENUM ('a')",
            "DROP TYPE public.e",
        ),
        (
            "a domain",
            "CREATE DOMAIN public.d AS int",
            "DROP DOMAIN public.d",
        ),
    ];
    for (what, create, undo) in cases {
        db.exec(create).await;
        let info = inspect_database(&db.url()).await.expect("inspect");
        assert!(!info.is_empty, "{what} must make the database not empty");
        db.exec(undo).await;
        let info = inspect_database(&db.url()).await.expect("inspect");
        assert!(info.is_empty, "undoing {what} must leave it empty");
    }
    // Back to not empty once more, to end the test on the other answer.
    db.exec("CREATE TABLE public.t (a int)").await;
    assert!(!inspect_database(&db.url()).await.expect("inspect").is_empty);
}

#[tokio::test]
async fn a_missing_database_is_told_apart() {
    let db = TestDb::new().await;
    let url = db.url().replace(
        db.url().rsplit('/').next().expect("name"),
        "fb_no_such_database_xyz",
    );
    let error = inspect_database(&url).await.expect_err("missing");
    assert!(error.is_missing_database(), "{error}");
    assert!(!error.to_string().contains("forgebase:forgebase"));
}
