//! Field permission levels over HTTP (M10.2, ADR 0024): what each caller reads and writes, the
//! metadata API (hidden fields left out, `read_only` for fields that can be read but not
//! written), `422 field_not_permitted`, and that a hidden field is unknown everywhere: the
//! answer is compared, byte for byte, with the answer of a server whose DocType does not have
//! the field at all.

mod common;

use axum::http::{Method, StatusCode};
use common::{Env, Reply, Session, get, post, req};
use serde_json::{Value as Json, json};

/// `credit_limit` (level 1), `vault_code` (level 2, nobody has a rule), a table `ledger` at
/// level 1. `Agent` is a plain clerk, `Boss` reads and writes level 1, `Auditor` reads level 1.
fn account(with_levels: bool) -> String {
    let hidden = if with_levels {
        "[[fields]]\nname = \"credit_limit\"\ntype = \"int\"\nperm_level = 1\n\
         [[fields]]\nname = \"vault_code\"\ntype = \"data\"\nperm_level = 2\n\
         [[fields]]\nname = \"ledger\"\ntype = \"table\"\ntarget = \"acct_line\"\nperm_level = 1\n"
    } else {
        ""
    };
    format!(
        r#"
name = "account"
label = "Account"
[[fields]]
name = "title"
type = "data"
[[fields]]
name = "region"
type = "data"
[[fields]]
name = "revenue"
type = "int"
{hidden}
[[permissions]]
role = "Agent"
read = true
write = true
create = true
delete = true
export = true
import = true
[[permissions]]
role = "Boss"
read = true
write = true
create = true
export = true
import = true
[[permissions]]
role = "Boss"
level = 1
read = true
write = true
[[permissions]]
role = "Auditor"
read = true
[[permissions]]
role = "Auditor"
level = 1
read = true
"#
    )
}

const LEDGER_LINE: &str = r#"
name = "acct_line"
label = "Acct Line"
is_child = true
[[fields]]
name = "qty"
type = "int"
"#;

struct Site {
    env: Env,
    admin: Session,
    agent: Session,
    boss: Session,
    auditor: Session,
}

async fn site(with_levels: bool) -> Site {
    let account = account(with_levels);
    // Without the table there is no child DocType either.
    let doctypes: Vec<&str> = if with_levels {
        vec![&account, LEDGER_LINE]
    } else {
        vec![&account]
    };
    let env = Env::with_doctypes(&doctypes).await;
    env.user("root", &["Administrator"]).await;
    env.user("agent", &["Agent"]).await;
    env.user("boss", &["Boss"]).await;
    // An auditor that also writes level 0: it can read level 1 but not write it.
    env.user("auditor", &["Auditor", "Agent"]).await;
    let (admin, agent, boss, auditor) = (
        env.login("root").await,
        env.login("agent").await,
        env.login("boss").await,
        env.login("auditor").await,
    );
    Site {
        env,
        admin,
        agent,
        boss,
        auditor,
    }
}

impl Site {
    async fn get(&self, who: &Session, path: &str) -> Reply {
        self.env.send(get(path).session(who)).await
    }

    async fn post(&self, who: &Session, path: &str, body: Json) -> Reply {
        self.env.send(post(path).session(who).json(body)).await
    }

    async fn patch(&self, who: &Session, name: &str, version: i64, data: Json) -> Reply {
        self.env
            .send(
                req(Method::PATCH, &format!("/api/v1/resource/account/{name}"))
                    .session(who)
                    .json(json!({"version": version, "data": data})),
            )
            .await
    }

    /// The administrator's account with every level filled.
    async fn full(&self) -> (String, i64) {
        let reply = self
            .post(
                &self.admin,
                "/api/v1/resource/account",
                json!({
                    "title": "Acme", "region": "north", "revenue": 10,
                    "credit_limit": 500, "vault_code": "1234",
                    "ledger": [{"qty": 1}, {"qty": 2}]
                }),
            )
            .await;
        assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.text);
        (
            reply.data()["name"].as_str().expect("name").to_owned(),
            reply.data()["version"].as_i64().expect("version"),
        )
    }
}

