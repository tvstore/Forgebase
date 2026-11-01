//! Fixtures at startup (M9.4, ADR 0022): reading the files of the apps, checking them and turning
//! each record into what `forgebase_db` applies at `migrate`.
//!
//! A problem is collected with the other app problems ([`StartupError`](crate::StartupError)) and
//! names the app, the file and the record. Only what can be decided without the database is
//! checked here: the file, the DocType and the rules for it, the identity of every record and
//! that every value has the JSON type of its field. Whether a value is valid (required fields,
//! lengths, options, links) is decided when the record is applied, by the document pipeline,
//! because a `before_insert` or `validate` hook may fill or change fields.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use forgebase_core::document::Document;
use forgebase_core::meta::{DocType, FieldType, Naming, Registry};
use forgebase_db::FixtureRecord;
use forgebase_http::json::input_from_json;
use serde_json::Value as Json;
use sha2::{Digest, Sha256};

use crate::app::App;
use crate::compose::StartupError;

/// One fixture record, owned: what `forgebase_db` gets a [`FixtureRecord`] view of.
#[derive(Debug, Clone)]
pub(crate) struct OwnedRecord {
    pub(crate) doctype: String,
    pub(crate) file: String,
    pub(crate) index: usize,
    pub(crate) name: String,
    pub(crate) checksum: String,
    pub(crate) document: Document,
}

impl OwnedRecord {
    pub(crate) fn view(&self) -> FixtureRecord<'_> {
        FixtureRecord {
            doctype: &self.doctype,
            file: &self.file,
            index: self.index,
            name: &self.name,
            checksum: &self.checksum,
            document: &self.document,
        }
    }
}

/// The checksum of a record: SHA-256, as lower-case hex, of its canonical JSON ([`canonical`]).
/// Whitespace and the order of keys in the file do not change it; a changed value does.
pub(crate) fn checksum(record: &Json) -> String {
    let mut text = String::with_capacity(64);
    for byte in Sha256::digest(canonical(record).as_bytes()) {
        let _ = write!(text, "{byte:02x}");
    }
    text
}

/// The canonical JSON of a value. What is guaranteed, and by whom:
///
/// - **Keys.** Written here, not by `serde_json`'s serializer: object keys are sorted by their
///   bytes, whatever order the map iterates in, so `preserve_order` cannot change the text.
/// - **Numbers.** The parser is `serde_json`'s, whose features are chosen by the app's
///   dependency graph (`arbitrary_precision`, `float_roundtrip`) and change how some numbers
///   parse or print. [`number_problem`] therefore **refuses at startup** every number that could
///   differ (negative zero, an integer outside `i64`/`u64`, a non-integer with more than 15
///   significant digits or a decimal exponent beyond 22), so a number that gets here has one
///   value and one text in every build: an integer as its digits, any other number as the
///   shortest `f64` text.
///
/// Stored checksums must never change for these reasons: the next `migrate` would update every
/// record and overwrite the site's edits. For accepted values the text equals `serde_json`'s
/// default compact output, so checksums computed before this writer existed are unchanged.
pub(crate) fn canonical(value: &Json) -> String {
    let mut out = String::new();
    write_canonical(value, &mut out);
    out
}

fn write_canonical(value: &Json, out: &mut String) {
    match value {
        Json::Null => out.push_str("null"),
        Json::Bool(true) => out.push_str("true"),
        Json::Bool(false) => out.push_str("false"),
        Json::Number(n) => {
            if let Some(i) = n.as_i64() {
                let _ = write!(out, "{i}");
            } else if let Some(u) = n.as_u64() {
                let _ = write!(out, "{u}");
            } else {
                let f = n.as_f64().unwrap_or(f64::NAN);
                // A float always has a form (JSON has no NaN or infinity); `serde_json` writes a
                // primitive `f64` the same way in every feature setting.
                out.push_str(&serde_json::to_string(&f).unwrap_or_else(|_| "null".to_owned()));
            }
        }
        Json::String(text) => out.push_str(&serde_json::to_string(text).expect("a string")),
        Json::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical(item, out);
            }
            out.push(']');
        }
        Json::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (i, key) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&serde_json::to_string(key).expect("a string"));
                out.push(':');
                write_canonical(&map[key.as_str()], out);
            }
            out.push('}');
        }
    }
}

