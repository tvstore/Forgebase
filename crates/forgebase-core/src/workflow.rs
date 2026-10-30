//! Workflows (M8.1, ADR 0015): the states a document of a DocType moves through and the
//! transitions between them, written by the app in the DocType file (`[workflow]`).
//!
//! A workflow is a state machine over the docstatus: every state has a docstatus (0 draft, 1
//! submitted, 2 cancelled), and a transition is the only way to change the state of a document
//! of a DocType that has a workflow. This module is data and pure functions: the loader builds
//! the types and [`DocType::validate`](crate::meta::DocType::validate) checks them; whether a
//! caller may take a transition is decided by
//! [`transition_access`](crate::permission::transition_access), and `forgebase-db` runs it.
//!
//! The first state is the initial state of every new document. Conditions on a transition are
//! written in Rust (the `before_transition` hook, [`crate::hooks`]), not in the file.

use std::fmt;

use crate::document::DocStatus;
use crate::permission::Right;

/// The longest name of a state or an action, in characters.
pub const MAX_NAME_LEN: usize = 64;

/// The workflow of a DocType (`[workflow]`): its states, the first of which is the initial
/// state, and the transitions between them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workflow {
    /// The states, in declaration order. The first is the initial state.
    pub states: Vec<WorkflowState>,
    /// The transitions, in declaration order.
    pub transitions: Vec<Transition>,
}

/// One state of a [`Workflow`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowState {
    /// The name: unique in the workflow, 1 to [`MAX_NAME_LEN`] characters. It is stored in the
    /// `workflow_state` column of the documents.
    pub name: String,
    /// The docstatus of a document in this state. 1 and 2 only on a submittable DocType.
    pub docstatus: DocStatus,
    /// The tone of the state's badge in the desk.
    pub style: StateStyle,
    /// Roles that may edit (update) a draft in this state, in addition to the right to write.
    /// Empty: the DocType's write rule alone decides. Only on a state with docstatus 0.
    pub edit_roles: Vec<String>,
}

/// The tone of a state's badge. Metadata for the desk only.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum StateStyle {
    /// No particular meaning (the default).
    #[default]
    Neutral,
    /// Done or approved.
    Success,
    /// Waiting or needs attention.
    Warning,
    /// Rejected or cancelled.
    Danger,
    /// Informational.
    Info,
}

impl StateStyle {
    /// The key used in the DocType file and in the API: `neutral`, `success`, `warning`,
    /// `danger` or `info`.
    pub fn name(self) -> &'static str {
        match self {
            StateStyle::Neutral => "neutral",
            StateStyle::Success => "success",
            StateStyle::Warning => "warning",
            StateStyle::Danger => "danger",
            StateStyle::Info => "info",
        }
    }

    /// The style for a key of the DocType file, `None` for an unknown one.
    pub fn from_name(name: &str) -> Option<Self> {
        [
            StateStyle::Neutral,
            StateStyle::Success,
            StateStyle::Warning,
            StateStyle::Danger,
            StateStyle::Info,
        ]
        .into_iter()
        .find(|style| style.name() == name)
    }
}

impl fmt::Display for StateStyle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// One transition of a [`Workflow`]: an action that moves a document from one state to another.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transition {
    /// The action, the label of the button: 1 to [`MAX_NAME_LEN`] characters, unique per `from`
    /// state.
    pub action: String,
    /// The name of the state the document must be in.
    pub from: String,
    /// The name of the state the document is in afterwards.
    pub to: String,
    /// The roles that may take it: at least one. The caller also needs the underlying right
    /// (see [`Move::right`]); the roles never grant more than the DocType's rules do.
    pub roles: Vec<String>,
}

/// What a transition does to the docstatus, which decides the pipeline it runs and the right it
/// needs. The other moves (anything from a cancelled state, or a drop back from submitted to
/// draft) are refused when the DocType is loaded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Move {
    /// Draft to draft (0 to 0): an update of the whole document. Needs the write right.
    Edit,
    /// Draft to submitted (0 to 1): the submit pipeline. Needs the submit right.
    Submit,
    /// Submitted to submitted (1 to 1): only `allow_on_submit` fields may change. Needs the
    /// write right.
    EditSubmitted,
    /// Submitted to cancelled (1 to 2): the cancel pipeline. Needs the cancel right.
    Cancel,
}

impl Move {
    /// The move from docstatus `from` to docstatus `to`, `None` for one a workflow does not
    /// allow.
    pub fn of(from: DocStatus, to: DocStatus) -> Option<Self> {
        match (from, to) {
            (DocStatus::Draft, DocStatus::Draft) => Some(Move::Edit),
            (DocStatus::Draft, DocStatus::Submitted) => Some(Move::Submit),
            (DocStatus::Submitted, DocStatus::Submitted) => Some(Move::EditSubmitted),
            (DocStatus::Submitted, DocStatus::Cancelled) => Some(Move::Cancel),
            _ => None,
        }
    }