fn enc(text: &str) -> String {
    let mut out = String::new();
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn keys(value: &Json) -> Vec<String> {
    value
        .as_object()
        .expect("an object")
        .keys()
        .cloned()
        .collect()
}

#[tokio::test]
async fn a_user_without_the_level_sees_no_trace_of_the_field() {
    let s = site(true).await;
    let (name, _) = s.full().await;

    // Get and list.
    let got = s
        .get(&s.agent, &format!("/api/v1/resource/account/{name}"))
        .await;
    assert_eq!(got.status, StatusCode::OK, "{}", got.text);
    let keys = keys(&got.data());
    for hidden in ["credit_limit", "vault_code", "ledger"] {
        assert!(!keys.contains(&hidden.to_owned()), "{hidden} in {keys:?}");
    }
    assert!(keys.contains(&"title".to_owned()));
    let list = s.get(&s.agent, "/api/v1/resource/account").await;
    assert!(
        !list.text.contains("credit_limit") && !list.text.contains("500"),
        "{}",
        list.text
    );

    // Summary.
    let summary = s
        .get(
            &s.agent,
            "/api/v1/summary/account?group_by=region&aggregates=sum:revenue",
        )
        .await;
    assert_eq!(summary.status, StatusCode::OK, "{}", summary.text);

    // Export: CSV columns and JSON keys.
    let csv = s.get(&s.agent, "/api/v1/export/account?format=csv").await;
    assert_eq!(csv.status, StatusCode::OK, "{}", csv.text);
    assert_eq!(
        csv.text.lines().next(),
        Some("name,title,region,revenue"),
        "{}",
        csv.text
    );
    let json = s.get(&s.agent, "/api/v1/export/account?format=json").await;
    assert!(
        !json.text.contains("credit_limit") && !json.text.contains("ledger"),
        "{}",
        json.text
    );

    // Meta.
    let meta = s.get(&s.agent, "/api/v1/meta/account").await;
    let meta_data = meta.data();
    let fields: Vec<&str> = meta_data["fields"]
        .as_array()
        .expect("fields")
        .iter()
        .map(|f| f["name"].as_str().expect("name"))
        .collect();
    assert_eq!(fields, ["title", "region", "revenue"]);
    assert_eq!(meta.header("cache-control").as_deref(), Some("no-store"));
    assert!(
        meta.data()["fields"]
            .as_array()
            .expect("fields")
            .iter()
            .all(|f| f["read_only"] == false)
    );

    // The administrator sees everything, level 2 included.
    let all = s
        .get(&s.admin, &format!("/api/v1/resource/account/{name}"))
        .await;
    assert_eq!(all.data()["credit_limit"], 500);
    assert_eq!(all.data()["vault_code"], "1234");
    assert_eq!(all.data()["ledger"].as_array().expect("rows").len(), 2);
    let meta = s.get(&s.admin, "/api/v1/meta/account").await;
    assert_eq!(meta.data()["fields"].as_array().expect("fields").len(), 6);
}

#[tokio::test]
async fn a_boss_reads_and_writes_level_one_and_never_sees_level_two() {
    let s = site(true).await;
    let (name, version) = s.full().await;
    let got = s
        .get(&s.boss, &format!("/api/v1/resource/account/{name}"))
        .await;
    assert_eq!(got.data()["credit_limit"], 500);
    assert_eq!(got.data()["ledger"].as_array().expect("rows").len(), 2);
    assert!(got.data().get("vault_code").is_none());
    let updated = s
        .patch(
            &s.boss,
            &name,
            version,
            json!({"credit_limit": 900, "ledger": [{"qty": 5}]}),
        )
        .await;
    assert_eq!(updated.status, StatusCode::OK, "{}", updated.text);
    assert_eq!(updated.data()["credit_limit"], 900);
    assert!(updated.data().get("vault_code").is_none());
    // The meta of a boss: level 1 fields are writable, level 2 is not there.
    let meta = s.get(&s.boss, "/api/v1/meta/account").await;
    let fields = meta.data()["fields"].as_array().expect("fields").clone();
    let names: Vec<&str> = fields
        .iter()
        .map(|f| f["name"].as_str().expect("name"))
        .collect();
    assert_eq!(
        names,
        ["title", "region", "revenue", "credit_limit", "ledger"]
    );
    assert!(fields.iter().all(|f| f["read_only"] == false));
    // Level 2 is unknown, the list filter refused as an unknown field.
    let filters = enc(&json!([{"field": "vault_code", "op": "=", "value": "1"}]).to_string());
    let refused = s
        .get(
            &s.boss,
            &format!("/api/v1/resource/account?filters={filters}"),
        )
        .await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST);
    assert_eq!(refused.code(), "invalid_query");
}

