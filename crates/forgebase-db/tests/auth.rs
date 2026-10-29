//! Users, sessions, API tokens and login lockout against a real PostgreSQL.

mod common;

use common::TestDb;
use forgebase_db::auth::{
    ApiTokenSecret, AuthError, AuthUser, LoginSession, add_role, check_csrf, create_api_token,
    create_user, login, logout, remove_role, revoke_api_token, set_enabled, set_password,
    validate_api_token, validate_session,
};

const PASSWORD: &str = "correct horse battery";

/// A database with the framework tables and the user `alice` (password [`PASSWORD`]).
async fn setup() -> TestDb {
    let db = TestDb::migrated().await;
    let mut client = db.pool.get().await.unwrap();
    create_user(&mut **client, "alice", PASSWORD, &["Sales", "Accounts"])
        .await
        .unwrap();
    db
}

async fn login_alice(db: &TestDb) -> LoginSession {
    let mut client = db.pool.get().await.unwrap();
    login(&mut **client, "alice", PASSWORD).await.unwrap()
}

async fn try_login(db: &TestDb, username: &str, password: &str) -> Result<LoginSession, AuthError> {
    let mut client = db.pool.get().await.unwrap();
    login(&mut **client, username, password).await
}

async fn session_user(db: &TestDb, token: &str) -> Option<AuthUser> {
    let client = db.pool.get().await.unwrap();
    validate_session(&**client, token).await.unwrap()
}

async fn scalar(db: &TestDb, sql: &str) -> String {
    db.rows(sql).await.remove(0).remove(0)
}

#[tokio::test]
async fn the_auth_tables_have_the_documented_columns() {
    let db = TestDb::migrated().await;
    let columns = |table: &'static str| {
        let db = &db;
        async move {
            db.columns(table)
                .await
                .into_iter()
                .map(|(name, kind)| format!("{name} {kind}"))
                .collect::<Vec<_>>()
        }
    };
    assert_eq!(
        columns("fb_user").await,
        [
            "id uuid/NO",
            "username text/NO",
            "password_hash text/NO",
            "enabled boolean/NO",
            "created_at timestamp with time zone/NO"
        ]
    );
    assert_eq!(
        columns("fb_session").await,
        [
            "token_hash bytea/NO",
            "csrf_hash bytea/NO",
            "user_id uuid/NO",
            "created_at timestamp with time zone/NO",
            "last_seen_at timestamp with time zone/NO",
            "expires_at timestamp with time zone/NO"
        ]
    );
    assert_eq!(
        columns("fb_api_token").await,
        [
            "key text/NO",
            "secret_hash bytea/NO",
            "user_id uuid/NO",
            "created_at timestamp with time zone/NO",
            "revoked_at timestamp with time zone/YES"
        ]
    );
    assert_eq!(
        columns("fb_login_failure").await,
        [
            "username text/NO",
            "failures integer/NO",
            "locked_until timestamp with time zone/YES"
        ]
    );
    assert_eq!(
        columns("fb_user_role").await,
        ["user_id uuid/NO", "role text/NO"]
    );
}

#[tokio::test]
async fn create_user_stores_a_hash_and_the_roles() {
    let db = setup().await;
    let hash = scalar(
        &db,
        "SELECT password_hash FROM fb_user WHERE username = 'alice'",
    )
    .await;
    assert!(
        hash.starts_with("$argon2id$v=19$m=19456,t=2,p=1$"),
        "{hash}"
    );
    assert!(!hash.contains(PASSWORD));
    assert_eq!(
        db.rows("SELECT role FROM fb_user_role ORDER BY role").await,
        vec![vec!["Accounts".to_owned()], vec!["Sales".into()]]
    );
    assert_eq!(
        scalar(&db, "SELECT enabled::text FROM fb_user").await,
        "true"
    );
}

