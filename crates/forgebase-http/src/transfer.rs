//! The file formats of import and export (M9.3, ADR 0021), shared by the HTTP API and the
//! command-line tool (`forgebase_http::transfer`), so both read and write identically.
//!
//! # Export
//!
//! [`Exporter`] turns pages of [`StoredDocument`]s (from `forgebase_db::Export`) into text.
//!
//! - **CSV** is written by hand (RFC 4180): UTF-8 without a BOM, CRLF line ends, a header row of
//!   the field keys, never child tables. A cell is quoted when it contains `,`, `"`, CR or LF or
//!   has leading or trailing spaces, and a `"` inside is doubled. Dates and datetimes are ISO 8601
//!   as in the JSON API, decimals are their exact strings, booleans `true` / `false`, links the
//!   target's name, null an empty cell.
//! - **Formula injection.** A spreadsheet may run a cell that starts with `=`, `+`, `-`, `@`, TAB
//!   or CR as a formula. A value of a **text kind** (Data, Text, Select, Link, and the text
//!   standard columns) that starts with one of them gets a leading `'`. Numbers, decimals, dates,
//!   datetimes and booleans are never prefixed, so `-5` stays `-5`. This is always on: to keep
//!   exact text, round-trip through JSON (CSV import does not strip the `'`).
//! - **JSON** is a top-level array of documents, written as `document_to_json` writes them
//!   (child rows included), so a JSON export can be imported again.
//!
//! # Import
//!
//! [`parse_import`] maps a file to [`ImportRow`]s. **CSV** is read with the `csv` crate: the first
//! row is the header of field keys (a `name` column is allowed; a UTF-8 BOM is stripped), a cell is
//! parsed by the type of its field (a date `YYYY-MM-DD`, a datetime RFC 3339, an integer, a
//! decimal, a boolean `true`/`false`/`1`/`0`, anything else text), and an empty cell is "not
//! given". Unknown, duplicate and table columns are refused for the whole file. **JSON** is an
//! array of objects as in `POST /resource` (child rows as arrays); the standard keys of an exported
//! document (`id`, `owner`, `created_at`, `modified_at`, `modified_by`, `version`, `docstatus`,
//! `amended_from`, and `workflow_state` only when the DocType has a workflow) are ignored, so an
//! export can be imported again.
//! A value that cannot be parsed is an error of its **row**, reported with the others.
//! [`run_import`] runs the rows with `forgebase_db::import` and builds the summary.

use std::collections::BTreeSet;

use forgebase_core::meta::{DocType, FieldType, Naming, Registry};
use forgebase_core::permission::Caller;
use forgebase_core::shorten;
use forgebase_core::value::Value;
use forgebase_db::{
    DbError, GenericClient, ImportMode, ImportRow, MAX_REPORTED_ERRORS, StoredDocument,
    import as run_rows,
};
use serde_json::{Map, Value as Json, json};

use crate::error::ApiError;
use crate::json::{JsonError, document_to_json, from_value, input_from_json};

/// The file format of an import or export.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// `text/csv`.
    Csv,
    /// `application/json`.
    Json,
}

impl Format {
    /// `csv` or `json`.
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "csv" => Some(Format::Csv),
            "json" => Some(Format::Json),
            _ => None,
        }
    }

    /// The file extension, without the dot.
    pub fn extension(self) -> &'static str {
        match self {
            Format::Csv => "csv",
            Format::Json => "json",
        }
    }

    /// The `Content-Type` of an export.
    pub fn content_type(self) -> &'static str {
        match self {
            Format::Csv => "text/csv; charset=utf-8",
            Format::Json => "application/json",
        }
    }
}

// ---------------------------------------------------------------------------------------------
// CSV writing
// ---------------------------------------------------------------------------------------------

/// A value as the text of a CSV cell, with the formula mitigation for text values (a value is
/// text exactly when its field is of a text kind). Null is an empty cell.
pub fn csv_cell(name: &str, value: &Value) -> Result<String, JsonError> {
    // Fail safe: only the known non-text kinds skip the guard, so a value kind added later is
    // guarded until someone decides it cannot be a formula.
    let text = match value {
        Value::Null => return Ok(String::new()),
        Value::Text(text) => text.clone(),
        other => match from_value(name, other)? {
            Json::String(text) => text,
            Json::Number(number) => number.to_string(),
            Json::Bool(flag) => flag.to_string(),
            _ => String::new(),
        },
    };
    Ok(match value {
        Value::Int(_)
        | Value::Decimal(_)
        | Value::Bool(_)
        | Value::Date(_)
        | Value::Datetime(_) => text,
        _ => neutralize(&text),
    })
}

/// Prefixes `'` to a text that a spreadsheet could take for a formula.
fn neutralize(text: &str) -> String {
    if text.starts_with(['=', '+', '-', '@', '\t', '\r']) {
        format!("'{text}")
    } else {
        text.to_owned()
    }
}

