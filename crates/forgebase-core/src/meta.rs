//! The DocType metadata model, its validation and the [`Registry`].

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;

use crate::hooks::DocTypeHooks;
use crate::ident::{self, IdentError};

/// Default `max_length` of a Data field.
pub const DEFAULT_MAX_LENGTH: u32 = 140;
/// Largest allowed `max_length` of a Data field.
pub const MAX_DATA_LENGTH: u32 = 65_535;
/// Largest allowed Decimal precision (the range of `rust_decimal`).
pub const MAX_DECIMAL_PRECISION: u8 = 28;

/// A document type: a named list of fields stored in one table.
///
/// A child DocType (`is_child`) has no documents of its own: its rows are stored in a table of
/// their own but always belong to a parent document, through a [`FieldType::Table`] field of
/// exactly one other DocType.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocType {
    /// Identifier, see [`ident::validate_doctype_name`].
    pub name: String,
    /// Human-readable name. Never derived from `name`.
    pub label: String,
    /// How document names are generated.
    pub naming: Naming,
    /// The fields, in declaration order.
    pub fields: Vec<DocField>,
    /// A child DocType: rows of a parent's table field, no documents or names of its own.
    /// A child has random naming, no Table fields (no nesting) and no `unique` fields.
    pub is_child: bool,
}

/// One field of a [`DocType`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocField {
    /// Identifier, see [`ident::validate_field_name`].
    pub name: String,
    /// Human-readable name; defaults to `name` when loaded from TOML without a label.
    pub label: String,
    /// The type and its parameters.
    pub field_type: FieldType,
    /// A value must be present: not null and, for Data, Text and Select, not an empty or
    /// whitespace-only string. Enforced by application validation, not by the database.
    pub required: bool,
    /// Values must be unique across documents.
    pub unique: bool,
}

/// The closed set of field types. Link arrived in M2.1, Table in M2.2.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldType {
    /// Short text with a maximum length in characters (`1..=65535`).
    Data {
        /// Maximum number of characters.
        max_length: u32,
    },
    /// Text without a length limit.
    Text,
    /// A 64-bit signed integer.
    Int,
    /// A fixed-point number with `precision` total digits, `scale` of them after the point.
    Decimal {
        /// Total number of digits (`1..=28`).
        precision: u8,
        /// Digits after the decimal point (`<= precision`).
        scale: u8,
    },
    /// A boolean.
    Check,
    /// A calendar date.
    Date,
    /// A point in time with an offset.
    Datetime,
    /// One of a fixed list of strings (non-empty, unique, non-empty entries).
    Select {
        /// The allowed values.
        options: Vec<String>,
    },
    /// A reference to a document of another DocType (or of the same one), by document name.
    ///
    /// The value is [`Value::Text`](crate::value::Value::Text) holding the target document's
    /// `name`. The database stores the target's `id` as a foreign key.
    Link {
        /// Name of the target DocType. It must exist in the [`Registry`] that holds this
        /// DocType, see [`Registry::check_references`].
        target: String,
    },
    /// An ordered list of rows of a child DocType. There is no [`Value`](crate::value::Value)
    /// for it and no column in the parent's table: the rows live in
    /// [`Document::tables`](crate::document::Document::tables). `required` means at least one
    /// row; `unique` is not allowed.
    Table {
        /// Name of the child DocType. It must exist in the [`Registry`] that holds this DocType
        /// and be a child DocType used by no other Table field, see
        /// [`Registry::check_references`].
        target: String,
    },
}

impl FieldType {
    /// Name of the type as written in metadata files.
    pub fn name(&self) -> &'static str {
        match self {
            FieldType::Data { .. } => "data",
            FieldType::Text => "text",
            FieldType::Int => "int",
            FieldType::Decimal { .. } => "decimal",
            FieldType::Check => "check",
            FieldType::Date => "date",
            FieldType::Datetime => "datetime",
            FieldType::Select { .. } => "select",
            FieldType::Link { .. } => "link",
            FieldType::Table { .. } => "table",
        }
    }
}

/// How the `name` of a new document is produced.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Naming {
    /// A generated random name.
    #[default]
    Random,
    /// The value of the named field. That field must be a required, unique Data field.
    Field(String),
}

