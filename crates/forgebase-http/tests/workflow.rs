//! The workflow routes over HTTP (M8.1, ADR 0015): transition, the list of what a caller may
//! take, the history, the metadata, and what plain submit and cancel and update do on a DocType
//! with a workflow, against a real PostgreSQL.

mod common;

use axum::http::{Method, StatusCode};
use common::{Env, PURCHASE, Reply, get, post, req};
use serde_json::{Value as Json, json};

/// The users: one per case of the authorization matrix, each with an API token (no CSRF).
struct World {
    env: Env,
    admin: String,
    clerk: String,
    boss: String,
    rita: String,
    ada: String,
    sam: String,
    nobody: String,
}

async fn world() -> World {
    let env = Env::with_doctypes(&[PURCHASE]).await;
    for (name, roles) in [
        ("admin", &["Administrator"][..]),
        ("clerk", &["Clerk"]),
        ("boss", &["Manager"]),
        ("rita", &["Reviewer"]),
        ("ada", &["Auditor"]),
        ("sam", &["Own Writer"]),
        ("nobody", &["Other"]),
    ] {
        env.user(name, roles).await;
    }
    World {
        admin: env.api_token("admin").await,
        clerk: env.api_token("clerk").await,
        boss: env.api_token("boss").await,
        rita: env.api_token("rita").await,
        ada: env.api_token("ada").await,
        sam: env.api_token("sam").await,
        nobody: env.api_token("nobody").await,
        env,
    }
}

fn uri(name: &str, tail: &str) -> String {
    format!("/api/v1/resource/purchase/{name}{tail}")
}

impl World {
    async fn insert(&self, token: &str, title: &str) -> Json {
        let reply = self
            .env
            .send(
                post("/api/v1/resource/purchase")
                    .token(token)
                    .json(json!({"title": title})),
            )
            .await;
        assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.text);
        reply.data()
    }

    async fn take(&self, token: &str, doc: &Json, action: &str) -> Reply {
        self.env
            .send(
                post(&uri(doc["name"].as_str().expect("name"), "/transition"))
                    .token(token)
                    .json(json!({"action": action, "version": doc["version"]})),
            )
            .await
    }

    /// Takes the actions as the Administrator.
    async fn walk(&self, doc: Json, actions: &[&str]) -> Json {
        let mut doc = doc;
        for action in actions {
            let reply = self.take(&self.admin, &doc, action).await;
            assert_eq!(reply.status, StatusCode::OK, "{action}: {}", reply.text);
            doc = reply.data();
        }
        doc
    }

    async fn get(&self, token: &str, tail: &str, name: &str) -> Reply {
        self.env.send(get(&uri(name, tail)).token(token)).await
    }
}

fn name(doc: &Json) -> &str {
    doc["name"].as_str().expect("name")
}

#[tokio::test]
async fn a_new_document_is_in_the_initial_state_and_a_transition_returns_it() {
    let w = world().await;
    let doc = w.insert(&w.clerk, "a").await;
    assert_eq!(doc["workflow_state"], "Draft");
    assert_eq!(doc["docstatus"], 0);
    assert_eq!(doc["version"], 1);

    let reply = w.take(&w.clerk, &doc, "Send").await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    let sent = reply.data();
    assert_eq!(sent["workflow_state"], "Pending");
    assert_eq!(sent["docstatus"], 0);
    assert_eq!(sent["version"], 2);
    assert_eq!(sent["name"], doc["name"]);
    // The document that `get` returns is the same.
    assert_eq!(w.get(&w.clerk, "", name(&doc)).await.data(), sent);

    // Approve is 0 to 1: the document is submitted.
    let approved = w.take(&w.boss, &sent, "Approve").await.data();
    assert_eq!(approved["workflow_state"], "Approved");
    assert_eq!(approved["docstatus"], 1);
    let cancelled = w.take(&w.boss, &approved, "Cancel").await.data();
    assert_eq!(
        (&cancelled["workflow_state"], &cancelled["docstatus"]),
        (&json!("Cancelled"), &json!(2))
    );
}