#[tokio::test]
async fn duplicate_and_case_different_usernames() {
    let db = setup().await;
    let mut client = db.pool.get().await.unwrap();
    let error = create_user(&mut **client, "alice", PASSWORD, &[])
        .await
        .unwrap_err();
    assert!(matches!(error, AuthError::UserExists), "{error}");
    // Usernames are case-sensitive.
    create_user(&mut **client, "Alice", PASSWORD, &[])
        .await
        .unwrap();
    assert_eq!(scalar(&db, "SELECT count(*)::text FROM fb_user").await, "2");
}

#[tokio::test]
async fn invalid_usernames_roles_and_passwords_are_rejected() {
    let db = TestDb::migrated().await;
    let mut client = db.pool.get().await.unwrap();
    let client = &mut **client;
    for bad in ["", "a b", "a/b", "ä", &"a".repeat(141)] {
        let error = create_user(client, bad, PASSWORD, &[]).await.unwrap_err();
        assert!(matches!(error, AuthError::InvalidUsername(_)), "{bad:?}");
    }
    create_user(client, &"a".repeat(140), PASSWORD, &[])
        .await
        .unwrap();
    for bad in ["", "  ", "a\nb", &"r".repeat(141)] {
        let error = create_user(client, "bob", PASSWORD, &[bad])
            .await
            .unwrap_err();
        assert!(matches!(error, AuthError::InvalidRole(_)), "{bad:?}");
    }
    for bad in ["1234567", &"p".repeat(1025)] {
        let error = create_user(client, "bob", bad, &[]).await.unwrap_err();
        assert!(matches!(error, AuthError::InvalidPassword(_)));
        // The message never contains the password.
        assert!(!error.to_string().contains(bad));
    }
    create_user(client, "bob", "12345678", &[]).await.unwrap();
    create_user(client, "carol", &"p".repeat(1024), &[])
        .await
        .unwrap();
    // Nothing was written for the rejected ones.
    assert_eq!(scalar(&db, "SELECT count(*)::text FROM fb_user").await, "3");
}

#[tokio::test]
async fn roles_can_be_added_and_removed() {
    let db = setup().await;
    let mut client = db.pool.get().await.unwrap();
    let client = &mut **client;
    assert!(add_role(client, "alice", "Admin").await.unwrap());
    assert!(!add_role(client, "alice", "Admin").await.unwrap());
    assert!(remove_role(client, "alice", "Sales").await.unwrap());
    assert!(!remove_role(client, "alice", "Sales").await.unwrap());
    let session = login(client, "alice", PASSWORD).await.unwrap();
    assert_eq!(session.user.roles, ["Accounts", "Admin"]);
    let error = add_role(client, "nobody", "Admin").await.unwrap_err();
    assert!(matches!(error, AuthError::UserNotFound(_)), "{error}");
    let error = add_role(client, "alice", " ").await.unwrap_err();
    assert!(matches!(error, AuthError::InvalidRole(_)), "{error}");
}

#[tokio::test]
async fn login_returns_tokens_and_the_database_holds_only_hashes() {
    let db = setup().await;
    let session = login_alice(&db).await;
    assert_eq!(session.user.username, "alice");
    assert_eq!(session.user.roles, ["Accounts", "Sales"]);
    assert_eq!(session.token.len(), 64);
    assert_eq!(session.csrf_token.len(), 64);
    assert_ne!(session.token, session.csrf_token);

    // No column of any auth table equals (or contains) a secret.
    for table in ["fb_user", "fb_session", "fb_api_token", "fb_login_failure"] {
        let dump = scalar(
            &db,
            &format!("SELECT coalesce(string_agg(t::text, ' '), '') FROM {table} t"),
        )
        .await;
        for secret in [PASSWORD, &session.token, &session.csrf_token] {
            assert!(!dump.contains(secret), "{table} contains a secret");
        }
    }
    // The stored digest is the SHA-256 of the token.
    assert_eq!(
        scalar(
            &db,
            &format!(
                "SELECT (token_hash = sha256(convert_to('{}', 'UTF8')) AND \
                 csrf_hash = sha256(convert_to('{}', 'UTF8')))::text FROM fb_session",
                session.token, session.csrf_token
            )
        )
        .await,
        "true"
    );
    // Absolute expiry: 7 days.
    assert_eq!(
        scalar(
            &db,
            "SELECT round(extract(epoch FROM expires_at - created_at) / 86400)::text FROM fb_session"
        )
        .await,
        "7"
    );
}