    /// The right the underlying operation needs: write, submit or cancel.
    pub fn right(self) -> Right {
        match self {
            Move::Edit | Move::EditSubmitted => Right::Write,
            Move::Submit => Right::Submit,
            Move::Cancel => Right::Cancel,
        }
    }
}

impl Workflow {
    /// The initial state: the first one. `None` only for a workflow without states, which
    /// loading refuses.
    pub fn initial(&self) -> Option<&WorkflowState> {
        self.states.first()
    }

    /// The state with this name.
    pub fn state(&self, name: &str) -> Option<&WorkflowState> {
        self.states.iter().find(|state| state.name == name)
    }

    /// The first state with this docstatus. Documents that exist when a workflow is added
    /// (`migrate`) get it.
    pub fn first_state_with(&self, docstatus: DocStatus) -> Option<&WorkflowState> {
        self.states
            .iter()
            .find(|state| state.docstatus == docstatus)
    }

    /// The transition that `action` names from the state `from`.
    pub fn transition(&self, from: &str, action: &str) -> Option<&Transition> {
        self.transitions
            .iter()
            .find(|t| t.from == from && t.action == action)
    }

    /// The transitions that leave the state `from`, in declaration order.
    pub fn transitions_from<'a>(&'a self, from: &'a str) -> impl Iterator<Item = &'a Transition> {
        self.transitions.iter().filter(move |t| t.from == from)
    }

    /// What `transition` does to the docstatus, `None` when it names a state that does not
    /// exist or makes a move a workflow does not allow (loading refuses both).
    pub fn move_of(&self, transition: &Transition) -> Option<Move> {
        let from = self.state(&transition.from)?;
        let to = self.state(&transition.to)?;
        Move::of(from.docstatus, to.docstatus)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(name: &str, docstatus: DocStatus) -> WorkflowState {
        WorkflowState {
            name: name.to_owned(),
            docstatus,
            style: StateStyle::default(),
            edit_roles: Vec::new(),
        }
    }

    fn transition(action: &str, from: &str, to: &str) -> Transition {
        Transition {
            action: action.to_owned(),
            from: from.to_owned(),
            to: to.to_owned(),
            roles: vec!["Clerk".to_owned()],
        }
    }

    fn workflow() -> Workflow {
        Workflow {
            states: vec![
                state("Draft", DocStatus::Draft),
                state("Approved", DocStatus::Submitted),
                state("Cancelled", DocStatus::Cancelled),
            ],
            transitions: vec![
                transition("Approve", "Draft", "Approved"),
                transition("Cancel", "Approved", "Cancelled"),
                transition("Reopen", "Draft", "Draft"),
            ],
        }
    }

    #[test]
    fn lookups() {
        let w = workflow();
        assert_eq!(w.initial().map(|s| s.name.as_str()), Some("Draft"));
        assert_eq!(
            w.state("Approved").map(|s| s.docstatus),
            Some(DocStatus::Submitted)
        );
        assert!(w.state("approved").is_none(), "names are case-sensitive");
        assert_eq!(
            w.first_state_with(DocStatus::Cancelled)
                .map(|s| s.name.as_str()),
            Some("Cancelled")
        );
        assert_eq!(
            w.transition("Draft", "Approve").map(|t| t.to.as_str()),
            Some("Approved")
        );
        assert!(w.transition("Approved", "Approve").is_none());
        let from_draft: Vec<&str> = w
            .transitions_from("Draft")
            .map(|t| t.action.as_str())
            .collect();
        assert_eq!(from_draft, ["Approve", "Reopen"]);
    }

    #[test]
    fn moves_and_their_rights() {
        let w = workflow();
        let moves: Vec<_> = w.transitions.iter().map(|t| w.move_of(t)).collect();
        assert_eq!(
            moves,
            [Some(Move::Submit), Some(Move::Cancel), Some(Move::Edit)]
        );
        assert_eq!(Move::Edit.right(), Right::Write);
        assert_eq!(Move::EditSubmitted.right(), Right::Write);
        assert_eq!(Move::Submit.right(), Right::Submit);
        assert_eq!(Move::Cancel.right(), Right::Cancel);
        // Everything else is not a move.
        use DocStatus::{Cancelled, Draft, Submitted};
        for (from, to) in [
            (Draft, Cancelled),
            (Submitted, Draft),
            (Cancelled, Draft),
            (Cancelled, Submitted),
            (Cancelled, Cancelled),
        ] {
            assert_eq!(Move::of(from, to), None, "{from} to {to}");
        }
        assert_eq!(w.move_of(&transition("X", "Draft", "Nowhere")), None);
    }

    #[test]
    fn style_names_round_trip() {
        for style in [
            StateStyle::Neutral,
            StateStyle::Success,
            StateStyle::Warning,
            StateStyle::Danger,
            StateStyle::Info,
        ] {
            assert_eq!(StateStyle::from_name(style.name()), Some(style));
        }
        assert_eq!(StateStyle::from_name("red"), None);
        assert_eq!(StateStyle::default(), StateStyle::Neutral);
    }
}
