//! Query reports (M8.4, ADR 0016): the definition of an app-defined SQL report, and the checks
//! that need no database.
//!
//! A report is a SQL statement the app compiled in (trusted code, like an app migration), the
//! roles that may run it, typed parameters and typed result columns. This module is pure data and
//! rules: it never runs SQL. `forgebase-db` runs a report (read-only, with a timeout and a row
//! limit) and `forgebase-http` serves it.
//!
//! The SQL parameters are **positional**: `$1..$n` are the declared parameters in order. A
//! parameter that is absent and not required has no value (SQL `NULL`).

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use crate::document::ValidationError;
use crate::ident::validate_doctype_name;
use crate::meta::DEFAULT_MAX_LENGTH;
use crate::permission::check_role_name;
use crate::value::Value;

/// The statement timeout of a report that sets none.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
/// The longest statement timeout a report may ask for.
pub const MAX_TIMEOUT: Duration = Duration::from_secs(300);
/// The row limit of a report that sets none.
pub const DEFAULT_MAX_ROWS: u32 = 10_000;
/// The largest row limit a report may ask for.
pub const MAX_MAX_ROWS: u32 = 100_000;
/// The most parameters or columns a report may have (a PostgreSQL statement has at most 65 535
/// parameters; a report this wide is a mistake).
pub const MAX_PARAMS_AND_COLUMNS: usize = 64;

/// The kind of a report parameter, which decides how a value is checked and bound.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParamKind {
    /// Text of at most 140 characters.
    Data,
    /// A 64-bit integer.
    Int,
    /// A fixed-point number.
    Decimal,
    /// A calendar date.
    Date,
    /// A boolean.
    Check,
    /// The `name` of a document of `doctype`. Not checked for existence: it is only bound.
    Link {
        /// The target DocType.
        doctype: String,
    },
    /// One of a fixed list of strings.
    Select {
        /// The allowed values.
        options: Vec<String>,
    },
}

impl ParamKind {
    /// The name used in the API: `data`, `int`, `decimal`, `date`, `check`, `link`, `select`.
    pub fn name(&self) -> &'static str {
        match self {
            ParamKind::Data => "data",
            ParamKind::Int => "int",
            ParamKind::Decimal => "decimal",
            ParamKind::Date => "date",
            ParamKind::Check => "check",
            ParamKind::Link { .. } => "link",
            ParamKind::Select { .. } => "select",
        }
    }

    /// The kind of [`Value`] a parameter of this kind takes.
    fn expected(&self) -> &'static str {
        match self {
            ParamKind::Data | ParamKind::Link { .. } | ParamKind::Select { .. } => "text",
            ParamKind::Int => "int",
            ParamKind::Decimal => "decimal",
            ParamKind::Date => "date",
            ParamKind::Check => "bool",
        }
    }
}

/// The kind of a result column. The SQL type must fit it; `forgebase migrate` checks that.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ColumnKind {
    /// Text (`text` or `varchar`).
    Data,
    /// An integer (`int2`, `int4` or `int8`).
    Int,
    /// A fixed-point number (`numeric`).
    Decimal,
    /// A calendar date (`date`).
    Date,
    /// A point in time (`timestamptz`).
    Datetime,
    /// A boolean (`bool`).
    Check,
    /// The `name` of a document of `doctype` (`text` or `varchar`).
    Link {
        /// The target DocType.
        doctype: String,
    },
}

impl ColumnKind {
    /// The name used in the API: `data`, `int`, `decimal`, `date`, `datetime`, `check`, `link`.
    pub fn name(&self) -> &'static str {
        match self {
            ColumnKind::Data => "data",
            ColumnKind::Int => "int",
            ColumnKind::Decimal => "decimal",
            ColumnKind::Date => "date",
            ColumnKind::Datetime => "datetime",
            ColumnKind::Check => "check",
            ColumnKind::Link { .. } => "link",
        }
    }
}

/// One declared parameter of a report.
#[derive(Debug, Clone, PartialEq)]
pub struct ReportParam {
    /// The name: the key in the request. Same rule as a DocType name.
    pub name: String,
    /// The label the desk shows.
    pub label: String,
    /// The kind.
    pub kind: ParamKind,
    /// A value must be given (a `default` counts).
    pub required: bool,
    /// The value used when the request does not mention the parameter. An explicit `null` is
    /// not replaced.
    pub default: Option<Value>,
}