#[tokio::test]
async fn every_login_creates_a_new_session() {
    let db = setup().await;
    let first = login_alice(&db).await;
    let second = login_alice(&db).await;
    assert_ne!(first.token, second.token);
    assert_ne!(first.csrf_token, second.csrf_token);
    assert_eq!(
        scalar(&db, "SELECT count(*)::text FROM fb_session").await,
        "2"
    );
    assert!(session_user(&db, &first.token).await.is_some());
    assert!(session_user(&db, &second.token).await.is_some());
}

#[tokio::test]
async fn bad_logins_all_give_the_same_error() {
    let db = setup().await;
    {
        let mut client = db.pool.get().await.unwrap();
        create_user(&mut **client, "dave", PASSWORD, &[])
            .await
            .unwrap();
        set_enabled(&mut **client, "dave", false).await.unwrap();
    }
    let mut messages = Vec::new();
    for (name, password) in [
        ("alice", "wrong password"),
        ("nobody", PASSWORD),
        ("dave", PASSWORD),
        // Rule violations fail the same way and are not stored.
        ("bad name!", PASSWORD),
        ("alice", "short"),
    ] {
        let error = try_login(&db, name, password).await.unwrap_err();
        assert!(matches!(error, AuthError::InvalidCredentials), "{name}");
        messages.push((error.to_string(), format!("{error:?}")));
    }
    assert!(messages.iter().all(|m| m == &messages[0]), "{messages:?}");
    assert_eq!(messages[0].0, "invalid username or password");
    assert_eq!(
        scalar(&db, "SELECT count(*)::text FROM fb_session").await,
        "0"
    );
    // Unknown users are counted too, under the submitted name; invalid names are not.
    assert_eq!(
        db.rows("SELECT username, failures::text FROM fb_login_failure ORDER BY username")
            .await,
        vec![
            vec!["alice".to_owned(), "1".into()],
            vec!["dave".into(), "1".into()],
            vec!["nobody".into(), "1".into()],
        ]
    );
}

#[tokio::test]
async fn five_failures_lock_the_username_for_fifteen_minutes() {
    let db = setup().await;
    for _ in 0..4 {
        try_login(&db, "alice", "wrong password").await.unwrap_err();
    }
    // Still not locked: the right password works and resets the counter.
    login_alice(&db).await;
    assert_eq!(
        scalar(&db, "SELECT count(*)::text FROM fb_login_failure").await,
        "0"
    );

    for _ in 0..5 {
        try_login(&db, "alice", "wrong password").await.unwrap_err();
    }
    assert_eq!(
        db.rows("SELECT failures::text, round(extract(epoch FROM locked_until - now()) / 60)::text FROM fb_login_failure")
            .await,
        vec![vec!["0".to_owned(), "15".into()]]
    );
    // Locked: even the correct password fails, with the same error, and does not extend the lock.
    let error = try_login(&db, "alice", PASSWORD).await.unwrap_err();
    assert!(matches!(error, AuthError::InvalidCredentials));
    assert_eq!(
        scalar(&db, "SELECT failures::text FROM fb_login_failure").await,
        "0"
    );

    // Other usernames are not affected.
    {
        let mut client = db.pool.get().await.unwrap();
        create_user(&mut **client, "bob", PASSWORD, &[])
            .await
            .unwrap();
    }
    try_login(&db, "bob", PASSWORD).await.unwrap();

    // After the lock has passed the correct password works and clears the row.
    db.exec("UPDATE fb_login_failure SET locked_until = now() - interval '1 second'")
        .await;
    login_alice(&db).await;
    assert_eq!(
        scalar(&db, "SELECT count(*)::text FROM fb_login_failure").await,
        "0"
    );
}