/// Appends one CSV record (cells already formatted) and its CRLF to `out`.
pub fn push_csv_record(out: &mut String, cells: &[String]) {
    for (i, cell) in cells.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let quoted =
            cell.contains([',', '"', '\r', '\n']) || cell.starts_with(' ') || cell.ends_with(' ');
        if quoted {
            out.push('"');
            out.push_str(&cell.replace('"', "\"\""));
            out.push('"');
        } else {
            out.push_str(cell);
        }
    }
    out.push_str("\r\n");
}

// ---------------------------------------------------------------------------------------------
// Export
// ---------------------------------------------------------------------------------------------

/// Writes the pages of an export as CSV or JSON text. See the module documentation.
#[derive(Debug)]
pub struct Exporter {
    format: Format,
    /// CSV: the columns. JSON: empty.
    columns: Vec<String>,
    /// JSON: the keys to keep, or `None` for the whole document.
    keep: Option<Vec<String>>,
    wrote: bool,
    /// The DocType has a workflow: only then is `workflow_state` a standard column.
    workflow: bool,
}

/// The standard columns a `fields` list may name, besides the fields of the DocType.
fn standard_column(doctype: &DocType, name: &str) -> bool {
    match name {
        "name" | "owner" | "modified_by" | "created_at" | "modified_at" | "version" => true,
        "docstatus" => doctype.submittable,
        "workflow_state" => doctype.workflow.is_some(),
        _ => false,
    }
}

impl Exporter {
    /// An exporter of `doctype`'s documents. `fields` are the keys to write (the field names, and
    /// the standard columns `name`, `owner`, `modified_by`, `created_at`, `modified_at`,
    /// `version`, and `docstatus` / `workflow_state` where the DocType has them); `None` is
    /// `name` and every field for CSV, and the whole document for JSON. A Table field can be
    /// written to JSON only. A bad list is [`JsonError::Query`].
    pub fn new(
        doctype: &DocType,
        format: Format,
        fields: Option<&[String]>,
    ) -> Result<Self, JsonError> {
        let bad = |text: String| JsonError::Query(format!("invalid fields: {text}"));
        let Some(fields) = fields else {
            let columns = match format {
                Format::Csv => std::iter::once("name".to_owned())
                    .chain(doctype.column_fields().map(|f| f.name.clone()))
                    .collect(),
                Format::Json => Vec::new(),
            };
            return Ok(Self {
                format,
                columns,
                keep: None,
                wrote: false,
                workflow: doctype.workflow.is_some(),
            });
        };
        if fields.is_empty() {
            return Err(bad("the list is empty".to_owned()));
        }
        let mut seen = BTreeSet::new();
        for field in fields {
            if !seen.insert(field.as_str()) {
                return Err(bad(format!("`{}` is listed twice", shorten(field))));
            }
            match doctype.field(field).map(|f| &f.field_type) {
                _ if standard_column(doctype, field) => {}
                Some(FieldType::Table { .. }) if format == Format::Csv => {
                    return Err(bad(format!(
                        "`{field}` is a table field: a CSV export has no child tables, use JSON"
                    )));
                }
                Some(_) => {}
                None => {
                    return Err(bad(format!(
                        "unknown field `{}` for DocType `{}`",
                        shorten(field),
                        doctype.name
                    )));
                }
            }
        }
        Ok(match format {
            Format::Csv => Self {
                format,
                columns: fields.to_vec(),
                keep: None,
                wrote: false,
                workflow: doctype.workflow.is_some(),
            },
            Format::Json => Self {
                format,
                columns: Vec::new(),
                keep: Some(fields.to_vec()),
                wrote: false,
                workflow: doctype.workflow.is_some(),
            },
        })
    }

    /// The format.
    pub fn format(&self) -> Format {
        self.format
    }

    /// Whether the pages must carry the rows of the Table fields.
    pub fn with_tables(&self) -> bool {
        self.format == Format::Json
    }

    /// The text that starts the file: the CSV header, or `[`.
    pub fn start(&self) -> String {
        match self.format {
            Format::Csv => {
                let mut out = String::new();
                push_csv_record(&mut out, &self.columns);
                out
            }
            Format::Json => "[".to_owned(),
        }
    }

    /// The text of a page of documents.
    pub fn rows(&mut self, documents: &[StoredDocument]) -> Result<String, JsonError> {
        let mut out = String::new();
        for document in documents {
            match self.format {
                Format::Csv => {
                    let mut cells = Vec::with_capacity(self.columns.len());
                    for column in &self.columns {
                        cells.push(csv_cell(
                            column,
                            &standard_or_field(document, column, self.workflow),
                        )?);
                    }
                    push_csv_record(&mut out, &cells);
                }
                Format::Json => {
                    let mut json = document_to_json(document)?;
                    if let (Some(keep), Json::Object(object)) = (&self.keep, &mut json) {
                        object.retain(|key, _| keep.contains(key));
                    }
                    if self.wrote {
                        out.push(',');
                    }
                    out.push('\n');
                    out.push_str(&serde_json::to_string(&json).map_err(JsonError::Serialize)?);
                    self.wrote = true;
                }
            }
        }
        Ok(out)
    }