/// Why a number cannot be part of a fixture record, or `None`: it must have the same value and the
/// same canonical text in every build of the app (see [`canonical`]). Accepted: an integer in the
/// `i64` or `u64` range (not negative zero), and a non-integer, or an integer written as a float,
/// (below 2^63) whose shortest `f64` text has at most 15 significant digits and a decimal exponent within
/// ±22, which is exact in every parsing mode (the fast path of `serde_json`). Refused: negative
/// zero (`-0` is `-0.0` by default and `0` with `arbitrary_precision`), a number that is not
/// finite, an integer beyond the `i64`/`u64` range (it goes through `f64`), and any number with
/// more digits or a larger exponent (`float_roundtrip` and `arbitrary_precision` can parse
/// those to another `f64`). A number that is out of range for `f64` (`1e400`) is a parse error
/// by default and refused here when it parses under `arbitrary_precision`.
fn number_problem(n: &serde_json::Number) -> Option<String> {
    let text = n.to_string();
    let advice = "write it with at most 15 significant digits, or as a string";
    // Negative zero: a sign and no non-zero digit before any exponent.
    let mantissa = text.split(['e', 'E']).next().unwrap_or("");
    if mantissa.starts_with('-') && !mantissa.bytes().any(|b| (b'1'..=b'9').contains(&b)) {
        return Some(format!(
            "number `{text}` is a negative zero, which cannot be represented the same way in every build; write 0"
        ));
    }
    if n.as_i64().is_some() || n.as_u64().is_some() {
        return None;
    }
    let Some(f) = n.as_f64().filter(|f| f.is_finite()) else {
        return Some(format!("number `{text}` is out of range"));
    };
    // An integer beyond the 64-bit range goes through `f64`, whatever it is written like.
    if f.fract() == 0.0 && f.abs() >= 9_223_372_036_854_775_808.0 {
        return Some(format!(
            "number `{text}` is an integer outside the 64-bit range and cannot be represented the same way in every build; write it as a string"
        ));
    }
    // `{:e}` is the shortest text that reads back as `f`: `d.ddde<exp>`.
    let shortest = format!("{f:e}");
    let (digits, exponent) = shortest.split_once('e').unwrap_or((&shortest, "0"));
    let significant = digits.bytes().filter(u8::is_ascii_digit).count();
    let exponent: i32 = exponent.parse().unwrap_or(i32::MAX);
    if significant > 15 || !(-22..=22).contains(&exponent) {
        return Some(format!(
            "number `{text}` cannot be represented the same way in every build; {advice}"
        ));
    }
    None
}

/// The problems of every number in `value`, rows of Table fields included.
fn number_problems(value: &Json, problems: &mut Vec<String>) {
    match value {
        Json::Number(n) => problems.extend(number_problem(n)),
        Json::Array(items) => items.iter().for_each(|i| number_problems(i, problems)),
        Json::Object(map) => map.values().for_each(|v| number_problems(v, problems)),
        _ => {}
    }
}

