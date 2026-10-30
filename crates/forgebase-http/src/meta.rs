//! The metadata routes for the desk: what a user may see of the DocTypes (`GET /api/v1/meta` and
//! `GET /api/v1/meta/{doctype}`). Only metadata, never documents; the caller's rights come from
//! the one evaluator of `forgebase-core`, and the DocType's `[[permissions]]` rules (which name
//! roles) are never sent.

use axum::extract::rejection::PathRejection;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Response;
use forgebase_core::meta::{DocField, DocType, FieldType, Naming};
use forgebase_core::permission::{Access, Caller, Right, evaluate};
use forgebase_db::DbError;
use serde_json::{Map, Value as Json, json};

use crate::AppState;
use crate::auth::Authenticated;
use crate::error::ApiError;
use crate::routes::{Reply, data, doctype};

/// `"all"`, `"own"` (only documents the caller owns) or `"none"`.
fn access_name(access: Access) -> &'static str {
    match access {
        Access::All => "all",
        Access::OwnerOnly => "own",
        Access::Denied => "none",
    }
}

/// The caller's rights on `doctype`, one entry per [`Right`]. Submit, cancel and amend are
/// `"none"` for a DocType that is not submittable (the API refuses them with `not_submittable`
/// even for an administrator, so the desk must not offer them).
fn rights(doctype: &DocType, caller: &Caller) -> Json {
    let mut map = Map::new();
    for right in Right::ALL {
        let lifecycle = matches!(right, Right::Submit | Right::Cancel | Right::Amend);
        let access = if lifecycle && !doctype.submittable {
            Access::Denied
        } else {
            evaluate(doctype, caller, right)
        };
        map.insert(right.name().to_owned(), json!(access_name(access)));
    }
    Json::Object(map)
}

fn naming(naming: &Naming) -> String {
    match naming {
        Naming::Random => "random".to_owned(),
        Naming::Field(field) => format!("field:{field}"),
        Naming::Series(_) => "series".to_owned(),
    }
}

/// One field: name, label, type, flags (`required`, `unique`, `allow_on_submit`, `read_only`) and
/// the type's parameters.
fn field(field: &DocField) -> Map<String, Json> {
    let mut map = Map::new();
    map.insert("name".to_owned(), json!(field.name));
    map.insert("label".to_owned(), json!(field.label));
    map.insert("type".to_owned(), json!(field.field_type.name()));
    map.insert("required".to_owned(), json!(field.required));
    map.insert("unique".to_owned(), json!(field.unique));
    map.insert("allow_on_submit".to_owned(), json!(field.allow_on_submit));
    map.insert("read_only".to_owned(), json!(field.read_only));
    match &field.field_type {
        FieldType::Data { max_length } => {
            map.insert("max_length".to_owned(), json!(max_length));
        }
        FieldType::Decimal { precision, scale } => {
            map.insert("precision".to_owned(), json!(precision));
            map.insert("scale".to_owned(), json!(scale));
        }
        FieldType::Select { options } => {
            map.insert("options".to_owned(), json!(options));
        }
        FieldType::Link { target } | FieldType::Table { target } => {
            map.insert("target".to_owned(), json!(target));
        }
        FieldType::Text
        | FieldType::Int
        | FieldType::Check
        | FieldType::Date
        | FieldType::Datetime => {}
    }
    map
}

/// The workflow for the desk (M8.1): the states with their docstatus and badge style, and the
/// transitions. The roles of the transitions and the `edit_roles` of the states are **never**
/// sent, like the `[[permissions]]` rules: `GET .../transitions` says what the caller may do.
fn workflow(doctype: &DocType) -> Json {
    let Some(workflow) = &doctype.workflow else {
        return Json::Null;
    };
    let states: Vec<Json> = workflow
        .states
        .iter()
        .map(
            |s| json!({"name": s.name, "docstatus": s.docstatus.as_i16(), "style": s.style.name()}),
        )
        .collect();
    let transitions: Vec<Json> = workflow
        .transitions
        .iter()
        .map(|t| json!({"action": t.action, "from": t.from, "to": t.to}))
        .collect();
    json!({"states": states, "transitions": transitions})
}

