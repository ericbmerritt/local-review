//! jjr composer layer.
//!
//! Re-exports shared pure types and the generic `handle_composer_key` from
//! `local_review_core::tui::composer` and defines the jjr-specific
//! `EditingContext`, `EditedComment`, and `Composer` — which carry the
//! `original_anchor` / `original` fields needed by jjr's save/update/delete
//! paths. `ComposerOps` is implemented here for jjr's `Composer` so that the
//! shared `handle_composer_key` can drive it.

pub(crate) use local_review_core::tui::composer::{
    default_severity, format_age, handle_composer_key, ComposerAction, ComposerInit, ComposerScope,
    DescriptionContext, LineTarget, ScopeTag, StackContextSnapshot,
};
use local_review_core::tui::composer::{ComposerFocus, ComposerOps};

use crossterm::event::KeyEvent;
use time::OffsetDateTime;

use crate::change_id::ChangeId;
use crate::comment::{Anchor, Comment, Severity};
use crate::tui::textarea::TextArea;

/// Context for an in-progress edit or delete of an existing comment in jjr.
///
/// `original_anchor` is captured at the moment the composer is opened and is
/// **always** the anchor used for the delete path, even if the user switches
/// scope while composing.  It is set unconditionally from `EditedComment::original_anchor`.
///
/// `original` is `Some` only when the edit was initiated from the **stack
/// overview** (where the full `Comment` must be carried because the overview
/// screen does not maintain an `InlineCommentMeta` index into the loaded-comments
/// list).  When the edit is initiated from the **main-view inline list**,
/// `original` is `None` and the caller uses `core::EditingContext::comment_index`
/// to look up the comment from the loaded list instead.
///
/// Joint constraint: `original.is_some()` implies the composer was opened from
/// the overview, meaning no comment index is available on this review path.
pub(crate) struct EditingContext {
    pub(crate) identity: OffsetDateTime,
    /// Anchor used as the on-disk delete key.  Captured at open time; not
    /// updated if the user switches scope during composition.
    pub(crate) original_anchor: Anchor,
    /// Full source `Comment`, present only when the edit was opened from the
    /// stack overview.  `None` when opened from the main-view inline list.
    pub(crate) original: Option<Comment>,
}

/// Bundle of fields drawn from a single `Comment` to seed a jjr edit-mode
/// composer.
///
/// `original_anchor` is the anchor used for the delete path (see
/// [`EditingContext::original_anchor`]).  It must always be set; use the
/// anchor from the comment record being edited.
///
/// `original` mirrors [`EditingContext::original`]: set it to `Some(comment)`
/// when opening from the stack overview, or `None` when opening from the
/// main-view inline list (where the caller has a `comment_index` instead).
pub(crate) struct EditedComment {
    pub(crate) init: ComposerInit,
    pub(crate) body: String,
    pub(crate) identity: OffsetDateTime,
    /// `Some` when opened from the stack overview; `None` when opened from
    /// the main-view inline list.
    pub(crate) original: Option<Comment>,
    /// Delete key anchor, captured at open time.
    pub(crate) original_anchor: Anchor,
}

/// State for the jjr comment composer modal.
///
/// The `editing` field uses jjr's own `EditingContext`, which carries
/// `original_anchor` and `original` — the extra data jjr needs for its
/// delete-then-re-anchor flow.
pub(crate) struct Composer {
    pub(crate) scope: ComposerScope,
    pub(crate) severity: Severity,
    pub(crate) body: TextArea,
    /// Which field has keyboard focus. Tab cycles; Space cycles values
    /// within a focused picker.
    pub(crate) focus: ComposerFocus,
    /// `Some` when the composer is in edit mode; `None` for new comments.
    pub(crate) editing: Option<EditingContext>,
    /// In-modal status hint, set when a chord is refused (Alt+D without
    /// description availability, Alt+K without stack availability, Alt+L
    /// without line availability). Cleared on the next keypress so the hint
    /// doesn't linger after the user moves on.
    pub(crate) refusal_status: Option<&'static str>,
    /// Target change for `Change`-scope save and the picker-row's short-id
    /// label. Differs from `app.details.change_id` when the composer was
    /// opened from an overview cursor pointing at a non-current change.
    pub(crate) change_id: ChangeId,
    /// Description text rendered as chrome on the Change-scope context block.
    pub(crate) change_description: String,
    /// Line-scope payload available from current cursor; `None` means cursor is on a non-commentable row.
    pub(crate) line_available: Option<LineTarget>,
    /// Stack-scope payload available from current app state; `None` in single-change mode.
    pub(crate) stack_available: Option<StackContextSnapshot>,
    /// Description-scope payload available from current cursor; `None` when cursor is not on a description line.
    pub(crate) description_available: Option<DescriptionContext>,
}