/// Checks the fixtures of `apps` and returns the records of each app, in app order (the result
/// has one entry per app, empty for an app without fixtures). `reach` has, per app, the apps it
/// depends on, directly or not.
pub(crate) fn check(
    apps: &[App],
    reach: &BTreeMap<String, BTreeSet<String>>,
    registry: &Registry,
    errors: &mut StartupError,
) -> Vec<Vec<OwnedRecord>> {
    // (doctype, name) -> the app that has the record.
    let mut identities: BTreeMap<(String, String), String> = BTreeMap::new();
    let mut result = Vec::new();
    for app in apps {
        let mut records = Vec::new();
        // DocType -> the file of this app that has its records.
        let mut files_of: BTreeMap<String, &str> = BTreeMap::new();
        for file in &app.fixtures {
            let problem = |text: String| format!("app `{}`: {}: {text}", app.name, file.path());
            let parsed = match parse(file.json()) {
                Ok(parsed) => parsed,
                Err(text) => {
                    errors.apps.push(problem(text));
                    continue;
                }
            };
            let doctype = match usable_doctype(&app.name, &parsed.doctype, reach, registry) {
                Ok(doctype) => doctype,
                Err(text) => {
                    errors.apps.push(problem(text));
                    continue;
                }
            };
            if let Some(first) = files_of.insert(doctype.name.clone(), file.path()) {
                errors.apps.push(problem(format!(
                    "doctype `{}` already has fixtures in {first}; use one file per DocType",
                    doctype.name
                )));
                continue;
            }
            let Naming::Field(naming_field) = &doctype.naming else {
                unreachable!("usable_doctype only returns DocTypes with field naming");
            };
            let mut seen: BTreeSet<String> = BTreeSet::new();
            for (position, json) in parsed.records.into_iter().enumerate() {
                let index = position + 1;
                let record_problem = |name: Option<&str>, text: String| match name {
                    Some(name) => problem(format!("record {index} (`{name}`): {text}")),
                    None => problem(format!("record {index}: {text}")),
                };
                let Json::Object(object) = &json else {
                    errors
                        .apps
                        .push(record_problem(None, "must be a JSON object".to_owned()));
                    continue;
                };
                let name = match object.get(naming_field) {
                    Some(Json::String(name)) if !name.trim().is_empty() => name.clone(),
                    _ => {
                        errors.apps.push(record_problem(
                            None,
                            format!(
                                "must have a non-empty text value for `{naming_field}`, the field that names a `{}` document",
                                doctype.name
                            ),
                        ));
                        continue;
                    }
                };
                if !seen.insert(name.clone()) {
                    errors.apps.push(record_problem(
                        Some(&name),
                        "appears twice in this app".to_owned(),
                    ));
                    continue;
                }
                if let Some(other) = identities.get(&(doctype.name.clone(), name.clone())) {
                    errors.apps.push(record_problem(
                        Some(&name),
                        format!(
                            "`{} {name}` is also a fixture of app `{other}`; a record belongs to one app",
                            doctype.name
                        ),
                    ));
                    continue;
                }
                let mut numbers = Vec::new();
                number_problems(&json, &mut numbers);
                if !numbers.is_empty() {
                    errors
                        .apps
                        .push(record_problem(Some(&name), numbers.join("; ")));
                    continue;
                }
                if let Some(text) = field_problems(registry, doctype, object) {
                    errors.apps.push(record_problem(Some(&name), text));
                    continue;
                }
                let input = match input_from_json(registry, doctype, json.clone()) {
                    Ok(input) => input,
                    Err(e) => {
                        errors.apps.push(record_problem(Some(&name), e.to_string()));
                        continue;
                    }
                };
                identities.insert((doctype.name.clone(), name.clone()), app.name.clone());
                records.push(OwnedRecord {
                    doctype: doctype.name.clone(),
                    file: file.path().to_owned(),
                    index,
                    name,
                    checksum: checksum(&json),
                    document: Document {
                        name: None,
                        values: input.values,
                        tables: input.tables,
                    },
                });
            }
        }
        result.push(records);
    }
    result
}

/// The content of a fixture file.
struct Parsed {
    doctype: String,
    records: Vec<Json>,
}

fn parse(text: &str) -> Result<Parsed, String> {
    let json: Json = serde_json::from_str(text).map_err(|e| format!("invalid JSON: {e}"))?;
    let Json::Object(mut object) = json else {
        return Err("must be a JSON object: {\"doctype\": \"...\", \"records\": [...]}".to_owned());
    };
    let doctype = match object.remove("doctype") {
        Some(Json::String(doctype)) => doctype,
        _ => return Err("needs a `doctype` (the name of the DocType, as text)".to_owned()),
    };
    let records = match object.remove("records") {
        Some(Json::Array(records)) => records,
        _ => return Err("needs `records` (a list of objects)".to_owned()),
    };
    if let Some(key) = object.keys().next() {
        return Err(format!(
            "unknown key `{key}`; a fixture file has only `doctype` and `records`"
        ));
    }
    Ok(Parsed { doctype, records })
}

