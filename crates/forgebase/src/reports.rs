//! Query reports (M8.4, ADR 0016): [`Report`], [`Param`] and [`Column`], the builders an app uses
//! with [`App::report`](crate::App::report).

use std::time::Duration;

use forgebase_core::meta::humanize;
use forgebase_core::report::{
    ColumnKind, DEFAULT_MAX_ROWS, DEFAULT_TIMEOUT, ParamKind, ReportColumn, ReportDef, ReportParam,
};
use forgebase_core::value::Value;

/// An app-defined SQL report, for [`App::report`](crate::App::report): the SQL, the roles that may
/// run it, typed parameters and typed result columns.
///
/// ```ignore
/// App::new("sales").report(
///     "sales_by_customer",
///     Report::sql(include_str!("../reports/sales_by_customer.sql"))
///         .label("Sales by customer")
///         .roles(&["Sales Manager"])
///         .param(Param::date("from").label("From").required())
///         .param(Param::date("to").label("To").required())
///         .param(Param::link("customer", "customer"))
///         .column(Column::link("customer", "customer"))
///         .column(Column::int("orders"))
///         .column(Column::decimal("total")),
/// )
/// ```
///
/// The SQL is **trusted code compiled into the binary**, like an app migration. Rules (ADR 0016):
///
/// - **Parameters are positional**: `$1..$n` are the declared params, in the order of
///   [`Report::param`]. They are always bound, never put into the text. A param that is absent
///   and not required is bound as `NULL`, so the SQL handles it:
///   `WHERE ($3::text IS NULL OR customer = $3)`. Cast a placeholder (`$1::date`) when
///   PostgreSQL cannot infer its type.
/// - **Columns are positional** too: the statement returns the declared columns in order, and the
///   SQL type must fit the kind (`int2`/`int4`/`int8` for [`Column::int`], `numeric` for
///   [`Column::decimal`], `text` or `varchar` for [`Column::data`] and [`Column::link`], `date`,
///   `timestamptz`, `bool`). `forgebase migrate` prepares every report and refuses one that does
///   not fit; `testing::check_reports` runs the same check in an app's tests.
/// - **Run as a read-only statement** with a statement timeout and a row limit
///   ([`Report::timeout`], [`Report::max_rows`]). A statement that writes fails.
/// - **Only the roles named here may run it** (the Administrator role always may). **Row-level
///   rules do not apply inside a report**: it shows what its SQL selects, to every role that may
///   run it. The caller is not available to the SQL in this version.
///
/// A trailing `;` and whitespace are stripped from the SQL. A report is only data: everything
/// wrong with it (an invalid name, no roles, a Link to an unknown DocType, ...) is reported
/// with the other problems when the binary starts ([`StartupError`](crate::StartupError)).
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Report {
    pub(crate) sql: String,
    pub(crate) label: Option<String>,
    pub(crate) roles: Vec<String>,
    pub(crate) params: Vec<Param>,
    pub(crate) columns: Vec<Column>,
    pub(crate) timeout: Duration,
    pub(crate) max_rows: u32,
}

impl Report {
    /// A report that runs this SQL, usually `include_str!("../reports/name.sql")`. Add the roles,
    /// params and columns with the other methods.
    pub fn sql(sql: &str) -> Self {
        Self {
            sql: sql.to_owned(),
            label: None,
            roles: Vec::new(),
            params: Vec::new(),
            columns: Vec::new(),
            timeout: DEFAULT_TIMEOUT,
            max_rows: DEFAULT_MAX_ROWS,
        }
    }

    /// The label the desk shows. Default: the report's name, humanized.
    pub fn label(mut self, label: &str) -> Self {
        self.label = Some(label.to_owned());
        self
    }

    /// Adds the roles that may run the report. At least one is required: there is no "any user"
    /// report. The Administrator role and the system context always may.
    pub fn roles(mut self, roles: &[&str]) -> Self {
        self.roles.extend(roles.iter().map(|r| (*r).to_owned()));
        self
    }

    /// Adds a parameter. The order of the calls is the order of `$1..$n`.
    pub fn param(mut self, param: Param) -> Self {
        self.params.push(param);
        self
    }

