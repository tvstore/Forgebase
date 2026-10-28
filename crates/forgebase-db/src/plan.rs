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
            | Step::AddColumn { sql, .. }
            | Step::CreateUniqueIndex { sql, .. } => write!(f, "{sql};"),
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

/// Snapshot of the live schema, limited to the tables of the registry.
#[derive(Debug, Default)]
pub(crate) struct Catalog {
    /// table -> column -> definition. A table without columns is absent.
    pub(crate) columns: BTreeMap<String, BTreeMap<String, Column>>,
    /// (table, column) pairs covered by a valid single-column unique index (not a primary key).
    pub(crate) unique: BTreeSet<(String, String)>,
    /// (table, column) pairs that are a single-column primary key.
    pub(crate) primary: BTreeSet<(String, String)>,
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

/// Steps grouped by phase. Order of execution: all new tables, then their keys, then added
/// columns, then late unique indexes, then registration.
#[derive(Default)]
struct Phases {
    create_tables: Vec<Step>,
    add_keys: Vec<Step>,
    add_columns: Vec<Step>,
    unique_indexes: Vec<Step>,
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
        phases.add_columns,
        phases.unique_indexes,
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
    for field in &doctype.fields {
        let has_unique = catalog
            .unique
            .contains(&(table.clone(), field.name.clone()));
        match columns.get(&field.name) {
            None => phases.add_columns.push(Step::AddColumn {
                doctype: doctype.name.clone(),
                column: field.name.clone(),
                sql: add_column_sql(&table, field),
            }),
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
            phases.unique_indexes.push(Step::CreateUniqueIndex {
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

#[cfg(test)]
mod tests {
    use super::*;

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
                Step::AddColumn { doctype, .. } => format!("column:{doctype}"),
                Step::CreateUniqueIndex { doctype, .. } => format!("index:{doctype}"),
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
