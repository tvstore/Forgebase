//! Pure schema planning: compares DocType metadata with a snapshot of the live schema.
//!
//! No I/O here; [`crate::sync`] loads the snapshot and executes the plan. Only additive steps
//! are ever generated. Generated SQL contains validated, quoted identifiers and numeric type
//! parameters only, never values.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use forgebase_core::ident::table_name;
use forgebase_core::meta::{DocField, DocType, FieldType, Registry};

use crate::ident::quote_ident;

const TIMESTAMPTZ: &str = "timestamp with time zone";

/// Standard columns of every DocType table: (name, PostgreSQL type). All are `NOT NULL`.
const STANDARD_COLUMNS: [(&str, &str); 7] = [
    ("id", "uuid"),
    ("name", "text"),
    ("owner", "text"),
    ("created_at", TIMESTAMPTZ),
    ("modified_at", TIMESTAMPTZ),
    ("modified_by", "text"),
    ("version", "bigint"),
];

/// One change the sync would make. Executed in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Create the table of a new DocType: columns only, no keys (see [`Step::AddKeys`]).
    CreateTable {
        /// DocType name.
        doctype: String,
        /// The `CREATE TABLE` statement.
        sql: String,
    },
    /// Add the primary key and unique constraints of a new table. Runs after every new table of
    /// the sync exists, so the names PostgreSQL picks for them cannot clash with a table name.
    AddKeys {
        /// DocType name.
        doctype: String,
        /// The `ALTER TABLE` statement.
        sql: String,
    },
    /// Add the foreign keys of the Link fields of a new table, or of Link columns added to an
    /// existing table. Runs after every new table has its primary key, so links between new
    /// tables (also cycles and self-links) work in one sync.
    AddForeignKeys {
        /// DocType name.
        doctype: String,
        /// The `ALTER TABLE` statement.
        sql: String,
    },
    /// Add a nullable column for a field added to an existing DocType.
    AddColumn {
        /// DocType name.
        doctype: String,
        /// Column name.
        column: String,
        /// The `ALTER TABLE` statement.
        sql: String,
    },
    /// Add a unique index for a `unique` field on an existing table.
    CreateUniqueIndex {
        /// DocType name.
        doctype: String,
        /// Column name.
        column: String,
        /// The `CREATE UNIQUE INDEX` statement.
        sql: String,
    },
    /// Add a non-unique index on a Link column (PostgreSQL does not index foreign keys itself).
    CreateIndex {
        /// DocType name.
        doctype: String,
        /// Column name.
        column: String,
        /// The `CREATE INDEX` statement.
        sql: String,
    },
    /// Record the DocType in `fb_doctype` (executed with bound parameters).
    RegisterDocType {
        /// DocType name.
        doctype: String,
    },
}

impl fmt::Display for Step {
    /// The SQL of the step, for dry-run output. `RegisterDocType` is shown as a comment.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Step::CreateTable { sql, .. }
            | Step::AddKeys { sql, .. }
            | Step::AddForeignKeys { sql, .. }
            | Step::AddColumn { sql, .. }
            | Step::CreateUniqueIndex { sql, .. }
            | Step::CreateIndex { sql, .. } => write!(f, "{sql};"),
            Step::RegisterDocType { doctype } => {
                write!(f, "-- register DocType {doctype:?} in fb_doctype")
            }
        }
    }
}

/// A difference that cannot be applied additively. Any conflict refuses the whole sync.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Conflict {
    /// The table `doc_<name>` exists but was not created by Forgebase.
    UnmanagedTable {
        /// Table name.
        table: String,
    },
    /// Something other than a usable table (index, view, sequence, type, a table with no visible columns, ...) already has the
    /// name `doc_<name>`, so the table cannot be created.
    RelationNameTaken {
        /// The relation name (the wanted table name).
        name: String,
    },
    /// A foreign key does not match the Link field: a Link column without the foreign key to the
    /// target's `id`, a foreign key to another table or column, a delete rule other than
    /// `RESTRICT`, or a foreign key on a column that is not a Link.
    ForeignKeyMismatch {
        /// Table name.
        table: String,
        /// Column name.
        column: String,
        /// What the metadata requires.
        expected: String,
        /// What the database has (`missing` if absent).
        actual: String,
    },
    /// A column exists with a different type (or, for standard columns, a different
    /// nullability, uniqueness or primary key), or a standard column is missing.
    ColumnMismatch {
        /// Table name.
        table: String,
        /// Column name.
        column: String,
        /// What the metadata requires.
        expected: String,
        /// What the database has (`missing` if absent).
        actual: String,
    },
}

impl fmt::Display for Conflict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Conflict::UnmanagedTable { table } => write!(
                f,
                "table {table} already exists but is not managed by Forgebase (not adopted)"
            ),
            Conflict::RelationNameTaken { name } => write!(
                f,
                "cannot create table {name}: a relation or type with that name already exists"
            ),
            Conflict::ColumnMismatch {
                table,
                column,
                expected,
                actual,
            } => write!(
                f,
                "column {table}.{column} is {actual} in the database but the metadata requires {expected}"
            ),
            Conflict::ForeignKeyMismatch {
                table,
                column,
                expected,
                actual,
            } => write!(
                f,
                "foreign key on {table}.{column} is {actual} in the database but the metadata requires {expected}"
            ),
        }
    }
}