#[tokio::test]
async fn a_reader_of_level_one_gets_field_not_permitted_and_read_only_in_the_meta() {
    let s = site(true).await;
    let (name, version) = s.full().await;
    let got = s
        .get(&s.auditor, &format!("/api/v1/resource/account/{name}"))
        .await;
    assert_eq!(got.data()["credit_limit"], 500);

    let refused = s
        .patch(&s.auditor, &name, version, json!({"credit_limit": 1}))
        .await;
    assert_eq!(
        refused.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "{}",
        refused.text
    );
    assert_eq!(refused.code(), "field_not_permitted");
    assert_eq!(
        refused.json()["error"]["details"],
        json!(["field `credit_limit` cannot be changed with your permissions"])
    );
    let refused = s
        .patch(&s.auditor, &name, version, json!({"ledger": []}))
        .await;
    assert_eq!(refused.code(), "field_not_permitted");
    let created = s
        .post(
            &s.auditor,
            "/api/v1/resource/account",
            json!({"title": "x", "credit_limit": 3}),
        )
        .await;
    assert_eq!(created.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(created.code(), "field_not_permitted");

    // Sending the stored value back is allowed, with another change.
    let ok = s
        .patch(
            &s.auditor,
            &name,
            version,
            json!({"credit_limit": 500, "title": "Acme 2", "ledger": [{"qty": 1}, {"qty": 2}]}),
        )
        .await;
    assert_eq!(ok.status, StatusCode::OK, "{}", ok.text);

    // The meta says read_only for the fields it cannot write, and only for those.
    let meta = s.get(&s.auditor, "/api/v1/meta/account").await;
    let fields = meta.data()["fields"].as_array().expect("fields").clone();
    let read_only: Vec<(&str, bool)> = fields
        .iter()
        .map(|f| {
            (
                f["name"].as_str().expect("name"),
                f["read_only"].as_bool().expect("flag"),
            )
        })
        .collect();
    assert_eq!(
        read_only,
        [
            ("title", false),
            ("region", false),
            ("revenue", false),
            ("credit_limit", true),
            ("ledger", true)
        ]
    );
}

/// Both servers answer the same request; the answers are equal in status and in text.
async fn same(hidden: &Site, plain: &Site, who: &str, request: impl Fn(&Site) -> common::Req) {
    let pick = |site: &Site| match who {
        "agent" => site.agent.clone(),
        other => panic!("{other}"),
    };
    let a = hidden
        .env
        .send(request(hidden).session(&pick(hidden)))
        .await;
    let b = plain.env.send(request(plain).session(&pick(plain))).await;
    assert_eq!(a.status, b.status, "{} vs {}", a.text, b.text);
    assert_eq!(a.text, b.text);
}

#[tokio::test]
async fn a_hidden_field_is_refused_exactly_like_a_field_that_does_not_exist() {
    let hidden = site(true).await;
    let plain = site(false).await;
    for field in ["credit_limit", "vault_code", "ledger"] {
        let filters = enc(&json!([{"field": field, "op": "=", "value": 1}]).to_string());
        same(&hidden, &plain, "agent", |_| {
            get(&format!("/api/v1/resource/account?filters={filters}"))
        })
        .await;
        same(&hidden, &plain, "agent", |_| {
            get(&format!("/api/v1/resource/account?sort={field}:desc"))
        })
        .await;
        same(&hidden, &plain, "agent", |_| {
            get(&format!("/api/v1/summary/account?group_by={field}"))
        })
        .await;
        same(&hidden, &plain, "agent", |_| {
            get(&format!(
                "/api/v1/summary/account?group_by=region&aggregates=sum:{field}"
            ))
        })
        .await;
        same(&hidden, &plain, "agent", |_| {
            get(&format!(
                "/api/v1/export/account?format=csv&fields=title,{field}"
            ))
        })
        .await;
        same(&hidden, &plain, "agent", |_| {
            get(&format!(
                "/api/v1/export/account?format=json&fields={field}"
            ))
        })
        .await;
        same(&hidden, &plain, "agent", |_| {
            get(&format!(
                "/api/v1/export/account?format=csv&filters={filters}"
            ))
        })
        .await;
        // A value of the wrong type is not even looked at: no type error names the field.
        for value in [
            json!("not a number"),
            json!(1),
            json!(null),
            json!([{"qty": "x"}]),
        ] {
            let body = json!({"title": "t", field: value});
            same(&hidden, &plain, "agent", |_| {
                post("/api/v1/resource/account").json(body.clone())
            })
            .await;
        }
        // A saved view.
        let view = json!({
            "label": "v",
            "definition": {"filters": [{"field": field, "op": "=", "value": 1}], "sort": [], "columns": []}
        });
        same(&hidden, &plain, "agent", |_| {
            post("/api/v1/views/account").json(view.clone())
        })
        .await;
        let columns = json!({"label": "c", "definition": {"filters": [], "sort": [field], "columns": [field]}});
        same(&hidden, &plain, "agent", |_| {
            post("/api/v1/views/account").json(columns.clone())
        })
        .await;
    }
    // Update: also with the stored value, and with a wrong type.
    let (name, version) = hidden.full().await;
    let (plain_name, plain_version) = {
        let reply = plain
            .post(
                &plain.admin,
                "/api/v1/resource/account",
                json!({"title": "Acme", "region": "north", "revenue": 10}),
            )
            .await;
        (
            reply.data()["name"].as_str().expect("name").to_owned(),
            reply.data()["version"].as_i64().expect("version"),
        )
    };
    for value in [json!(500), json!("zzz"), json!(null)] {
        let a = hidden
            .patch(
                &hidden.agent,
                &name,
                version,
                json!({"credit_limit": value}),
            )
            .await;
        let b = plain
            .patch(
                &plain.agent,
                &plain_name,
                plain_version,
                json!({"credit_limit": value}),
            )
            .await;
        assert_eq!(a.status, b.status);
        assert_eq!(a.text, b.text);
        assert_eq!(a.code(), "validation_failed");
    }
}

#[tokio::test]
async fn import_columns_of_hidden_fields_are_unknown() {
    let hidden = site(true).await;
    let plain = site(false).await;
    let csv = "title,credit_limit\nA,5\n";
    for site in [&hidden, &plain] {
        let reply = site
            .env
            .send(
                post("/api/v1/import/account?format=csv&mode=insert")
                    .session(&site.agent)
                    .raw("text/csv", csv.as_bytes()),
            )
            .await;
        assert_eq!(
            reply.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{}",
            reply.text
        );
        assert_eq!(reply.code(), "invalid_import");
    }
    let a = hidden
        .env
        .send(
            post("/api/v1/import/account?format=csv&mode=insert")
                .session(&hidden.agent)
                .raw("text/csv", csv.as_bytes()),
        )
        .await;
    let b = plain
        .env
        .send(
            post("/api/v1/import/account?format=csv&mode=insert")
                .session(&plain.agent)
                .raw("text/csv", csv.as_bytes()),
        )
        .await;
    assert_eq!(a.text, b.text);
    // JSON rows: the row error is the one of an unknown field.
    let rows = json!([{"title": "A", "credit_limit": 5}]).to_string();
    let a = hidden
        .env
        .send(
            post("/api/v1/import/account?format=json&mode=insert")
                .session(&hidden.agent)
                .raw("application/json", rows.as_bytes()),
        )
        .await;
    let b = plain
        .env
        .send(
            post("/api/v1/import/account?format=json&mode=insert")
                .session(&plain.agent)
                .raw("application/json", rows.as_bytes()),
        )
        .await;
    assert_eq!(a.status, StatusCode::UNPROCESSABLE_ENTITY, "{}", a.text);
    assert_eq!(a.text, b.text);
    // A boss imports the column.
    let ok = hidden
        .env
        .send(
            post("/api/v1/import/account?format=csv&mode=insert")
                .session(&hidden.boss)
                .raw("text/csv", csv.as_bytes()),
        )
        .await;
    assert_eq!(ok.status, StatusCode::OK, "{}", ok.text);
    // And the reader of level 1 only cannot: not permitted, as a row error.
    let denied = hidden
        .env
        .send(
            post("/api/v1/import/account?format=csv&mode=insert")
                .session(&hidden.auditor)
                .raw("text/csv", csv.as_bytes()),
        )
        .await;
    assert_eq!(
        denied.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "{}",
        denied.text
    );
    assert!(
        denied
            .text
            .contains("cannot be changed with your permissions"),
        "{}",
        denied.text
    );
}