    /// Adds a result column. The order of the calls is the order of the statement's columns.
    pub fn column(mut self, column: Column) -> Self {
        self.columns.push(column);
        self
    }

    /// The statement timeout. Default 30 seconds, at most 300 seconds; a larger value is a
    /// startup error.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// The most rows returned. Default 10 000, at most 100 000; a larger value is a startup
    /// error. When there are more rows the result is cut and marked `truncated`.
    pub fn max_rows(mut self, rows: u32) -> Self {
        self.max_rows = rows;
        self
    }

    /// The definition for `app` and `name`, with the SQL trimmed of whitespace and a trailing `;`.
    pub(crate) fn definition(&self, app: &str, name: &str) -> ReportDef {
        let mut sql = self.sql.trim_end();
        while let Some(rest) = sql.strip_suffix(';') {
            sql = rest.trim_end();
        }
        ReportDef {
            app: app.to_owned(),
            name: name.to_owned(),
            label: self.label.clone().unwrap_or_else(|| humanize(name)),
            sql: sql.trim_start().to_owned(),
            roles: self.roles.clone(),
            params: self.params.iter().map(Param::definition).collect(),
            columns: self.columns.iter().map(Column::definition).collect(),
            timeout: self.timeout,
            max_rows: self.max_rows,
        }
    }
}

/// A parameter of a [`Report`]: a typed value the caller gives. Values are checked like the
/// fields of the same kind and always bound to the statement, never interpolated.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Param {
    name: String,
    label: String,
    kind: ParamKind,
    required: bool,
    default: Option<Value>,
}

impl Param {
    fn new(name: &str, kind: ParamKind) -> Self {
        Self {
            name: name.to_owned(),
            label: humanize(name),
            kind,
            required: false,
            default: None,
        }
    }

    /// Text of at most 140 characters. The `name` follows the identifier rule (lower-case ASCII
    /// letters, digits and underscores, starting with a letter) and is the key in the request.
    pub fn data(name: &str) -> Self {
        Self::new(name, ParamKind::Data)
    }

    /// A 64-bit integer.
    pub fn int(name: &str) -> Self {
        Self::new(name, ParamKind::Int)
    }

    /// A fixed-point number, a string in the API (`"12.50"`).
    pub fn decimal(name: &str) -> Self {
        Self::new(name, ParamKind::Decimal)
    }

    /// A calendar date, `YYYY-MM-DD` in the API.
    pub fn date(name: &str) -> Self {
        Self::new(name, ParamKind::Date)
    }

    /// A boolean.
    pub fn check(name: &str) -> Self {
        Self::new(name, ParamKind::Check)
    }

    /// The `name` of a document of `doctype` (a DocType of the app or of a dependency). Only
    /// bound: it is not checked for existence, and the caller's read rules do not apply.
    pub fn link(name: &str, doctype: &str) -> Self {
        Self::new(
            name,
            ParamKind::Link {
                doctype: doctype.to_owned(),
            },
        )
    }

    /// One of `options` (not empty, no duplicates).
    pub fn select(name: &str, options: &[&str]) -> Self {
        Self::new(
            name,
            ParamKind::Select {
                options: options.iter().map(|o| (*o).to_owned()).collect(),
            },
        )
    }

    /// The label the desk shows. Default: the name, humanized.
    pub fn label(mut self, label: &str) -> Self {
        self.label = label.to_owned();
        self
    }

    /// A value must be given (the default counts). Without it, an absent value is `NULL`.
    pub fn required(mut self) -> Self {
        self.required = true;
        self
    }

    /// The value used when the request does not mention the param; the desk fills it in. It must
    /// fit the kind (a startup error otherwise).
    pub fn default_value(mut self, value: Value) -> Self {
        self.default = Some(value);
        self
    }

    fn definition(&self) -> ReportParam {
        ReportParam {
            name: self.name.clone(),
            label: self.label.clone(),
            kind: self.kind.clone(),
            required: self.required,
            default: self.default.clone(),
        }
    }
}