/// Something noticed but intentionally not changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Warning {
    /// A column exists that no field describes (field removed); the column is kept.
    ColumnNotInMetadata {
        /// Table name.
        table: String,
        /// Column name.
        column: String,
    },
    /// A field is no longer `unique` but its unique index remains.
    UniqueRemoved {
        /// Table name.
        table: String,
        /// Column name.
        column: String,
    },
    /// A field column is `NOT NULL` in the database, but fields are nullable (`required` is
    /// enforced by the application), so inserts may fail. Reported, not changed.
    ColumnNotNullable {
        /// Table name.
        table: String,
        /// Column name.
        column: String,
    },
    /// A DocType is recorded in `fb_doctype` but not in the registry; its table is kept.
    DocTypeNotInRegistry {
        /// DocType name.
        doctype: String,
    },
}

impl fmt::Display for Warning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Warning::ColumnNotInMetadata { table, column } => {
                write!(f, "column {table}.{column} is not in the metadata; kept")
            }
            Warning::UniqueRemoved { table, column } => write!(
                f,
                "field {table}.{column} is no longer unique in the metadata; unique index kept"
            ),
            Warning::ColumnNotNullable { table, column } => write!(
                f,
                "column {table}.{column} is NOT NULL in the database but fields are nullable; left as is"
            ),
            Warning::DocTypeNotInRegistry { doctype } => write!(
                f,
                "DocType {doctype:?} is registered in the database but not in the metadata; table kept"
            ),
        }
    }
}

/// A column as found in the live schema.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Column {
    /// Rendered like the DDL type: `bigint`, `character varying(140)`, `numeric(12,2)`.
    pub(crate) data_type: String,
    pub(crate) not_null: bool,
}

/// A single-column foreign key as found in the live schema.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ForeignKey {
    /// Referenced table (`schema.table` when it is not in the current schema).
    pub(crate) table: String,
    pub(crate) column: String,
    /// `pg_constraint.confdeltype`: `a` no action, `r` restrict, `c` cascade, `n` set null,
    /// `d` set default.
    pub(crate) on_delete: char,
    /// `pg_constraint.convalidated`: `false` for a `NOT VALID` constraint, whose existing rows
    /// were never checked.
    pub(crate) validated: bool,
}

impl ForeignKey {
    fn describe(&self) -> String {
        let rule = match self.on_delete {
            'a' => "NO ACTION",
            'r' => "RESTRICT",
            'c' => "CASCADE",
            'n' => "SET NULL",
            'd' => "SET DEFAULT",
            _ => "an unknown rule",
        };
        let table = match self.table.split_once('.') {
            Some((schema, table)) => format!("{}.{}", quote_ident(schema), quote_ident(table)),
            None => quote_ident(&self.table),
        };
        let validated = if self.validated {
            ""
        } else {
            " (not validated)"
        };
        format!(
            "REFERENCES {table} ({}) ON DELETE {rule}{validated}",
            quote_ident(&self.column)
        )
    }
}

/// The foreign key a Link field to `target` must have.
fn expected_foreign_key(target: &str) -> ForeignKey {
    ForeignKey {
        table: table_name(target),
        column: "id".to_owned(),
        on_delete: 'r',
        validated: true,
    }
}

/// Snapshot of the live schema, limited to the tables of the registry.
#[derive(Debug, Default)]
pub(crate) struct Catalog {
    /// table -> column -> definition. A table without columns is absent.
    pub(crate) columns: BTreeMap<String, BTreeMap<String, Column>>,
    /// (table, column) pairs covered by a valid single-column unique index (not a primary key).
    pub(crate) unique: BTreeSet<(String, String)>,
    /// (table, column) pairs that are a single-column primary key.
    pub(crate) primary: BTreeSet<(String, String)>,
    /// (table, column) pairs covered by a valid, single-column, non-partial index of any kind
    /// (including unique indexes and primary keys).
    pub(crate) indexed: BTreeSet<(String, String)>,
    /// (table, column) -> the single-column foreign keys on that column.
    pub(crate) foreign_keys: BTreeMap<(String, String), Vec<ForeignKey>>,
    /// Wanted table names already taken in the schema by a relation of any kind (table, index,
    /// view, sequence, ...) or by a standalone type (domain, enum, ...).
    pub(crate) relations: BTreeSet<String>,
    /// The subset of `relations` that are ordinary (or partitioned) tables.
    pub(crate) tables: BTreeSet<String>,
    /// DocType names recorded in `fb_doctype`.
    pub(crate) managed: BTreeSet<String>,
}

/// The outcome of comparing the registry with the live schema.
#[derive(Debug, Default)]
pub(crate) struct Plan {
    pub(crate) steps: Vec<Step>,
    pub(crate) conflicts: Vec<Conflict>,
    pub(crate) warnings: Vec<Warning>,
}

