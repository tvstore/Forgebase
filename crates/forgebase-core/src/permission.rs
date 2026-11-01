//! The permission model, phase 1 (M3.2, ADR 0011): rules, rights, the caller and the one
//! evaluator that decides.
//!
//! A DocType declares `[[permissions]]` rules: each names a role and the [`Right`]s it grants,
//! optionally only on documents the caller owns (`if_owner`). There is **no inheritance and no
//! deny rule**: access is the union of what the caller's roles grant, and a DocType without
//! rules is inaccessible to everyone except the [`ADMINISTRATOR`] role and the trusted
//! [`Caller::System`] context (deny by default). Child DocTypes have no rules: their rows follow
//! the parent.
//!
//! [`evaluate`] is the **only** place that makes a permission decision. It is pure (no I/O, no
//! clock): `forgebase-db` calls it and turns the [`Access`] it returns into SQL (for read access,
//! `WHERE owner = $caller`) or into a refusal. Nothing else compares roles.
//!
//! **Field levels (M10.2, ADR 0024).** A field has a `perm_level` (0 to 9, 0 by default) and a
//! rule has a `level`. [`evaluate`] decides about documents with the rules of level 0 only;
//! [`field_access`] decides which levels of fields a caller reads and writes, and
//! [`readable_fields`], [`readable_field`] and [`readable_doctype`] apply the answer to the
//! metadata. A field the caller cannot read is *unknown* to the caller: it is not in the
//! metadata the caller works with, so every lookup of it fails exactly like a lookup of a name
//! that does not exist.
//!
//! **User permissions (M10.3, ADR 0025).** A [`Caller::User`] carries its [`UserRestrictions`]:
//! for some DocTypes, the names of the only documents the user may see. [`row_scope`] turns them
//! into a [`RowScope`] for one DocType, the rows of it the caller may see on top of the rights
//! of [`evaluate`]: `forgebase-db` only turns that into SQL, so what a caller may see is decided
//! here and nowhere else.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use crate::meta::{DocField, DocType, FieldType};
use crate::method::MethodAccess;
use crate::workflow::Transition;

/// The built-in role that bypasses every permission rule.
pub const ADMINISTRATOR: &str = "Administrator";

/// The highest permission level of a field or a rule (levels are 0 to 9).
pub const MAX_PERM_LEVEL: u8 = 9;

/// The most documents one user can be restricted to on one DocType (M10.3, ADR 0025). The names
/// are bound as one array in every statement that reads documents, so the number is kept small.
pub const MAX_USER_PERMISSIONS: usize = 1000;

/// Longest allowed role name, in characters.
pub const MAX_ROLE_LEN: usize = 140;

/// Checks a role name: 1 to [`MAX_ROLE_LEN`] characters, not blank, no control characters. The
/// same rule applies to the roles of users (`forgebase-db`'s `auth` module) and to the roles in
/// `[[permissions]]`, so a rule can always name a role a user can have. The error is the reason,
/// ready to follow "invalid role: ".
pub fn check_role_name(role: &str) -> Result<(), &'static str> {
    let length = role.chars().count();
    if length == 0 || length > MAX_ROLE_LEN {
        Err("must be 1 to 140 characters")
    } else if role.trim().is_empty() {
        Err("must not be blank")
    } else if role.chars().any(char::is_control) {
        Err("must not contain control characters")
    } else {
        Ok(())
    }
}

/// One thing a caller may do with documents of a DocType.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Right {
    /// See documents (get and list).
    Read,
    /// Change a document.
    Write,
    /// Create a document.
    Create,
    /// Delete a document.
    Delete,
    /// Submit a draft (submittable DocTypes).
    Submit,
    /// Cancel a submitted document (submittable DocTypes).
    Cancel,
    /// Amend a cancelled document (submittable DocTypes).
    Amend,
    /// Download documents as a file (M9.3, ADR 0021). Needs `read` as well.
    Export,
    /// Upload a file of documents (M9.3, ADR 0021). Needs `create` (and `write` to upsert).
    Import,
}

impl Right {
    /// Every right, in the order of the `[[permissions]]` keys.
    pub const ALL: [Right; 9] = [
        Right::Read,
        Right::Write,
        Right::Create,
        Right::Delete,
        Right::Submit,
        Right::Cancel,
        Right::Amend,
        Right::Export,
        Right::Import,
    ];

    /// The key used in `[[permissions]]` and in messages: `read`, `write`, ...
    pub fn name(self) -> &'static str {
        match self {
            Right::Read => "read",
            Right::Write => "write",
            Right::Create => "create",
            Right::Delete => "delete",
            Right::Submit => "submit",
            Right::Cancel => "cancel",
            Right::Amend => "amend",
            Right::Export => "export",
            Right::Import => "import",
        }
    }
}

impl fmt::Display for Right {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// A `[[permissions]]` entry of a DocType: the rights one role has, all `false` by default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionRule {
    /// The role the rule is for, see [`check_role_name`].
    pub role: String,
    /// May read.
    pub read: bool,
    /// May write.
    pub write: bool,
    /// May create.
    pub create: bool,
    /// May delete.
    pub delete: bool,
    /// May submit. Only allowed on a submittable DocType.
    pub submit: bool,
    /// May cancel. Only allowed on a submittable DocType.
    pub cancel: bool,
    /// May amend. Only allowed on a submittable DocType.
    pub amend: bool,
    /// May export documents as a file (needs `read` too).
    pub export: bool,
    /// May import documents from a file (needs `create`, and `write` to upsert).
    pub import: bool,
    /// The rights apply to documents whose `owner` is the caller only. (A create is always for
    /// a new document the caller owns, so `if_owner` does not restrict create.)
    pub if_owner: bool,
    /// The permission level of the rule (M10.2, ADR 0024), 0 to [`MAX_PERM_LEVEL`], 0 by
    /// default. A rule at level 0 is a document rule as in ADR 0011. A rule at level `n > 0`
    /// grants `read` and `write` on the fields marked `perm_level = n` and nothing else (see
    /// [`field_access`]).
    pub level: u8,
}