/// A result column of a [`Report`]. The columns are positional: the statement returns them in
/// the order they are declared, and the SQL type must fit the kind.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Column {
    name: String,
    label: String,
    kind: ColumnKind,
}

impl Column {
    fn new(name: &str, kind: ColumnKind) -> Self {
        Self {
            name: name.to_owned(),
            label: humanize(name),
            kind,
        }
    }

    /// Text (`text` or `varchar`). The `name` follows the identifier rule and need not be the
    /// SQL column's name.
    pub fn data(name: &str) -> Self {
        Self::new(name, ColumnKind::Data)
    }

    /// An integer (`int2`, `int4` or `int8`).
    pub fn int(name: &str) -> Self {
        Self::new(name, ColumnKind::Int)
    }

    /// A fixed-point number (`numeric`), a string in the API.
    pub fn decimal(name: &str) -> Self {
        Self::new(name, ColumnKind::Decimal)
    }

    /// A calendar date (`date`).
    pub fn date(name: &str) -> Self {
        Self::new(name, ColumnKind::Date)
    }

    /// A point in time (`timestamptz`), RFC 3339 in UTC in the API.
    pub fn datetime(name: &str) -> Self {
        Self::new(name, ColumnKind::Datetime)
    }

    /// A boolean (`bool`).
    pub fn check(name: &str) -> Self {
        Self::new(name, ColumnKind::Check)
    }

    /// The `name` of a document of `doctype` (`text` or `varchar`). The desk shows it as a link.
    pub fn link(name: &str, doctype: &str) -> Self {
        Self::new(
            name,
            ColumnKind::Link {
                doctype: doctype.to_owned(),
            },
        )
    }

    /// The label the desk shows. Default: the name, humanized.
    pub fn label(mut self, label: &str) -> Self {
        self.label = label.to_owned();
        self
    }

    fn definition(&self) -> ReportColumn {
        ReportColumn {
            name: self.name.clone(),
            label: self.label.clone(),
            kind: self.kind.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_default_to_the_humanized_name() {
        let report = Report::sql("SELECT 1")
            .roles(&["A"])
            .param(Param::date("from_date"))
            .column(Column::int("order_count").label("Orders"))
            .column(Column::data("customer_name"));
        let def = report.definition("sales", "sales_by_customer");
        assert_eq!(def.label, "Sales By Customer");
        assert_eq!(def.params[0].label, "From Date");
        assert_eq!(def.columns[0].label, "Orders");
        assert_eq!(def.columns[1].label, "Customer Name");
        assert_eq!(def.full_name(), "sales.sales_by_customer");
    }

    #[test]
    fn the_sql_loses_its_trailing_semicolons_and_whitespace() {
        for (text, expected) in [
            ("SELECT 1;\n", "SELECT 1"),
            ("  SELECT 1 ; ;  \n", "SELECT 1"),
            ("SELECT ';' ;", "SELECT ';'"),
            ("SELECT 1", "SELECT 1"),
        ] {
            let def = Report::sql(text).definition("a", "r");
            assert_eq!(def.sql, expected, "{text:?}");
        }
    }

    #[test]
    fn the_builders_set_the_definition() {
        let def = Report::sql("SELECT 1")
            .label("Totals")
            .roles(&["A", "B"])
            .roles(&["C"])
            .timeout(Duration::from_secs(5))
            .max_rows(7)
            .param(Param::select("kind", &["x", "y"]).required())
            .param(Param::link("customer", "customer").default_value(Value::Text("C".into())))
            .column(Column::link("customer", "customer"))
            .definition("sales", "totals");
        assert_eq!(def.roles, ["A", "B", "C"]);
        assert_eq!(def.timeout, Duration::from_secs(5));
        assert_eq!(def.max_rows, 7);
        assert!(def.params[0].required);
        assert_eq!(def.params[1].default, Some(Value::Text("C".into())));
        assert_eq!(def.params[1].kind.name(), "link");
        assert_eq!(def.columns[0].kind.name(), "link");
        assert!(def.problems().is_empty(), "{:?}", def.problems());
    }
}