#[tokio::test]
async fn unknown_usernames_are_locked_too() {
    let db = setup().await;
    for _ in 0..5 {
        try_login(&db, "ghost", PASSWORD).await.unwrap_err();
    }
    assert_eq!(
        scalar(
            &db,
            "SELECT (locked_until > now())::text FROM fb_login_failure WHERE username = 'ghost'"
        )
        .await,
        "true"
    );
}

#[tokio::test]
async fn concurrent_failures_cannot_exceed_the_limit() {
    let db = setup().await;
    let mut tasks = Vec::new();
    for _ in 0..10 {
        let pool = db.pool.clone();
        tasks.push(tokio::spawn(async move {
            let mut client = pool.get().await.unwrap();
            login(&mut **client, "alice", "wrong password").await
        }));
    }
    for task in tasks {
        let error = task.await.unwrap().unwrap_err();
        assert!(matches!(error, AuthError::InvalidCredentials));
    }
    // Exactly five attempts were counted; the other five met the lock and were not counted.
    assert_eq!(
        db.rows("SELECT failures::text, (locked_until > now())::text FROM fb_login_failure")
            .await,
        vec![vec!["0".to_owned(), "true".into()]]
    );
    let error = try_login(&db, "alice", PASSWORD).await.unwrap_err();
    assert!(matches!(error, AuthError::InvalidCredentials));
}

#[tokio::test]
async fn failure_accounting_survives_a_caller_transaction_only_when_committed() {
    // Documented limit: inside a caller's transaction that rolls back, the accounting is lost.
    let db = setup().await;
    let mut client = db.pool.get().await.unwrap();
    let raw: &mut forgebase_db::Client = &mut client;
    let mut tx = raw.transaction().await.unwrap();
    login(&mut tx, "alice", "wrong password").await.unwrap_err();
    tx.rollback().await.unwrap();
    assert_eq!(
        scalar(&db, "SELECT count(*)::text FROM fb_login_failure").await,
        "0"
    );
}

#[tokio::test]
async fn sessions_are_valid_then_touched_then_expire() {
    let db = setup().await;
    let session = login_alice(&db).await;

    // Valid, and the use is recorded.
    db.exec("UPDATE fb_session SET last_seen_at = now() - interval '23 hours'")
        .await;
    let user = session_user(&db, &session.token).await.unwrap();
    assert_eq!(user.username, "alice");
    assert_eq!(user.roles, ["Accounts", "Sales"]);
    assert_eq!(
        scalar(
            &db,
            "SELECT (now() - last_seen_at < interval '1 minute')::text FROM fb_session"
        )
        .await,
        "true"
    );

    // Idle for 24 hours or more: expired, and the row is deleted.
    db.exec("UPDATE fb_session SET last_seen_at = now() - interval '24 hours 1 second'")
        .await;
    assert!(session_user(&db, &session.token).await.is_none());
    assert_eq!(
        scalar(&db, "SELECT count(*)::text FROM fb_session").await,
        "0"
    );

    // Absolute expiry applies even to a session in constant use.
    let session = login_alice(&db).await;
    db.exec("UPDATE fb_session SET expires_at = now() - interval '1 second'")
        .await;
    assert!(session_user(&db, &session.token).await.is_none());
    assert_eq!(
        scalar(&db, "SELECT count(*)::text FROM fb_session").await,
        "0"
    );
}