    /// The text that ends the file: nothing for CSV, `]` for JSON.
    pub fn finish(&self) -> String {
        match self.format {
            Format::Csv => String::new(),
            Format::Json => "\n]\n".to_owned(),
        }
    }
}

/// The value of a column of a document: a standard column or a field (Null when absent).
fn standard_or_field(document: &StoredDocument, key: &str, workflow: bool) -> Value {
    match key {
        "name" => Value::Text(document.name.clone()),
        "owner" => Value::Text(document.owner.clone()),
        "modified_by" => Value::Text(document.modified_by.clone()),
        "created_at" => Value::Datetime(document.created_at),
        "modified_at" => Value::Datetime(document.modified_at),
        "version" => Value::Int(document.version),
        "docstatus" => document
            .docstatus
            .map_or(Value::Null, |s| Value::Int(i64::from(s.as_i16()))),
        "workflow_state" if workflow => document
            .workflow_state
            .clone()
            .map_or(Value::Null, Value::Text),
        field => document.values.get(field).cloned().unwrap_or(Value::Null),
    }
}

// ---------------------------------------------------------------------------------------------
// Import
// ---------------------------------------------------------------------------------------------

/// A row whose values could not be mapped; reported like a row the database refused.
#[derive(Debug, Clone, PartialEq)]
pub struct RowProblem {
    /// The 1-based data row number.
    pub row: usize,
    /// The line of a CSV file.
    pub line: Option<u64>,
    /// The `name` of the row, if any.
    pub name: Option<String>,
    /// What is wrong, one entry per problem.
    pub details: Vec<String>,
}

/// A file that was read.
#[derive(Debug, Default)]
pub struct ParsedImport {
    /// The rows that were mapped.
    pub rows: Vec<ImportRow>,
    /// The rows that could not be mapped.
    pub problems: Vec<RowProblem>,
}

impl ParsedImport {
    /// The number of data rows in the file.
    pub fn total(&self) -> usize {
        self.rows.len() + self.problems.len()
    }
}

/// Why a file cannot be imported at all: nothing runs.
#[derive(Debug, PartialEq)]
pub enum FileError {
    /// The file is not usable; one entry per problem.
    Invalid(Vec<String>),
    /// More rows than the limit.
    TooLarge {
        /// The limit.
        limit: usize,
    },
}

/// Keeps at most [`MAX_REPORTED_ERRORS`] problems and ends the list with "and N more", so the
/// number of problems does not grow with the file. Each problem is short too, because what it
/// repeats of the input goes through `shorten` (about 64 characters).
fn capped(mut problems: Vec<String>) -> Vec<String> {
    if problems.len() > MAX_REPORTED_ERRORS {
        let more = problems.len() - MAX_REPORTED_ERRORS;
        problems.truncate(MAX_REPORTED_ERRORS);
        problems.push(format!("and {more} more"));
    }
    problems
}

fn invalid(problem: impl Into<String>) -> FileError {
    FileError::Invalid(vec![problem.into()])
}

/// The keys of an exported document that are not fields: ignored in a JSON import.
const STANDARD_KEYS: [&str; 9] = [
    "id",
    "owner",
    "created_at",
    "modified_at",
    "modified_by",
    "version",
    "docstatus",
    "amended_from",
    "workflow_state",
];

/// Whether `key` is a standard key of an exported document that import ignores: the keys of
/// [`STANDARD_KEYS`], except that `workflow_state` is one only for a DocType with a workflow (on
/// another it may be a field of the DocType's own, which import must keep).
fn is_standard_key(doctype: &DocType, key: &str) -> bool {
    STANDARD_KEYS.contains(&key) && (key != "workflow_state" || doctype.workflow.is_some())
}

/// Reads `body` as a file of `format` for `doctype`. At most `max_rows` data rows. See the module
/// documentation for what is refused for the whole file ([`FileError`]) and what per row.
pub fn parse_import(
    registry: &Registry,
    doctype: &DocType,
    format: Format,
    mode: ImportMode,
    body: &[u8],
    max_rows: usize,
) -> Result<ParsedImport, FileError> {
    let text = std::str::from_utf8(body).map_err(|e| {
        invalid(format!(
            "the file is not valid UTF-8 (invalid byte at offset {})",
            e.valid_up_to()
        ))
    })?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    match format {
        Format::Csv => parse_csv(doctype, mode, text, max_rows),
        Format::Json => parse_json(registry, doctype, text, max_rows),
    }
}