/// The DocType a fixture file is for, if the rules allow fixtures for it.
fn usable_doctype<'r>(
    app: &str,
    name: &str,
    reach: &BTreeMap<String, BTreeSet<String>>,
    registry: &'r Registry,
) -> Result<&'r DocType, String> {
    let Some(doctype) = registry.get(name) else {
        return Err(format!("doctype `{name}` is not defined by any app"));
    };
    let owner = registry.owner(name).unwrap_or_default();
    if owner != app && !reach.get(app).is_some_and(|uses| uses.contains(owner)) {
        return Err(format!(
            "doctype `{name}` belongs to app `{owner}`, which app `{app}` does not depend on"
        ));
    }
    if doctype.is_child {
        return Err(format!(
            "doctype `{name}` is a child DocType: its rows belong to a parent document; ship the parent's fixture with the rows in its table field"
        ));
    }
    if doctype.submittable {
        return Err(format!(
            "doctype `{name}` is submittable: a fixture update must not fight the document lifecycle (draft, submitted, cancelled)"
        ));
    }
    if doctype.workflow.is_some() {
        return Err(format!(
            "doctype `{name}` has a workflow: a fixture update must not fight the workflow, which owns the state of its documents"
        ));
    }
    match &doctype.naming {
        Naming::Field(_) => Ok(doctype),
        Naming::Random => Err(format!(
            "doctype `{name}` uses random naming: a fixture record needs a name that is the same on every site; use field naming (naming = {{ field = \"...\" }})"
        )),
        Naming::Series(_) => Err(format!(
            "doctype `{name}` uses a naming series: a fixture record needs a name that is the same on every site; use field naming (naming = {{ field = \"...\" }})"
        )),
    }
}

/// What is wrong with the keys of a record and of the rows of its Table fields: a key that is not
/// a field of the DocType (of the child DocType, for a row), and a key the pipeline would refuse
/// from a caller (a `read_only` field). `None` when there is nothing.
fn field_problems(
    registry: &Registry,
    doctype: &DocType,
    record: &serde_json::Map<String, Json>,
) -> Option<String> {
    let mut problems = Vec::new();
    keys_problems(doctype, record, "", &mut problems);
    for (key, value) in record {
        let Some(FieldType::Table { target }) = doctype.field(key).map(|f| &f.field_type) else {
            continue;
        };
        let (Some(child), Json::Array(rows)) = (registry.get(target), value) else {
            continue;
        };
        for (i, row) in rows.iter().enumerate() {
            if let Json::Object(row) = row {
                let prefix = format!("`{key}` row {}: ", i + 1);
                keys_problems(child, row, &prefix, &mut problems);
            }
        }
    }
    (!problems.is_empty()).then(|| problems.join("; "))
}