#[tokio::test]
async fn tampered_unknown_and_malformed_tokens_are_rejected() {
    let db = setup().await;
    let session = login_alice(&db).await;
    let mut tampered = session.token.clone();
    let last = if tampered.ends_with('0') { '1' } else { '0' };
    tampered.pop();
    tampered.push(last);
    for bad in [
        tampered.as_str(),
        "",
        "x",
        &session.csrf_token,
        &session.token.to_uppercase(),
        &format!("{}0", session.token),
    ] {
        assert!(session_user(&db, bad).await.is_none(), "{bad}");
    }
    assert!(session_user(&db, &session.token).await.is_some());
}

#[tokio::test]
async fn logout_ends_only_that_session() {
    let db = setup().await;
    let first = login_alice(&db).await;
    let second = login_alice(&db).await;
    let client = db.pool.get().await.unwrap();
    assert!(logout(&**client, &first.token).await.unwrap());
    assert!(!logout(&**client, &first.token).await.unwrap());
    assert!(session_user(&db, &first.token).await.is_none());
    assert!(session_user(&db, &second.token).await.is_some());
}

#[tokio::test]
async fn changing_the_password_ends_all_sessions() {
    let db = setup().await;
    let first = login_alice(&db).await;
    let second = login_alice(&db).await;
    {
        let mut client = db.pool.get().await.unwrap();
        set_password(&mut **client, "alice", "a brand new password")
            .await
            .unwrap();
    }
    assert!(session_user(&db, &first.token).await.is_none());
    assert!(session_user(&db, &second.token).await.is_none());
    let error = try_login(&db, "alice", PASSWORD).await.unwrap_err();
    assert!(matches!(error, AuthError::InvalidCredentials));
    try_login(&db, "alice", "a brand new password")
        .await
        .unwrap();

    let mut client = db.pool.get().await.unwrap();
    let error = set_password(&mut **client, "nobody", PASSWORD)
        .await
        .unwrap_err();
    assert!(matches!(error, AuthError::UserNotFound(_)));
    let error = set_password(&mut **client, "alice", "short")
        .await
        .unwrap_err();
    assert!(matches!(error, AuthError::InvalidPassword(_)));
}

#[tokio::test]
async fn disabling_a_user_ends_sessions_and_blocks_login() {
    let db = setup().await;
    let session = login_alice(&db).await;
    let api = {
        let client = db.pool.get().await.unwrap();
        create_api_token(&**client, "alice").await.unwrap()
    };
    {
        let mut client = db.pool.get().await.unwrap();
        set_enabled(&mut **client, "alice", false).await.unwrap();
    }
    assert!(session_user(&db, &session.token).await.is_none());
    assert_eq!(
        scalar(&db, "SELECT count(*)::text FROM fb_session").await,
        "0"
    );
    let client = db.pool.get().await.unwrap();
    assert!(
        validate_api_token(&**client, &api.key, &api.secret)
            .await
            .unwrap()
            .is_none()
    );
    drop(client);
    let error = try_login(&db, "alice", PASSWORD).await.unwrap_err();
    assert!(matches!(error, AuthError::InvalidCredentials));

    // Enabling restores the account (the lockout counter of the failed attempt is below the limit).
    {
        let mut client = db.pool.get().await.unwrap();
        set_enabled(&mut **client, "alice", true).await.unwrap();
        let client = &**client;
        assert!(
            validate_api_token(client, &api.key, &api.secret)
                .await
                .unwrap()
                .is_some()
        );
    }
    login_alice(&db).await;
}