#[tokio::test]
async fn the_transition_route_needs_csrf_like_the_other_changes() {
    let w = world().await;
    let doc = w.insert(&w.clerk, "a").await;
    let session = w.env.login("clerk").await;
    let body = json!({"action": "Send", "version": 1});
    let url = uri(name(&doc), "/transition");
    // A cookie without the CSRF token is refused; with it the transition is taken.
    let reply = w
        .env
        .send(post(&url).cookie(&session).json(body.clone()))
        .await;
    assert_eq!(
        (reply.status, reply.code().as_str()),
        (StatusCode::FORBIDDEN, "csrf_failed")
    );
    let reply = w
        .env
        .send(post(&url).session(&session).json(body.clone()))
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    // No credentials at all.
    let reply = w.env.send(post(&url).json(body)).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    // The body limit and the content type are those of the other routes.
    let reply = w
        .env
        .send(post(&url).token(&w.clerk).raw("text/plain", b"{}"))
        .await;
    assert_eq!(reply.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
}

#[tokio::test]
async fn the_error_codes_of_a_transition() {
    let w = world().await;
    let doc = w.insert(&w.clerk, "a").await;
    let sent = w.take(&w.clerk, &doc, "Send").await.data();

    // 403 for the role without the right, the right without the role, and no read right.
    for (who, why) in [
        (&w.rita, "role without right"),
        (&w.ada, "right without role"),
    ] {
        let reply = w.take(who, &sent, "Approve").await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN, "{why}: {}", reply.text);
        assert_eq!(reply.code(), "permission_denied", "{why}");
    }
    let reply = w.take(&w.nobody, &sent, "Approve").await;
    assert_eq!(
        (reply.status, reply.code().as_str()),
        (StatusCode::FORBIDDEN, "permission_denied")
    );
    // 404: a missing document.
    let reply = w
        .env
        .send(
            post(&uri("missing", "/transition"))
                .token(&w.boss)
                .json(json!({"action": "Approve", "version": 1})),
        )
        .await;
    assert_eq!(
        (reply.status, reply.code().as_str()),
        (StatusCode::NOT_FOUND, "not_found")
    );
    // 404: a DocType that does not exist.
    let reply = w
        .env
        .send(
            post("/api/v1/resource/nothing/x/transition")
                .token(&w.boss)
                .json(json!({"action": "Approve", "version": 1})),
        )
        .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    // 409 unknown_transition: a wrong state and an unknown action.
    for action in ["Send", "Nope"] {
        let reply = w.take(&w.boss, &sent, action).await;
        assert_eq!(
            reply.status,
            StatusCode::CONFLICT,
            "{action}: {}",
            reply.text
        );
        assert_eq!(reply.code(), "unknown_transition");
        assert!(
            reply.json()["error"]["message"]
                .as_str()
                .expect("message")
                .contains(&format!("`{action}`"))
        );
    }
    // 409 conflict: a stale version.
    let mut stale = sent.clone();
    stale["version"] = json!(1);
    let reply = w.take(&w.boss, &stale, "Approve").await;
    assert_eq!(
        (reply.status, reply.code().as_str()),
        (StatusCode::CONFLICT, "conflict")
    );
    // 409 no_workflow: a DocType without one (also for a document that does not exist).
    for target in [
        "/api/v1/resource/bill/x/transition",
        "/api/v1/resource/note/x/transition",
    ] {
        let reply = w
            .env
            .send(
                post(target)
                    .token(&w.admin)
                    .json(json!({"action": "Send", "version": 1})),
            )
            .await;
        assert_eq!(
            (reply.status, reply.code().as_str()),
            (StatusCode::CONFLICT, "no_workflow"),
            "{target}"
        );
    }
    // 422: a hook is not involved here, but the body is checked: 400 for the wrong shape.
    for body in [
        json!({"version": 2}),
        json!({"action": 5, "version": 2}),
        json!({"action": "Approve"}),
        json!({"action": "Approve", "version": "2"}),
        json!({"action": "Approve", "version": 2, "extra": true}),
        json!(["Approve"]),
    ] {
        let reply = w
            .env
            .send(
                post(&uri(name(&sent), "/transition"))
                    .token(&w.boss)
                    .json(body.clone()),
            )
            .await;
        assert_eq!(
            (reply.status, reply.code().as_str()),
            (StatusCode::BAD_REQUEST, "bad_request"),
            "{body}"
        );
    }
    // Nothing was changed by any of it.
    assert_eq!(w.get(&w.boss, "", name(&sent)).await.data(), sent);
}