/// One problem found in a DocType definition.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DefinitionError {
    /// The DocType name is not a valid identifier.
    #[error("doctype name `{name}` {reason}")]
    InvalidDocTypeName {
        /// The rejected name.
        name: String,
        /// Why it was rejected.
        reason: IdentError,
    },
    /// The DocType label is empty.
    #[error("doctype label must not be empty")]
    EmptyLabel,
    /// A field name is not a valid identifier or is reserved.
    #[error("field name `{field}` {reason}")]
    InvalidFieldName {
        /// The rejected name.
        field: String,
        /// Why it was rejected.
        reason: IdentError,
    },
    /// The DocType label contains a NUL character.
    #[error("doctype label must not contain a NUL character")]
    NulInLabel,
    /// A field label is empty.
    #[error("field `{field}`: label must not be empty")]
    EmptyFieldLabel {
        /// The field.
        field: String,
    },
    /// A field label contains a NUL character.
    #[error("field `{field}`: label must not contain a NUL character")]
    NulInFieldLabel {
        /// The field.
        field: String,
    },
    /// Two fields share a name.
    #[error("field `{field}` is defined more than once")]
    DuplicateField {
        /// The repeated name.
        field: String,
    },
    /// The `type` value is not one of the supported field types.
    #[error(
        "field `{field}`: unknown type `{found}` (expected one of data, text, int, decimal, check, date, datetime, select, link, table)"
    )]
    UnknownFieldType {
        /// The field.
        field: String,
        /// The rejected type name.
        found: String,
    },
    /// A parameter required by the field type is missing.
    #[error("field `{field}`: type `{field_type}` requires `{param}`")]
    MissingParam {
        /// The field.
        field: String,
        /// The type name.
        field_type: &'static str,
        /// The missing key.
        param: &'static str,
    },
    /// A parameter is not valid for the field type.
    #[error("field `{field}`: `{param}` is not allowed for type `{field_type}`")]
    UnexpectedParam {
        /// The field.
        field: String,
        /// The type name.
        field_type: String,
        /// The offending key.
        param: &'static str,
    },
    /// `max_length` is outside `1..=65535`.
    #[error("field `{field}`: max_length {value} is out of range (1..={MAX_DATA_LENGTH})")]
    MaxLengthOutOfRange {
        /// The field.
        field: String,
        /// The rejected value.
        value: u32,
    },
    /// Decimal `precision` is outside `1..=28`.
    #[error("field `{field}`: precision {value} is out of range (1..={MAX_DECIMAL_PRECISION})")]
    PrecisionOutOfRange {
        /// The field.
        field: String,
        /// The rejected value.
        value: u8,
    },
    /// Decimal `scale` is larger than `precision`.
    #[error("field `{field}`: scale {scale} must not exceed precision {precision}")]
    ScaleExceedsPrecision {
        /// The field.
        field: String,
        /// The scale.
        scale: u8,
        /// The precision.
        precision: u8,
    },
    /// A Select field has no options.
    #[error("field `{field}`: options must not be empty")]
    EmptyOptions {
        /// The field.
        field: String,
    },
    /// A Select option is the empty string.
    #[error("field `{field}`: options must not contain an empty string")]
    EmptyOption {
        /// The field.
        field: String,
    },
    /// A Select option contains a NUL character, which PostgreSQL text cannot store.
    #[error("field `{field}`: options must not contain a NUL character")]
    NulInOption {
        /// The field.
        field: String,
    },
    /// A Select option appears twice.
    #[error("field `{field}`: duplicate option `{option}`")]
    DuplicateOption {
        /// The field.
        field: String,
        /// The repeated option.
        option: String,
    },
    /// `naming` is a string other than `"random"`.
    #[error("unknown naming strategy `{found}` (expected \"random\" or {{ field = \"<name>\" }})")]
    UnknownNamingStrategy {
        /// The rejected value.
        found: String,
    },
    /// `naming` is neither a string nor a table with the single key `field`.
    #[error("naming must be \"random\" or {{ field = \"<fieldname>\" }}")]
    MalformedNaming,
    /// The naming field does not exist.
    #[error("naming refers to field `{field}`, which does not exist")]
    NamingFieldMissing {
        /// The referenced field.
        field: String,
    },
    /// The naming field is not a Data field.
    #[error("naming field `{field}` must be of type data, found `{found}`")]
    NamingFieldNotData {
        /// The referenced field.
        field: String,
        /// Its actual type name.
        found: &'static str,
    },
    /// The naming field is not required.
    #[error("naming field `{field}` must be required")]
    NamingFieldNotRequired {
        /// The referenced field.
        field: String,
    },
    /// The naming field is not unique.
    #[error("naming field `{field}` must be unique")]
    NamingFieldNotUnique {
        /// The referenced field.
        field: String,
    },
    /// A Link field's `target` is not a valid DocType name.
    #[error("field `{field}`: link target `{target}` {reason}")]
    InvalidLinkTarget {
        /// The field.
        field: String,
        /// The rejected target.
        target: String,
        /// Why it was rejected.
        reason: IdentError,
    },
    /// A Table field's `target` is not a valid DocType name.
    #[error("field `{field}`: table target `{target}` {reason}")]
    InvalidTableTarget {
        /// The field.
        field: String,
        /// The rejected target.
        target: String,
        /// Why it was rejected.
        reason: IdentError,
    },
    /// A Table field is `unique`; uniqueness of a list of rows is not meaningful.
    #[error("field `{field}`: a table field cannot be unique")]
    UniqueTable {
        /// The field.
        field: String,
    },
    /// A child DocType has a `naming` rule; its rows have no names.
    #[error("a child doctype has no naming rule (its rows have no names)")]
    ChildWithNaming,
    /// A child DocType has a Table field; child tables cannot be nested.
    #[error("field `{field}`: a child doctype cannot have table fields (no nesting)")]
    ChildWithTable {
        /// The field.
        field: String,
    },
    /// A child DocType has a `unique` field; uniqueness across all parents is not supported.
    #[error("field `{field}`: a child doctype cannot have unique fields")]
    ChildWithUnique {
        /// The field.
        field: String,
    },
    /// A registry already holds a DocType with this name.
    #[error("doctype `{name}` is already registered")]
    DuplicateDocType {
        /// The repeated name.
        name: String,
    },
    /// Hooks were registered for a DocType that is not in the registry
    /// ([`Registry::add_hooks`]).
    #[error("cannot register hooks: unknown doctype `{name}`")]
    UnknownHooksDocType {
        /// The DocType name that was given.
        name: String,
    },
    /// Hooks were registered for a child DocType ([`Registry::add_hooks`]).
    #[error(
        "cannot register hooks for child doctype `{doctype}`: register them on its parent, whose hooks see the rows"
    )]
    HooksOnChildDocType {
        /// The child DocType.
        doctype: String,
    },
    /// A Link field refers to a DocType that is not in the registry
    /// ([`Registry::check_references`]).
    #[error("doctype `{doctype}`: field `{field}` links to unknown doctype `{target}`")]
    UnknownLinkTarget {
        /// The DocType that has the Link field.
        doctype: String,
        /// The field.
        field: String,
        /// The missing target DocType.
        target: String,
    },
    /// A Table field refers to a DocType that is not in the registry
    /// ([`Registry::check_references`]).
    #[error("doctype `{doctype}`: field `{field}` is a table of unknown doctype `{target}`")]
    UnknownTableTarget {
        /// The DocType that has the Table field.
        doctype: String,
        /// The field.
        field: String,
        /// The missing target DocType.
        target: String,
    },
    /// A Table field refers to a DocType that is not a child DocType
    /// ([`Registry::check_references`]).
    #[error(
        "doctype `{doctype}`: field `{field}` is a table of `{target}`, which is not a child doctype (`is_child = true`)"
    )]
    TableTargetNotChild {
        /// The DocType that has the Table field.
        doctype: String,
        /// The field.
        field: String,
        /// The target DocType.
        target: String,
    },
    /// A Link field refers to a child DocType; child rows cannot be linked to
    /// ([`Registry::check_references`]).
    #[error(
        "doctype `{doctype}`: field `{field}` links to child doctype `{target}`, which cannot be linked to"
    )]
    LinkToChild {
        /// The DocType that has the Link field.
        doctype: String,
        /// The field.
        field: String,
        /// The child DocType.
        target: String,
    },
    /// A child DocType is the target of no Table field ([`Registry::check_references`]).
    #[error("child doctype `{doctype}` is not used by any table field")]
    UnusedChild {
        /// The child DocType.
        doctype: String,
    },
    /// A child DocType is the target of several Table fields; it belongs to exactly one parent
    /// ([`Registry::check_references`]).
    #[error("child doctype `{child}` is used by more than one table field: {}", .uses.join(", "))]
    ChildSharedByTables {
        /// The child DocType.
        child: String,
        /// The Table fields, as `doctype.field`.
        uses: Vec<String>,
    },
}