/// One declared result column of a report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportColumn {
    /// The name: the key in the API's column list. Same rule as a DocType name.
    pub name: String,
    /// The label the desk shows.
    pub label: String,
    /// The kind.
    pub kind: ColumnKind,
}

/// A report: SQL, roles, parameters and columns. Plain data with public fields, like
/// [`Document`](crate::document::Document); check it with [`ReportDef::problems`].
#[derive(Debug, Clone, PartialEq)]
pub struct ReportDef {
    /// The app that defines it.
    pub app: String,
    /// The name, unique within the app. Same rule as a DocType name.
    pub name: String,
    /// The label the desk shows.
    pub label: String,
    /// The statement, with `$1..$n` for the parameters. No trailing `;`.
    pub sql: String,
    /// The roles that may run it. Never empty in a valid report.
    pub roles: Vec<String>,
    /// The parameters, in the order of `$1..$n`.
    pub params: Vec<ReportParam>,
    /// The result columns, in the order the statement returns them.
    pub columns: Vec<ReportColumn>,
    /// The statement timeout.
    pub timeout: Duration,
    /// The most rows returned; one more row than this only sets `truncated`.
    pub max_rows: u32,
}

impl ReportDef {
    /// `<app>.<name>`: how the report is known in the API and the desk.
    pub fn full_name(&self) -> String {
        format!("{}.{}", self.app, self.name)
    }

    /// What is wrong with the definition itself, one message per problem. Whether a Link
    /// DocType exists is checked by the caller that has the registry
    /// ([`ReportDef::link_targets`]).
    pub fn problems(&self) -> Vec<String> {
        let mut problems = Vec::new();
        if let Err(reason) = validate_doctype_name(&self.name) {
            problems.push(format!("name {:?} {reason}", self.name));
        }
        if self.sql.trim().is_empty() {
            problems.push("the SQL is empty".to_owned());
        }
        if self.label.trim().is_empty() {
            problems.push("the label is empty".to_owned());
        }
        if self.roles.is_empty() {
            problems.push(
                "no roles: name the roles that may run it with .roles(&[..]); there is no \"any user\" report"
                    .to_owned(),
            );
        }
        for role in &self.roles {
            if let Err(reason) = check_role_name(role) {
                problems.push(format!("invalid role {role:?}: {reason}"));
            }
        }
        if self.columns.is_empty() {
            problems.push("no columns: declare the result columns with .column(..)".to_owned());
        }
        if self.params.len() > MAX_PARAMS_AND_COLUMNS {
            problems.push(format!(
                "{} parameters; at most {MAX_PARAMS_AND_COLUMNS}",
                self.params.len()
            ));
        }
        if self.columns.len() > MAX_PARAMS_AND_COLUMNS {
            problems.push(format!(
                "{} columns; at most {MAX_PARAMS_AND_COLUMNS}",
                self.columns.len()
            ));
        }
        if self.timeout.is_zero() || self.timeout > MAX_TIMEOUT {
            problems.push(format!(
                "timeout {:?} must be more than zero and at most {:?}",
                self.timeout, MAX_TIMEOUT
            ));
        }
        if self.max_rows == 0 || self.max_rows > MAX_MAX_ROWS {
            problems.push(format!(
                "max_rows {} must be 1 to {MAX_MAX_ROWS}",
                self.max_rows
            ));
        }
        let mut seen = BTreeSet::new();
        for param in &self.params {
            let name = &param.name;
            if let Err(reason) = validate_doctype_name(name) {
                problems.push(format!("param name {name:?} {reason}"));
            } else if !seen.insert(name.as_str()) {
                problems.push(format!("param `{name}` is declared more than once"));
            }
            if let ParamKind::Select { options } = &param.kind {
                let mut unique = BTreeSet::new();
                if options.is_empty() || options.iter().any(|o| o.trim().is_empty()) {
                    problems.push(format!(
                        "param `{name}`: a select needs options, none of them empty"
                    ));
                } else if !options.iter().all(|o| unique.insert(o.as_str())) {
                    problems.push(format!("param `{name}`: select options must be unique"));
                }
            }
            if let Some(default) = &param.default {
                let mut errors = Vec::new();
                check_param(param, default, &mut errors);
                for error in errors {
                    problems.push(format!("param `{name}`: invalid default value: {error}"));
                }
            }
        }
        let mut seen = BTreeSet::new();
        for column in &self.columns {
            let name = &column.name;
            if let Err(reason) = validate_doctype_name(name) {
                problems.push(format!("column name {name:?} {reason}"));
            } else if !seen.insert(name.as_str()) {
                problems.push(format!("column `{name}` is declared more than once"));
            }
        }
        problems
    }