/// PostgreSQL type of a field column. Also what `information_schema` reports once rendered.
fn pg_type(field_type: &FieldType) -> String {
    match field_type {
        FieldType::Data { max_length } => format!("character varying({max_length})"),
        FieldType::Text | FieldType::Select { .. } => "text".to_owned(),
        FieldType::Int => "bigint".to_owned(),
        FieldType::Decimal { precision, scale } => format!("numeric({precision},{scale})"),
        FieldType::Check => "boolean".to_owned(),
        FieldType::Date => "date".to_owned(),
        FieldType::Datetime => TIMESTAMPTZ.to_owned(),
        FieldType::Link { .. } => "uuid".to_owned(),
    }
}

fn standard_column_sql(name: &str, data_type: &str) -> String {
    let constraint = match name {
        "version" => "NOT NULL DEFAULT 1",
        _ => "NOT NULL",
    };
    format!("{} {data_type} {constraint}", quote_ident(name))
}

fn create_table_sql(doctype: &DocType, table: &str) -> String {
    let mut lines: Vec<String> = STANDARD_COLUMNS
        .iter()
        .map(|(name, data_type)| standard_column_sql(name, data_type))
        .collect();
    for field in &doctype.fields {
        lines.push(format!(
            "{} {}",
            quote_ident(&field.name),
            pg_type(&field.field_type)
        ));
    }
    format!(
        "CREATE TABLE {} (\n    {}\n)",
        quote_ident(table),
        lines.join(",\n    ")
    )
}

/// Keys of a new table, added once all new tables exist. Unnamed on purpose: PostgreSQL picks
/// free names (`<table>_<column>_key`, adjusted if taken or too long). Because every table of
/// the sync already exists, none of those names can clash with a table name.
fn add_keys_sql(doctype: &DocType, table: &str) -> String {
    let mut clauses = vec![
        format!("ADD PRIMARY KEY ({})", quote_ident("id")),
        format!("ADD UNIQUE ({})", quote_ident("name")),
    ];
    clauses.extend(
        doctype
            .fields
            .iter()
            .filter(|f| f.unique)
            .map(|f| format!("ADD UNIQUE ({})", quote_ident(&f.name))),
    );
    format!("ALTER TABLE {} {}", quote_ident(table), clauses.join(", "))
}

/// Foreign keys of the given Link fields in one statement. Unnamed on purpose, like the keys.
/// The delete rule is `RESTRICT`: a document that is still linked cannot be deleted.
fn add_foreign_keys_sql(table: &str, links: &[(&DocField, &str)]) -> String {
    let clauses: Vec<String> = links
        .iter()
        .map(|(field, target)| {
            format!(
                "ADD FOREIGN KEY ({}) REFERENCES {} ({}) ON DELETE RESTRICT",
                quote_ident(&field.name),
                quote_ident(&table_name(target)),
                quote_ident("id")
            )
        })
        .collect();
    format!("ALTER TABLE {} {}", quote_ident(table), clauses.join(", "))
}

fn link_index_sql(table: &str, column: &str) -> String {
    format!(
        "CREATE INDEX ON {} ({})",
        quote_ident(table),
        quote_ident(column)
    )
}

/// The Link fields of a DocType with their targets.
fn link_fields(doctype: &DocType) -> Vec<(&DocField, &str)> {
    doctype
        .fields
        .iter()
        .filter_map(|f| match &f.field_type {
            FieldType::Link { target } => Some((f, target.as_str())),
            _ => None,
        })
        .collect()
}

fn add_column_sql(table: &str, field: &DocField) -> String {
    format!(
        "ALTER TABLE {} ADD COLUMN {} {}",
        quote_ident(table),
        quote_ident(&field.name),
        pg_type(&field.field_type)
    )
}

fn unique_index_sql(table: &str, column: &str) -> String {
    format!(
        "CREATE UNIQUE INDEX ON {} ({})",
        quote_ident(table),
        quote_ident(column)
    )
}

/// Steps grouped by phase. Order of execution: all new tables, then their primary and unique
/// keys, then their foreign keys (every primary key exists by then), then added columns, then
/// the foreign keys of added Link columns, then late unique and Link indexes, then registration.
#[derive(Default)]
struct Phases {
    create_tables: Vec<Step>,
    add_keys: Vec<Step>,
    add_foreign_keys: Vec<Step>,
    add_columns: Vec<Step>,
    add_column_foreign_keys: Vec<Step>,
    indexes: Vec<Step>,
    register: Vec<Step>,
}

/// Compares every DocType in `registry` with `catalog`.
pub(crate) fn plan(registry: &Registry, catalog: &Catalog) -> Plan {
    let mut plan = Plan::default();
    let mut phases = Phases::default();
    for doctype in registry.iter() {
        plan_doctype(doctype, catalog, &mut plan, &mut phases);
    }
    plan.steps = [
        phases.create_tables,
        phases.add_keys,
        phases.add_foreign_keys,
        phases.add_columns,
        phases.add_column_foreign_keys,
        phases.indexes,
        phases.register,
    ]
    .concat();
    for managed in &catalog.managed {
        if registry.get(managed).is_none() {
            plan.warnings.push(Warning::DocTypeNotInRegistry {
                doctype: managed.clone(),
            });
        }
    }
    plan
}