impl PermissionRule {
    /// A rule for `role` that grants nothing yet: set the rights with struct update syntax
    /// (`PermissionRule { read: true, ..PermissionRule::new("Sales User") }`).
    pub fn new(role: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            read: false,
            write: false,
            create: false,
            delete: false,
            submit: false,
            cancel: false,
            amend: false,
            export: false,
            import: false,
            if_owner: false,
            level: 0,
        }
    }

    /// Whether the rule grants `right` (ignoring `if_owner`).
    pub fn grants(&self, right: Right) -> bool {
        match right {
            Right::Read => self.read,
            Right::Write => self.write,
            Right::Create => self.create,
            Right::Delete => self.delete,
            Right::Submit => self.submit,
            Right::Cancel => self.cancel,
            Right::Amend => self.amend,
            Right::Export => self.export,
            Right::Import => self.import,
        }
    }
}

/// Who performs an operation: a real user, or the trusted system.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Caller {
    /// A user with the roles they have. The roles come from the database (`fb_user_role`) at the
    /// time the caller is built; this type does not check them. The user is
    /// an administrator if `roles` contains [`ADMINISTRATOR`].
    User {
        /// The username, recorded as `owner` and `modified_by` and given to hooks.
        username: String,
        /// The user's roles.
        roles: Vec<String>,
        /// The documents the user is restricted to (M10.3, ADR 0025); empty for a user without
        /// user permissions. Read from the database with the roles.
        restrictions: UserRestrictions,
    },
    /// The trusted system context: bypasses every permission rule. For code that is not acting for
    /// a user, such as the command-line tool, migrations, jobs and tests. **Never build one from
    /// request data.** `actor` is recorded as `owner` and `modified_by` and given to hooks, like
    /// a username.
    System {
        /// Who to record.
        actor: String,
    },
}

impl Caller {
    /// A user caller.
    pub fn user(username: impl Into<String>, roles: Vec<String>) -> Self {
        Caller::User {
            username: username.into(),
            roles,
            restrictions: UserRestrictions::default(),
        }
    }

    /// This caller with the given user permissions (see [`UserRestrictions`]). The system
    /// context is returned unchanged: it is never restricted.
    pub fn with_restrictions(self, restrictions: UserRestrictions) -> Self {
        match self {
            Caller::User {
                username, roles, ..
            } => Caller::User {
                username,
                roles,
                restrictions,
            },
            system => system,
        }
    }

    /// The trusted system caller recorded as `actor`.
    pub fn system(actor: impl Into<String>) -> Self {
        Caller::System {
            actor: actor.into(),
        }
    }

    /// The name recorded as `owner` / `modified_by` and given to hooks: the username, or the
    /// system actor.
    pub fn name(&self) -> &str {
        match self {
            Caller::User { username, .. } => username,
            Caller::System { actor } => actor,
        }
    }

    /// True for the system context and for a user with the [`ADMINISTRATOR`] role: the callers
    /// the rules do not apply to.
    pub fn bypasses_permissions(&self) -> bool {
        match self {
            Caller::System { .. } => true,
            Caller::User { roles, .. } => roles.iter().any(|r| r == ADMINISTRATOR),
        }
    }
}

/// The user permissions of one user (M10.3, ADR 0025): for a DocType, the names of the only
/// documents of it the user may see. A DocType that is not listed is **not restricted** for the
/// user, and so is one with an empty list (the empty value means unrestricted). Cheap to clone.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UserRestrictions(Arc<BTreeMap<String, Vec<String>>>);

impl UserRestrictions {
    /// The restrictions made of `(doctype, name)` pairs: each DocType's names are sorted and
    /// deduplicated.
    pub fn from_pairs<I, D, N>(pairs: I) -> Self
    where
        I: IntoIterator<Item = (D, N)>,
        D: Into<String>,
        N: Into<String>,
    {
        let mut map: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (doctype, name) in pairs {
            map.entry(doctype.into()).or_default().push(name.into());
        }
        for names in map.values_mut() {
            names.sort();
            names.dedup();
        }
        Self(Arc::new(map))
    }

    /// The names the user may see of `doctype`, sorted; `None` when the user is not restricted
    /// on it.
    pub fn allowed(&self, doctype: &str) -> Option<&[String]> {
        self.0
            .get(doctype)
            .map(Vec::as_slice)
            .filter(|names| !names.is_empty())
    }

    /// True when the user has no user permissions at all.
    pub fn is_empty(&self) -> bool {
        self.0.values().all(Vec::is_empty)
    }
}

/// One top-level Link field of a DocType whose target the caller is restricted on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LinkScope<'a> {
    /// The Link field.
    pub field: &'a str,
    /// The DocType the field points at.
    pub target: &'a str,
    /// The names of the target the caller may see (sorted, never empty).
    pub allowed: &'a [String],
}

/// Which documents of one DocType a caller may see because of their user permissions, as decided
/// by [`row_scope`]. It is **in addition to** the rights of [`evaluate`] (and to `if_owner`):
/// every part must hold. Empty for a caller without a restriction that touches the DocType.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RowScope<'a> {
    /// The DocType itself is restricted: the document's own name must be one of these.
    pub names: Option<&'a [String]>,
    /// Every top-level Link field to a restricted target: its value must be empty or one of
    /// the allowed names of its target. All of them must hold (AND), in declaration order.
    pub links: Vec<LinkScope<'a>>,
}

impl RowScope<'_> {
    /// True when nothing is restricted: every statement is then exactly what it is without user
    /// permissions.
    pub fn is_empty(&self) -> bool {
        self.names.is_none() && self.links.is_empty()
    }
}