    /// The DocTypes that Link params and Link columns name, with what names them (for
    /// messages): `("customer", "param `customer`")`.
    pub fn link_targets(&self) -> Vec<(&str, String)> {
        let params = self.params.iter().filter_map(|p| match &p.kind {
            ParamKind::Link { doctype } => Some((doctype.as_str(), format!("param `{}`", p.name))),
            _ => None,
        });
        let columns = self.columns.iter().filter_map(|c| match &c.kind {
            ColumnKind::Link { doctype } => {
                Some((doctype.as_str(), format!("column `{}`", c.name)))
            }
            _ => None,
        });
        params.chain(columns).collect()
    }

    /// Checks the values a caller gave and returns the value of every declared parameter, in
    /// declared order (`Value::Null` for a parameter without a value), ready to be bound as
    /// `$1..$n`.
    ///
    /// Every problem is reported, naming the parameter: a key that is not a declared parameter,
    /// a missing required parameter (an empty or blank text counts as missing), a value of the
    /// wrong kind, a text with a NUL character or longer than 140 characters (Data, Link), a
    /// Select value that is not an option. A parameter that is absent gets its default. Link
    /// values are not checked against the documents.
    pub fn bind_params(
        &self,
        given: &BTreeMap<String, Value>,
    ) -> Result<Vec<Value>, Vec<ValidationError>> {
        let mut errors = Vec::new();
        for key in given.keys() {
            if !self.params.iter().any(|p| &p.name == key) {
                errors.push(ValidationError::UnknownField { field: key.clone() });
            }
        }
        let mut values = Vec::with_capacity(self.params.len());
        for param in &self.params {
            let value = match given.get(&param.name) {
                Some(value) => value.clone(),
                None => param.default.clone().unwrap_or(Value::Null),
            };
            check_param(param, &value, &mut errors);
            values.push(value);
        }
        if errors.is_empty() {
            Ok(values)
        } else {
            Err(errors)
        }
    }
}