fn parse_json(
    registry: &Registry,
    doctype: &DocType,
    text: &str,
    max_rows: usize,
) -> Result<ParsedImport, FileError> {
    let Json::Array(items) = serde_json::from_str::<Json>(text)
        .map_err(|e| invalid(format!("the file is not valid JSON: {e}")))?
    else {
        return Err(invalid("the JSON must be an array of document objects"));
    };
    // The row limit counts the documents and their child rows together: a small file of
    // documents must not carry millions of rows.
    let mut total = items.len();
    for item in &items {
        if let Json::Object(object) = item {
            for (field, _) in doctype.table_fields() {
                if let Some(Json::Array(rows)) = object.get(&field.name) {
                    total = total.saturating_add(rows.len());
                }
            }
        }
    }
    if total > max_rows {
        return Err(FileError::TooLarge { limit: max_rows });
    }
    let mut parsed = ParsedImport::default();
    for (i, item) in items.into_iter().enumerate() {
        let row = i + 1;
        let problem = |name: Option<String>, detail: String| RowProblem {
            row,
            line: None,
            name,
            details: vec![detail],
        };
        let Json::Object(mut object) = item else {
            parsed
                .problems
                .push(problem(None, "the row must be a JSON object".to_owned()));
            continue;
        };
        for key in STANDARD_KEYS {
            if is_standard_key(doctype, key) {
                object.remove(key);
            }
        }
        let name = match object.remove("name") {
            None | Some(Json::Null) => None,
            Some(Json::String(name)) => Some(name),
            Some(_) => {
                parsed
                    .problems
                    .push(problem(None, "`name` must be a string".to_owned()));
                continue;
            }
        };
        match input_from_json(registry, doctype, Json::Object(object)) {
            Ok(input) => parsed.rows.push(ImportRow {
                row,
                line: None,
                name: known_name(doctype, name, &input.values),
                values: input.values,
                tables: input.tables,
                empty: Vec::new(),
            }),
            Err(error) => parsed.problems.push(problem(name, error.to_string())),
        }
    }
    Ok(parsed)
}

/// The name of a row: the one it gives, else, for a DocType named by a field, the value of that
/// field (the document's name will be exactly that), so an error can name the document and an
/// upsert can find it.
fn known_name(
    doctype: &DocType,
    name: Option<String>,
    values: &std::collections::BTreeMap<String, Value>,
) -> Option<String> {
    name.or_else(|| {
        let Naming::Field(field) = &doctype.naming else {
            return None;
        };
        match values.get(field) {
            Some(Value::Text(text)) => Some(text.clone()),
            _ => None,
        }
    })
}

/// A column of a CSV file: the `name`, or a field of the DocType.
enum Column<'a> {
    Name,
    Field(&'a FieldType),
    /// A standard column of an exported file (`owner`, `modified_at`, ...): read, not used.
    Ignored,
}

fn parse_csv(
    doctype: &DocType,
    mode: ImportMode,
    text: &str,
    max_rows: usize,
) -> Result<ParsedImport, FileError> {
    let mut reader = csv::ReaderBuilder::new()
        .has_headers(true)
        .flexible(false)
        .from_reader(text.as_bytes());
    let headers = reader
        .headers()
        .map_err(|e| invalid(format!("the CSV header cannot be read: {e}")))?
        .clone();
    if headers.is_empty() || (headers.len() == 1 && headers.get(0) == Some("")) {
        return Err(invalid("the CSV file has no header row"));
    }
    // More columns than the DocType can have is refused before any column is looked at: the
    // list of problems must not grow with the file.
    let possible = doctype.fields.len() + 1 + STANDARD_KEYS.len();
    if headers.len() > possible {
        return Err(invalid(format!(
            "the header has {} columns; at most {possible} are possible",
            headers.len()
        )));
    }
    let mut problems = Vec::new();
    let mut seen = BTreeSet::new();
    // One entry per column; a refused column ends the import below, so alignment only matters
    // when there are no problems.
    let mut columns: Vec<Column<'_>> = Vec::with_capacity(headers.len());
    for header in &headers {
        if !seen.insert(header.to_owned()) {
            problems.push(format!("duplicate column `{}`", shorten(header)));
        }
        if header == "name" {
            columns.push(Column::Name);
            continue;
        }
        if is_standard_key(doctype, header) {
            columns.push(Column::Ignored);
            continue;
        }
        match doctype.field(header).map(|f| &f.field_type) {
            Some(FieldType::Table { .. }) => problems.push(format!(
                "column `{header}` is a table field: CSV import of child tables is not supported; use JSON"
            )),
            Some(field_type) => columns.push(Column::Field(field_type)),
            None => problems.push(format!(
                "unknown column `{}` for DocType `{}`",
                shorten(header),
                doctype.name
            )),
        }
    }
    if mode == ImportMode::Upsert && !seen.contains("name") {
        problems.push("an upsert needs a `name` column".to_owned());
    }
    if !problems.is_empty() {
        return Err(FileError::Invalid(capped(problems)));
    }
    let mut parsed = ParsedImport::default();
    let mut record = csv::StringRecord::new();
    // (byte, line) of the last position asked for, so the lines are counted once over the file.
    let mut cursor = (0_usize, 1_u64);
    loop {
        match reader.read_record(&mut record) {
            Ok(true) => {}
            Ok(false) => break,
            Err(error) => {
                return Err(invalid(format!("the CSV file cannot be read: {error}")));
            }
        }
        if parsed.total() >= max_rows {
            return Err(FileError::TooLarge { limit: max_rows });
        }
        let row = parsed.total() + 1;
        let line = record
            .position()
            .map(|p| line_of(text.as_bytes(), &mut cursor, p.byte() as usize));
        let mut item = ImportRow {
            row,
            line,
            ..ImportRow::default()
        };
        let mut details = Vec::new();
        for ((header, column), cell) in headers.iter().zip(&columns).zip(record.iter()) {
            match column {
                Column::Name => {
                    if !cell.is_empty() {
                        item.name = Some(cell.to_owned());
                    }
                }
                Column::Ignored => {}
                Column::Field(_) if cell.is_empty() => item.empty.push(header.to_owned()),
                Column::Field(field_type) => match cell_value(header, field_type, cell) {
                    Ok(value) => {
                        item.values.insert(header.to_owned(), value);
                    }
                    Err(error) => details.push(error.to_string()),
                },
            }
        }
        item.name = known_name(doctype, item.name, &item.values);
        if details.is_empty() {
            parsed.rows.push(item);
        } else {
            parsed.problems.push(RowProblem {
                row,
                line,
                name: item.name,
                details,
            });
        }
    }
    Ok(parsed)
}