/// A field of the DocType; a Table field also carries the field list of its child DocType
/// (`fields`), so the desk needs one request per DocType.
fn field_with_children(state: &AppState, f: &DocField) -> Json {
    let mut map = field(f);
    if let FieldType::Table { target } = &f.field_type {
        // The registry was checked at startup, so the child exists.
        let rows: Vec<Json> = state
            .registry()
            .get(target)
            .map(|child| {
                child
                    .fields
                    .iter()
                    .map(|c| Json::Object(field(c)))
                    .collect()
            })
            .unwrap_or_default();
        map.insert("fields".to_owned(), Json::Array(rows));
    }
    Json::Object(map)
}

/// `GET /api/v1/meta`: the non-child DocTypes the caller can read, sorted by name, each with
/// `name`, `label`, `app` (the owning app, `null` when the registry records none),
/// `submittable` and the caller's `rights`.
pub async fn list(State(state): State<AppState>, authenticated: Authenticated) -> Reply {
    let caller = authenticated.caller();
    let items: Vec<Json> = state
        .registry()
        .iter()
        .filter(|d| !d.is_child && evaluate(d, &caller, Right::Read) != Access::Denied)
        .map(|d| {
            json!({
                "name": d.name,
                "label": d.label,
                "app": state.registry().owner(&d.name),
                "submittable": d.submittable,
                "rights": rights(d, &caller),
            })
        })
        .collect();
    Ok(data(StatusCode::OK, Json::Array(items)))
}

/// `GET /api/v1/meta/{doctype}`: the UI metadata of one DocType. An unknown or child DocType is
/// 404; one the caller cannot read is 403 `permission_denied` (DocType level, like the resource
/// routes).
pub async fn read(
    State(state): State<AppState>,
    authenticated: Authenticated,
    path: Result<Path<String>, PathRejection>,
) -> Result<Response, ApiError> {
    let Path(name) = path?;
    let doctype = doctype(&state, &name)?;
    let caller = authenticated.caller();
    if evaluate(doctype, &caller, Right::Read) == Access::Denied {
        return Err(DbError::PermissionDenied {
            doctype: doctype.name.clone(),
            right: Right::Read,
        }
        .into());
    }
    let fields: Vec<Json> = doctype
        .fields
        .iter()
        .map(|f| field_with_children(&state, f))
        .collect();
    Ok(data(
        StatusCode::OK,
        json!({
            "name": doctype.name,
            "label": doctype.label,
            "app": state.registry().owner(&doctype.name),
            "naming": naming(&doctype.naming),
            "submittable": doctype.submittable,
            "title_field": doctype.title_field,
            "list_fields": doctype.list_fields,
            "fields": fields,
            "rights": rights(doctype, &caller),
            "workflow": workflow(doctype),
        }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doctype() -> DocType {
        DocType::from_toml_str(
            r#"
name = "invoice"
label = "Invoice"
naming = { series = "INV-.YYYY.-.#####" }
submittable = true
[[fields]]
name = "amount"
label = "Amount"
type = "decimal"
precision = 12
scale = 2
[[fields]]
name = "state"
type = "select"
options = ["open", "paid"]
allow_on_submit = true
[[fields]]
name = "customer"
type = "link"
target = "customer"
required = true
"#,
        )
        .expect("valid DocType")
    }

    #[test]
    fn naming_kinds() {
        let invoice = doctype();
        assert_eq!(naming(&invoice.naming), "series");
        assert_eq!(naming(&Naming::Random), "random");
        assert_eq!(naming(&Naming::Field("code".to_owned())), "field:code");
    }

    #[test]
    fn field_parameters_by_type() {
        let invoice = doctype();
        let amount = Json::Object(field(&invoice.fields[0]));
        assert_eq!(
            amount,
            json!({"name": "amount", "label": "Amount", "type": "decimal", "required": false,
                   "unique": false, "allow_on_submit": false, "read_only": false,
                   "precision": 12, "scale": 2})
        );
        let state = Json::Object(field(&invoice.fields[1]));
        assert_eq!(state["options"], json!(["open", "paid"]));
        assert_eq!(state["allow_on_submit"], true);
        let customer = Json::Object(field(&invoice.fields[2]));
        assert_eq!(customer["target"], "customer");
        assert_eq!(customer["required"], true);
    }
}