impl Composer {
    pub(crate) fn new(init: ComposerInit) -> Self {
        Self {
            scope: init.scope,
            severity: init.severity,
            body: TextArea::default(),
            focus: ComposerFocus::Body,
            editing: None,
            refusal_status: None,
            change_id: init.change_id,
            change_description: init.change_description,
            line_available: init.line_available,
            stack_available: init.stack_available,
            description_available: init.description_available,
        }
    }

    pub(crate) fn for_edit(edited: EditedComment) -> Self {
        let mut textarea = TextArea::default();
        for (i, line) in edited.body.lines().enumerate() {
            if i > 0 {
                textarea.insert_newline();
            }
            textarea.insert_str(line);
        }
        Self {
            scope: edited.init.scope,
            severity: edited.init.severity,
            body: textarea,
            focus: ComposerFocus::Body,
            editing: Some(EditingContext {
                identity: edited.identity,
                original_anchor: edited.original_anchor,
                original: edited.original,
            }),
            refusal_status: None,
            change_id: edited.init.change_id,
            change_description: edited.init.change_description,
            line_available: edited.init.line_available,
            stack_available: edited.init.stack_available,
            description_available: edited.init.description_available,
        }
    }

    /// Modal title, reflecting scope and edit vs. new-comment mode.
    pub(crate) fn title(&self) -> String {
        let prefix = if self.editing.is_some() {
            "Edit comment"
        } else {
            "Comment"
        };
        match &self.scope {
            ComposerScope::Line(line) => {
                let file = line.file.display();
                let line_no = line.target_line.or(line.source_line).unwrap_or(0);
                format!("{prefix} · {file}:{line_no}")
            }
            ComposerScope::Change => {
                let id = self.change_id.as_str();
                format!("{prefix} · change {id}")
            }
            ComposerScope::Stack(_) => format!("{prefix} · stack"),
            ComposerScope::Description(ctx) => {
                let line = ctx.target_line.unwrap_or(0);
                format!("{prefix} · description:{line}")
            }
        }
    }

    pub(crate) fn body_text(&self) -> String {
        self.body.lines().join("\n")
    }
}

impl ComposerOps for Composer {
    fn editing_is_some(&self) -> bool {
        self.editing.is_some()
    }

    fn line_available_clone(&self) -> Option<LineTarget> {
        self.line_available.clone()
    }

    fn stack_available_clone(&self) -> Option<StackContextSnapshot> {
        self.stack_available.clone()
    }

    fn description_available_clone(&self) -> Option<DescriptionContext> {
        self.description_available.clone()
    }

    fn current_severity(&self) -> Severity {
        self.severity
    }

    fn current_scope_tag(&self) -> ScopeTag {
        ScopeTag::of(&self.scope)
    }

    fn focus(&self) -> ComposerFocus {
        self.focus
    }

    fn set_focus(&mut self, focus: ComposerFocus) {
        self.focus = focus;
    }

    fn set_scope(&mut self, scope: ComposerScope) {
        self.scope = scope;
    }

    fn set_severity(&mut self, severity: Severity) {
        self.severity = severity;
    }

    fn clear_refusal_status(&mut self) {
        self.refusal_status = None;
    }

    fn set_refusal_status(&mut self, status: &'static str) {
        self.refusal_status = Some(status);
    }