/// Decides which documents of `doctype` `caller` may see because of their user permissions
/// (M10.3, ADR 0025). Pure, like [`evaluate`].
///
/// - The system context, an administrator and a user without restrictions get the empty scope.
/// - **Own name.** If the caller is restricted on `doctype` itself, the name of a document must
///   be one of the allowed names.
/// - **Links.** For each top-level Link field of `doctype` whose target the caller is restricted
///   on, the value must be empty or one of the allowed names of the target. Every such field
///   must hold, whatever its `perm_level`: a field the caller cannot read still restricts, and
///   its value is never revealed. A field that is not in `doctype` (a disabled custom field is
///   not in the effective registry) restricts nothing.
/// - **Child rows are not considered**: a Link inside a row of a Table field does not decide the
///   parent's visibility. A child DocType is never read alone and has an empty scope.
pub fn row_scope<'a>(doctype: &'a DocType, caller: &'a Caller) -> RowScope<'a> {
    let Caller::User { restrictions, .. } = caller else {
        return RowScope::default();
    };
    if doctype.is_child || restrictions.is_empty() || caller.bypasses_permissions() {
        return RowScope::default();
    }
    let links = doctype
        .fields
        .iter()
        .filter_map(|field| {
            let FieldType::Link { target } = &field.field_type else {
                return None;
            };
            Some(LinkScope {
                field: &field.name,
                target,
                allowed: restrictions.allowed(target)?,
            })
        })
        .collect();
    RowScope {
        names: restrictions.allowed(&doctype.name),
        links,
    }
}

/// How much of a DocType's documents a caller may use a right on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// Every document.
    All,
    /// Only documents whose `owner` is the caller (`if_owner`).
    OwnerOnly,
    /// No document. Not a decision about one document: it does not depend on the data.
    Denied,
}

/// Decides how far `caller` may use `right` on documents of `doctype`. The one permission
/// decision of Forgebase (see the module documentation).
///
/// - The system context and an administrator get [`Access::All`] for every right on every DocType.
/// - Otherwise the result is the union over the rules whose role the caller has and that grant
///   `right`: a rule without `if_owner` gives [`Access::All`]; if there is none, a rule with
///   `if_owner` gives [`Access::OwnerOnly`]; if there is none, [`Access::Denied`].
/// - A DocType without rules (a child DocType too) is [`Access::Denied`] for everyone else.
///
/// A role name is compared exactly (case-sensitive, like the roles of users).
pub fn evaluate(doctype: &DocType, caller: &Caller, right: Right) -> Access {
    if caller.bypasses_permissions() {
        return Access::All;
    }
    let Caller::User { roles, .. } = caller else {
        return Access::Denied; // unreachable: the system caller bypasses above
    };
    let mut access = Access::Denied;
    // Only the rules of level 0 decide about documents; a rule of a higher level is about fields.
    for rule in doctype.permissions.iter().filter(|rule| rule.level == 0) {
        if rule.grants(right) && roles.contains(&rule.role) {
            if !rule.if_owner {
                return Access::All;
            }
            access = Access::OwnerOnly;
        }
    }
    access
}

/// Which permission levels of fields a caller may read and write on one DocType, as decided by
/// [`field_access`]: a bit per level 0 to [`MAX_PERM_LEVEL`].
///
/// Level 0 is **not** a filter on the fields: a field at level 0 is governed by the document
/// rights alone (the caller is refused or not before any field is looked at), so a DocType
/// without `perm_level` behaves exactly as before. The level 0 bits are the base that the higher
/// levels need ([`field_access`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FieldAccess {
    read: u16,
    write: u16,
}

impl FieldAccess {
    const MASK: u16 = (1 << (MAX_PERM_LEVEL as u16 + 1)) - 1;

    /// Every level readable and writable: what the system context and an administrator have.
    pub const ALL: FieldAccess = FieldAccess {
        read: Self::MASK,
        write: Self::MASK,
    };

    /// Whether fields at `level` may be read. A level above [`MAX_PERM_LEVEL`] never can.
    pub fn can_read(self, level: u8) -> bool {
        level <= MAX_PERM_LEVEL && self.read & (1 << level) != 0
    }

    /// Whether a field at `level` is shown to the caller: every field at level 0 (the document
    /// rights decide about those, see the type documentation) and a field at a higher level the
    /// caller can read. What the helpers that hide fields use, so a DocType without `perm_level`
    /// is never changed by them.
    pub fn shows(self, level: u8) -> bool {
        level == 0 || self.can_read(level)
    }

    /// Whether fields at `level` may be written. A level above [`MAX_PERM_LEVEL`] never can.
    pub fn can_write(self, level: u8) -> bool {
        level <= MAX_PERM_LEVEL && self.write & (1 << level) != 0
    }

    /// True when every level can be read and written (no field is hidden or protected).
    pub fn is_all(self) -> bool {
        self == Self::ALL
    }
}

/// Decides which levels of fields `caller` may read and write on `doctype` (M10.2, ADR 0024).
/// Pure, like [`evaluate`], and built on it:
///
/// - The system context and an administrator get every level.
/// - **Level 0** is readable when [`evaluate`] does not deny `read`, and writable when it does
///   not deny `write` **or** `create`. Whether the caller may write one particular document is
///   decided by the document checks of `forgebase-db`, which run first and name their own right
///   (`create` for an insert, `write` for an update); the bit only says that the caller has one
///   of them at all, so that the higher levels need it too.
/// - **Level `n > 0`** is readable when level 0 is readable **and** a role of the caller has a
///   rule `level = n` with `read`; it is writable when level 0 is writable **and** a role of
///   the caller has a rule `level = n` with `write`. Rules of a level have no `if_owner` (a
///   definition error), so `if_owner` at level 0 alone limits the documents a level is seen on:
///   an owner-only user sees a level on their own documents only.
/// - A level that no rule names is readable by nobody but the administrator and the system.
pub fn field_access(doctype: &DocType, caller: &Caller) -> FieldAccess {
    let Caller::User { roles, .. } = caller else {
        return FieldAccess::ALL;
    };
    if caller.bypasses_permissions() {
        return FieldAccess::ALL;
    }
    let readable = evaluate(doctype, caller, Right::Read) != Access::Denied;
    let writable = evaluate(doctype, caller, Right::Write) != Access::Denied
        || evaluate(doctype, caller, Right::Create) != Access::Denied;
    let mut access = FieldAccess { read: 0, write: 0 };
    if readable {
        access.read |= 1;
    }
    if writable {
        access.write |= 1;
    }
    for rule in doctype
        .permissions
        .iter()
        .filter(|rule| rule.level > 0 && rule.level <= MAX_PERM_LEVEL)
    {
        if !roles.contains(&rule.role) {
            continue;
        }
        if readable && rule.read {
            access.read |= 1 << rule.level;
        }
        if writable && rule.write {
            access.write |= 1 << rule.level;
        }
    }
    access
}