fn keys_problems(
    doctype: &DocType,
    object: &serde_json::Map<String, Json>,
    prefix: &str,
    problems: &mut Vec<String>,
) {
    for key in object.keys() {
        match doctype.field(key) {
            None => problems.push(format!(
                "{prefix}`{key}` is not a field of `{}`",
                doctype.name
            )),
            Some(field) if field.read_only => problems.push(format!(
                "{prefix}`{key}` is a read_only field: only hooks may write it"
            )),
            Some(_) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn json(text: &str) -> Json {
        serde_json::from_str(text).unwrap()
    }

    #[test]
    fn whitespace_and_key_order_do_not_change_the_checksum() {
        let a = checksum(&json(
            r#"{"title": "Retail", "rows": [{"b": 1, "a": 2}], "x": null}"#,
        ));
        let b = checksum(&json(
            "{\n  \"x\": null,\n  \"rows\": [ { \"a\": 2,\n \"b\": 1 } ],\n  \"title\": \"Retail\"\n}",
        ));
        assert_eq!(a, b);
        assert_eq!(a.len(), 64);
        assert!(
            a.bytes()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
    }

    #[test]
    fn a_changed_value_changes_the_checksum() {
        let base = checksum(&json(r#"{"title": "Retail", "n": 1}"#));
        assert_ne!(base, checksum(&json(r#"{"title": "Retail ", "n": 1}"#)));
        assert_ne!(base, checksum(&json(r#"{"title": "Retail", "n": 2}"#)));
        assert_ne!(base, checksum(&json(r#"{"title": "Retail"}"#)));
        assert_ne!(
            base,
            checksum(&json(r#"{"title": "Retail", "n": 1, "extra": null}"#))
        );
        // Row order is a value: it is the order of the rows.
        assert_ne!(
            checksum(&json(r#"{"rows": [{"a": 1}, {"a": 2}]}"#)),
            checksum(&json(r#"{"rows": [{"a": 2}, {"a": 1}]}"#))
        );
    }

    #[test]
    fn the_canonical_form_does_not_depend_on_the_order_of_a_map() {
        // Keys inserted out of order, nested objects and arrays: the map's own order (insertion
        // order under `preserve_order`, sorted otherwise) must not matter.
        let mut inner = serde_json::Map::new();
        inner.insert("z".into(), json("1"));
        inner.insert("y".into(), json(r#"[{"q": 2, "p": 1}, 3]"#));
        let mut out_of_order = serde_json::Map::new();
        out_of_order.insert("b".into(), Json::Object(inner));
        out_of_order.insert("c".into(), json("[true, null, \"x\\ny\", 1.5, -7]"));
        out_of_order.insert("a".into(), json("3"));
        let mut sorted_inner = serde_json::Map::new();
        sorted_inner.insert("y".into(), json(r#"[{"p": 1, "q": 2}, 3]"#));
        sorted_inner.insert("z".into(), json("1"));
        let mut sorted = serde_json::Map::new();
        sorted.insert("a".into(), json("3"));
        sorted.insert("b".into(), Json::Object(sorted_inner));
        sorted.insert("c".into(), json("[true, null, \"x\\ny\", 1.5, -7]"));
        let (left, right) = (Json::Object(out_of_order), Json::Object(sorted));
        let expected = r#"{"a":3,"b":{"y":[{"p":1,"q":2},3],"z":1},"c":[true,null,"x\ny",1.5,-7]}"#;
        assert_eq!(canonical(&left), expected);
        assert_eq!(canonical(&right), expected);
        assert_eq!(checksum(&left), checksum(&right));
        // The same text as serde_json's default form for a sorted value.
        assert_eq!(serde_json::to_string(&right).unwrap(), expected);
    }

    #[test]
    fn numbers_have_one_form() {
        assert_eq!(
            canonical(&json(
                "[0, 12, -12, 18446744073709551615, 1.0, 2.5e3, 1e-7]"
            )),
            "[0,12,-12,18446744073709551615,1.0,2500.0,1e-7]"
        );
    }

    #[test]
    fn the_checksum_is_a_known_sha256() {
        // sha256 of `{"a":1}`.
        assert_eq!(
            checksum(&json(r#"{ "a" : 1 }"#)),
            "015abd7f5cc57a2dd94b7590f04ad8084273905ee33ec5cebeae62276a97f862"
        );
    }

    fn number(text: &str) -> serde_json::Number {
        match json(text) {
            Json::Number(n) => n,
            other => panic!("{text} is {other}"),
        }
    }

    #[test]
    fn numbers_that_could_parse_differently_are_refused() {
        for bad in [
            // negative zero, integer or float
            "-0",
            "-0.0",
            "-0e5",
            // 16 or more significant digits
            "9007199254740993.0",
            "0.1234567890123456",
            "100000000000000000000000.5",
            "2.2250738585072011e-308",
            // beyond the exponent range of the exact fast path
            "1e23",
            "1e-23",
            "1.5e100",
            // integers outside i64 and u64
            "18446744073709551616",
            "-9223372036854775809",
            "100000000000000000000",
            "1e19",
        ] {
            assert!(number_problem(&number(bad)).is_some(), "{bad} was accepted");
        }
        let problem = number_problem(&number("0.1234567890123456")).unwrap();
        assert!(
            problem.contains("cannot be represented the same way in every build")
                && problem.contains("15 significant digits, or as a string"),
            "{problem}"
        );
    }

    #[test]
    fn numbers_that_parse_the_same_everywhere_are_accepted() {
        for good in [
            "0",
            "0.0",
            "0.1",
            "12.50",
            "1e2",
            "1e18",
            "1e-22",
            "2.5e3",
            "123456789012345",
            "-7",
            "-1.5",
            "18446744073709551615",
            "-9223372036854775808",
            "9223372036854775807",
        ] {
            assert_eq!(number_problem(&number(good)), None, "{good} was refused");
        }
    }

    #[test]
    fn an_out_of_range_number_is_refused() {
        // By default this does not even parse; with `arbitrary_precision` it parses and
        // `number_problem` refuses it (not finite as an f64).
        assert!(
            serde_json::from_str::<Json>("1e400").is_err() || {
                let Json::Number(n) = json("1e400") else {
                    unreachable!()
                };
                number_problem(&n).is_some()
            }
        );
    }

    // ---- the startup checks -------------------------------------------------------------------

    use crate::app::{DocTypeFile, FixtureFile};
    use crate::compose::compose;

    const ITEM: &str = "name = \"item\"\nlabel = \"Item\"\nnaming = { field = \"code\" }\n\
        [[fields]]\nname = \"code\"\ntype = \"data\"\nrequired = true\nunique = true\n\
        [[fields]]\nname = \"title\"\ntype = \"data\"\n\
        [[fields]]\nname = \"stamp\"\ntype = \"data\"\nread_only = true\n";
    const RANDOM: &str =
        "name = \"random\"\nlabel = \"R\"\n[[fields]]\nname = \"title\"\ntype = \"data\"\n";
    const SERIES: &str = "name = \"series\"\nlabel = \"S\"\nnaming = { series = \"N-.####\" }\n[[fields]]\nname = \"title\"\ntype = \"data\"\n";
    const LOCKED: &str = "name = \"locked\"\nlabel = \"L\"\nsubmittable = true\n[[fields]]\nname = \"title\"\ntype = \"data\"\n";
    const FLOW: &str = "name = \"flow\"\nlabel = \"F\"\nnaming = { field = \"code\" }\n\
        [[fields]]\nname = \"code\"\ntype = \"data\"\nrequired = true\nunique = true\n\
        [workflow]\n[[workflow.states]]\nname = \"Open\"\ndocstatus = 0\n\
        [[workflow.states]]\nname = \"Done\"\ndocstatus = 0\n\
        [[workflow.transitions]]\naction = \"Finish\"\nfrom = \"Open\"\nto = \"Done\"\nroles = [\"Admin\"]\n";
    const PARENT: &str = "name = \"parent\"\nlabel = \"P\"\nnaming = { field = \"code\" }\n\
        [[fields]]\nname = \"code\"\ntype = \"data\"\nrequired = true\nunique = true\n\
        [[fields]]\nname = \"rows\"\ntype = \"table\"\ntarget = \"row\"\n\
        [[fields]]\nname = \"size\"\ntype = \"int\"\n";
    const ROW: &str = "name = \"row\"\nlabel = \"Row\"\nis_child = true\n[[fields]]\nname = \"qty\"\ntype = \"int\"\n";

    fn leak<T>(value: T) -> &'static T {
        Box::leak(Box::new(value))
    }

    /// The app `core` with every DocType above.
    fn core() -> App {
        App::new("core").doctypes(
            const {
                &[
                    DocTypeFile::new("doctypes/item.toml", ITEM),
                    DocTypeFile::new("doctypes/random.toml", RANDOM),
                    DocTypeFile::new("doctypes/series.toml", SERIES),
                    DocTypeFile::new("doctypes/locked.toml", LOCKED),
                    DocTypeFile::new("doctypes/flow.toml", FLOW),
                    DocTypeFile::new("doctypes/parent.toml", PARENT),
                    DocTypeFile::new("doctypes/row.toml", ROW),
                ]
            },
        )
    }

    /// An app `shop` that depends on `core` and has these fixture files (`path`, `json`).
    fn shop(files: &[(&'static str, &'static str)]) -> App {
        let files: &'static [FixtureFile] = leak(
            files
                .iter()
                .map(|(path, json)| FixtureFile::new(path, json))
                .collect::<Vec<_>>(),
        );
        App::new("shop").depends_on(&["core"]).fixtures(files)
    }

    /// The startup error of `core` and `shop`, as text.
    fn problems(files: &[(&'static str, &'static str)]) -> String {
        match compose(&[core(), shop(files)], None) {
            Ok(_) => String::new(),
            Err(e) => e.to_string(),
        }
    }

    #[track_caller]
    fn refused(files: &[(&'static str, &'static str)], expected: &[&str]) {
        let text = problems(files);
        assert!(!text.is_empty(), "no problem found for {files:?}");
        for part in expected {
            assert!(text.contains(part), "`{part}` is not in:\n{text}");
        }
        assert!(
            text.contains("app `shop`: fixtures/f.json"),
            "the problem does not name app and file:\n{text}"
        );
    }

    #[test]
    fn valid_fixtures_compose_and_are_in_file_and_record_order() {
        let composed = compose(
            &[
                core(),
                shop(&[(
                    "fixtures/f.json",
                    r#"{"doctype": "item", "records": [{"code": "b"}, {"code": "a", "title": "A"}]}"#,
                ), (
                    "fixtures/p.json",
                    r#"{"doctype": "parent", "records": [{"code": "p", "size": 3, "rows": [{"qty": 1}]}]}"#,
                )]),
            ],
            None,
        )
        .expect("composes");
        let records: Vec<_> = composed
            .fixture_records("shop")
            .iter()
            .map(|r| (r.doctype.as_str(), r.name.as_str(), r.index))
            .collect();
        assert_eq!(
            records,
            [("item", "b", 1), ("item", "a", 2), ("parent", "p", 1)]
        );
        assert!(composed.fixture_records("core").is_empty());
        assert_eq!(
            composed.fixture_records("shop")[2].document.tables["rows"].len(),
            1
        );
    }

    #[test]
    fn bad_json_is_refused() {
        refused(&[("fixtures/f.json", "{\"doctype\": ")], &["invalid JSON"]);
        refused(&[("fixtures/f.json", "[]")], &["must be a JSON object"]);
        refused(
            &[("fixtures/f.json", r#"{"records": []}"#)],
            &["needs a `doctype`"],
        );
        refused(
            &[("fixtures/f.json", r#"{"doctype": "item"}"#)],
            &["needs `records`"],
        );
        refused(
            &[(
                "fixtures/f.json",
                r#"{"doctype": "item", "records": [], "extra": 1}"#,
            )],
            &["unknown key `extra`"],
        );
    }

    #[test]
    fn an_unknown_doctype_is_refused() {
        refused(
            &[(
                "fixtures/f.json",
                r#"{"doctype": "nothing", "records": []}"#,
            )],
            &["doctype `nothing` is not defined by any app"],
        );
    }

    #[test]
    fn a_doctype_of_an_app_that_is_not_a_dependency_is_refused() {
        let stranger = App::new("stranger")
            .doctypes(const { &[DocTypeFile::new("doctypes/item2.toml", ITEM_OF_STRANGER)] });
        let shop = App::new("shop").fixtures(leak(vec![FixtureFile::new(
            "fixtures/f.json",
            r#"{"doctype": "item2", "records": [{"code": "a"}]}"#,
        )]));
        let text = compose(&[stranger, shop], None)
            .err()
            .expect("refused")
            .to_string();
        assert!(
            text.contains("app `shop`: fixtures/f.json: doctype `item2` belongs to app `stranger`, which app `shop` does not depend on"),
            "{text}"
        );
    }

    const ITEM_OF_STRANGER: &str = "name = \"item2\"\nlabel = \"I\"\nnaming = { field = \"code\" }\n\
        [[fields]]\nname = \"code\"\ntype = \"data\"\nrequired = true\nunique = true\n";

    #[test]
    fn random_and_series_naming_are_refused_with_the_reason() {
        for name in ["random", "series"] {
            let json: &'static str = leak(format!(
                r#"{{"doctype": "{name}", "records": [{{"title": "x"}}]}}"#
            ));
            refused(
                &[("fixtures/f.json", json)],
                &["the same on every site", "use field naming"],
            );
        }
        refused(
            &[("fixtures/f.json", r#"{"doctype": "random", "records": []}"#)],
            &["doctype `random` uses random naming"],
        );
        refused(
            &[("fixtures/f.json", r#"{"doctype": "series", "records": []}"#)],
            &["doctype `series` uses a naming series"],
        );
    }

    #[test]
    fn submittable_workflow_and_child_doctypes_are_refused() {
        refused(
            &[("fixtures/f.json", r#"{"doctype": "locked", "records": []}"#)],
            &["doctype `locked` is submittable", "document lifecycle"],
        );
        refused(
            &[("fixtures/f.json", r#"{"doctype": "flow", "records": []}"#)],
            &["doctype `flow` has a workflow", "owns the state"],
        );
        refused(
            &[("fixtures/f.json", r#"{"doctype": "row", "records": []}"#)],
            &["doctype `row` is a child DocType", "table field"],
        );
    }

    #[test]
    fn a_record_must_be_an_object_with_the_naming_field() {
        refused(
            &[(
                "fixtures/f.json",
                r#"{"doctype": "item", "records": [{"code": "a"}, 5]}"#,
            )],
            &["record 2: must be a JSON object"],
        );
        refused(
            &[(
                "fixtures/f.json",
                r#"{"doctype": "item", "records": [{"title": "no code"}]}"#,
            )],
            &["record 1: must have a non-empty text value for `code`"],
        );
        for bad in [
            r#"{"code": ""}"#,
            r#"{"code": "  "}"#,
            r#"{"code": null}"#,
            r#"{"code": 5}"#,
        ] {
            let json: &'static str = leak(format!(r#"{{"doctype": "item", "records": [{bad}]}}"#));
            refused(
                &[("fixtures/f.json", json)],
                &["non-empty text value for `code`"],
            );
        }
    }

    #[test]
    fn a_record_with_unknown_read_only_or_mistyped_values_is_refused() {
        refused(
            &[(
                "fixtures/f.json",
                r#"{"doctype": "item", "records": [{"code": "a", "nope": 1}]}"#,
            )],
            &["record 1 (`a`)", "`nope` is not a field of `item`"],
        );
        refused(
            &[(
                "fixtures/f.json",
                r#"{"doctype": "item", "records": [{"code": "a", "stamp": "x"}]}"#,
            )],
            &["`stamp` is a read_only field"],
        );
        refused(
            &[(
                "fixtures/f.json",
                r#"{"doctype": "parent", "records": [{"code": "a", "size": "big"}]}"#,
            )],
            &["record 1 (`a`)", "size"],
        );
    }

    #[test]
    fn the_rows_of_a_table_field_are_checked_against_the_child_doctype() {
        refused(
            &[(
                "fixtures/f.json",
                r#"{"doctype": "parent", "records": [{"code": "a", "rows": [{"qty": 1}, {"qty": 2, "nope": 3}]}]}"#,
            )],
            &[
                "record 1 (`a`)",
                "`rows` row 2: `nope` is not a field of `row`",
            ],
        );
    }

    #[test]
    fn a_number_that_could_parse_differently_is_a_startup_error_also_in_rows() {
        refused(
            &[(
                "fixtures/f.json",
                r#"{"doctype": "parent", "records": [{"code": "a", "size": -0}]}"#,
            )],
            &["record 1 (`a`)", "negative zero"],
        );
        refused(
            &[(
                "fixtures/f.json",
                r#"{"doctype": "parent", "records": [{"code": "a", "rows": [{"qty": 1}, {"qty": 0.1234567890123456}]}]}"#,
            )],
            &[
                "record 1 (`a`)",
                "cannot be represented the same way in every build",
            ],
        );
        refused(
            &[(
                "fixtures/f.json",
                r#"{"doctype": "parent", "records": [{"code": "a", "size": 18446744073709551616}]}"#,
            )],
            &["record 1 (`a`)", "number `"],
        );
    }

    #[test]
    fn a_duplicate_inside_an_app_is_refused() {
        refused(
            &[(
                "fixtures/f.json",
                r#"{"doctype": "item", "records": [{"code": "a"}, {"code": "a", "title": "again"}]}"#,
            )],
            &["record 2 (`a`): appears twice in this app"],
        );
    }

    #[test]
    fn two_files_for_one_doctype_are_refused() {
        let text = problems(&[
            (
                "fixtures/first.json",
                r#"{"doctype": "item", "records": [{"code": "a"}]}"#,
            ),
            (
                "fixtures/f.json",
                r#"{"doctype": "item", "records": [{"code": "b"}]}"#,
            ),
        ]);
        assert!(
            text.contains("app `shop`: fixtures/f.json: doctype `item` already has fixtures in fixtures/first.json; use one file per DocType"),
            "{text}"
        );
    }

    #[test]
    fn a_duplicate_across_apps_names_both_apps() {
        let record = r#"{"doctype": "item", "records": [{"code": "a"}]}"#;
        let other = App::new("other")
            .depends_on(&["core"])
            .fixtures(leak(vec![FixtureFile::new("fixtures/f.json", record)]));
        let text = compose(&[core(), shop(&[("fixtures/f.json", record)]), other], None)
            .err()
            .expect("refused")
            .to_string();
        assert!(
            text.contains("app `other`: fixtures/f.json: record 1 (`a`): `item a` is also a fixture of app `shop`; a record belongs to one app"),
            "{text}"
        );
    }
}