fn plan_doctype(doctype: &DocType, catalog: &Catalog, plan: &mut Plan, phases: &mut Phases) {
    let table = table_name(&doctype.name);
    let managed = catalog.managed.contains(&doctype.name);
    let taken = catalog.relations.contains(&table);
    // Only an ordinary table is ever "unmanaged"; anything else with the name is a name clash.
    if taken && !catalog.tables.contains(&table) {
        plan.conflicts
            .push(Conflict::RelationNameTaken { name: table });
        return;
    }
    let Some(columns) = catalog.columns.get(&table) else {
        if taken {
            // An ordinary table with no visible columns: cannot be created over, not adopted.
            plan.conflicts
                .push(Conflict::RelationNameTaken { name: table });
            return;
        }
        phases.create_tables.push(Step::CreateTable {
            doctype: doctype.name.clone(),
            sql: create_table_sql(doctype, &table),
        });
        phases.add_keys.push(Step::AddKeys {
            doctype: doctype.name.clone(),
            sql: add_keys_sql(doctype, &table),
        });
        let links = link_fields(doctype);
        if !links.is_empty() {
            phases.add_foreign_keys.push(Step::AddForeignKeys {
                doctype: doctype.name.clone(),
                sql: add_foreign_keys_sql(&table, &links),
            });
        }
        // A unique Link field is already indexed by its unique constraint.
        for (field, _) in links.iter().filter(|(f, _)| !f.unique) {
            phases.indexes.push(Step::CreateIndex {
                doctype: doctype.name.clone(),
                column: field.name.clone(),
                sql: link_index_sql(&table, &field.name),
            });
        }
        if !managed {
            phases.register.push(Step::RegisterDocType {
                doctype: doctype.name.clone(),
            });
        }
        return;
    };
    if !managed {
        plan.conflicts.push(Conflict::UnmanagedTable { table });
        return;
    }
    for (name, data_type) in STANDARD_COLUMNS {
        let mismatch = |actual: String| Conflict::ColumnMismatch {
            table: table.clone(),
            column: name.to_owned(),
            expected: format!("{data_type} NOT NULL"),
            actual,
        };
        match columns.get(name) {
            None => plan.conflicts.push(mismatch("missing".to_owned())),
            Some(c) if c.data_type != data_type || !c.not_null => {
                let null = if c.not_null { " NOT NULL" } else { "" };
                plan.conflicts
                    .push(mismatch(format!("{}{null}", c.data_type)));
            }
            Some(_) => {}
        }
    }
    let key = |column: &str| (table.clone(), column.to_owned());
    for (column, present, expected, actual) in [
        (
            "id",
            catalog.primary.contains(&key("id")),
            "primary key",
            "not a primary key",
        ),
        (
            "name",
            catalog.unique.contains(&key("name")),
            "unique",
            "not unique",
        ),
    ] {
        if columns.contains_key(column) && !present {
            plan.conflicts.push(Conflict::ColumnMismatch {
                table: table.clone(),
                column: column.to_owned(),
                expected: expected.to_owned(),
                actual: actual.to_owned(),
            });
        }
    }
    let mut added_links: Vec<(&DocField, &str)> = Vec::new();
    for field in &doctype.fields {
        let key = (table.clone(), field.name.clone());
        let has_unique = catalog.unique.contains(&key);
        let target = match &field.field_type {
            FieldType::Link { target } => Some(target.as_str()),
            _ => None,
        };
        let added = !columns.contains_key(&field.name);
        match columns.get(&field.name) {
            None => {
                phases.add_columns.push(Step::AddColumn {
                    doctype: doctype.name.clone(),
                    column: field.name.clone(),
                    sql: add_column_sql(&table, field),
                });
                if let Some(target) = target {
                    added_links.push((field, target));
                }
            }
            Some(c) if c.data_type != pg_type(&field.field_type) => {
                plan.conflicts.push(Conflict::ColumnMismatch {
                    table: table.clone(),
                    column: field.name.clone(),
                    expected: pg_type(&field.field_type),
                    actual: c.data_type.clone(),
                });
                continue;
            }
            Some(c) if c.not_null => plan.warnings.push(Warning::ColumnNotNullable {
                table: table.clone(),
                column: field.name.clone(),
            }),
            Some(_) => {}
        }
        if field.unique && !has_unique {
            phases.indexes.push(Step::CreateUniqueIndex {
                doctype: doctype.name.clone(),
                column: field.name.clone(),
                sql: unique_index_sql(&table, &field.name),
            });
        } else if !field.unique && has_unique {
            plan.warnings.push(Warning::UniqueRemoved {
                table: table.clone(),
                column: field.name.clone(),
            });
        }
        let foreign_keys = catalog
            .foreign_keys
            .get(&key)
            .map_or(&[][..], Vec::as_slice);
        match target {
            // A column added in this sync gets its foreign key (and index) in the same sync.
            Some(_) if added => {}
            Some(target) => {
                check_foreign_keys(
                    &table,
                    &field.name,
                    target,
                    foreign_keys,
                    &mut plan.conflicts,
                );
            }
            None => {
                for fk in foreign_keys {
                    plan.conflicts.push(Conflict::ForeignKeyMismatch {
                        table: table.clone(),
                        column: field.name.clone(),
                        expected: "no foreign key".to_owned(),
                        actual: fk.describe(),
                    });
                }
            }
        }
        // The unique index of a unique field covers lookups; otherwise a Link column needs a
        // plain index (also when it is only missing, for example dropped by hand).
        if target.is_some() && !field.unique && !catalog.indexed.contains(&key) {
            phases.indexes.push(Step::CreateIndex {
                doctype: doctype.name.clone(),
                column: field.name.clone(),
                sql: link_index_sql(&table, &field.name),
            });
        }
    }
    if !added_links.is_empty() {
        phases.add_column_foreign_keys.push(Step::AddForeignKeys {
            doctype: doctype.name.clone(),
            sql: add_foreign_keys_sql(&table, &added_links),
        });
    }
    for column in columns.keys() {
        let known =
            STANDARD_COLUMNS.iter().any(|(n, _)| n == column) || doctype.field(column).is_some();
        if !known {
            plan.warnings.push(Warning::ColumnNotInMetadata {
                table: table.clone(),
                column: column.clone(),
            });
        }
    }
}