/// The fields of `doctype` that `access` lets the caller read, in declaration order.
pub fn readable_fields(
    doctype: &DocType,
    access: FieldAccess,
) -> impl Iterator<Item = &DocField> + '_ {
    doctype
        .fields
        .iter()
        .filter(move |f| access.shows(f.perm_level))
}

/// The field called `name` if the caller may read it. A field the caller cannot read gives
/// `None`, exactly like a name that is not a field: callers report both as an unknown field, so
/// that nothing tells a hidden field from a missing one.
pub fn readable_field<'a>(
    doctype: &'a DocType,
    access: FieldAccess,
    name: &str,
) -> Option<&'a DocField> {
    doctype.field(name).filter(|f| access.shows(f.perm_level))
}

/// `doctype` as the caller sees it: the same DocType without the fields that `access` does not
/// let it read. Borrowed (no copy) when nothing is hidden, which is always the case for a
/// DocType without `perm_level`. Everything that reads, filters, sorts, groups or exports on
/// behalf of a caller works on this, so a hidden field is unknown to it everywhere. **Never use
/// it for a write**: hooks and the stored document need every field.
pub fn readable_doctype(doctype: &DocType, access: FieldAccess) -> Cow<'_, DocType> {
    if doctype.fields.iter().all(|f| access.shows(f.perm_level)) {
        return Cow::Borrowed(doctype);
    }
    let mut visible = doctype.clone();
    visible.fields.retain(|f| access.shows(f.perm_level));
    Cow::Owned(visible)
}

/// [`readable_doctype`] for what [`field_access`] says about `caller`: the DocType as this caller
/// sees it.
pub fn readable_doctype_for<'a>(doctype: &'a DocType, caller: &Caller) -> Cow<'a, DocType> {
    readable_doctype(doctype, field_access(doctype, caller))
}

/// Whether `caller` may run a query report (M8.4, ADR 0016) that names `roles`: the system
/// context and an administrator always may; anyone else needs one of the roles. A report with
/// no roles is run by nobody else (deny by default). **Row-level rules do not apply inside a
/// report**: this is the whole access check.
pub fn can_run_report(roles: &[String], caller: &Caller) -> bool {
    match caller {
        Caller::System { .. } => true,
        Caller::User { roles: have, .. } => {
            caller.bypasses_permissions() || roles.iter().any(|r| have.contains(r))
        }
    }
}

/// Whether `caller` may call an app method (M9.1, ADR 0019) with this `access`: with
/// [`MethodAccess::AnyUser`] any caller; with [`MethodAccess::Roles`] the same rule as
/// [`can_run_report`] (one of the roles, or the Administrator; the system context always). This
/// is only the **gate**: the documents a method reads and writes are checked by [`evaluate`] for
/// the caller, as for any other request.
pub fn can_call_method(access: &MethodAccess, caller: &Caller) -> bool {
    match access {
        MethodAccess::AnyUser => true,
        MethodAccess::Roles(roles) => can_run_report(roles, caller),
    }
}