/// The 1-based line of the file at byte offset `byte`. The `csv` crate reports the start of a
/// record after a CRLF terminator at the `\n` of that terminator, one line early, so the offset
/// is moved past it first. `cursor` is the last (byte, line) answered; offsets only grow.
fn line_of(text: &[u8], cursor: &mut (usize, u64), mut byte: usize) -> u64 {
    if byte > 0 && text.get(byte) == Some(&b'\n') && text.get(byte - 1) == Some(&b'\r') {
        byte += 1;
    }
    let byte = byte.max(cursor.0).min(text.len());
    cursor.1 += text[cursor.0..byte].iter().filter(|b| **b == b'\n').count() as u64;
    cursor.0 = byte;
    cursor.1
}

/// A CSV cell as a value of its field's type, mapped like the JSON API maps the same value.
fn cell_value(name: &str, field_type: &FieldType, cell: &str) -> Result<Value, JsonError> {
    let bad = |problem: String| JsonError::Field {
        field: name.to_owned(),
        problem,
    };
    let json = match field_type {
        FieldType::Data { .. }
        | FieldType::Text
        | FieldType::Select { .. }
        | FieldType::Link { .. } => Json::String(cell.to_owned()),
        FieldType::Int => {
            let text = cell.trim();
            Json::from(
                text.parse::<i64>()
                    .map_err(|_| bad(format!("`{}` is not an integer", shorten(text))))?,
            )
        }
        FieldType::Check => match cell.trim().to_ascii_lowercase().as_str() {
            "true" | "1" => Json::Bool(true),
            "false" | "0" => Json::Bool(false),
            other => {
                return Err(bad(format!(
                    "`{}` is not true, false, 1 or 0",
                    shorten(other)
                )));
            }
        },
        FieldType::Decimal { .. } | FieldType::Date | FieldType::Datetime => {
            Json::String(cell.trim().to_owned())
        }
        FieldType::Table { .. } => return Err(bad("a table field is not a CSV column".to_owned())),
    };
    crate::json::to_value(name, field_type, &json)
}

// ---------------------------------------------------------------------------------------------
// Running an import and its summary
// ---------------------------------------------------------------------------------------------

/// One row error of an import, as it is reported.
#[derive(Debug, Clone, PartialEq)]
pub struct RowReport {
    /// The 1-based data row number.
    pub row: usize,
    /// The line of a CSV file.
    pub line: Option<u64>,
    /// The document name, if the row gave one.
    pub name: Option<String>,
    /// The error code, as the API names it (`validation_failed`, `hook_rejected`, ...).
    pub code: String,
    /// The message.
    pub message: String,
    /// One entry per problem, for a validation error.
    pub details: Vec<String>,
}

/// What an import did, or, for a dry run or a failed one, would have done (nothing is kept then).
#[derive(Debug, Clone, PartialEq)]
pub struct ImportSummary {
    /// A dry run: nothing was kept.
    pub dry_run: bool,
    /// The mode.
    pub mode: ImportMode,
    /// The data rows of the file.
    pub rows: usize,
    /// Rows that inserted a document.
    pub inserted: u64,
    /// Rows that updated a document.
    pub updated: u64,
    /// The first [`MAX_REPORTED_ERRORS`] errors, in row order.
    pub errors: Vec<RowReport>,
    /// The number of rows with an error.
    pub error_count: u64,
}