#[tokio::test]
async fn a_disabled_user_cannot_use_an_existing_session_even_if_the_row_survives() {
    let db = setup().await;
    let session = login_alice(&db).await;
    db.exec("UPDATE fb_user SET enabled = false").await;
    assert!(session_user(&db, &session.token).await.is_none());
    let client = db.pool.get().await.unwrap();
    assert!(
        !check_csrf(&**client, &session.token, &session.csrf_token)
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn csrf_tokens_belong_to_their_session() {
    let db = setup().await;
    let first = login_alice(&db).await;
    let second = login_alice(&db).await;
    let client = db.pool.get().await.unwrap();
    let client = &**client;
    assert!(
        check_csrf(client, &first.token, &first.csrf_token)
            .await
            .unwrap()
    );
    assert!(
        !check_csrf(client, &first.token, &second.csrf_token)
            .await
            .unwrap()
    );
    assert!(
        !check_csrf(client, &first.token, &first.token)
            .await
            .unwrap()
    );
    assert!(!check_csrf(client, &first.token, "").await.unwrap());
    assert!(!check_csrf(client, "nope", &first.csrf_token).await.unwrap());
    // An expired session has no valid CSRF token either.
    db.exec("UPDATE fb_session SET expires_at = now() - interval '1 second'")
        .await;
    assert!(
        !check_csrf(client, &first.token, &first.csrf_token)
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn api_tokens_validate_until_revoked() {
    let db = setup().await;
    let client = db.pool.get().await.unwrap();
    let client = &**client;
    let token = create_api_token(client, "alice").await.unwrap();
    assert_eq!(token.key.len(), 32);
    assert_eq!(token.secret.len(), 64);

    // Only the digest is stored.
    let dump = scalar(&db, "SELECT string_agg(t::text, ' ') FROM fb_api_token t").await;
    assert!(dump.contains(&token.key) && !dump.contains(&token.secret));

    let user = validate_api_token(client, &token.key, &token.secret)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(user.username, "alice");
    assert_eq!(user.roles, ["Accounts", "Sales"]);

    let other = create_api_token(client, "alice").await.unwrap();
    assert_ne!(other.key, token.key);
    for (key, secret) in [
        (token.key.as_str(), other.secret.as_str()),
        (token.key.as_str(), "wrong"),
        (token.key.as_str(), ""),
        ("00000000000000000000000000000000", token.secret.as_str()),
        ("", ""),
    ] {
        assert!(
            validate_api_token(client, key, secret)
                .await
                .unwrap()
                .is_none(),
            "{key} {secret}"
        );
    }

    revoke_api_token(client, &token.key).await.unwrap();
    revoke_api_token(client, &token.key).await.unwrap();
    assert!(
        validate_api_token(client, &token.key, &token.secret)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        validate_api_token(client, &other.key, &other.secret)
            .await
            .unwrap()
            .is_some()
    );
    let error = revoke_api_token(client, "unknown").await.unwrap_err();
    assert!(matches!(error, AuthError::TokenNotFound(_)));
    let error = create_api_token(client, "nobody").await.unwrap_err();
    assert!(matches!(error, AuthError::UserNotFound(_)));
}

#[tokio::test]
async fn secrets_never_appear_in_errors_or_debug_output() {
    let db = setup().await;
    let session = login_alice(&db).await;
    let api: ApiTokenSecret = {
        let client = db.pool.get().await.unwrap();
        create_api_token(&**client, "alice").await.unwrap()
    };
    let wrong = "this is the wrong password";
    let mut texts = vec![
        format!("{session:?}"),
        format!("{api:?}"),
        format!("{:?}", session.user),
    ];
    let error = try_login(&db, "alice", wrong).await.unwrap_err();
    texts.push(format!("{error} {error:?}"));
    let mut client = db.pool.get().await.unwrap();
    for error in [
        create_user(&mut **client, "alice", wrong, &[])
            .await
            .unwrap_err(),
        create_user(&mut **client, "x y", wrong, &[])
            .await
            .unwrap_err(),
        create_user(&mut **client, "bob", "short", &[])
            .await
            .unwrap_err(),
        set_password(&mut **client, "nobody", wrong)
            .await
            .unwrap_err(),
    ] {
        texts.push(format!("{error} {error:?}"));
    }
    for text in &texts {
        for secret in [
            PASSWORD,
            wrong,
            "short",
            &session.token,
            &session.csrf_token,
            &api.secret,
        ] {
            assert!(!text.contains(secret), "{text}");
        }
    }
    // The key is public and shown.
    assert!(format!("{api:?}").contains(&api.key));
}