/// Decides how far `caller` may take `transition` of the workflow of `doctype` (M8.1, ADR
/// 0015, the transition-role rule that extends ADR 0011): **the role and the right**.
///
/// - The system context and an administrator get [`Access::All`], as in [`evaluate`].
/// - Otherwise the caller needs one of the transition's `roles` **and** the right that the
///   docstatus move needs ([`Move::right`](crate::workflow::Move::right): write, submit or
///   cancel). The answer is then what [`evaluate`] says for that right: [`Access::All`], or
///   [`Access::OwnerOnly`] when only an `if_owner` rule grants it (the caller applies it to the
///   document). A role never grants a right: a transition role without the right is
///   [`Access::Denied`], and so is the right without a transition role.
/// - A DocType without a workflow, or a transition that does not belong to it (an unknown
///   state, a docstatus move a workflow does not allow), is [`Access::Denied`].
///
/// The right is decided by [`evaluate`] and nowhere else; this function only adds the role.
pub fn transition_access(doctype: &DocType, caller: &Caller, transition: &Transition) -> Access {
    let Some(workflow) = &doctype.workflow else {
        return Access::Denied;
    };
    let Some(movement) = workflow.move_of(transition) else {
        return Access::Denied;
    };
    let has_role = match caller {
        Caller::User { roles, .. } => transition.roles.iter().any(|r| roles.contains(r)),
        Caller::System { .. } => true,
    };
    if !caller.bypasses_permissions() && !has_role {
        return Access::Denied;
    }
    evaluate(doctype, caller, movement.right())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meta::Naming;

    fn doctype(permissions: Vec<PermissionRule>) -> DocType {
        DocType {
            name: "thing".into(),
            label: "Thing".into(),
            naming: Naming::Random,
            fields: vec![],
            is_child: false,
            submittable: true,
            permissions,
            title_field: None,
            list_fields: None,
            icon: None,
            workflow: None,
        }
    }

    fn user(roles: &[&str]) -> Caller {
        Caller::user("u", roles.iter().map(|r| (*r).to_owned()).collect())
    }

    /// A rule granting exactly `right`.
    fn only(role: &str, right: Right, if_owner: bool) -> PermissionRule {
        let mut rule = PermissionRule::new(role);
        rule.if_owner = if_owner;
        match right {
            Right::Read => rule.read = true,
            Right::Write => rule.write = true,
            Right::Create => rule.create = true,
            Right::Delete => rule.delete = true,
            Right::Submit => rule.submit = true,
            Right::Cancel => rule.cancel = true,
            Right::Amend => rule.amend = true,
            Right::Export => rule.export = true,
            Right::Import => rule.import = true,
        }
        rule
    }

    #[test]
    fn a_report_is_for_its_roles_the_administrator_and_the_system() {
        let roles = vec!["Manager".to_owned()];
        assert!(can_run_report(&roles, &user(&["Manager"])));
        assert!(can_run_report(&roles, &user(&["Clerk", "Manager"])));
        assert!(!can_run_report(&roles, &user(&["Clerk"])));
        assert!(!can_run_report(&roles, &user(&["manager"])));
        assert!(!can_run_report(&roles, &user(&[])));
        assert!(can_run_report(&roles, &user(&[ADMINISTRATOR])));
        assert!(can_run_report(&roles, &Caller::system("cli")));
        // No roles: only the callers that bypass.
        assert!(!can_run_report(&[], &user(&["Manager"])));
        assert!(can_run_report(&[], &user(&[ADMINISTRATOR])));
    }

    #[test]
    fn a_method_is_for_its_roles_the_administrator_or_any_user() {
        let roles = MethodAccess::Roles(vec!["Manager".to_owned()]);
        assert!(can_call_method(&roles, &user(&["Manager"])));
        assert!(!can_call_method(&roles, &user(&["Clerk"])));
        assert!(!can_call_method(&roles, &user(&[])));
        assert!(can_call_method(&roles, &user(&[ADMINISTRATOR])));
        assert!(can_call_method(&MethodAccess::AnyUser, &user(&[])));
        assert!(can_call_method(&MethodAccess::AnyUser, &user(&["Clerk"])));
        // A list that somehow is empty admits nobody but the Administrator.
        let empty = MethodAccess::Roles(vec![]);
        assert!(!can_call_method(&empty, &user(&["Manager"])));
        assert!(can_call_method(&empty, &user(&[ADMINISTRATOR])));
    }

    #[test]
    fn matrix_of_rights_roles_and_if_owner() {
        for granted in Right::ALL {
            for if_owner in [false, true] {
                let d = doctype(vec![only("Clerk", granted, if_owner)]);
                for asked in Right::ALL {
                    let expected = match (asked == granted, if_owner) {
                        (false, _) => Access::Denied,
                        (true, false) => Access::All,
                        (true, true) => Access::OwnerOnly,
                    };
                    assert_eq!(
                        evaluate(&d, &user(&["Clerk"]), asked),
                        expected,
                        "granted {granted} if_owner={if_owner}, asked {asked}"
                    );
                    // Without the role nothing is granted; a similar role is not the role.
                    for other in [&[][..], &["Other"], &["clerk"], &["Clerk "]] {
                        assert_eq!(evaluate(&d, &user(other), asked), Access::Denied);
                    }
                }
            }
        }
    }

    #[test]
    fn no_rules_means_denied_except_for_administrator_and_system() {
        let d = doctype(vec![]);
        for right in Right::ALL {
            assert_eq!(evaluate(&d, &user(&["Clerk"]), right), Access::Denied);
            assert_eq!(evaluate(&d, &user(&[]), right), Access::Denied);
            assert_eq!(evaluate(&d, &user(&[ADMINISTRATOR]), right), Access::All);
            assert_eq!(
                evaluate(&d, &user(&["Clerk", ADMINISTRATOR]), right),
                Access::All
            );
            assert_eq!(evaluate(&d, &Caller::system("cli"), right), Access::All);
        }
    }

    #[test]
    fn administrator_is_not_limited_by_if_owner_rules() {
        let d = doctype(vec![only("Clerk", Right::Read, true)]);
        assert_eq!(
            evaluate(&d, &user(&[ADMINISTRATOR]), Right::Read),
            Access::All
        );
        assert_eq!(
            evaluate(&d, &user(&[ADMINISTRATOR]), Right::Delete),
            Access::All
        );
    }

    #[test]
    fn roles_union_and_the_wider_rule_wins() {
        let d = doctype(vec![
            only("Reader", Right::Read, false),
            only("Writer", Right::Write, false),
            only("Own", Right::Read, true),
            only("Own", Right::Delete, true),
        ]);
        let both = user(&["Reader", "Writer"]);
        assert_eq!(evaluate(&d, &both, Right::Read), Access::All);
        assert_eq!(evaluate(&d, &both, Right::Write), Access::All);
        assert_eq!(evaluate(&d, &both, Right::Delete), Access::Denied);
        // OwnerOnly from one role and All from another: All.
        let mixed = user(&["Own", "Reader"]);
        assert_eq!(evaluate(&d, &mixed, Right::Read), Access::All);
        assert_eq!(evaluate(&d, &mixed, Right::Delete), Access::OwnerOnly);
        // Declaration order does not matter.
        let reversed = doctype(d.permissions.iter().rev().cloned().collect());
        assert_eq!(evaluate(&reversed, &mixed, Right::Read), Access::All);
        // One rule can grant several rights.
        let multi = doctype(vec![PermissionRule {
            read: true,
            write: true,
            ..PermissionRule::new("Clerk")
        }]);
        assert_eq!(
            evaluate(&multi, &user(&["Clerk"]), Right::Write),
            Access::All
        );
        assert_eq!(
            evaluate(&multi, &user(&["Clerk"]), Right::Create),
            Access::Denied
        );
    }

    #[test]
    fn caller_helpers() {
        assert_eq!(Caller::user("ann", vec![]).name(), "ann");
        assert_eq!(Caller::system("cli").name(), "cli");
        assert!(Caller::system("cli").bypasses_permissions());
        assert!(user(&[ADMINISTRATOR]).bypasses_permissions());
        assert!(!user(&["administrator"]).bypasses_permissions());
        assert!(!user(&[]).bypasses_permissions());
    }

    #[test]
    fn right_names() {
        let names: Vec<&str> = Right::ALL.iter().map(|r| r.name()).collect();
        assert_eq!(
            names,
            [
                "read", "write", "create", "delete", "submit", "cancel", "amend", "export",
                "import"
            ]
        );
        assert_eq!(Right::Amend.to_string(), "amend");
    }

    #[test]
    fn role_name_rules() {
        assert!(check_role_name("Sales Manager").is_ok());
        assert!(check_role_name("Verkäufer").is_ok());
        assert!(check_role_name(&"r".repeat(140)).is_ok());
        assert!(check_role_name(&"r".repeat(141)).is_err());
        for bad in ["", "   ", "a\tb", "a\0b", "\n"] {
            assert!(check_role_name(bad).is_err(), "{bad:?}");
        }
    }

    /// A rule at `level` granting read and/or write.
    fn at_level(role: &str, level: u8, read: bool, write: bool) -> PermissionRule {
        PermissionRule {
            read,
            write,
            level,
            ..PermissionRule::new(role)
        }
    }

    /// A DocType with fields at levels 0, 1 and 2.
    fn leveled(permissions: Vec<PermissionRule>) -> DocType {
        let mut d = doctype(permissions);
        for (name, level) in [("plain", 0), ("limit", 1), ("secret", 2)] {
            d.fields.push(DocField {
                name: name.into(),
                label: name.into(),
                field_type: crate::meta::FieldType::Int,
                required: false,
                unique: false,
                allow_on_submit: false,
                read_only: false,
                perm_level: level,
            });
        }
        d
    }

    #[test]
    fn a_rule_of_a_level_grants_no_document_right() {
        // Level 1 rules with read and write, and nothing at level 0: no document access at all.
        let d = leveled(vec![at_level("Clerk", 1, true, true)]);
        for right in Right::ALL {
            assert_eq!(evaluate(&d, &user(&["Clerk"]), right), Access::Denied);
        }
        // Next to a level 0 rule the level 1 rule changes nothing about the document rights.
        let d = leveled(vec![
            only("Clerk", Right::Read, true),
            at_level("Clerk", 1, true, true),
        ]);
        assert_eq!(
            evaluate(&d, &user(&["Clerk"]), Right::Read),
            Access::OwnerOnly
        );
        assert_eq!(
            evaluate(&d, &user(&["Clerk"]), Right::Write),
            Access::Denied
        );
    }

    #[test]
    fn field_access_system_and_administrator_have_every_level() {
        let d = leveled(vec![]);
        for caller in [user(&[ADMINISTRATOR]), Caller::system("cli")] {
            let access = field_access(&d, &caller);
            assert!(access.is_all());
            for level in 0..=MAX_PERM_LEVEL {
                assert!(access.can_read(level) && access.can_write(level), "{level}");
            }
        }
        // Even with rules that name only another role and level.
        let d = leveled(vec![at_level("Clerk", 1, true, true)]);
        assert!(field_access(&d, &user(&[ADMINISTRATOR])).is_all());
    }

    #[test]
    fn field_access_matrix_of_level_zero_rights_if_owner_and_level_rules() {
        // Level 0: which rights the caller has; level 1: which of read/write the rule grants.
        for (r0, w0, c0) in [
            (false, false, false),
            (true, false, false),
            (false, true, false),
            (false, false, true),
            (true, true, false),
            (true, false, true),
            (true, true, true),
        ] {
            for if_owner in [false, true] {
                for (r1, w1) in [(false, false), (true, false), (true, true), (false, true)] {
                    let mut level0 = PermissionRule::new("Clerk");
                    level0.read = r0;
                    level0.write = w0;
                    level0.create = c0;
                    level0.if_owner = if_owner;
                    // An empty rule would be a definition error, but the evaluator does not care.
                    let rules = vec![level0, at_level("Clerk", 1, r1, w1)];
                    let d = leveled(rules);
                    let access = field_access(&d, &user(&["Clerk"]));
                    let label =
                        format!("r0={r0} w0={w0} c0={c0} if_owner={if_owner} r1={r1} w1={w1}");
                    // Level 0 follows the rights; if_owner changes nothing about fields.
                    assert_eq!(access.can_read(0), r0, "{label}");
                    assert_eq!(access.can_write(0), w0 || c0, "{label}");
                    // Level 1 needs level 0 and the rule.
                    assert_eq!(access.can_read(1), r0 && r1, "{label}");
                    assert_eq!(access.can_write(1), (w0 || c0) && w1, "{label}");
                    // No rule names level 2, and the other levels are closed.
                    for level in 2..=MAX_PERM_LEVEL {
                        assert!(
                            !access.can_read(level) && !access.can_write(level),
                            "{label}"
                        );
                    }
                    // Without the role nothing is granted, at any level.
                    let other = field_access(&d, &user(&["Other"]));
                    for level in 0..=MAX_PERM_LEVEL {
                        assert!(!other.can_read(level) && !other.can_write(level), "{label}");
                    }
                }
            }
        }
    }

    #[test]
    fn field_access_unions_roles_and_levels_are_independent() {
        let d = leveled(vec![
            only("Reader", Right::Read, false),
            only("Writer", Right::Write, false),
            at_level("Lead", 1, true, false),
            at_level("Boss", 1, true, true),
            at_level("Boss", 2, true, false),
            at_level("Spy", 2, true, true),
        ]);
        // A level rule alone gives nothing without a level 0 right.
        let spy = field_access(&d, &user(&["Spy"]));
        assert!(!spy.can_read(2) && !spy.can_write(2) && !spy.can_read(0));
        // Reader + Lead: reads level 1 but cannot write it.
        let a = field_access(&d, &user(&["Reader", "Lead"]));
        assert!(a.can_read(1) && !a.can_write(1) && !a.can_read(2));
        // Reader + Writer + Boss: level 1 read and write, level 2 read only (the rule), level 0 both.
        let b = field_access(&d, &user(&["Reader", "Writer", "Boss"]));
        assert!(b.can_read(1) && b.can_write(1));
        assert!(b.can_read(2) && !b.can_write(2));
        assert!(b.can_read(0) && b.can_write(0));
        // Roles of different levels add up.
        let c = field_access(&d, &user(&["Reader", "Writer", "Lead", "Spy"]));
        assert!(c.can_read(1) && !c.can_write(1) && c.can_read(2) && c.can_write(2));
        // A level above the maximum is never accessible, and the bit helpers do not overflow.
        assert!(!FieldAccess::ALL.can_read(10) && !FieldAccess::ALL.can_write(200));
    }

    #[test]
    fn readable_fields_and_names_hide_what_the_caller_cannot_read() {
        let d = leveled(vec![
            only("Reader", Right::Read, false),
            at_level("Reader", 1, true, false),
        ]);
        let access = field_access(&d, &user(&["Reader"]));
        let names: Vec<&str> = readable_fields(&d, access)
            .map(|f| f.name.as_str())
            .collect();
        assert_eq!(names, ["plain", "limit"]);
        assert!(readable_field(&d, access, "limit").is_some());
        // A hidden field is exactly like a name that is not a field.
        assert!(readable_field(&d, access, "secret").is_none());
        assert!(readable_field(&d, access, "nope").is_none());
        let plain_reader = field_access(&d, &user(&[]));
        let names: Vec<&str> = readable_fields(&d, plain_reader)
            .map(|f| f.name.as_str())
            .collect();
        // Level 0 is never hidden by field levels (the document rights decide about it).
        assert_eq!(names, ["plain"]);
        assert!(plain_reader.shows(0) && !plain_reader.shows(1) && !plain_reader.can_read(0));
        // The caller's DocType has the readable fields only, and borrows when nothing is hidden.
        let view = readable_doctype(&d, access);
        assert_eq!(view.fields.len(), 2);
        assert!(view.field("secret").is_none());
        assert!(matches!(
            readable_doctype(&d, FieldAccess::ALL),
            Cow::Borrowed(_)
        ));
        let plain = doctype(vec![only("Reader", Right::Read, false)]);
        assert!(matches!(
            readable_doctype_for(&plain, &user(&["Nobody"])),
            Cow::Borrowed(_)
        ));
        assert_eq!(readable_doctype_for(&d, &user(&["Reader"])).fields.len(), 2);
        assert_eq!(
            readable_doctype_for(&d, &Caller::system("cli"))
                .fields
                .len(),
            3
        );
    }

    fn with_workflow(permissions: Vec<PermissionRule>) -> DocType {
        let mut doctype = DocType::from_toml_str(
            r#"
name = "order"
label = "Order"
submittable = true
[workflow]
[[workflow.states]]
name = "Draft"
docstatus = 0
[[workflow.states]]
name = "Pending"
docstatus = 0
[[workflow.states]]
name = "Approved"
docstatus = 1
[[workflow.states]]
name = "Cancelled"
docstatus = 2
[[workflow.transitions]]
action = "Send"
from = "Draft"
to = "Pending"
roles = ["Clerk"]
[[workflow.transitions]]
action = "Approve"
from = "Pending"
to = "Approved"
roles = ["Manager"]
[[workflow.transitions]]
action = "Cancel"
from = "Approved"
to = "Cancelled"
roles = ["Manager"]
"#,
        )
        .expect("valid DocType");
        doctype.permissions = permissions;
        doctype
    }

    fn take(doctype: &DocType, caller: &Caller, action: &str) -> Access {
        let workflow = doctype.workflow.as_ref().expect("workflow");
        let transition = workflow
            .transitions
            .iter()
            .find(|t| t.action == action)
            .expect("transition");
        transition_access(doctype, caller, transition)
    }

    #[test]
    fn a_transition_needs_a_role_and_the_right_of_its_move() {
        let d = with_workflow(vec![
            only("Clerk", Right::Write, false),
            only("Manager", Right::Submit, false),
            only("Auditor", Right::Cancel, false),
            only("Owner", Right::Submit, true),
        ]);
        // Draft to draft needs write: Clerk has the role and the right.
        assert_eq!(take(&d, &user(&["Clerk"]), "Send"), Access::All);
        // Approve moves 0 to 1, which needs the submit right: Manager has the role and the right.
        assert_eq!(take(&d, &user(&["Manager"]), "Approve"), Access::All);
        // The role without the right: Manager has no cancel right.
        assert_eq!(take(&d, &user(&["Manager"]), "Cancel"), Access::Denied);
        // The right without the role: Auditor may cancel but is not a role of Cancel.
        assert_eq!(take(&d, &user(&["Auditor"]), "Cancel"), Access::Denied);
        assert_eq!(
            take(&d, &user(&["Manager", "Auditor"]), "Cancel"),
            Access::All
        );
        // A user with neither.
        assert_eq!(take(&d, &user(&["Other"]), "Send"), Access::Denied);
        assert_eq!(take(&d, &user(&[]), "Send"), Access::Denied);
        // A similar role name is not the role.
        assert_eq!(take(&d, &user(&["clerk"]), "Send"), Access::Denied);
    }

    #[test]
    fn if_owner_limits_the_right_of_a_transition() {
        let d = with_workflow(vec![
            only("Clerk", Right::Write, true),
            only("Manager", Right::Submit, true),
            only("Manager", Right::Cancel, false),
        ]);
        assert_eq!(take(&d, &user(&["Clerk"]), "Send"), Access::OwnerOnly);
        assert_eq!(take(&d, &user(&["Manager"]), "Approve"), Access::OwnerOnly);
        assert_eq!(take(&d, &user(&["Manager"]), "Cancel"), Access::All);
    }

    #[test]
    fn administrator_and_system_pass_the_role_check() {
        // No rules at all, and the roles of the transitions are not theirs.
        let d = with_workflow(vec![]);
        for action in ["Send", "Approve", "Cancel"] {
            assert_eq!(take(&d, &user(&[ADMINISTRATOR]), action), Access::All);
            assert_eq!(take(&d, &Caller::system("cli"), action), Access::All);
            assert_eq!(
                take(&d, &user(&["Clerk", "Manager"]), action),
                Access::Denied
            );
        }
    }

    #[test]
    fn no_workflow_or_a_foreign_transition_is_denied() {
        let plain = doctype(vec![only("Clerk", Right::Write, false)]);
        let foreign = Transition {
            action: "Send".into(),
            from: "Draft".into(),
            to: "Pending".into(),
            roles: vec!["Clerk".into()],
        };
        assert_eq!(
            transition_access(&plain, &Caller::system("cli"), &foreign),
            Access::Denied
        );
        let d = with_workflow(vec![]);
        let unknown_state = Transition {
            to: "Nowhere".into(),
            ..foreign
        };
        assert_eq!(
            transition_access(&d, &Caller::system("cli"), &unknown_state),
            Access::Denied
        );
    }

    // ---- user permissions (M10.3, ADR 0025) ----

    fn link_field(name: &str, target: &str, perm_level: u8) -> DocField {
        DocField {
            name: name.into(),
            label: name.into(),
            field_type: FieldType::Link {
                target: target.into(),
            },
            required: false,
            unique: false,
            allow_on_submit: false,
            read_only: false,
            perm_level,
        }
    }

    fn with_links(name: &str, links: &[(&str, &str)]) -> DocType {
        let mut d = doctype(vec![only("Clerk", Right::Read, false)]);
        d.name = name.into();
        d.fields = links.iter().map(|(f, t)| link_field(f, t, 0)).collect();
        d
    }

    fn restricted(roles: &[&str], pairs: &[(&str, &str)]) -> Caller {
        user(roles).with_restrictions(UserRestrictions::from_pairs(pairs.iter().copied()))
    }

    fn link<'a>(field: &'a str, target: &'a str, allowed: &'a [String]) -> LinkScope<'a> {
        LinkScope {
            field,
            target,
            allowed,
        }
    }

    #[test]
    fn restrictions_are_sorted_deduplicated_and_empty_means_unrestricted() {
        let r = UserRestrictions::from_pairs([("group", "b"), ("group", "a"), ("group", "b")]);
        assert_eq!(
            r.allowed("group"),
            Some(&["a".to_owned(), "b".to_owned()][..])
        );
        assert_eq!(r.allowed("other"), None);
        assert!(!r.is_empty());
        let none = UserRestrictions::default();
        assert!(none.is_empty());
        assert_eq!(none.allowed("group"), None);
        // An empty list is the same as no entry.
        let empty = UserRestrictions(Arc::new(BTreeMap::from([("group".to_owned(), vec![])])));
        assert!(empty.is_empty());
        assert_eq!(empty.allowed("group"), None);
    }

    #[test]
    fn a_user_without_restrictions_has_an_empty_scope() {
        let d = with_links("customer", &[("group", "group")]);
        assert!(row_scope(&d, &user(&["Clerk"])).is_empty());
        // A restriction on something the DocType does not touch changes nothing.
        let c = restricted(&["Clerk"], &[("territory", "North")]);
        assert!(row_scope(&d, &c).is_empty());
    }

    #[test]
    fn the_own_name_is_restricted_when_the_doctype_is() {
        let d = with_links("group", &[]);
        let c = restricted(&["Clerk"], &[("group", "Retail"), ("group", "Online")]);
        let scope = row_scope(&d, &c);
        assert_eq!(
            scope.names,
            Some(&["Online".to_owned(), "Retail".to_owned()][..])
        );
        assert!(scope.links.is_empty());
        assert!(!scope.is_empty());
    }

    #[test]
    fn a_link_to_a_restricted_target_is_restricted_and_a_link_to_another_is_not() {
        let d = with_links("customer", &[("group", "group"), ("region", "region")]);
        let c = restricted(&["Clerk"], &[("group", "Retail")]);
        let scope = row_scope(&d, &c);
        assert_eq!(scope.names, None);
        let allowed = ["Retail".to_owned()];
        assert_eq!(scope.links, vec![link("group", "group", &allowed)]);
    }

    #[test]
    fn two_links_to_the_same_target_both_restrict() {
        let d = with_links(
            "customer",
            &[("group", "group"), ("billing_group", "group")],
        );
        let c = restricted(&["Clerk"], &[("group", "Retail")]);
        let fields: Vec<&str> = row_scope(&d, &c).links.iter().map(|l| l.field).collect();
        assert_eq!(fields, ["group", "billing_group"]);
    }

    #[test]
    fn links_to_two_restricted_targets_all_restrict() {
        let d = with_links("order", &[("customer", "customer"), ("group", "group")]);
        let c = restricted(&["Clerk"], &[("group", "Retail"), ("customer", "C-1")]);
        let scope = row_scope(&d, &c);
        let targets: Vec<&str> = scope.links.iter().map(|l| l.target).collect();
        assert_eq!(targets, ["customer", "group"]);
        // The own name and the links together.
        let d = with_links("customer", &[("group", "group")]);
        let c = restricted(&["Clerk"], &[("group", "Retail"), ("customer", "C-1")]);
        let scope = row_scope(&d, &c);
        assert_eq!(scope.names, Some(&["C-1".to_owned()][..]));
        assert_eq!(scope.links.len(), 1);
    }

    #[test]
    fn a_hidden_link_field_still_restricts() {
        let mut d = with_links("customer", &[]);
        d.fields.push(link_field("group", "group", 3));
        let c = restricted(&["Clerk"], &[("group", "Retail")]);
        assert_eq!(row_scope(&d, &c).links.len(), 1);
    }

    #[test]
    fn administrator_and_system_are_never_restricted() {
        let d = with_links("customer", &[("group", "group")]);
        let pairs = [("group", "Retail"), ("customer", "C-1")];
        let admin = restricted(&[ADMINISTRATOR], &pairs);
        assert!(row_scope(&d, &admin).is_empty());
        let system = Caller::system("cli").with_restrictions(UserRestrictions::from_pairs(pairs));
        assert_eq!(system, Caller::system("cli"));
        assert!(row_scope(&d, &system).is_empty());
    }

    #[test]
    fn a_child_doctype_has_an_empty_scope() {
        let mut line = with_links("line", &[("group", "group")]);
        line.is_child = true;
        let c = restricted(&["Clerk"], &[("group", "Retail"), ("line", "x")]);
        assert!(row_scope(&line, &c).is_empty());
        // The parent does not look at the Link fields of its child rows.
        let mut parent = with_links("order", &[]);
        parent.fields.push(DocField {
            field_type: FieldType::Table {
                target: "line".into(),
            },
            ..link_field("lines", "line", 0)
        });
        assert!(row_scope(&parent, &c).is_empty());
    }
}