#[tokio::test]
async fn if_owner_decides_for_the_caller_who_owns_the_document() {
    let w = world().await;
    let mine = w.insert(&w.sam, "mine").await;
    let theirs = w.insert(&w.clerk, "theirs").await;
    // Own Writer may read all, but write and submit only its own documents.
    assert_eq!(w.take(&w.sam, &mine, "Send").await.status, StatusCode::OK);
    let reply = w.take(&w.sam, &theirs, "Send").await;
    assert_eq!(
        (reply.status, reply.code().as_str()),
        (StatusCode::FORBIDDEN, "permission_denied")
    );
}

#[tokio::test]
async fn plain_submit_and_cancel_are_workflow_required_and_edit_roles_apply_to_updates() {
    let w = world().await;
    let doc = w.insert(&w.clerk, "a").await;
    for step in ["submit", "cancel"] {
        let reply = w
            .env
            .send(
                post(&uri(name(&doc), &format!("/{step}")))
                    .token(&w.admin)
                    .json(json!({"version": 1})),
            )
            .await;
        assert_eq!(reply.status, StatusCode::CONFLICT, "{step}: {}", reply.text);
        assert_eq!(reply.code(), "workflow_required");
        assert!(
            reply.json()["error"]["message"]
                .as_str()
                .expect("message")
                .contains("use a transition")
        );
    }
    // A document of a DocType without a workflow is not affected.
    let reply = w
        .env
        .send(
            post("/api/v1/resource/bill")
                .token(&w.admin)
                .json(json!({"title": "b"})),
        )
        .await;
    assert_eq!(reply.status, StatusCode::CREATED);
    assert!(reply.data().get("workflow_state").is_none());
    let bill = reply.data();
    let reply = w
        .env
        .send(
            post(&format!("/api/v1/resource/bill/{}/submit", name(&bill)))
                .token(&w.admin)
                .json(json!({"version": 1})),
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);

    // In Pending only a Manager may update: 403 for a Clerk with the write right.
    let sent = w.take(&w.clerk, &doc, "Send").await.data();
    let patch = |token: &str, version: i64| {
        req(Method::PATCH, &uri(name(&sent), ""))
            .token(token)
            .json(json!({"version": version, "data": {"amount": 5}}))
    };
    let reply = w.env.send(patch(&w.clerk, 2)).await;
    assert_eq!(
        (reply.status, reply.code().as_str()),
        (StatusCode::FORBIDDEN, "permission_denied")
    );
    let reply = w.env.send(patch(&w.boss, 2)).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    assert_eq!(reply.data()["workflow_state"], "Pending");
}

#[tokio::test]
async fn a_client_cannot_set_the_workflow_state() {
    let w = world().await;
    let reply = w
        .env
        .send(
            post("/api/v1/resource/purchase")
                .token(&w.admin)
                .json(json!({"title": "a", "workflow_state": "Approved"})),
        )
        .await;
    assert_eq!(
        (reply.status, reply.code().as_str()),
        (StatusCode::UNPROCESSABLE_ENTITY, "validation_failed"),
        "{}",
        reply.text
    );
    let doc = w.insert(&w.admin, "a").await;
    let reply = w
        .env
        .send(
            req(Method::PATCH, &uri(name(&doc), ""))
                .token(&w.admin)
                .json(json!({"version": 1, "data": {"workflow_state": "Approved"}})),
        )
        .await;
    assert_eq!(
        (reply.status, reply.code().as_str()),
        (StatusCode::UNPROCESSABLE_ENTITY, "validation_failed"),
        "{}",
        reply.text
    );
}