/// Error returned when loading a DocType from TOML.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MetaError {
    /// The text is not valid TOML or does not match the metadata schema (unknown key, wrong value type, missing key).
    #[error("cannot parse doctype definition: {0}")]
    Parse(#[from] toml::de::Error),
    /// The definition parsed but breaks one or more rules; all problems are listed.
    #[error("invalid doctype definition: {}", join(.0))]
    Invalid(Vec<DefinitionError>),
}

fn join(errors: &[DefinitionError]) -> String {
    errors
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ")
}

impl DocType {
    /// Parses and validates a DocType from TOML text.
    ///
    /// The format is described in `docs/blueprint/architecture.md` section 3 ("DocType file
    /// format (M1)"); an example is `crates/forgebase-core/tests/fixtures/customer.toml`.
    /// Every rule violation is reported, not just the first. Decimal values with more decimal
    /// places than `scale` are rejected on validation, not rounded.
    pub fn from_toml_str(text: &str) -> Result<DocType, MetaError> {
        crate::load::from_toml_str(text)
    }

    /// Looks up a field by name.
    pub fn field(&self, name: &str) -> Option<&DocField> {
        self.fields.iter().find(|f| f.name == name)
    }

    /// The fields that have a column in the DocType's table: every field except Table fields.
    pub fn column_fields(&self) -> impl Iterator<Item = &DocField> {
        self.fields
            .iter()
            .filter(|f| !matches!(f.field_type, FieldType::Table { .. }))
    }

    /// The Table fields with the name of their child DocType, in declaration order.
    pub fn table_fields(&self) -> impl Iterator<Item = (&DocField, &str)> {
        self.fields.iter().filter_map(|f| match &f.field_type {
            FieldType::Table { target } => Some((f, target.as_str())),
            _ => None,
        })
    }

    /// Checks every definition rule and returns all problems found.
    pub fn validate(&self) -> Result<(), Vec<DefinitionError>> {
        let errors = self.check(&[]);
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }

    /// `failed` lists the names of fields that could not be converted (and so are not in
    /// `self.fields`): their names are still checked, and naming errors about them are
    /// suppressed because the real problem is already reported.
    pub(crate) fn check(&self, failed: &[String]) -> Vec<DefinitionError> {
        let mut errors = Vec::new();
        if let Err(reason) = ident::validate_doctype_name(&self.name) {
            errors.push(DefinitionError::InvalidDocTypeName {
                name: self.name.clone(),
                reason,
            });
        }
        if self.label.trim().is_empty() {
            errors.push(DefinitionError::EmptyLabel);
        }
        if self.label.contains('\0') {
            errors.push(DefinitionError::NulInLabel);
        }
        let mut seen = BTreeSet::new();
        for field in &self.fields {
            if !seen.insert(field.name.as_str()) {
                errors.push(DefinitionError::DuplicateField {
                    field: field.name.clone(),
                });
                continue;
            }
            field.check(&mut errors);
        }
        for name in failed {
            if let Err(reason) = ident::validate_field_name(name) {
                errors.push(DefinitionError::InvalidFieldName {
                    field: name.clone(),
                    reason,
                });
            }
            if !seen.insert(name.as_str()) {
                errors.push(DefinitionError::DuplicateField {
                    field: name.clone(),
                });
            }
        }
        if self.is_child {
            if self.naming != Naming::Random {
                errors.push(DefinitionError::ChildWithNaming);
            }
            for field in &self.fields {
                if matches!(field.field_type, FieldType::Table { .. }) {
                    errors.push(DefinitionError::ChildWithTable {
                        field: field.name.clone(),
                    });
                }
                // A unique Table field is reported by the field check.
                if field.unique && !matches!(field.field_type, FieldType::Table { .. }) {
                    errors.push(DefinitionError::ChildWithUnique {
                        field: field.name.clone(),
                    });
                }
            }
        } else if let Naming::Field(name) = &self.naming
            && !failed.contains(name)
        {
            self.check_naming_field(name, &mut errors);
        }
        errors
    }

