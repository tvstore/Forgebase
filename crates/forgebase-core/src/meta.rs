//! The DocType metadata model, its validation and the [`Registry`].

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;

use crate::document::DocStatus;
use crate::hooks::DocTypeHooks;
use crate::ident::{self, IdentError};
use crate::permission::{self, PermissionRule, Right};
use crate::series::{SeriesError, SeriesPattern};
use crate::workflow::{MAX_NAME_LEN, Move, Workflow};

/// Default `max_length` of a Data field.
pub const DEFAULT_MAX_LENGTH: u32 = 140;
/// Largest allowed `max_length` of a Data field.
pub const MAX_DATA_LENGTH: u32 = 65_535;
/// Largest allowed Decimal precision (the range of `rust_decimal`).
pub const MAX_DECIMAL_PRECISION: u8 = 28;
/// Largest number of fields in a DocType's `list_fields`.
pub const MAX_LIST_FIELDS: usize = 8;
/// Field kinds a `title_field` may have: short values that read as a title. Not Text (long),
/// Check (a flag, no name) or Table (no value).
pub const TITLE_FIELD_KINDS: [&str; 7] = [
    "data", "int", "decimal", "date", "datetime", "select", "link",
];

/// The default label of a name: split on `_`, capitalise the first letter of each part, join
/// with a space. `order_date` gives `Order Date`.
pub fn humanize(name: &str) -> String {
    name.split('_')
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().chain(chars).collect::<String>(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// A document type: a named list of fields stored in one table.
///
/// A child DocType (`is_child`) has no documents of its own: its rows are stored in a table of
/// their own but always belong to a parent document, through a [`FieldType::Table`] field of
/// exactly one other DocType.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocType {
    /// Identifier, see [`ident::validate_doctype_name`].
    pub name: String,
    /// Human-readable name. Required in the TOML file, never derived from `name`.
    pub label: String,
    /// How document names are generated.
    pub naming: Naming,
    /// The fields, in declaration order.
    pub fields: Vec<DocField>,
    /// A child DocType: rows of a parent's table field, no documents or names of its own.
    /// A child has random naming, no Table fields (no nesting) and no `unique` fields.
    pub is_child: bool,
    /// A submittable DocType has a docstatus (draft, submitted, cancelled) and can be amended
    /// (M2.4): its table gets the `docstatus` and `amended_from` columns. It cannot be a child
    /// DocType and cannot use [`Naming::Field`].
    pub submittable: bool,
    /// Who may do what with the DocType's documents (`[[permissions]]`, M3.2, ADR 0011). Empty
    /// means only the `Administrator` role and the trusted system caller have access (deny by
    /// default). A child DocType has none: its rows follow the parent's rules. Decisions are made
    /// by [`evaluate`](crate::permission::evaluate), nowhere else.
    pub permissions: Vec<PermissionRule>,
    /// The field that best names a document for a person (`title_field`, UX.1), or `None`.
    /// A non-Table field of a kind in [`TITLE_FIELD_KINDS`]; not allowed on a child DocType.
    /// Metadata for the desk only: the framework attaches no behaviour to it.
    pub title_field: Option<String>,
    /// The columns the desk list shows after `name` (`list_fields`, UX.1), or `None` for the
    /// desk's own default. Existing fields, no Table or Text field, no duplicates, at most
    /// [`MAX_LIST_FIELDS`]; not allowed on a child DocType. Metadata for the desk only.
    pub list_fields: Option<Vec<String>>,
    /// The states a document moves through and the transitions between them (`[workflow]`,
    /// M8.1, ADR 0015), or `None`. Defined by the app in the DocType file. On a DocType with a
    /// workflow, plain submit and cancel are refused and the state changes only through
    /// transitions. Not allowed on a child DocType, and a field of the DocType may not be named
    /// `workflow_state`.
    pub workflow: Option<Workflow>,
}

/// One field of a [`DocType`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocField {
    /// Identifier, see [`ident::validate_field_name`].
    pub name: String,
    /// Human-readable name; when loaded from TOML without a label, `name` [`humanize`]d.
    pub label: String,
    /// The type and its parameters.
    pub field_type: FieldType,
    /// A value must be present: not null and, for Data, Text and Select, not an empty or
    /// whitespace-only string. Enforced by application validation, not by the database.
    pub required: bool,
    /// Values must be unique across documents.
    pub unique: bool,
    /// The field may still be changed on a submitted document. Only allowed on a column field
    /// (not a Table field) of a submittable DocType; the rows of Table fields are always locked
    /// once the document is submitted.
    pub allow_on_submit: bool,
    /// The field is written only by hooks (and by migrations), never by a caller: `forgebase-db`
    /// refuses caller input for it (UX.1). Not allowed on a Table field, and not together with
    /// `required` (a caller could never provide the value).
    pub read_only: bool,
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
    /// A generated name from a counter, such as `INV-2026-00007`, see [`SeriesPattern`]
    /// (`naming = { series = "INV-.YYYY.-.#####" }`). Not allowed on a child DocType.
    Series(SeriesPattern),
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
    #[error(
        "unknown naming strategy `{found}` (expected \"random\", {{ field = \"<name>\" }} or {{ series = \"<pattern>\" }})"
    )]
    UnknownNamingStrategy {
        /// The rejected value.
        found: String,
    },
    /// `naming` is neither a string nor a table with the single key `field` or `series`.
    #[error(
        "naming must be \"random\", {{ field = \"<fieldname>\" }} or {{ series = \"<pattern>\" }}"
    )]
    MalformedNaming,
    /// The series pattern of `naming = { series = ... }` is not valid.
    #[error("naming series pattern `{pattern}` {reason}")]
    InvalidSeriesPattern {
        /// The rejected pattern.
        pattern: String,
        /// Why it was rejected.
        reason: SeriesError,
    },
    /// A child DocType is `submittable`; only the parent has a docstatus.
    #[error("a child doctype cannot be submittable (the document that owns its rows is)")]
    ChildSubmittable,
    /// A submittable DocType uses field naming; amended documents are named
    /// `<original>-<n>`, which a field value cannot provide.
    #[error(
        "a submittable doctype cannot use field naming (an amended document is named <original>-<n>); use random naming or a series"
    )]
    SubmittableWithFieldNaming,
    /// `allow_on_submit` on a field of a DocType that is not submittable.
    #[error("field `{field}`: allow_on_submit needs a submittable doctype (`submittable = true`)")]
    AllowOnSubmitNotSubmittable {
        /// The field.
        field: String,
    },
    /// `allow_on_submit` on a Table field: the rows of a submitted document are always locked.
    #[error(
        "field `{field}`: a table field cannot be allow_on_submit (its rows are locked once submitted)"
    )]
    AllowOnSubmitTable {
        /// The field.
        field: String,
    },
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
    /// A Table field is `read_only` (its rows are not computed columns).
    #[error("field `{field}`: a table field cannot be read_only")]
    ReadOnlyTable {
        /// The field.
        field: String,
    },
    /// A field is both `read_only` and `required`: a caller could never provide the value.
    #[error("field `{field}`: a read_only field cannot be required")]
    ReadOnlyRequired {
        /// The field.
        field: String,
    },
    /// `title_field` or `list_fields` on a child DocType.
    #[error("a child doctype cannot have `{key}`")]
    ChildWithDeskKey {
        /// `title_field` or `list_fields`.
        key: &'static str,
    },
    /// `title_field` names a field the DocType does not have.
    #[error("title_field `{field}` is not a field of the doctype")]
    TitleFieldMissing {
        /// The named field.
        field: String,
    },
    /// `title_field` names a field of a kind that cannot be a title.
    #[error(
        "title_field `{field}` is a {found} field (allowed: data, int, decimal, date, datetime, select, link)"
    )]
    TitleFieldKind {
        /// The named field.
        field: String,
        /// Its actual type name.
        found: &'static str,
    },
    /// `list_fields` is an empty list.
    #[error("list_fields must not be empty (leave it out for the default)")]
    EmptyListFields,
    /// `list_fields` has more than [`MAX_LIST_FIELDS`] entries.
    #[error("list_fields has {count} fields, at most {max} are allowed")]
    TooManyListFields {
        /// Number of entries.
        count: usize,
        /// The limit.
        max: usize,
    },
    /// `list_fields` names a field the DocType does not have.
    #[error("list_fields `{field}` is not a field of the doctype")]
    ListFieldMissing {
        /// The named field.
        field: String,
    },
    /// `list_fields` names a Table or Text field.
    #[error("list_fields `{field}` is a {found} field (table and text fields cannot be listed)")]
    ListFieldKind {
        /// The named field.
        field: String,
        /// Its actual type name.
        found: &'static str,
    },
    /// `list_fields` names a field twice.
    #[error("list_fields names `{field}` more than once")]
    DuplicateListField {
        /// The repeated field.
        field: String,
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
    /// A `[[permissions]]` rule names a role that breaks the role rule
    /// ([`check_role_name`](crate::permission::check_role_name)).
    #[error("permission rule for role {role:?}: invalid role: {reason}")]
    InvalidPermissionRole {
        /// The rejected role.
        role: String,
        /// Why it was rejected.
        reason: &'static str,
    },
    /// A `[[permissions]]` rule grants nothing (every right is false), which is most likely a
    /// mistake.
    #[error("permission rule for role {role:?} grants no right")]
    EmptyPermissionRule {
        /// The rule's role.
        role: String,
    },
    /// A `[[permissions]]` rule grants submit, cancel or amend on a DocType that is not
    /// submittable.
    #[error(
        "permission rule for role {role:?}: `{right}` needs a submittable doctype (`submittable = true`)"
    )]
    PermissionNeedsSubmittable {
        /// The rule's role.
        role: String,
        /// The right that cannot be granted.
        right: Right,
    },
    /// Two `[[permissions]]` rules have the same role and the same `if_owner`; merge them.
    #[error("more than one permission rule for role {role:?} with if_owner = {if_owner}")]
    DuplicatePermissionRule {
        /// The repeated role.
        role: String,
        /// The repeated `if_owner`.
        if_owner: bool,
    },
    /// A child DocType has `[[permissions]]`; its rows follow the permissions of the parent.
    #[error("a child doctype has no permission rules (its rows follow the parent's)")]
    ChildWithPermissions,
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
    /// A child DocType has a `[workflow]`; only the parent has states.
    #[error("a child doctype cannot have a workflow")]
    ChildWithWorkflow,
    /// A workflow has no states.
    #[error("workflow: at least one state is required (the first is the initial state)")]
    WorkflowNoStates,
    /// The first state, the initial one, is not a draft.
    #[error("workflow: the first state `{state}` is the initial state and must have docstatus 0")]
    WorkflowInitialNotDraft {
        /// The first state.
        state: String,
    },
    /// The name of a state or an action is empty (after trimming), too long or has a NUL character.
    #[error("workflow: {kind} name `{name}` {reason}")]
    WorkflowName {
        /// `state` or `action`.
        kind: &'static str,
        /// The rejected name.
        name: String,
        /// Why it was rejected.
        reason: &'static str,
    },
    /// Two states share a name.
    #[error("workflow: state `{state}` is defined more than once")]
    WorkflowDuplicateState {
        /// The repeated name.
        state: String,
    },
    /// The docstatus of a state is not 0, 1 or 2.
    #[error("workflow: state `{state}`: docstatus {found} is not 0, 1 or 2")]
    WorkflowInvalidDocStatus {
        /// The state.
        state: String,
        /// The rejected value.
        found: i64,
    },
    /// The style of a state is not one of the known ones.
    #[error(
        "workflow: state `{state}`: unknown style `{found}` (expected one of neutral, success, warning, danger, info)"
    )]
    WorkflowUnknownStyle {
        /// The state.
        state: String,
        /// The rejected style.
        found: String,
    },
    /// A state has docstatus 1 or 2 on a DocType that is not submittable.
    #[error(
        "workflow: state `{state}` has docstatus {docstatus}, which needs `submittable = true`"
    )]
    WorkflowStateNeedsSubmittable {
        /// The state.
        state: String,
        /// Its docstatus.
        docstatus: i16,
    },
    /// A state with docstatus 1 or 2 has `edit_roles`; only a draft can be edited by them.
    #[error("workflow: state `{state}`: edit_roles are only allowed on a state with docstatus 0")]
    WorkflowEditRolesNotDraft {
        /// The state.
        state: String,
    },
    /// A role of a state's `edit_roles` or of a transition's `roles` is not a valid role name.
    #[error("workflow: {place}: invalid role `{role}`: {reason}")]
    WorkflowInvalidRole {
        /// Where the role is written: `state `x` edit_roles` or `transition `a` from `b``.
        place: String,
        /// The rejected role.
        role: String,
        /// Why it was rejected.
        reason: &'static str,
    },
    /// A transition names a state that does not exist in `from` or `to`.
    #[error("workflow: transition `{action}` from `{from}`: unknown state `{state}` in `{key}`")]
    WorkflowUnknownState {
        /// The transition's action.
        action: String,
        /// The transition's `from`.
        from: String,
        /// `from` or `to`.
        key: &'static str,
        /// The name that is not a state.
        state: String,
    },
    /// Two transitions from the same state have the same action.
    #[error("workflow: action `{action}` is defined more than once from state `{from}`")]
    WorkflowDuplicateAction {
        /// The action.
        action: String,
        /// The state.
        from: String,
    },
    /// A transition has no roles.
    #[error("workflow: transition `{action}` from `{from}` needs at least one role")]
    WorkflowNoRoles {
        /// The action.
        action: String,
        /// The state.
        from: String,
    },
    /// A role is listed twice in the roles of one transition.
    #[error(
        "workflow: transition `{action}` from `{from}`: role `{role}` is listed more than once"
    )]
    WorkflowDuplicateRole {
        /// The action.
        action: String,
        /// The state.
        from: String,
        /// The repeated role.
        role: String,
    },
    /// A transition leads to its own state.
    #[error("workflow: transition `{action}` leads from state `{state}` to itself")]
    WorkflowSelfTransition {
        /// The action.
        action: String,
        /// The state.
        state: String,
    },
    /// A transition makes a docstatus move that is not allowed.
    #[error(
        "workflow: transition `{action}` from `{from}` to `{to}` moves docstatus {from_docstatus} to {to_docstatus}; only 0 to 0, 0 to 1, 1 to 1 and 1 to 2 are allowed"
    )]
    WorkflowBadMove {
        /// The action.
        action: String,
        /// The `from` state.
        from: String,
        /// The `to` state.
        to: String,
        /// Docstatus of `from`.
        from_docstatus: i16,
        /// Docstatus of `to`.
        to_docstatus: i16,
    },
    /// No chain of transitions leads from the initial state to this state.
    #[error("workflow: state `{state}` cannot be reached from the initial state")]
    WorkflowUnreachableState {
        /// The state.
        state: String,
    },
    /// A field is named `workflow_state` on a DocType with a workflow, which has that column.
    #[error("field `workflow_state` clashes with the column of the workflow state")]
    WorkflowStateFieldName,
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
        for field in &self.fields {
            if !field.allow_on_submit {
                continue;
            }
            if matches!(field.field_type, FieldType::Table { .. }) {
                errors.push(DefinitionError::AllowOnSubmitTable {
                    field: field.name.clone(),
                });
            } else if !self.submittable {
                errors.push(DefinitionError::AllowOnSubmitNotSubmittable {
                    field: field.name.clone(),
                });
            }
        }
        for field in &self.fields {
            if !field.read_only {
                continue;
            }
            if matches!(field.field_type, FieldType::Table { .. }) {
                errors.push(DefinitionError::ReadOnlyTable {
                    field: field.name.clone(),
                });
            }
            if field.required {
                errors.push(DefinitionError::ReadOnlyRequired {
                    field: field.name.clone(),
                });
            }
        }
        self.check_desk_keys(failed, &mut errors);
        self.check_permissions(&mut errors);
        self.check_workflow(failed, &mut errors);
        if self.submittable {
            if self.is_child {
                errors.push(DefinitionError::ChildSubmittable);
            }
            if matches!(self.naming, Naming::Field(_)) {
                errors.push(DefinitionError::SubmittableWithFieldNaming);
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

    /// `title_field` and `list_fields`. A name in `failed` is a field that could not be converted;
    /// its real problem is already reported, so it is not reported again as missing.
    fn check_desk_keys(&self, failed: &[String], errors: &mut Vec<DefinitionError>) {
        if self.is_child {
            if self.title_field.is_some() {
                errors.push(DefinitionError::ChildWithDeskKey { key: "title_field" });
            }
            if self.list_fields.is_some() {
                errors.push(DefinitionError::ChildWithDeskKey { key: "list_fields" });
            }
            return;
        }
        if let Some(name) = &self.title_field
            && !failed.contains(name)
        {
            match self.field(name) {
                None => errors.push(DefinitionError::TitleFieldMissing {
                    field: name.clone(),
                }),
                Some(f) if !TITLE_FIELD_KINDS.contains(&f.field_type.name()) => {
                    errors.push(DefinitionError::TitleFieldKind {
                        field: name.clone(),
                        found: f.field_type.name(),
                    });
                }
                Some(_) => {}
            }
        }
        if let Some(list) = &self.list_fields {
            if list.is_empty() {
                errors.push(DefinitionError::EmptyListFields);
            }
            if list.len() > MAX_LIST_FIELDS {
                errors.push(DefinitionError::TooManyListFields {
                    count: list.len(),
                    max: MAX_LIST_FIELDS,
                });
            }
            let mut seen = BTreeSet::new();
            for name in list {
                if !seen.insert(name.as_str()) {
                    errors.push(DefinitionError::DuplicateListField {
                        field: name.clone(),
                    });
                } else if failed.contains(name) {
                    continue;
                } else {
                    match self.field(name) {
                        None => errors.push(DefinitionError::ListFieldMissing {
                            field: name.clone(),
                        }),
                        Some(f)
                            if matches!(
                                f.field_type,
                                FieldType::Table { .. } | FieldType::Text
                            ) =>
                        {
                            errors.push(DefinitionError::ListFieldKind {
                                field: name.clone(),
                                found: f.field_type.name(),
                            });
                        }
                        Some(_) => {}
                    }
                }
            }
        }
    }

    fn check_permissions(&self, errors: &mut Vec<DefinitionError>) {
        if self.is_child && !self.permissions.is_empty() {
            errors.push(DefinitionError::ChildWithPermissions);
        }
        let mut seen = BTreeSet::new();
        for rule in &self.permissions {
            let role = || rule.role.clone();
            if let Err(reason) = permission::check_role_name(&rule.role) {
                errors.push(DefinitionError::InvalidPermissionRole {
                    role: role(),
                    reason,
                });
            }
            if Right::ALL.iter().all(|right| !rule.grants(*right)) {
                errors.push(DefinitionError::EmptyPermissionRule { role: role() });
            }
            if !self.submittable {
                for right in [Right::Submit, Right::Cancel, Right::Amend] {
                    if rule.grants(right) {
                        errors.push(DefinitionError::PermissionNeedsSubmittable {
                            role: role(),
                            right,
                        });
                    }
                }
            }
            if !seen.insert((rule.role.as_str(), rule.if_owner)) {
                errors.push(DefinitionError::DuplicatePermissionRule {
                    role: role(),
                    if_owner: rule.if_owner,
                });
            }
        }
    }

    /// The `[workflow]`: names, docstatus rules, roles, transitions and reachability.
    fn check_workflow(&self, failed: &[String], errors: &mut Vec<DefinitionError>) {
        let Some(workflow) = &self.workflow else {
            return;
        };
        if self.is_child {
            errors.push(DefinitionError::ChildWithWorkflow);
            return;
        }
        if self.field(WORKFLOW_COLUMN).is_some() || failed.iter().any(|f| f == WORKFLOW_COLUMN) {
            errors.push(DefinitionError::WorkflowStateFieldName);
        }
        let Some(initial) = workflow.initial() else {
            errors.push(DefinitionError::WorkflowNoStates);
            return;
        };
        if initial.docstatus != DocStatus::Draft {
            errors.push(DefinitionError::WorkflowInitialNotDraft {
                state: initial.name.clone(),
            });
        }
        let mut names = BTreeSet::new();
        for state in &workflow.states {
            check_workflow_name("state", &state.name, errors);
            if !names.insert(state.name.as_str()) {
                errors.push(DefinitionError::WorkflowDuplicateState {
                    state: state.name.clone(),
                });
            }
            if state.docstatus != DocStatus::Draft && !self.submittable {
                errors.push(DefinitionError::WorkflowStateNeedsSubmittable {
                    state: state.name.clone(),
                    docstatus: state.docstatus.as_i16(),
                });
            }
            if !state.edit_roles.is_empty() && state.docstatus != DocStatus::Draft {
                errors.push(DefinitionError::WorkflowEditRolesNotDraft {
                    state: state.name.clone(),
                });
            }
            let place = format!("state `{}` edit_roles", state.name);
            for role in &state.edit_roles {
                check_workflow_role(&place, role, errors);
            }
        }
        let mut actions = BTreeSet::new();
        for t in &workflow.transitions {
            check_workflow_name("action", &t.action, errors);
            if !actions.insert((t.from.as_str(), t.action.as_str())) {
                errors.push(DefinitionError::WorkflowDuplicateAction {
                    action: t.action.clone(),
                    from: t.from.clone(),
                });
            }
            for (key, state) in [("from", &t.from), ("to", &t.to)] {
                if !names.contains(state.as_str()) {
                    errors.push(DefinitionError::WorkflowUnknownState {
                        action: t.action.clone(),
                        from: t.from.clone(),
                        key,
                        state: state.clone(),
                    });
                }
            }
            if t.roles.is_empty() {
                errors.push(DefinitionError::WorkflowNoRoles {
                    action: t.action.clone(),
                    from: t.from.clone(),
                });
            }
            let place = format!("transition `{}` from `{}`", t.action, t.from);
            let mut roles = BTreeSet::new();
            for role in &t.roles {
                check_workflow_role(&place, role, errors);
                if !roles.insert(role.as_str()) {
                    errors.push(DefinitionError::WorkflowDuplicateRole {
                        action: t.action.clone(),
                        from: t.from.clone(),
                        role: role.clone(),
                    });
                }
            }
            if t.from == t.to && names.contains(t.from.as_str()) {
                errors.push(DefinitionError::WorkflowSelfTransition {
                    action: t.action.clone(),
                    state: t.from.clone(),
                });
            } else if let (Some(from), Some(to)) = (workflow.state(&t.from), workflow.state(&t.to))
                && Move::of(from.docstatus, to.docstatus).is_none()
            {
                errors.push(DefinitionError::WorkflowBadMove {
                    action: t.action.clone(),
                    from: t.from.clone(),
                    to: t.to.clone(),
                    from_docstatus: from.docstatus.as_i16(),
                    to_docstatus: to.docstatus.as_i16(),
                });
            }
        }
        // Reachability from the initial state, over the transitions that name existing states.
        let mut reached = BTreeSet::from([initial.name.as_str()]);
        let mut queue = vec![initial.name.as_str()];
        while let Some(from) = queue.pop() {
            for t in workflow.transitions_from(from) {
                if names.contains(t.to.as_str()) && reached.insert(t.to.as_str()) {
                    queue.push(t.to.as_str());
                }
            }
        }
        for state in &workflow.states {
            if !reached.contains(state.name.as_str()) {
                errors.push(DefinitionError::WorkflowUnreachableState {
                    state: state.name.clone(),
                });
            }
        }
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

/// The name of the framework column that holds the state of a document of a DocType with a
/// workflow.
pub const WORKFLOW_COLUMN: &str = "workflow_state";

/// A state or action name: not blank, no leading or trailing whitespace, at most
/// [`MAX_NAME_LEN`] characters, no NUL.
fn check_workflow_name(kind: &'static str, name: &str, errors: &mut Vec<DefinitionError>) {
    let reason = if name.trim().is_empty() {
        "must not be empty"
    } else if name.trim() != name {
        "must not start or end with whitespace"
    } else if name.chars().count() > MAX_NAME_LEN {
        "is longer than 64 characters"
    } else if name.contains('\0') {
        "must not contain a NUL character"
    } else {
        return;
    };
    errors.push(DefinitionError::WorkflowName {
        kind,
        name: name.to_owned(),
        reason,
    });
}

fn check_workflow_role(place: &str, role: &str, errors: &mut Vec<DefinitionError>) {
    if let Err(reason) = permission::check_role_name(role) {
        errors.push(DefinitionError::WorkflowInvalidRole {
            place: place.to_owned(),
            role: role.to_owned(),
            reason,
        });
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

/// A set of DocTypes keyed by name, an optional owner tag per DocType, and the lifecycle hooks
/// the app registered for them.
///
/// Cloning is cheap for the hooks (they are shared). A registry is not comparable: hook sets
/// are trait objects.
#[derive(Clone, Default)]
pub struct Registry {
    doctypes: BTreeMap<String, DocType>,
    /// DocType name -> the tag of the app that owns it (only for DocTypes added with
    /// [`Registry::add_owned`]).
    owners: BTreeMap<String, String>,
    hooks: BTreeMap<String, Vec<Arc<dyn DocTypeHooks>>>,
    /// The full names (`<app>.<job>`) of the background jobs the binary knows. Names only: the
    /// registry knows nothing about handlers.
    jobs: BTreeSet<String>,
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
            .field("jobs", &self.jobs)
            .finish()
    }
}

impl Registry {
    /// Creates an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a background job name (`<app>.<job>`) as known, so that enqueueing it is
    /// accepted. Returns `false` when the name was already known. The name is not checked here.
    pub fn add_job(&mut self, name: &str) -> bool {
        self.jobs.insert(name.to_owned())
    }

    /// Whether a job with this full name is known.
    pub fn has_job(&self, name: &str) -> bool {
        self.jobs.contains(name)
    }

    /// The known job names, in name order.
    pub fn job_names(&self) -> impl Iterator<Item = &str> {
        self.jobs.iter().map(String::as_str)
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

    /// Like [`Registry::add`], and records `owner` as the owner of the DocType
    /// ([`Registry::owner`]). The owner is only a tag (an app name): this crate attaches no
    /// rules to it.
    pub fn add_owned(&mut self, owner: &str, doctype: DocType) -> Result<(), Vec<DefinitionError>> {
        let name = doctype.name.clone();
        self.add(doctype)?;
        self.owners.insert(name, owner.to_owned());
        Ok(())
    }

    /// The owner tag given to [`Registry::add_owned`], or `None` for a DocType added with
    /// [`Registry::add`] (and for an unknown name).
    pub fn owner(&self, doctype: &str) -> Option<&str> {
        self.owners.get(doctype).map(String::as_str)
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

    #[test]
    fn humanize_splits_on_underscores_and_capitalises() {
        for (name, label) in [
            ("order_date", "Order Date"),
            ("qty", "Qty"),
            ("customer_group", "Customer Group"),
            ("a_b_c", "A B C"),
            ("item2_total", "Item2 Total"),
            ("", ""),
        ] {
            assert_eq!(humanize(name), label, "{name}");
        }
    }

    fn field(name: &str, field_type: FieldType) -> DocField {
        DocField {
            name: name.into(),
            label: name.into(),
            field_type,
            required: false,
            unique: false,
            allow_on_submit: false,
            read_only: false,
        }
    }

    fn doctype(fields: Vec<DocField>) -> DocType {
        DocType {
            name: "thing".into(),
            label: "Thing".into(),
            naming: Naming::Random,
            fields,
            is_child: false,
            submittable: false,
            permissions: vec![],
            title_field: None,
            list_fields: None,
            workflow: None,
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

    /// A hook set that does nothing; tests tell registered sets apart by allocation.
    struct Named;

    impl DocTypeHooks for Named {}

    #[test]
    fn hooks_are_kept_in_registration_order() {
        let first = Arc::new(Named);
        let second = Arc::new(Named);
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
        let hooks: Arc<dyn DocTypeHooks> = Arc::new(Named);
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
        registry.add_hooks("thing", Arc::new(Named)).unwrap();
        registry.add_hooks("thing", Arc::new(Named)).unwrap();
        let text = format!("{registry:?}");
        assert!(text.contains("hooks: {\"thing\": 2}"), "{text}");
        assert!(text.contains("doctypes:"), "{text}");
    }

    #[test]
    fn owner_is_a_tag_set_by_add_owned() {
        let mut registry = Registry::new();
        registry.add_owned("sales", doctype(vec![])).unwrap();
        let mut other = doctype(vec![]);
        other.name = "other".into();
        registry.add(other).unwrap();
        assert_eq!(registry.owner("thing"), Some("sales"));
        assert_eq!(registry.owner("other"), None);
        assert_eq!(registry.owner("unknown"), None);
        // A refused DocType records no owner.
        let mut dup = doctype(vec![]);
        dup.name = "other".into();
        assert!(registry.add_owned("crm", dup).is_err());
        assert_eq!(registry.owner("other"), None);
    }
}