    fn body_input(&mut self, key: KeyEvent) {
        self.body.input(key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::change_id::ChangeId;
    use crate::comment::{Anchor, LineAnchor, Severity, Side};
    use crossterm::event::{KeyCode, KeyModifiers};
    use std::path::PathBuf;
    use time::OffsetDateTime;

    fn make_anchor() -> Anchor {
        Anchor::Line {
            change_id: ChangeId::parse(&"a".repeat(32)).unwrap(),
            location: LineAnchor {
                file: PathBuf::from("src/lib.rs"),
                side: Side::New,
                old_line: None,
                new_line: Some(10),
                hunk_header: "@@ -1,1 +1,1 @@".to_owned(),
                target_text: "let x = 1;".to_owned(),
                context_before: vec![],
                context_after: vec![],
            },
        }
    }

    fn make_init() -> ComposerInit {
        ComposerInit {
            scope: ComposerScope::Change,
            severity: Severity::Note,
            change_id: ChangeId::parse(&"a".repeat(32)).unwrap(),
            change_description: "test change".to_owned(),
            line_available: None,
            stack_available: None,
            description_available: None,
        }
    }

    #[test]
    fn for_edit_delete_path_uses_original_anchor() {
        let anchor = make_anchor();
        let edited = EditedComment {
            init: make_init(),
            body: "original body".to_owned(),
            identity: OffsetDateTime::UNIX_EPOCH,
            original: None,
            original_anchor: anchor.clone(),
        };
        let mut composer = Composer::for_edit(edited);
        assert!(composer.editing.is_some(), "composer must be in edit mode");
        let key = KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL);
        let action = handle_composer_key(&mut composer, key);
        assert_eq!(action, ComposerAction::Delete);
        let ctx = composer
            .editing
            .as_ref()
            .expect("editing context must remain after Delete action");
        assert!(
            matches!(ctx.original_anchor, Anchor::Line { .. }),
            "original_anchor must be preserved for the delete path"
        );
    }

    #[test]
    fn for_edit_prefills_body_and_severity() {
        let anchor = make_anchor();
        let mut init = make_init();
        init.severity = Severity::Required;
        let edited = EditedComment {
            init,
            body: "prefilled body text".to_owned(),
            identity: OffsetDateTime::UNIX_EPOCH,
            original: None,
            original_anchor: anchor,
        };
        let composer = Composer::for_edit(edited);
        assert_eq!(
            composer.body_text(),
            "prefilled body text",
            "body_text() must equal the EditedComment body"
        );
        assert_eq!(
            composer.severity,
            Severity::Required,
            "severity must match the init severity from EditedComment"
        );
    }

    #[test]
    fn editing_is_some_false_for_new_composer() {
        let composer = Composer::new(make_init());
        assert!(
            !ComposerOps::editing_is_some(&composer),
            "new composer must not be in edit mode"
        );
    }

    #[test]
    fn editing_is_some_true_for_edit_composer() {
        let composer = Composer::for_edit(EditedComment {
            init: make_init(),
            body: "body".to_owned(),
            identity: OffsetDateTime::UNIX_EPOCH,
            original: None,
            original_anchor: make_anchor(),
        });
        assert!(
            ComposerOps::editing_is_some(&composer),
            "for_edit composer must be in edit mode"
        );
    }

    #[test]
    fn line_available_clone_returns_none_when_not_set() {
        let composer = Composer::new(make_init());
        assert!(
            composer.line_available_clone().is_none(),
            "line_available must be None when not provided"
        );
    }

    #[test]
    fn line_available_clone_returns_some_when_set() {
        let mut init = make_init();
        init.line_available = Some(LineTarget {
            file: PathBuf::from("src/lib.rs"),
            rendered_index: 0,
            source_line: None,
            target_line: Some(10),
            target_text: "let x = 1;".to_owned(),
            hunk_header: "@@ -1,1 +1,1 @@".to_owned(),
            context_before: vec![],
            context_after: vec![],
        });
        let composer = Composer::new(init);
        assert!(
            composer.line_available_clone().is_some(),
            "line_available must be Some when provided"
        );
    }

    #[test]
    fn set_scope_overwrites_active_scope() {
        let mut composer = Composer::new(make_init());
        assert!(matches!(composer.scope, ComposerScope::Change));
        ComposerOps::set_scope(
            &mut composer,
            ComposerScope::Stack(StackContextSnapshot {
                revset: "trunk()..@".to_owned(),
                revset_hash: local_review_core::revset_hash::RevsetHash::from_revset("trunk()..@"),
            }),
        );
        assert!(matches!(composer.scope, ComposerScope::Stack(_)));
    }

    #[test]
    fn set_severity_changes_severity() {
        let mut composer = Composer::new(make_init());
        assert_eq!(composer.severity, Severity::Note);
        ComposerOps::set_severity(&mut composer, Severity::Required);
        assert_eq!(composer.severity, Severity::Required);
    }

    #[test]
    fn refusal_status_round_trips() {
        let mut composer = Composer::new(make_init());
        assert!(composer.refusal_status.is_none(), "starts clear");
        ComposerOps::set_refusal_status(&mut composer, "test status");
        assert_eq!(composer.refusal_status, Some("test status"));
        ComposerOps::clear_refusal_status(&mut composer);
        assert!(
            composer.refusal_status.is_none(),
            "must be clear after clear_refusal_status"
        );
    }

    #[test]
    fn body_input_inserts_char_into_body() {
        let mut composer = Composer::new(make_init());
        assert_eq!(composer.body_text(), "");
        ComposerOps::body_input(
            &mut composer,
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
        );
        assert_eq!(composer.body_text(), "x");
    }
}