    fn check_naming_field(&self, name: &str, errors: &mut Vec<DefinitionError>) {
        let field = name.to_owned();
        match self.field(name) {
            None => errors.push(DefinitionError::NamingFieldMissing { field }),
            Some(f) => {
                if !matches!(f.field_type, FieldType::Data { .. }) {
                    errors.push(DefinitionError::NamingFieldNotData {
                        field: field.clone(),
                        found: f.field_type.name(),
                    });
                }
                if !f.required {
                    errors.push(DefinitionError::NamingFieldNotRequired {
                        field: field.clone(),
                    });
                }
                if !f.unique {
                    errors.push(DefinitionError::NamingFieldNotUnique { field });
                }
            }
        }
    }
}

impl DocField {
    fn check(&self, errors: &mut Vec<DefinitionError>) {
        let field = || self.name.clone();
        if let Err(reason) = ident::validate_field_name(&self.name) {
            errors.push(DefinitionError::InvalidFieldName {
                field: field(),
                reason,
            });
        }
        if self.label.trim().is_empty() {
            errors.push(DefinitionError::EmptyFieldLabel { field: field() });
        }
        if self.label.contains('\0') {
            errors.push(DefinitionError::NulInFieldLabel { field: field() });
        }
        match &self.field_type {
            FieldType::Data { max_length } => {
                if !(1..=MAX_DATA_LENGTH).contains(max_length) {
                    errors.push(DefinitionError::MaxLengthOutOfRange {
                        field: field(),
                        value: *max_length,
                    });
                }
            }
            FieldType::Decimal { precision, scale } => {
                if !(1..=MAX_DECIMAL_PRECISION).contains(precision) {
                    errors.push(DefinitionError::PrecisionOutOfRange {
                        field: field(),
                        value: *precision,
                    });
                } else if scale > precision {
                    errors.push(DefinitionError::ScaleExceedsPrecision {
                        field: field(),
                        scale: *scale,
                        precision: *precision,
                    });
                }
            }
            FieldType::Select { options } => {
                if options.is_empty() {
                    errors.push(DefinitionError::EmptyOptions { field: field() });
                }
                let mut seen = BTreeSet::new();
                for option in options {
                    if option.contains('\0') {
                        errors.push(DefinitionError::NulInOption { field: field() });
                    } else if option.is_empty() {
                        errors.push(DefinitionError::EmptyOption { field: field() });
                    } else if !seen.insert(option.as_str()) {
                        errors.push(DefinitionError::DuplicateOption {
                            field: field(),
                            option: option.clone(),
                        });
                    }
                }
            }
            FieldType::Link { target } => {
                if let Err(reason) = ident::validate_doctype_name(target) {
                    errors.push(DefinitionError::InvalidLinkTarget {
                        field: field(),
                        target: target.clone(),
                        reason,
                    });
                }
            }
            FieldType::Table { target } => {
                if let Err(reason) = ident::validate_doctype_name(target) {
                    errors.push(DefinitionError::InvalidTableTarget {
                        field: field(),
                        target: target.clone(),
                        reason,
                    });
                }
                if self.unique {
                    errors.push(DefinitionError::UniqueTable { field: field() });
                }
            }
            FieldType::Text
            | FieldType::Int
            | FieldType::Check
            | FieldType::Date
            | FieldType::Datetime => {}
        }
    }
}

/// A set of DocTypes keyed by name, and the lifecycle hooks the app registered for them.
///
/// Cloning is cheap for the hooks (they are shared). A registry is not comparable: hook sets
/// are trait objects.
#[derive(Clone, Default)]
pub struct Registry {
    doctypes: BTreeMap<String, DocType>,
    hooks: BTreeMap<String, Vec<Arc<dyn DocTypeHooks>>>,
}

impl fmt::Debug for Registry {
    /// Prints the DocTypes and the number of hook sets per DocType (the hooks themselves have
    /// no `Debug`).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let hook_counts: BTreeMap<&str, usize> = self
            .hooks
            .iter()
            .map(|(name, hooks)| (name.as_str(), hooks.len()))
            .collect();
        f.debug_struct("Registry")
            .field("doctypes", &self.doctypes)
            .field("hooks", &hook_counts)
            .finish()
    }
}

impl Registry {
    /// Creates an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Validates and adds a DocType. Fails with every definition problem found
    /// ([`DocType::validate`]), or with [`DefinitionError::DuplicateDocType`] if one with the
    /// same name is already present. The registry therefore only holds valid DocTypes.
    ///
    /// Only the DocType itself is checked: a Link or Table field may name a DocType that is
    /// added later (definition files are loaded in any order). **Call
    /// [`Registry::check_references`] after the last DocType has been added.**
    pub fn add(&mut self, doctype: DocType) -> Result<(), Vec<DefinitionError>> {
        doctype.validate()?;
        if self.doctypes.contains_key(&doctype.name) {
            return Err(vec![DefinitionError::DuplicateDocType {
                name: doctype.name,
            }]);
        }
        self.doctypes.insert(doctype.name.clone(), doctype);
        Ok(())
    }

    /// Registers a hook set for a DocType. The DocType must already be in the registry
    /// ([`DefinitionError::UnknownHooksDocType`]) and must not be a child DocType
    /// ([`DefinitionError::HooksOnChildDocType`]: a child table has no lifecycle of its own, the
    /// hooks of its parent see its rows).
    ///
    /// Several hook sets may be registered for one DocType; they run in registration order
    /// (see [`DocTypeHooks`] for the pipeline). Registration is explicit and happens at startup
    /// (ADR 0006, ADR 0009).
    pub fn add_hooks(
        &mut self,
        doctype: &str,
        hooks: Arc<dyn DocTypeHooks>,
    ) -> Result<(), DefinitionError> {
        match self.doctypes.get(doctype) {
            None => Err(DefinitionError::UnknownHooksDocType {
                name: doctype.to_owned(),
            }),
            Some(found) if found.is_child => Err(DefinitionError::HooksOnChildDocType {
                doctype: doctype.to_owned(),
            }),
            Some(_) => {
                self.hooks
                    .entry(doctype.to_owned())
                    .or_default()
                    .push(hooks);
                Ok(())
            }
        }
    }