/// A Link column that exists in the database must have exactly the expected foreign key.
fn check_foreign_keys(
    table: &str,
    column: &str,
    target: &str,
    found: &[ForeignKey],
    conflicts: &mut Vec<Conflict>,
) {
    let expected = expected_foreign_key(target);
    let mismatch = |actual: String| Conflict::ForeignKeyMismatch {
        table: table.to_owned(),
        column: column.to_owned(),
        expected: expected.describe(),
        actual,
    };
    if found.is_empty() {
        conflicts.push(mismatch("missing".to_owned()));
    }
    for fk in found.iter().filter(|fk| **fk != expected) {
        conflicts.push(mismatch(fk.describe()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_not_validated_foreign_key_is_a_conflict() {
        let expected = expected_foreign_key("item");
        let found = ForeignKey {
            validated: false,
            ..expected.clone()
        };
        let mut conflicts = Vec::new();
        check_foreign_keys("doc_x", "link", "item", &[found], &mut conflicts);
        assert!(
            matches!(&conflicts[..], [Conflict::ForeignKeyMismatch { actual, .. }] if actual.contains("not validated")),
            "{conflicts:?}"
        );
    }

    fn registry(toml: &str) -> Registry {
        let mut registry = Registry::new();
        registry.add(DocType::from_toml_str(toml).unwrap()).unwrap();
        registry
    }

    const ITEM: &str = r#"
name = "item"
label = "Item"
[[fields]]
name = "code"
type = "data"
max_length = 20
unique = true
[[fields]]
name = "price"
type = "decimal"
precision = 10
scale = 2
"#;

    #[test]
    fn new_doctype_creates_table_and_registers() {
        let plan = plan(&registry(ITEM), &Catalog::default());
        assert_eq!(plan.steps.len(), 3);
        let Step::CreateTable { sql, .. } = &plan.steps[0] else {
            panic!("expected CreateTable");
        };
        assert_eq!(
            sql,
            "CREATE TABLE \"doc_item\" (\n    \
             \"id\" uuid NOT NULL,\n    \
             \"name\" text NOT NULL,\n    \
             \"owner\" text NOT NULL,\n    \
             \"created_at\" timestamp with time zone NOT NULL,\n    \
             \"modified_at\" timestamp with time zone NOT NULL,\n    \
             \"modified_by\" text NOT NULL,\n    \
             \"version\" bigint NOT NULL DEFAULT 1,\n    \
             \"code\" character varying(20),\n    \
             \"price\" numeric(10,2)\n)"
        );
        let Step::AddKeys { sql, .. } = &plan.steps[1] else {
            panic!("expected AddKeys");
        };
        assert_eq!(
            sql,
            "ALTER TABLE \"doc_item\" ADD PRIMARY KEY (\"id\"), ADD UNIQUE (\"name\"), ADD UNIQUE (\"code\")"
        );
        assert_eq!(
            plan.steps[2],
            Step::RegisterDocType {
                doctype: "item".into()
            }
        );
    }

    /// A catalog that matches `ITEM` exactly.
    fn synced_catalog() -> Catalog {
        let col = |t: &str| Column {
            data_type: t.into(),
            not_null: false,
        };
        let mut columns: BTreeMap<String, Column> = STANDARD_COLUMNS
            .iter()
            .map(|(n, t)| {
                (
                    (*n).to_owned(),
                    Column {
                        data_type: (*t).into(),
                        not_null: true,
                    },
                )
            })
            .collect();
        columns.insert("code".into(), col("character varying(20)"));
        columns.insert("price".into(), col("numeric(10,2)"));
        Catalog {
            columns: BTreeMap::from([("doc_item".to_owned(), columns)]),
            unique: BTreeSet::from([
                ("doc_item".to_owned(), "code".to_owned()),
                ("doc_item".to_owned(), "name".to_owned()),
            ]),
            primary: BTreeSet::from([("doc_item".to_owned(), "id".to_owned())]),
            relations: BTreeSet::from(["doc_item".to_owned()]),
            tables: BTreeSet::from(["doc_item".to_owned()]),
            managed: BTreeSet::from(["item".to_owned()]),
            ..Catalog::default()
        }
    }

    #[test]
    fn matching_schema_is_a_no_op() {
        let plan = plan(&registry(ITEM), &synced_catalog());
        assert!(plan.steps.is_empty() && plan.conflicts.is_empty() && plan.warnings.is_empty());
    }

    #[test]
    fn differences_are_classified() {
        let mut catalog = synced_catalog();
        let columns = catalog.columns.get_mut("doc_item").unwrap();
        columns.get_mut("code").unwrap().data_type = "character varying(10)".into();
        columns.remove("price");
        columns.insert("legacy".into(), columns["code"].clone());
        columns.get_mut("owner").unwrap().not_null = false;
        catalog.managed.insert("gone".into());
        let plan = plan(&registry(ITEM), &catalog);
        assert_eq!(plan.conflicts.len(), 2);
        assert!(matches!(&plan.steps[..], [Step::AddColumn { column, .. }] if column == "price"));
        assert!(plan.warnings.contains(&Warning::ColumnNotInMetadata {
            table: "doc_item".into(),
            column: "legacy".into()
        }));
        assert!(plan.warnings.contains(&Warning::DocTypeNotInRegistry {
            doctype: "gone".into()
        }));
    }

    #[test]
    fn unmanaged_table_conflicts() {
        let mut catalog = synced_catalog();
        catalog.managed.clear();
        let plan = plan(&registry(ITEM), &catalog);
        assert_eq!(
            plan.conflicts,
            vec![Conflict::UnmanagedTable {
                table: "doc_item".into()
            }]
        );
        assert!(plan.steps.is_empty());
    }

    #[test]
    fn unique_added_and_removed() {
        let mut catalog = synced_catalog();
        catalog.unique.clear();
        let added = plan(&registry(ITEM), &catalog);
        assert!(
            matches!(&added.steps[..], [Step::CreateUniqueIndex { sql, .. }]
            if sql == "CREATE UNIQUE INDEX ON \"doc_item\" (\"code\")")
        );
        let without_unique = ITEM.replace("unique = true", "");
        let removed = plan(&registry(&without_unique), &synced_catalog());
        assert_eq!(
            removed.warnings,
            vec![Warning::UniqueRemoved {
                table: "doc_item".into(),
                column: "code".into()
            }]
        );
    }

    #[test]
    fn standard_key_drift_is_a_conflict() {
        let mut catalog = synced_catalog();
        catalog
            .unique
            .remove(&("doc_item".to_owned(), "name".to_owned()));
        catalog.primary.clear();
        let plan = plan(&registry(ITEM), &catalog);
        let columns: Vec<&str> = plan
            .conflicts
            .iter()
            .map(|c| match c {
                Conflict::ColumnMismatch { column, .. } => column.as_str(),
                other => panic!("{other}"),
            })
            .collect();
        assert_eq!(columns, ["id", "name"]);
    }

    #[test]
    fn not_null_field_column_is_a_warning() {
        let mut catalog = synced_catalog();
        catalog
            .columns
            .get_mut("doc_item")
            .unwrap()
            .get_mut("price")
            .unwrap()
            .not_null = true;
        let plan = plan(&registry(ITEM), &catalog);
        assert!(plan.conflicts.is_empty() && plan.steps.is_empty());
        assert_eq!(
            plan.warnings,
            vec![Warning::ColumnNotNullable {
                table: "doc_item".into(),
                column: "price".into()
            }]
        );
    }

    #[test]
    fn non_table_relation_with_the_table_name_is_a_conflict() {
        let mut catalog = Catalog::default();
        catalog.relations.insert("doc_item".to_owned());
        let plan = plan(&registry(ITEM), &catalog);
        assert_eq!(
            plan.conflicts,
            vec![Conflict::RelationNameTaken {
                name: "doc_item".into()
            }]
        );
        assert!(plan.steps.is_empty());
    }

    #[test]
    fn steps_run_in_phases_across_doctypes() {
        let mut registry = Registry::new();
        for name in ["item", "item_code_key"] {
            let toml = format!(
                "name = \"{name}\"\nlabel = \"L\"\n[[fields]]\nname = \"c\"\ntype = \"data\"\nunique = true\n"
            );
            registry
                .add(DocType::from_toml_str(&toml).unwrap())
                .unwrap();
        }
        // An existing table `doc_old` gains a column and a unique index in the same sync.
        let old = "name = \"old\"\nlabel = \"L\"\n[[fields]]\nname = \"c\"\ntype = \"data\"\nunique = true\n";
        registry.add(DocType::from_toml_str(old).unwrap()).unwrap();
        let mut catalog = Catalog::default();
        let mut columns: BTreeMap<String, Column> = STANDARD_COLUMNS
            .iter()
            .map(|(n, t)| {
                (
                    (*n).to_owned(),
                    Column {
                        data_type: (*t).into(),
                        not_null: true,
                    },
                )
            })
            .collect();
        columns.insert(
            "c".into(),
            Column {
                data_type: "character varying(140)".into(),
                not_null: false,
            },
        );
        catalog.columns.insert("doc_old".into(), columns);
        catalog.unique.insert(("doc_old".into(), "name".into()));
        catalog.primary.insert(("doc_old".into(), "id".into()));
        catalog.relations.insert("doc_old".into());
        catalog.tables.insert("doc_old".into());
        catalog.managed.insert("old".into());
        let plan = plan(&registry, &catalog);
        let kinds: Vec<String> = plan
            .steps
            .iter()
            .map(|s| match s {
                Step::CreateTable { doctype, .. } => format!("table:{doctype}"),
                Step::AddKeys { doctype, .. } => format!("keys:{doctype}"),
                Step::AddForeignKeys { doctype, .. } => format!("fks:{doctype}"),
                Step::AddColumn { doctype, .. } => format!("column:{doctype}"),
                Step::CreateUniqueIndex { doctype, .. } | Step::CreateIndex { doctype, .. } => {
                    format!("index:{doctype}")
                }
                Step::RegisterDocType { doctype } => format!("register:{doctype}"),
            })
            .collect();
        assert_eq!(
            kinds,
            [
                "table:item",
                "table:item_code_key",
                "keys:item",
                "keys:item_code_key",
                "index:old",
                "register:item",
                "register:item_code_key"
            ]
        );
    }

    const GROUP: &str = "name = \"grp\"\nlabel = \"G\"\n";

    fn order(link_extra: &str) -> String {
        format!(
            "name = \"ord\"\nlabel = \"O\"\n[[fields]]\nname = \"grp\"\ntype = \"link\"\ntarget = \"grp\"\n{link_extra}\n\
             [[fields]]\nname = \"prev\"\ntype = \"link\"\ntarget = \"ord\"\nunique = true\n"
        )
    }

    fn registry_of(texts: &[&str]) -> Registry {
        let mut registry = Registry::new();
        for text in texts {
            registry.add(DocType::from_toml_str(text).unwrap()).unwrap();
        }
        registry
    }

    fn std_columns() -> BTreeMap<String, Column> {
        STANDARD_COLUMNS
            .iter()
            .map(|(n, t)| {
                (
                    (*n).to_owned(),
                    Column {
                        data_type: (*t).into(),
                        not_null: true,
                    },
                )
            })
            .collect()
    }

    /// A catalog where `grp` and `ord` exist and match (`ord` has Link columns `grp` and `prev`).
    fn linked_catalog() -> Catalog {
        let uuid = || Column {
            data_type: "uuid".into(),
            not_null: false,
        };
        let mut ord = std_columns();
        ord.insert("grp".into(), uuid());
        ord.insert("prev".into(), uuid());
        let key = |t: &str, c: &str| (t.to_owned(), c.to_owned());
        let fk = |table: &str| ForeignKey {
            table: table.into(),
            column: "id".into(),
            on_delete: 'r',
            validated: true,
        };
        Catalog {
            columns: BTreeMap::from([
                ("doc_grp".to_owned(), std_columns()),
                ("doc_ord".to_owned(), ord),
            ]),
            unique: BTreeSet::from([
                key("doc_grp", "name"),
                key("doc_ord", "name"),
                key("doc_ord", "prev"),
            ]),
            primary: BTreeSet::from([key("doc_grp", "id"), key("doc_ord", "id")]),
            indexed: BTreeSet::from([
                key("doc_grp", "id"),
                key("doc_grp", "name"),
                key("doc_ord", "id"),
                key("doc_ord", "name"),
                key("doc_ord", "prev"),
                key("doc_ord", "grp"),
            ]),
            foreign_keys: BTreeMap::from([
                (key("doc_ord", "grp"), vec![fk("doc_grp")]),
                (key("doc_ord", "prev"), vec![fk("doc_ord")]),
            ]),
            relations: BTreeSet::from(["doc_grp".to_owned(), "doc_ord".to_owned()]),
            tables: BTreeSet::from(["doc_grp".to_owned(), "doc_ord".to_owned()]),
            managed: BTreeSet::from(["grp".to_owned(), "ord".to_owned()]),
        }
    }

    #[test]
    fn new_linked_tables_get_foreign_keys_after_all_keys_and_indexes_last() {
        // `ord` links to `grp` and to itself; `grp` sorts first, but order does not matter.
        let plan = plan(&registry_of(&[&order(""), GROUP]), &Catalog::default());
        assert!(plan.conflicts.is_empty());
        let sql: Vec<String> = plan.steps.iter().map(ToString::to_string).collect();
        let position = |needle: &str| {
            sql.iter()
                .position(|s| s.contains(needle))
                .unwrap_or_else(|| panic!("{needle} in {sql:#?}"))
        };
        assert!(position("ADD PRIMARY KEY") < position("ADD FOREIGN KEY"));
        assert!(position("CREATE TABLE \"doc_ord\"") < position("ADD PRIMARY KEY"));
        assert_eq!(
            sql[position("ADD FOREIGN KEY")],
            "ALTER TABLE \"doc_ord\" ADD FOREIGN KEY (\"grp\") REFERENCES \"doc_grp\" (\"id\") ON DELETE RESTRICT, \
             ADD FOREIGN KEY (\"prev\") REFERENCES \"doc_ord\" (\"id\") ON DELETE RESTRICT;"
        );
        // Only the non-unique Link field gets a plain index; `prev` is covered by its unique key.
        let indexes: Vec<&String> = sql
            .iter()
            .filter(|s| s.starts_with("CREATE INDEX"))
            .collect();
        assert_eq!(indexes, ["CREATE INDEX ON \"doc_ord\" (\"grp\");"]);
        assert!(position("ADD FOREIGN KEY") < position("CREATE INDEX"));
        assert!(position("CREATE INDEX") < position("-- register"));
        // Both new tables have both key steps before any foreign key step.
        let last_keys = sql
            .iter()
            .rposition(|s| s.contains("ADD PRIMARY KEY"))
            .unwrap();
        assert!(last_keys < position("ADD FOREIGN KEY"));
    }

    #[test]
    fn matching_linked_schema_is_a_no_op() {
        let plan = plan(&registry_of(&[&order(""), GROUP]), &linked_catalog());
        assert!(plan.steps.is_empty(), "{:?}", plan.steps);
        assert!(plan.conflicts.is_empty() && plan.warnings.is_empty());
    }

    #[test]
    fn link_column_added_to_an_existing_table_gets_foreign_key_then_index() {
        let mut catalog = linked_catalog();
        let key = ("doc_ord".to_owned(), "grp".to_owned());
        catalog.columns.get_mut("doc_ord").unwrap().remove("grp");
        catalog.foreign_keys.remove(&key);
        catalog.indexed.remove(&key);
        let plan = plan(&registry_of(&[&order(""), GROUP]), &catalog);
        assert!(plan.conflicts.is_empty(), "{:?}", plan.conflicts);
        let sql: Vec<String> = plan.steps.iter().map(ToString::to_string).collect();
        assert_eq!(
            sql,
            [
                "ALTER TABLE \"doc_ord\" ADD COLUMN \"grp\" uuid;",
                "ALTER TABLE \"doc_ord\" ADD FOREIGN KEY (\"grp\") REFERENCES \"doc_grp\" (\"id\") ON DELETE RESTRICT;",
                "CREATE INDEX ON \"doc_ord\" (\"grp\");",
            ]
        );
    }

    #[test]
    fn missing_index_on_a_link_column_is_a_step_not_a_conflict() {
        let mut catalog = linked_catalog();
        catalog
            .indexed
            .remove(&("doc_ord".to_owned(), "grp".to_owned()));
        let plan = plan(&registry_of(&[&order(""), GROUP]), &catalog);
        assert!(plan.conflicts.is_empty());
        assert!(
            matches!(&plan.steps[..], [Step::CreateIndex { column, .. }] if column == "grp"),
            "{:?}",
            plan.steps
        );
    }

    #[test]
    fn foreign_key_drift_is_a_conflict() {
        let key = ("doc_ord".to_owned(), "grp".to_owned());
        let registry = registry_of(&[&order(""), GROUP]);
        let describe = |catalog: &Catalog| -> Vec<String> {
            plan(&registry, catalog)
                .conflicts
                .iter()
                .map(ToString::to_string)
                .collect()
        };

        let mut dropped = linked_catalog();
        dropped.foreign_keys.remove(&key);
        assert_eq!(
            describe(&dropped),
            [
                "foreign key on doc_ord.grp is missing in the database but the metadata requires \
              REFERENCES \"doc_grp\" (\"id\") ON DELETE RESTRICT"
            ]
        );

        let mut cascade = linked_catalog();
        cascade.foreign_keys.get_mut(&key).unwrap()[0].on_delete = 'c';
        assert_eq!(
            describe(&cascade),
            [
                "foreign key on doc_ord.grp is REFERENCES \"doc_grp\" (\"id\") ON DELETE CASCADE in the \
              database but the metadata requires REFERENCES \"doc_grp\" (\"id\") ON DELETE RESTRICT"
            ]
        );

        let mut no_action = linked_catalog();
        no_action.foreign_keys.get_mut(&key).unwrap()[0].on_delete = 'a';
        assert_eq!(describe(&no_action).len(), 1);

        let mut wrong_table = linked_catalog();
        wrong_table.foreign_keys.get_mut(&key).unwrap()[0].table = "doc_ord".into();
        assert_eq!(describe(&wrong_table).len(), 1);

        let mut wrong_column = linked_catalog();
        wrong_column.foreign_keys.get_mut(&key).unwrap()[0].column = "name".into();
        assert_eq!(describe(&wrong_column).len(), 1);
    }

    #[test]
    fn foreign_key_on_a_non_link_column_is_a_conflict() {
        let mut catalog = synced_catalog();
        catalog.foreign_keys.insert(
            ("doc_item".to_owned(), "price".to_owned()),
            vec![ForeignKey {
                table: "doc_other".into(),
                column: "id".into(),
                on_delete: 'a',
                validated: true,
            }],
        );
        let plan = plan(&registry(ITEM), &catalog);
        assert!(
            matches!(&plan.conflicts[..], [Conflict::ForeignKeyMismatch { column, expected, .. }]
                if column == "price" && expected == "no foreign key"),
            "{:?}",
            plan.conflicts
        );
    }

    #[test]
    fn non_table_relation_over_existing_columns_is_a_name_clash() {
        // A view has columns in the catalog but is not an ordinary table.
        let mut catalog = synced_catalog();
        catalog.tables.clear();
        let plan = plan(&registry(ITEM), &catalog);
        assert_eq!(
            plan.conflicts,
            vec![Conflict::RelationNameTaken {
                name: "doc_item".into()
            }]
        );
    }
}