/// Checks one value against its parameter, like the same kind of document field.
fn check_param(param: &ReportParam, value: &Value, errors: &mut Vec<ValidationError>) {
    let field = || param.name.clone();
    if matches!(value, Value::Null) {
        if param.required {
            errors.push(ValidationError::Required { field: field() });
        }
        return;
    }
    if let Value::Text(text) = value {
        if text.contains('\0') {
            errors.push(ValidationError::ContainsNul { field: field() });
            return;
        }
        if param.required && text.trim().is_empty() {
            errors.push(ValidationError::Required { field: field() });
            return;
        }
    }
    match (&param.kind, value) {
        (ParamKind::Data | ParamKind::Link { .. }, Value::Text(text)) => {
            let len = text.chars().count();
            if len > DEFAULT_MAX_LENGTH as usize {
                errors.push(ValidationError::TooLong {
                    field: field(),
                    len,
                    max: DEFAULT_MAX_LENGTH,
                });
            }
        }
        (ParamKind::Select { options }, Value::Text(text)) => {
            if !options.contains(text) {
                errors.push(ValidationError::InvalidOption {
                    field: field(),
                    value: text.clone(),
                });
            }
        }
        (ParamKind::Int, Value::Int(_))
        | (ParamKind::Decimal, Value::Decimal(_))
        | (ParamKind::Date, Value::Date(_))
        | (ParamKind::Check, Value::Bool(_)) => {}
        (kind, value) => errors.push(ValidationError::TypeMismatch {
            field: field(),
            expected: kind.expected(),
            found: value.kind(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::Decimal;
    use time::{Date, Month};

    fn param(name: &str, kind: ParamKind, required: bool) -> ReportParam {
        ReportParam {
            name: name.to_owned(),
            label: name.to_owned(),
            kind,
            required,
            default: None,
        }
    }

    fn report() -> ReportDef {
        ReportDef {
            app: "sales".to_owned(),
            name: "totals".to_owned(),
            label: "Totals".to_owned(),
            sql: "SELECT 1".to_owned(),
            roles: vec!["Manager".to_owned()],
            params: vec![
                param("from", ParamKind::Date, true),
                param("note", ParamKind::Data, false),
                param(
                    "kind",
                    ParamKind::Select {
                        options: vec!["a".to_owned(), "b".to_owned()],
                    },
                    false,
                ),
            ],
            columns: vec![ReportColumn {
                name: "n".to_owned(),
                label: "N".to_owned(),
                kind: ColumnKind::Int,
            }],
            timeout: DEFAULT_TIMEOUT,
            max_rows: DEFAULT_MAX_ROWS,
        }
    }

    fn date() -> Value {
        Value::Date(Date::from_calendar_date(2026, Month::October, 30).expect("date"))
    }

    fn given(pairs: &[(&str, Value)]) -> BTreeMap<String, Value> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.clone()))
            .collect()
    }

    #[test]
    fn a_valid_report_has_no_problems_and_a_full_name() {
        assert!(report().problems().is_empty());
        assert_eq!(report().full_name(), "sales.totals");
    }

    #[test]
    fn every_definition_problem_is_reported() {
        let mut r = report();
        r.name = "Bad Name".to_owned();
        r.sql = "  ".to_owned();
        r.roles = vec![];
        r.columns = vec![];
        r.timeout = Duration::from_secs(301);
        r.max_rows = 100_001;
        let text = r.problems().join("\n");
        for needle in [
            "name \"Bad Name\"",
            "the SQL is empty",
            "no roles",
            "no columns",
            "timeout",
            "max_rows 100001",
        ] {
            assert!(text.contains(needle), "{needle} in {text}");
        }
        let mut r = report();
        r.timeout = Duration::ZERO;
        r.max_rows = 0;
        assert_eq!(r.problems().len(), 2);
    }

    #[test]
    fn the_limits_may_be_raised_up_to_their_caps() {
        let mut r = report();
        r.timeout = MAX_TIMEOUT;
        r.max_rows = MAX_MAX_ROWS;
        assert!(r.problems().is_empty());
    }

    #[test]
    fn roles_params_columns_and_options_are_checked() {
        let mut r = report();
        r.roles = vec!["  ".to_owned()];
        r.params.push(param("from", ParamKind::Int, false));
        r.params.push(param("Bad", ParamKind::Int, false));
        r.params.push(param(
            "s",
            ParamKind::Select {
                options: vec!["x".to_owned(), "x".to_owned()],
            },
            false,
        ));
        r.params
            .push(param("e", ParamKind::Select { options: vec![] }, false));
        r.columns.push(r.columns[0].clone());
        let text = r.problems().join("\n");
        for needle in [
            "invalid role \"  \"",
            "param `from` is declared more than once",
            "param name \"Bad\"",
            "param `s`: select options must be unique",
            "param `e`: a select needs options",
            "column `n` is declared more than once",
        ] {
            assert!(text.contains(needle), "{needle} in {text}");
        }
    }

    #[test]
    fn a_default_must_fit_its_param() {
        let mut r = report();
        r.params[1].default = Some(Value::Int(1));
        r.params[2].default = Some(Value::Text("zzz".to_owned()));
        let text = r.problems().join("\n");
        assert!(
            text.contains("param `note`: invalid default value"),
            "{text}"
        );
        assert!(
            text.contains("param `kind`: invalid default value"),
            "{text}"
        );
        r.params[1].default = Some(Value::Text("x".to_owned()));
        r.params[2].default = Some(Value::Text("a".to_owned()));
        assert!(r.problems().is_empty());
    }

    #[test]
    fn link_targets_name_params_and_columns() {
        let mut r = report();
        r.params.push(param(
            "customer",
            ParamKind::Link {
                doctype: "customer".to_owned(),
            },
            false,
        ));
        r.columns.push(ReportColumn {
            name: "c".to_owned(),
            label: "C".to_owned(),
            kind: ColumnKind::Link {
                doctype: "item".to_owned(),
            },
        });
        assert_eq!(
            r.link_targets(),
            vec![
                ("customer", "param `customer`".to_owned()),
                ("item", "column `c`".to_owned())
            ]
        );
    }

    #[test]
    fn params_are_bound_in_declared_order_with_null_for_absent_ones() {
        let values = report()
            .bind_params(&given(&[("from", date())]))
            .expect("valid");
        assert_eq!(values, vec![date(), Value::Null, Value::Null]);
    }

    #[test]
    fn a_default_fills_an_absent_param_but_not_an_explicit_null() {
        let mut r = report();
        r.params[1].default = Some(Value::Text("dflt".to_owned()));
        let values = r.bind_params(&given(&[("from", date())])).expect("valid");
        assert_eq!(values[1], Value::Text("dflt".to_owned()));
        let values = r
            .bind_params(&given(&[("from", date()), ("note", Value::Null)]))
            .expect("valid");
        assert_eq!(values[1], Value::Null);
    }

    #[test]
    fn every_param_problem_is_reported_by_param() {
        let errors = report()
            .bind_params(&given(&[
                ("note", Value::Int(1)),
                ("kind", Value::Text("c".to_owned())),
                ("extra", Value::Null),
            ]))
            .expect_err("invalid");
        let text: Vec<String> = errors.iter().map(ToString::to_string).collect();
        assert!(
            text.contains(&"unknown field `extra`".to_owned()),
            "{text:?}"
        );
        assert!(
            text.contains(&"field `from` is required".to_owned()),
            "{text:?}"
        );
        assert!(
            text.contains(&"field `note`: expected text, found int".to_owned()),
            "{text:?}"
        );
        assert!(
            text.contains(&"field `kind`: `c` is not one of the allowed options".to_owned()),
            "{text:?}"
        );
        assert_eq!(errors.len(), 4);
    }

    #[test]
    fn text_params_refuse_nul_and_long_values_and_blank_required_text() {
        let mut r = report();
        r.params.push(param("must", ParamKind::Data, true));
        let long = "x".repeat(141);
        let errors = r
            .bind_params(&given(&[
                ("from", date()),
                ("note", Value::Text("a\0b".to_owned())),
                ("must", Value::Text("   ".to_owned())),
            ]))
            .expect_err("invalid");
        assert_eq!(
            errors.iter().map(ToString::to_string).collect::<Vec<_>>(),
            [
                "field `note`: value must not contain a NUL character",
                "field `must` is required"
            ]
        );
        let errors = r
            .bind_params(&given(&[
                ("from", date()),
                ("note", Value::Text(long)),
                ("must", Value::Text("ok".to_owned())),
            ]))
            .expect_err("invalid");
        assert_eq!(
            errors[0].to_string(),
            "field `note`: 141 characters exceeds max_length 140"
        );
    }

    #[test]
    fn each_kind_takes_its_own_value_only() {
        let mut r = report();
        r.params = vec![
            param("i", ParamKind::Int, false),
            param("d", ParamKind::Decimal, false),
            param("b", ParamKind::Check, false),
            param(
                "l",
                ParamKind::Link {
                    doctype: "customer".to_owned(),
                },
                false,
            ),
        ];
        let ok = r
            .bind_params(&given(&[
                ("i", Value::Int(3)),
                ("d", Value::Decimal(Decimal::new(125, 2))),
                ("b", Value::Bool(false)),
                ("l", Value::Text("CUST-1".to_owned())),
            ]))
            .expect("valid");
        assert_eq!(ok.len(), 4);
        let errors = r
            .bind_params(&given(&[
                ("i", Value::Text("3".to_owned())),
                ("d", Value::Int(1)),
                ("b", Value::Int(0)),
                ("l", Value::Int(1)),
            ]))
            .expect_err("invalid");
        assert_eq!(errors.len(), 4);
    }
}