#[tokio::test]
async fn the_list_shows_the_state_and_can_filter_and_sort_by_it() {
    let w = world().await;
    let a = w.insert(&w.admin, "a").await;
    w.insert(&w.admin, "b").await;
    w.walk(a, &["Send"]).await;
    let filters = r#"[{"field":"workflow_state","op":"=","value":"Pending"}]"#;
    let reply = w
        .env
        .send(
            get(&format!(
                "/api/v1/resource/purchase?filters={}&sort=workflow_state:desc",
                urlencode(filters)
            ))
            .token(&w.admin),
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    let items = reply.data();
    assert_eq!(items.as_array().expect("array").len(), 1);
    assert_eq!(items[0]["workflow_state"], "Pending");
    // A DocType without a workflow has no such column.
    let reply = w
        .env
        .send(
            get(&format!(
                "/api/v1/resource/bill?filters={}",
                urlencode(filters)
            ))
            .token(&w.admin),
        )
        .await;
    assert_eq!(
        (reply.status, reply.code().as_str()),
        (StatusCode::BAD_REQUEST, "invalid_query"),
        "{}",
        reply.text
    );
}

fn urlencode(text: &str) -> String {
    text.bytes().map(|b| format!("%{b:02X}")).collect()
}

#[tokio::test]
async fn transitions_lists_what_the_caller_may_take_now() {
    let w = world().await;
    let doc = w.insert(&w.sam, "a").await;
    // Draft: Send for Clerk and Own Writer (the owner); nothing for Manager (not a role of Send).
    let sends = |reply: Reply| {
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
        reply.data()
    };
    let data = sends(w.get(&w.clerk, "/transitions", name(&doc)).await);
    assert_eq!(
        data,
        json!({"state": "Draft", "transitions": [{"action": "Send", "to": "Pending"}]})
    );
    assert_eq!(
        sends(w.get(&w.sam, "/transitions", name(&doc)).await)["transitions"][0]["action"],
        "Send"
    );
    // Administrator may take everything from the state, a Manager has no role of Send.
    assert_eq!(
        sends(w.get(&w.admin, "/transitions", name(&doc)).await)["transitions"]
            .as_array()
            .expect("array")
            .len(),
        1
    );
    assert_eq!(
        sends(w.get(&w.boss, "/transitions", name(&doc)).await)["transitions"],
        json!([])
    );

    // Pending: Reject (Manager) and Approve (Manager, Reviewer, Own Writer).
    let sent = w.take(&w.clerk, &doc, "Send").await;
    assert_eq!(sent.status, StatusCode::OK, "{}", sent.text);
    let actions = |data: &Json| -> Vec<String> {
        data["transitions"]
            .as_array()
            .expect("array")
            .iter()
            .map(|t| t["action"].as_str().expect("action").to_owned())
            .collect()
    };
    assert_eq!(
        actions(&sends(w.get(&w.boss, "/transitions", name(&doc)).await)),
        ["Reject", "Approve"]
    );
    // The role without the right (Reviewer cannot submit), the right without the role
    // (Auditor), and Own Writer on a document it owns but has no write... it owns it, so Approve.
    assert!(actions(&sends(w.get(&w.rita, "/transitions", name(&doc)).await)).is_empty());
    assert!(actions(&sends(w.get(&w.ada, "/transitions", name(&doc)).await)).is_empty());
    assert_eq!(
        actions(&sends(w.get(&w.sam, "/transitions", name(&doc)).await)),
        ["Approve"]
    );
    // Somebody else's document: Own Writer reads it but may not submit it.
    let other = w.insert(&w.clerk, "b").await;
    let w_other = w.take(&w.clerk, &other, "Send").await;
    assert_eq!(w_other.status, StatusCode::OK);
    assert!(actions(&sends(w.get(&w.sam, "/transitions", name(&other)).await)).is_empty());
    // The state is in the answer, and the list follows the state.
    let approved = w.take(&w.boss, &sent.data(), "Approve").await.data();
    let data = sends(w.get(&w.boss, "/transitions", name(&approved)).await);
    assert_eq!(
        data,
        json!({"state": "Approved", "transitions": [{"action": "Cancel", "to": "Cancelled"}]})
    );

    // No read right: 403. A missing document: 404. A DocType without a workflow: 409.
    let reply = w.get(&w.nobody, "/transitions", name(&doc)).await;
    assert_eq!(
        (reply.status, reply.code().as_str()),
        (StatusCode::FORBIDDEN, "permission_denied")
    );
    let reply = w.get(&w.boss, "/transitions", "missing").await;
    assert_eq!(
        (reply.status, reply.code().as_str()),
        (StatusCode::NOT_FOUND, "not_found")
    );
    let reply = w
        .env
        .send(get("/api/v1/resource/bill/x/transitions").token(&w.admin))
        .await;
    assert_eq!(
        (reply.status, reply.code().as_str()),
        (StatusCode::CONFLICT, "no_workflow")
    );
    // Query parameters are refused like on every other route.
    let reply = w
        .env
        .send(get(&format!("{}?x=1", uri(name(&doc), "/transitions"))).token(&w.boss))
        .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    // GET only.
    let reply = w
        .env
        .send(
            post(&uri(name(&doc), "/transitions"))
                .token(&w.boss)
                .json(json!({})),
        )
        .await;
    assert_eq!(reply.status, StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test]
async fn history_is_oldest_first_and_needs_read_access() {
    let w = world().await;
    let doc = w.insert(&w.clerk, "a").await;
    let sent = w.take(&w.clerk, &doc, "Send").await.data();
    let rejected = w.take(&w.boss, &sent, "Reject").await.data();
    w.take(&w.clerk, &rejected, "Send").await;

    let reply = w.get(&w.boss, "/history", name(&doc)).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    let history = reply.data()["history"].clone();
    let rows: Vec<(String, String, String, String)> = history
        .as_array()
        .expect("array")
        .iter()
        .map(|e| {
            let text = |k: &str| {
                e[k].as_str()
                    .unwrap_or_else(|| panic!("{k} in {e}"))
                    .to_owned()
            };
            (text("action"), text("from"), text("to"), text("actor"))
        })
        .collect();
    let row = |a: &str, f: &str, t: &str, who: &str| {
        (a.to_owned(), f.to_owned(), t.to_owned(), who.to_owned())
    };
    assert_eq!(
        rows,
        [
            row("Send", "Draft", "Pending", "clerk"),
            row("Reject", "Pending", "Draft", "boss"),
            row("Send", "Draft", "Pending", "clerk"),
        ]
    );
    for entry in history.as_array().expect("array") {
        let mut keys: Vec<_> = entry.as_object().expect("object").keys().cloned().collect();
        keys.sort();
        assert_eq!(keys, ["action", "actor", "at", "from", "to"]);
        let at = entry["at"].as_str().expect("at");
        assert!(
            at.ends_with('Z') && at.contains('T'),
            "RFC 3339 in UTC: {at}"
        );
    }
    // The same read rule as get: no read right 403, a missing document 404, no workflow 409.
    let reply = w.get(&w.nobody, "/history", name(&doc)).await;
    assert_eq!(
        (reply.status, reply.code().as_str()),
        (StatusCode::FORBIDDEN, "permission_denied")
    );
    let reply = w.get(&w.boss, "/history", "missing").await;
    assert_eq!(
        (reply.status, reply.code().as_str()),
        (StatusCode::NOT_FOUND, "not_found")
    );
    let reply = w
        .env
        .send(get("/api/v1/resource/bill/x/history").token(&w.admin))
        .await;
    assert_eq!(
        (reply.status, reply.code().as_str()),
        (StatusCode::CONFLICT, "no_workflow")
    );
    // A document with no transitions has an empty history.
    let fresh = w.insert(&w.clerk, "b").await;
    assert_eq!(
        w.get(&w.boss, "/history", name(&fresh)).await.data(),
        json!({"history": []})
    );
}

#[tokio::test]
async fn the_meta_has_the_workflow_without_any_role() {
    let w = world().await;
    let reply = w
        .env
        .send(get("/api/v1/meta/purchase").token(&w.clerk))
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    let data = reply.data();
    assert_eq!(
        data["workflow"],
        json!({
            "states": [
                {"name": "Draft", "docstatus": 0, "style": "neutral"},
                {"name": "Pending", "docstatus": 0, "style": "warning"},
                {"name": "Approved", "docstatus": 1, "style": "success"},
                {"name": "Cancelled", "docstatus": 2, "style": "danger"},
            ],
            "transitions": [
                {"action": "Send", "from": "Draft", "to": "Pending"},
                {"action": "Reject", "from": "Pending", "to": "Draft"},
                {"action": "Approve", "from": "Pending", "to": "Approved"},
                {"action": "Cancel", "from": "Approved", "to": "Cancelled"},
            ]
        })
    );
    // Neither the roles of the transitions nor the edit roles nor the rules are sent.
    for role in [
        "Manager",
        "Reviewer",
        "Own Writer",
        "Auditor",
        "edit_roles",
        "roles",
        "permissions",
    ] {
        assert!(!reply.text.contains(role), "{role} in {}", reply.text);
    }
    // A DocType without a workflow has `"workflow": null`, and nothing else is new.
    let reply = w.env.send(get("/api/v1/meta/bill").token(&w.admin)).await;
    assert_eq!(reply.data()["workflow"], Json::Null);
    let mut keys: Vec<_> = reply
        .data()
        .as_object()
        .expect("object")
        .keys()
        .cloned()
        .collect();
    keys.sort();
    assert_eq!(
        keys,
        [
            "app",
            "fields",
            "label",
            "list_fields",
            "name",
            "naming",
            "rights",
            "submittable",
            "title_field",
            "workflow"
        ]
    );
}