    /// The hook sets registered for a DocType, in registration order (empty when there are
    /// none or the DocType is unknown).
    pub fn hooks(&self, doctype: &str) -> &[Arc<dyn DocTypeHooks>] {
        self.hooks.get(doctype).map_or(&[], Vec::as_slice)
    }

    /// Checks that the DocTypes of this registry fit together. Returns every problem found:
    ///
    /// - a Link field refers to a DocType that is in the registry (a DocType may link to itself)
    ///   and is not a child DocType;
    /// - a Table field refers to a DocType that is in the registry and is a child DocType;
    /// - every child DocType is the target of exactly one Table field (it belongs to exactly
    ///   one parent).
    ///
    /// [`Registry::add`] cannot do this because DocTypes arrive one at a time. Call it once
    /// after all DocTypes are loaded; `forgebase-db`'s `sync` and the CLI loader do.
    pub fn check_references(&self) -> Result<(), Vec<DefinitionError>> {
        let mut errors = Vec::new();
        // child DocType -> the `doctype.field` of every Table field that targets it
        let mut used_by: BTreeMap<&str, Vec<String>> = BTreeMap::new();
        for doctype in self.doctypes.values() {
            for field in &doctype.fields {
                let (doctype_name, field_name) = (doctype.name.clone(), field.name.clone());
                match &field.field_type {
                    FieldType::Link { target } => match self.doctypes.get(target) {
                        None => errors.push(DefinitionError::UnknownLinkTarget {
                            doctype: doctype_name,
                            field: field_name,
                            target: target.clone(),
                        }),
                        Some(t) if t.is_child => errors.push(DefinitionError::LinkToChild {
                            doctype: doctype_name,
                            field: field_name,
                            target: target.clone(),
                        }),
                        Some(_) => {}
                    },
                    FieldType::Table { target } => match self.doctypes.get(target) {
                        None => errors.push(DefinitionError::UnknownTableTarget {
                            doctype: doctype_name,
                            field: field_name,
                            target: target.clone(),
                        }),
                        Some(t) if !t.is_child => {
                            errors.push(DefinitionError::TableTargetNotChild {
                                doctype: doctype_name,
                                field: field_name,
                                target: target.clone(),
                            });
                        }
                        Some(_) => used_by
                            .entry(target.as_str())
                            .or_default()
                            .push(format!("{doctype_name}.{field_name}")),
                    },
                    _ => {}
                }
            }
        }
        for child in self.doctypes.values().filter(|d| d.is_child) {
            match used_by.remove(child.name.as_str()) {
                None => errors.push(DefinitionError::UnusedChild {
                    doctype: child.name.clone(),
                }),
                Some(uses) if uses.len() > 1 => {
                    errors.push(DefinitionError::ChildSharedByTables {
                        child: child.name.clone(),
                        uses,
                    });
                }
                Some(_) => {}
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }

    /// The DocType and Table field that own the rows of child DocType `child`, if a Table field
    /// targets it. After [`Registry::check_references`] succeeded that is exactly one for every
    /// child DocType.
    pub fn parent_of(&self, child: &str) -> Option<(&DocType, &DocField)> {
        self.doctypes.values().find_map(|doctype| {
            doctype
                .table_fields()
                .find(|(_, target)| *target == child)
                .map(|(field, _)| (doctype, field))
        })
    }

    /// Looks up a DocType by name.
    pub fn get(&self, name: &str) -> Option<&DocType> {
        self.doctypes.get(name)
    }

    /// Iterates over the DocTypes in name order.
    pub fn iter(&self) -> impl Iterator<Item = &DocType> {
        self.doctypes.values()
    }

    /// Number of registered DocTypes.
    pub fn len(&self) -> usize {
        self.doctypes.len()
    }

    /// True when no DocType is registered.
    pub fn is_empty(&self) -> bool {
        self.doctypes.is_empty()
    }
}

impl fmt::Display for FieldType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field(name: &str, field_type: FieldType) -> DocField {
        DocField {
            name: name.into(),
            label: name.into(),
            field_type,
            required: false,
            unique: false,
        }
    }

    fn doctype(fields: Vec<DocField>) -> DocType {
        DocType {
            name: "thing".into(),
            label: "Thing".into(),
            naming: Naming::Random,
            fields,
            is_child: false,
        }
    }

    fn errors_of(d: &DocType) -> Vec<DefinitionError> {
        d.validate().expect_err("expected validation errors")
    }

    #[test]
    fn valid_doctype_passes() {
        let d = doctype(vec![
            field("a", FieldType::Data { max_length: 1 }),
            field("b", FieldType::Data { max_length: 65_535 }),
            field(
                "c",
                FieldType::Decimal {
                    precision: 28,
                    scale: 28,
                },
            ),
            field(
                "d",
                FieldType::Select {
                    options: vec!["x".into(), "y".into()],
                },
            ),
        ]);
        assert_eq!(d.validate(), Ok(()));
    }

    #[test]
    fn name_and_label_rules() {
        let mut d = doctype(vec![]);
        d.name = "Thing".into();
        d.label = "  ".into();
        assert_eq!(
            errors_of(&d),
            vec![
                DefinitionError::InvalidDocTypeName {
                    name: "Thing".into(),
                    reason: IdentError::InvalidFormat
                },
                DefinitionError::EmptyLabel,
            ]
        );
    }

    #[test]
    fn field_rules() {
        let cases: Vec<(DocField, DefinitionError)> = vec![
            (
                field("Bad", FieldType::Int),
                DefinitionError::InvalidFieldName {
                    field: "Bad".into(),
                    reason: IdentError::InvalidFormat,
                },
            ),
            (
                field("owner", FieldType::Int),
                DefinitionError::InvalidFieldName {
                    field: "owner".into(),
                    reason: IdentError::Reserved,
                },
            ),
            (
                DocField {
                    label: String::new(),
                    ..field("x", FieldType::Int)
                },
                DefinitionError::EmptyFieldLabel { field: "x".into() },
            ),
            (
                field("x", FieldType::Data { max_length: 0 }),
                DefinitionError::MaxLengthOutOfRange {
                    field: "x".into(),
                    value: 0,
                },
            ),
            (
                field("x", FieldType::Data { max_length: 65_536 }),
                DefinitionError::MaxLengthOutOfRange {
                    field: "x".into(),
                    value: 65_536,
                },
            ),
            (
                field(
                    "x",
                    FieldType::Decimal {
                        precision: 0,
                        scale: 0,
                    },
                ),
                DefinitionError::PrecisionOutOfRange {
                    field: "x".into(),
                    value: 0,
                },
            ),
            (
                field(
                    "x",
                    FieldType::Decimal {
                        precision: 29,
                        scale: 2,
                    },
                ),
                DefinitionError::PrecisionOutOfRange {
                    field: "x".into(),
                    value: 29,
                },
            ),
            (
                field(
                    "x",
                    FieldType::Decimal {
                        precision: 5,
                        scale: 6,
                    },
                ),
                DefinitionError::ScaleExceedsPrecision {
                    field: "x".into(),
                    scale: 6,
                    precision: 5,
                },
            ),
            (
                field("x", FieldType::Select { options: vec![] }),
                DefinitionError::EmptyOptions { field: "x".into() },
            ),
            (
                field(
                    "x",
                    FieldType::Select {
                        options: vec!["a".into(), String::new()],
                    },
                ),
                DefinitionError::EmptyOption { field: "x".into() },
            ),
            (
                field(
                    "x",
                    FieldType::Select {
                        options: vec!["a".into(), "a".into()],
                    },
                ),
                DefinitionError::DuplicateOption {
                    field: "x".into(),
                    option: "a".into(),
                },
            ),
        ];
        for (f, expected) in cases {
            assert_eq!(
                errors_of(&doctype(vec![f.clone()])),
                vec![expected],
                "field {f:?}"
            );
        }
    }

    #[test]
    fn duplicate_fields_are_rejected() {
        let d = doctype(vec![
            field("a", FieldType::Int),
            field("a", FieldType::Text),
        ]);
        assert_eq!(
            errors_of(&d),
            vec![DefinitionError::DuplicateField { field: "a".into() }]
        );
    }

    #[test]
    fn naming_field_rules() {
        let good = DocField {
            required: true,
            unique: true,
            ..field("email", FieldType::Data { max_length: 10 })
        };
        let naming = |name: &str| Naming::Field(name.into());

        let mut d = doctype(vec![good.clone()]);
        d.naming = naming("email");
        assert_eq!(d.validate(), Ok(()));

        d.naming = naming("nope");
        assert_eq!(
            errors_of(&d),
            vec![DefinitionError::NamingFieldMissing {
                field: "nope".into()
            }]
        );

        let not_data = DocField {
            field_type: FieldType::Text,
            ..good.clone()
        };
        let mut d = doctype(vec![not_data]);
        d.naming = naming("email");
        assert_eq!(
            errors_of(&d),
            vec![DefinitionError::NamingFieldNotData {
                field: "email".into(),
                found: "text"
            }]
        );

        let weak = DocField {
            required: false,
            unique: false,
            ..good
        };
        let mut d = doctype(vec![weak]);
        d.naming = naming("email");
        assert_eq!(
            errors_of(&d),
            vec![
                DefinitionError::NamingFieldNotRequired {
                    field: "email".into()
                },
                DefinitionError::NamingFieldNotUnique {
                    field: "email".into()
                },
            ]
        );
    }

    #[test]
    fn multiple_errors_are_collected() {
        let mut d = doctype(vec![
            field("id", FieldType::Int),
            field("x", FieldType::Select { options: vec![] }),
        ]);
        d.label = String::new();
        assert_eq!(errors_of(&d).len(), 3);
    }

    #[test]
    fn nul_is_rejected_in_labels_and_options() {
        let mut d = doctype(vec![
            field(
                "s",
                FieldType::Select {
                    options: vec!["a\0b".into(), "ok".into()],
                },
            ),
            field("t", FieldType::Text),
        ]);
        d.fields[1].label = "x\0".into();
        d.label = "T\0".into();
        assert_eq!(
            errors_of(&d),
            vec![
                DefinitionError::NulInLabel,
                DefinitionError::NulInOption { field: "s".into() },
                DefinitionError::NulInFieldLabel { field: "t".into() },
            ]
        );
    }

    #[test]
    fn registry_rejects_invalid_doctypes() {
        let mut registry = Registry::new();
        let mut bad_name = doctype(vec![]);
        bad_name.name = "x\"; drop table y; --".into();
        let errors = registry.add(bad_name).unwrap_err();
        assert!(matches!(
            errors.as_slice(),
            [DefinitionError::InvalidDocTypeName { .. }]
        ));

        let bad_scale = doctype(vec![field(
            "amount",
            FieldType::Decimal {
                precision: 2,
                scale: 3,
            },
        )]);
        assert_eq!(
            registry.add(bad_scale),
            Err(vec![DefinitionError::ScaleExceedsPrecision {
                field: "amount".into(),
                scale: 3,
                precision: 2
            }])
        );
        assert!(registry.is_empty());
    }

    fn link(name: &str, target: &str) -> DocField {
        field(
            name,
            FieldType::Link {
                target: target.into(),
            },
        )
    }

    #[test]
    fn link_target_must_be_a_valid_doctype_name() {
        assert_eq!(doctype(vec![link("c", "customer")]).validate(), Ok(()));
        for bad in ["Customer", "", "x\"; drop table y; --", "9a"] {
            let errors = errors_of(&doctype(vec![link("c", bad)]));
            assert!(
                matches!(errors.as_slice(), [DefinitionError::InvalidLinkTarget { target, .. }] if target == bad),
                "{bad:?}: {errors:?}"
            );
        }
        let too_long = "a".repeat(ident::MAX_DOCTYPE_NAME_LEN + 1);
        assert_eq!(errors_of(&doctype(vec![link("c", &too_long)])).len(), 1);
    }

    #[test]
    fn check_references_reports_unknown_targets() {
        let mut registry = Registry::new();
        let mut order = doctype(vec![
            link("customer", "customer"),
            link("owner_group", "grp"),
        ]);
        order.name = "order".into();
        registry.add(order).unwrap();
        assert_eq!(
            registry.check_references(),
            Err(vec![
                DefinitionError::UnknownLinkTarget {
                    doctype: "order".into(),
                    field: "customer".into(),
                    target: "customer".into()
                },
                DefinitionError::UnknownLinkTarget {
                    doctype: "order".into(),
                    field: "owner_group".into(),
                    target: "grp".into()
                },
            ])
        );
        let mut customer = doctype(vec![]);
        customer.name = "customer".into();
        registry.add(customer).unwrap();
        let mut grp = doctype(vec![]);
        grp.name = "grp".into();
        registry.add(grp).unwrap();
        // A target added after the DocType that links to it is fine.
        assert_eq!(registry.check_references(), Ok(()));
        assert_eq!(
            Registry::new().check_references(),
            Ok(()),
            "an empty registry has no references"
        );
    }

    #[test]
    fn self_link_is_allowed() {
        let mut registry = Registry::new();
        let mut tree = doctype(vec![link("parent", "tree")]);
        tree.name = "tree".into();
        registry.add(tree).unwrap();
        assert_eq!(registry.check_references(), Ok(()));
    }

    fn table(name: &str, target: &str) -> DocField {
        field(
            name,
            FieldType::Table {
                target: target.into(),
            },
        )
    }

    fn named(name: &str, is_child: bool, fields: Vec<DocField>) -> DocType {
        DocType {
            name: name.into(),
            is_child,
            ..doctype(fields)
        }
    }

    #[test]
    fn table_target_must_be_a_valid_doctype_name_and_not_unique() {
        assert_eq!(doctype(vec![table("rows", "row")]).validate(), Ok(()));
        let errors = errors_of(&doctype(vec![table("rows", "Bad Name")]));
        assert!(
            matches!(errors.as_slice(), [DefinitionError::InvalidTableTarget { target, .. }] if target == "Bad Name"),
            "{errors:?}"
        );
        let mut unique = table("rows", "row");
        unique.unique = true;
        assert_eq!(
            errors_of(&doctype(vec![unique])),
            vec![DefinitionError::UniqueTable {
                field: "rows".into()
            }]
        );
        // `required` is allowed: at least one row.
        let mut required = table("rows", "row");
        required.required = true;
        assert_eq!(doctype(vec![required]).validate(), Ok(()));
        assert_eq!(FieldType::Table { target: "r".into() }.name(), "table");
    }

    #[test]
    fn child_doctype_rules() {
        let child = |fields| named("row", true, fields);
        assert_eq!(
            child(vec![field("qty", FieldType::Int), link("c", "x")]).validate(),
            Ok(())
        );
        assert_eq!(
            errors_of(&child(vec![table("more", "other")])),
            vec![DefinitionError::ChildWithTable {
                field: "more".into()
            }]
        );
        let mut unique = field("code", FieldType::Int);
        unique.unique = true;
        assert_eq!(
            errors_of(&child(vec![unique])),
            vec![DefinitionError::ChildWithUnique {
                field: "code".into()
            }]
        );
        let mut with_naming = child(vec![]);
        with_naming.naming = Naming::Field("code".into());
        assert_eq!(
            errors_of(&with_naming),
            vec![DefinitionError::ChildWithNaming]
        );
        // The same fields are fine in a non-child DocType (apart from the table target rules).
        let mut unique = field("code", FieldType::Int);
        unique.unique = true;
        assert_eq!(doctype(vec![unique]).validate(), Ok(()));
    }

    fn registry_of(doctypes: Vec<DocType>) -> Registry {
        let mut registry = Registry::new();
        for doctype in doctypes {
            registry.add(doctype).unwrap();
        }
        registry
    }

    #[test]
    fn check_references_for_child_tables() {
        let good = registry_of(vec![
            named("order", false, vec![table("items", "item")]),
            named("item", true, vec![field("qty", FieldType::Int)]),
        ]);
        assert_eq!(good.check_references(), Ok(()));
        let (parent, field) = good.parent_of("item").unwrap();
        assert_eq!(
            (parent.name.as_str(), field.name.as_str()),
            ("order", "items")
        );
        assert!(good.parent_of("order").is_none());

        // Unknown target, target that is not a child.
        let bad = registry_of(vec![
            named(
                "order",
                false,
                vec![table("a", "nope"), table("b", "plain")],
            ),
            named("plain", false, vec![]),
        ]);
        assert_eq!(
            bad.check_references(),
            Err(vec![
                DefinitionError::UnknownTableTarget {
                    doctype: "order".into(),
                    field: "a".into(),
                    target: "nope".into()
                },
                DefinitionError::TableTargetNotChild {
                    doctype: "order".into(),
                    field: "b".into(),
                    target: "plain".into()
                },
            ])
        );
    }

    #[test]
    fn a_child_belongs_to_exactly_one_table_field() {
        let shared = registry_of(vec![
            named("a", false, vec![table("rows", "row"), table("more", "row")]),
            named("b", false, vec![table("rows", "row")]),
            named("row", true, vec![]),
        ]);
        assert_eq!(
            shared.check_references(),
            Err(vec![DefinitionError::ChildSharedByTables {
                child: "row".into(),
                uses: vec!["a.rows".into(), "a.more".into(), "b.rows".into()],
            }])
        );
        let message = shared.check_references().unwrap_err()[0].to_string();
        assert_eq!(
            message,
            "child doctype `row` is used by more than one table field: a.rows, a.more, b.rows"
        );

        let unused = registry_of(vec![named("row", true, vec![])]);
        assert_eq!(
            unused.check_references(),
            Err(vec![DefinitionError::UnusedChild {
                doctype: "row".into()
            }])
        );
    }

    #[test]
    fn a_link_to_a_child_is_refused() {
        let registry = registry_of(vec![
            named(
                "order",
                false,
                vec![table("items", "item"), link("fav", "item")],
            ),
            named("item", true, vec![]),
        ]);
        assert_eq!(
            registry.check_references(),
            Err(vec![DefinitionError::LinkToChild {
                doctype: "order".into(),
                field: "fav".into(),
                target: "item".into()
            }])
        );
    }

    #[test]
    fn column_and_table_fields() {
        let d = doctype(vec![
            field("a", FieldType::Int),
            table("rows", "row"),
            field("b", FieldType::Text),
        ]);
        let columns: Vec<&str> = d.column_fields().map(|f| f.name.as_str()).collect();
        assert_eq!(columns, ["a", "b"]);
        let tables: Vec<(&str, &str)> = d
            .table_fields()
            .map(|(f, t)| (f.name.as_str(), t))
            .collect();
        assert_eq!(tables, [("rows", "row")]);
    }

    #[test]
    fn registry_rejects_duplicates() {
        let mut registry = Registry::new();
        assert!(registry.is_empty());
        registry.add(doctype(vec![])).unwrap();
        assert_eq!(
            registry.add(doctype(vec![])),
            Err(vec![DefinitionError::DuplicateDocType {
                name: "thing".into()
            }])
        );
        assert_eq!(registry.len(), 1);
        assert!(registry.get("thing").is_some());
        assert!(registry.get("other").is_none());
        assert_eq!(registry.iter().count(), 1);
    }
    /// Records its name in a shared log when called, to see the registration order.
    struct Named(&'static str, std::sync::Mutex<Vec<&'static str>>);

    impl DocTypeHooks for Named {}

    #[test]
    fn hooks_are_kept_in_registration_order() {
        let first = Arc::new(Named("first", Default::default()));
        let second = Arc::new(Named("second", Default::default()));
        let mut registry = Registry::new();
        registry.add(doctype(vec![])).unwrap();
        assert!(registry.hooks("thing").is_empty());
        registry.add_hooks("thing", first.clone()).unwrap();
        registry.add_hooks("thing", second.clone()).unwrap();
        let hooks = registry.hooks("thing");
        assert_eq!(hooks.len(), 2);
        // Same allocations, same order (compare data pointers).
        assert!(std::ptr::eq(
            Arc::as_ptr(&hooks[0]).cast::<()>(),
            Arc::as_ptr(&first).cast::<()>()
        ));
        assert!(std::ptr::eq(
            Arc::as_ptr(&hooks[1]).cast::<()>(),
            Arc::as_ptr(&second).cast::<()>()
        ));
        assert_eq!((first.0, second.0), ("first", "second"));
        assert!(first.1.lock().unwrap().is_empty());
        // A clone shares the hooks.
        assert_eq!(registry.clone().hooks("thing").len(), 2);
        assert!(registry.hooks("other").is_empty());
    }

    #[test]
    fn hooks_need_a_known_parent_doctype() {
        let mut registry = Registry::new();
        registry.add(doctype(vec![])).unwrap();
        let mut child = doctype(vec![]);
        child.name = "row".into();
        child.is_child = true;
        registry.add(child).unwrap();
        let hooks: Arc<dyn DocTypeHooks> = Arc::new(Named("x", Default::default()));
        assert_eq!(
            registry.add_hooks("nope", hooks.clone()),
            Err(DefinitionError::UnknownHooksDocType {
                name: "nope".into()
            })
        );
        assert_eq!(
            registry.add_hooks("row", hooks),
            Err(DefinitionError::HooksOnChildDocType {
                doctype: "row".into()
            })
        );
        assert!(registry.hooks("nope").is_empty() && registry.hooks("row").is_empty());
    }

    #[test]
    fn registry_debug_shows_hook_counts() {
        let mut registry = Registry::new();
        registry.add(doctype(vec![])).unwrap();
        registry
            .add_hooks("thing", Arc::new(Named("a", Default::default())))
            .unwrap();
        registry
            .add_hooks("thing", Arc::new(Named("b", Default::default())))
            .unwrap();
        let text = format!("{registry:?}");
        assert!(text.contains("hooks: {\"thing\": 2}"), "{text}");
        assert!(text.contains("doctypes:"), "{text}");
    }
}