impl ImportSummary {
    /// The summary as JSON: `{"dry_run", "mode", "rows", "inserted", "updated", "errors",
    /// "error_count"}`.
    pub fn to_json(&self) -> Json {
        let errors: Vec<Json> = self
            .errors
            .iter()
            .map(|e| {
                let mut map = Map::new();
                map.insert("row".to_owned(), json!(e.row));
                if let Some(line) = e.line {
                    map.insert("line".to_owned(), json!(line));
                }
                map.insert("name".to_owned(), json!(e.name));
                map.insert("code".to_owned(), json!(e.code));
                map.insert("message".to_owned(), json!(e.message));
                if !e.details.is_empty() {
                    map.insert("details".to_owned(), json!(e.details));
                }
                Json::Object(map)
            })
            .collect();
        json!({
            "dry_run": self.dry_run,
            "mode": match self.mode {
                ImportMode::Insert => "insert",
                ImportMode::Upsert => "upsert",
            },
            "rows": self.rows,
            "inserted": self.inserted,
            "updated": self.updated,
            "errors": errors,
            "error_count": self.error_count,
        })
    }
}

/// Runs a parsed file as `caller`: all rows in one transaction, committed only when `dry_run` is
/// false and no row (parse problems included) has an error. See `forgebase_db::import`.
pub async fn run_import(
    client: &mut impl GenericClient,
    registry: &Registry,
    doctype: &str,
    caller: &Caller,
    parsed: ParsedImport,
    mode: ImportMode,
    dry_run: bool,
) -> Result<ImportSummary, DbError> {
    let total = parsed.total();
    let commit = !dry_run && parsed.problems.is_empty();
    let outcome = run_rows(
        client,
        registry,
        doctype,
        caller,
        &parsed.rows,
        mode,
        commit,
    )
    .await?;
    let mut errors: Vec<RowReport> = parsed
        .problems
        .into_iter()
        .map(|p| RowReport {
            row: p.row,
            line: p.line,
            name: p.name.as_deref().map(shorten),
            code: "validation_failed".to_owned(),
            message: "validation failed".to_owned(),
            details: p.details,
        })
        .collect();
    let problems = errors.len() as u64;
    errors.extend(outcome.errors.into_iter().map(|e| {
        let (code, message, details) = ApiError::from(e.error).into_parts();
        RowReport {
            row: e.row,
            line: e.line,
            name: e.name.as_deref().map(shorten),
            code: code.to_owned(),
            message,
            details,
        }
    }));
    errors.sort_by_key(|e| e.row);
    errors.truncate(MAX_REPORTED_ERRORS);
    Ok(ImportSummary {
        dry_run,
        mode,
        rows: total,
        inserted: outcome.inserted,
        updated: outcome.updated,
        errors,
        error_count: problems + outcome.error_count,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn customer() -> DocType {
        DocType::from_toml_str(
            r#"
name = "thing"
label = "Thing"
[[fields]]
name = "title"
type = "data"
[[fields]]
name = "qty"
type = "int"
[[fields]]
name = "price"
type = "decimal"
precision = 12
scale = 2
[[fields]]
name = "due"
type = "date"
[[fields]]
name = "done"
type = "check"
[[fields]]
name = "lines"
type = "table"
target = "thing_line"
"#,
        )
        .expect("doctype")
    }

    #[test]
    fn csv_records_are_quoted_only_when_needed() {
        let mut out = String::new();
        let cells: Vec<String> = [
            "plain",
            "a,b",
            "say \"hi\"",
            "two\r\nlines",
            "line\nfeed",
            " lead",
            "trail ",
            "mid dle",
            "",
            "héllo ✓",
        ]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
        push_csv_record(&mut out, &cells);
        assert_eq!(
            out,
            "plain,\"a,b\",\"say \"\"hi\"\"\",\"two\r\nlines\",\"line\nfeed\",\" lead\",\"trail \",mid dle,,héllo ✓\r\n"
        );
    }

    #[test]
    fn only_text_values_are_neutralized() {
        let text = |s: &str| csv_cell("f", &Value::Text(s.to_owned())).unwrap();
        for risky in ["=1+1", "+1", "-1", "@SUM(A1)", "\tx", "\rx"] {
            assert_eq!(text(risky), format!("'{risky}"));
        }
        assert_eq!(text("safe"), "safe");
        assert_eq!(text(" =x"), " =x");
        assert_eq!(text(""), "");
        // Numbers, decimals, booleans and dates are never prefixed.
        assert_eq!(csv_cell("f", &Value::Int(-5)).unwrap(), "-5");
        let decimal = Value::Decimal("-12.50".parse().unwrap());
        assert_eq!(csv_cell("f", &decimal).unwrap(), "-12.50");
        assert_eq!(csv_cell("f", &Value::Bool(true)).unwrap(), "true");
        assert_eq!(csv_cell("f", &Value::Null).unwrap(), "");
    }

    #[test]
    fn exporter_fields_are_checked() {
        let doctype = customer();
        let list = |names: &[&str]| -> Vec<String> { names.iter().map(|s| (*s).into()).collect() };
        let err = |format, names: &[&str]| {
            Exporter::new(&doctype, format, Some(&list(names)))
                .unwrap_err()
                .to_string()
        };
        assert!(err(Format::Csv, &["nope"]).contains("unknown field `nope`"));
        assert!(err(Format::Csv, &["lines"]).contains("table field"));
        assert!(err(Format::Csv, &["title", "title"]).contains("twice"));
        assert!(err(Format::Csv, &[]).contains("empty"));
        assert!(err(Format::Csv, &["docstatus"]).contains("unknown field"));
        assert!(Exporter::new(&doctype, Format::Json, Some(&list(&["lines", "name"]))).is_ok());
        assert!(Exporter::new(&doctype, Format::Csv, Some(&list(&["owner", "title"]))).is_ok());
        let default = Exporter::new(&doctype, Format::Csv, None).unwrap();
        assert_eq!(default.start(), "name,title,qty,price,due,done\r\n");
        assert!(!default.with_tables());
    }

    fn registry() -> Registry {
        let mut registry = Registry::new();
        registry
            .add(
                DocType::from_toml_str(
                    "name = \"thing_line\"\nlabel = \"L\"\nis_child = true\n[[fields]]\nname = \"item\"\ntype = \"data\"\n",
                )
                .unwrap(),
            )
            .unwrap();
        registry.add(customer()).unwrap();
        registry
    }

    fn csv(text: &str) -> Result<ParsedImport, FileError> {
        parse_import(
            &registry(),
            &customer(),
            Format::Csv,
            ImportMode::Insert,
            text.as_bytes(),
            100,
        )
    }

    #[test]
    fn csv_cells_are_parsed_by_the_field_type_and_empty_cells_are_not_given() {
        let parsed = csv(
            "\u{feff}name,title,qty,price,due,done\r\nA,Hello,3,1.50,2026-10-31,TRUE\r\n,,,,,\r\nB,x,,,,0\r\n",
        )
        .unwrap();
        assert!(parsed.problems.is_empty(), "{:?}", parsed.problems);
        assert_eq!(parsed.rows.len(), 3);
        let first = &parsed.rows[0];
        assert_eq!(first.name.as_deref(), Some("A"));
        assert_eq!(first.line, Some(2));
        assert_eq!(first.values["qty"], Value::Int(3));
        assert_eq!(first.values["done"], Value::Bool(true));
        assert!(matches!(first.values["price"], Value::Decimal(_)));
        assert!(matches!(first.values["due"], Value::Date(_)));
        let blank = &parsed.rows[1];
        assert!(blank.name.is_none() && blank.values.is_empty());
        assert_eq!(blank.empty.len(), 5);
        let last = &parsed.rows[2];
        assert_eq!(last.row, 3);
        assert_eq!(last.values["done"], Value::Bool(false));
        assert!(last.empty.contains(&"qty".to_owned()));
    }

    #[test]
    fn csv_quoted_cells_keep_commas_quotes_and_line_breaks() {
        let parsed = csv("title\r\n\"a,b\"\r\n\"say \"\"hi\"\"\"\r\n\"two\r\nlines\"\r\n").unwrap();
        let titles: Vec<&Value> = parsed.rows.iter().map(|r| &r.values["title"]).collect();
        assert_eq!(
            titles,
            [
                &Value::Text("a,b".into()),
                &Value::Text("say \"hi\"".into()),
                &Value::Text("two\r\nlines".into())
            ]
        );
        assert_eq!(parsed.rows[2].line, Some(4));
    }

    #[test]
    fn a_bad_cell_is_a_row_problem_with_its_number() {
        let parsed =
            csv("title,qty,due\nok,1,2026-01-01\nbad,x,31/10/2026\nok,2,2026-01-02\n").unwrap();
        assert_eq!(parsed.rows.len(), 2);
        assert_eq!(parsed.problems.len(), 1);
        let problem = &parsed.problems[0];
        assert_eq!((problem.row, problem.line), (2, Some(3)));
        assert_eq!(problem.details.len(), 2);
        assert!(problem.details[0].contains("qty") && problem.details[1].contains("due"));
    }

    #[test]
    fn csv_files_with_bad_headers_are_refused_whole() {
        let refused = |text: &str| match csv(text) {
            Err(FileError::Invalid(problems)) => problems.join("; "),
            other => panic!("expected a refusal, got {other:?}"),
        };
        assert!(refused("title,nope\na,b\n").contains("unknown column `nope`"));
        assert!(refused("title,title\na,b\n").contains("duplicate column `title`"));
        let table = refused("title,lines\na,b\n");
        assert!(table.contains("child tables is not supported") && table.contains("use JSON"));
        assert!(refused("title,qty\na\n").contains("cannot be read"));
        assert!(refused("").contains("no header"));
        // Several problems are reported together.
        let all = refused("x,y,y\n1,2,3\n");
        assert!(all.contains("unknown column `x`") && all.contains("duplicate column `y`"));
        // An upsert needs the name column.
        let upsert = parse_import(
            &registry(),
            &customer(),
            Format::Csv,
            ImportMode::Upsert,
            b"title\na\n",
            10,
        );
        assert!(matches!(upsert, Err(FileError::Invalid(p)) if p[0].contains("`name` column")));
        // Not UTF-8.
        let latin1 = parse_import(
            &registry(),
            &customer(),
            Format::Csv,
            ImportMode::Insert,
            b"title\n\xe9\n",
            10,
        );
        assert!(matches!(latin1, Err(FileError::Invalid(p)) if p[0].contains("UTF-8")));
    }

    #[test]
    fn the_row_limit_is_checked_while_reading() {
        let rows = "title\na\nb\nc\n";
        let result = parse_import(
            &registry(),
            &customer(),
            Format::Csv,
            ImportMode::Insert,
            rows.as_bytes(),
            2,
        );
        assert_eq!(result.unwrap_err(), FileError::TooLarge { limit: 2 });
        let json = parse_import(
            &registry(),
            &customer(),
            Format::Json,
            ImportMode::Insert,
            br#"[{"title":"a"},{"title":"b"},{"title":"c"}]"#,
            2,
        );
        assert_eq!(json.unwrap_err(), FileError::TooLarge { limit: 2 });
    }

    #[test]
    fn json_rows_may_carry_child_rows_and_ignore_the_standard_keys_of_an_export() {
        let parsed = parse_import(
            &registry(),
            &customer(),
            Format::Json,
            ImportMode::Insert,
            br#"[{"id":"x","name":"N1","owner":"ann","version":3,"title":"t","qty":2,
                 "lines":[{"item":"a"},{"item":"b"}]}, 5, {"name": 7}, {"qty":"x"}]"#,
            10,
        )
        .unwrap();
        assert_eq!(parsed.rows.len(), 1);
        let row = &parsed.rows[0];
        assert_eq!(row.name.as_deref(), Some("N1"));
        assert_eq!(row.tables["lines"].len(), 2);
        assert_eq!(row.values.len(), 2);
        let rows: Vec<usize> = parsed.problems.iter().map(|p| p.row).collect();
        assert_eq!(rows, [2, 3, 4]);
        assert!(parsed.problems[0].details[0].contains("object"));
        let refused = parse_import(
            &registry(),
            &customer(),
            Format::Json,
            ImportMode::Insert,
            b"{}",
            10,
        );
        assert!(matches!(refused, Err(FileError::Invalid(_))));
    }

    #[test]
    fn a_header_with_too_many_columns_is_one_short_problem() {
        let header = vec!["a"; 100_000].join(",");
        let started = std::time::Instant::now();
        let Err(FileError::Invalid(problems)) = csv(&format!("{header}\n")) else {
            panic!("expected a refusal");
        };
        assert_eq!(problems.len(), 1);
        assert!(
            problems[0].contains("100000 columns; at most"),
            "{problems:?}"
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
    }

    #[test]
    fn the_problems_of_a_header_are_capped() {
        // As many columns as allowed (6 fields + name + 9 standard = 16 here), all unknown.
        let header = (0..16)
            .map(|i| format!("x{i}"))
            .collect::<Vec<_>>()
            .join(",");
        let Err(FileError::Invalid(problems)) = csv(&format!("{header}\n")) else {
            panic!("expected a refusal");
        };
        assert_eq!(problems.len(), 16);
        let many: Vec<String> = (0..250).map(|i| i.to_string()).collect();
        let capped = capped(many);
        assert_eq!(capped.len(), MAX_REPORTED_ERRORS + 1);
        assert_eq!(capped.last().map(String::as_str), Some("and 150 more"));
    }

    #[test]
    fn csv_ignores_the_standard_columns_of_an_export() {
        let parsed = csv("name,owner,modified_at,version,title,docstatus\nN,ann,2026-01-01T00:00:00Z,3,Hello,0\n").unwrap();
        assert!(parsed.problems.is_empty(), "{:?}", parsed.problems);
        let row = &parsed.rows[0];
        assert_eq!(row.name.as_deref(), Some("N"));
        assert_eq!(row.values.len(), 1);
        assert!(row.empty.is_empty());
    }

    #[test]
    fn child_rows_count_toward_the_row_limit() {
        let file = br#"[{"title":"a","lines":[{"item":"1"},{"item":"2"}]},{"title":"b","lines":[{"item":"3"}]}]"#;
        let run = |limit| {
            parse_import(
                &registry(),
                &customer(),
                Format::Json,
                ImportMode::Insert,
                file,
                limit,
            )
        };
        // 2 documents + 3 child rows = 5.
        assert!(run(5).is_ok());
        assert_eq!(run(4).unwrap_err(), FileError::TooLarge { limit: 4 });
    }

    #[test]
    fn a_text_is_guarded_whatever_else_is() {
        assert_eq!(csv_cell("f", &Value::Text("=1".into())).unwrap(), "'=1");
        assert_eq!(csv_cell("f", &Value::Int(-1)).unwrap(), "-1");
    }
}
