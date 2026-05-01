use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use time::OffsetDateTime;
use tui_textarea::TextArea;

use crate::change_id::ChangeId;
use crate::comment::{Anchor, Comment, Severity};
use crate::stack::RevsetHash;

const SECS_PER_MIN: i64 = 60;
const SECS_PER_HOUR: i64 = 60 * SECS_PER_MIN;
const SECS_PER_DAY: i64 = 24 * SECS_PER_HOUR;

/// Where the comment is being anchored. Each variant carries the data needed
/// to build its `Anchor` at save time, so a variant cannot exist without its
/// backing context. The `Change` variant is a unit because the `change_id`
/// and description live on `Composer` directly (always rendered in the
/// picker label, not just when the scope is `Change`).
#[derive(Debug, Clone)]
pub(crate) enum ComposerScope {
    Line(LineTarget),
    Change,
    Stack(StackContextSnapshot),
    Description(DescriptionContext),
}

/// Discriminator-only view of `ComposerScope`. Variant set mirrors
/// `ComposerScope`; [`ScopeTag::of`] is the canonical projection and is
/// exhaustive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ScopeTag {
    Line,
    Change,
    Stack,
    Description,
}

impl ScopeTag {
    /// The tag corresponding to a borrowed scope. Pure: depends only on the
    /// enum discriminator.
    #[must_use]
    pub(crate) fn of(scope: &ComposerScope) -> Self {
        match scope {
            ComposerScope::Line(_) => Self::Line,
            ComposerScope::Change => Self::Change,
            ComposerScope::Stack(_) => Self::Stack,
            ComposerScope::Description(_) => Self::Description,
        }
    }
}

/// Status hint surfaced when Alt+K is pressed in single-change mode (no
/// stack availability).
pub(crate) const STATUS_STACK_UNAVAILABLE: &str = "stack scope unavailable in single-change mode";

/// Status hint surfaced when Alt+D is pressed without a description
/// availability snapshot (composer not opened from a description line).
pub(crate) const STATUS_DESCRIPTION_UNAVAILABLE: &str =
    "description scope unavailable: open from a description line";

/// Status hint surfaced when Alt+L is pressed without a line availability
/// snapshot (composer opened from a non-commentable cursor — e.g., the
/// overview screen, or a description-only context).
pub(crate) const STATUS_LINE_UNAVAILABLE: &str =
    "line scope unavailable: cursor is not on a commentable line";

/// All data needed to build a `LineAnchor` once the composer saves.
/// Naming note: `source_line`/`target_line` map to `old_line`/`new_line` in
/// the persisted `LineAnchor` — this struct uses the diff-view-side names.
#[derive(Debug, Clone)]
pub(crate) struct LineTarget {
    pub(crate) file: PathBuf,
    pub(crate) rendered_index: usize,
    pub(crate) source_line: Option<u32>,
    pub(crate) target_line: Option<u32>,
    pub(crate) target_text: String,
    pub(crate) hunk_header: String,
    pub(crate) context_before: Vec<String>,
    pub(crate) context_after: Vec<String>,
}

/// Snapshot of stack-level context captured at composer open time.
#[derive(Debug, Clone)]
pub(crate) struct StackContextSnapshot {
    pub(crate) revset: String,
    pub(crate) revset_hash: RevsetHash,
}

/// Description-scope context captured at composer open time. Carries the
/// cursor's 1-based line number plus the surrounding context window used to
/// build a `DescriptionAnchor` on save. The `change_id` may differ from the
/// composer's top-level `change_id` when editing a description-anchored
/// comment that belongs to a non-current change.
#[derive(Debug, Clone)]
pub(crate) struct DescriptionContext {
    pub(crate) change_id: ChangeId,
    pub(crate) target_line: Option<u32>,
    pub(crate) target_text: String,
    pub(crate) context_before: Vec<String>,
    pub(crate) context_after: Vec<String>,
}

/// Construction data for a new composer. The `*_available` snapshots advertise
/// "you can switch to this scope at chord time"; the chosen `scope` carries
/// the payload for the currently active scope. `change_id` and
/// `change_description` live on the composer regardless of scope because the
/// scope picker label always shows the change's short id, and the
/// Change-scope chrome reads the description.
pub(crate) struct ComposerInit {
    pub(crate) scope: ComposerScope,
    pub(crate) severity: Severity,
    pub(crate) change_id: ChangeId,
    pub(crate) change_description: String,
    pub(crate) line_available: Option<LineTarget>,
    pub(crate) stack_available: Option<StackContextSnapshot>,
    pub(crate) description_available: Option<DescriptionContext>,
}

/// Edit-mode coupling for `Composer`. When the composer is in edit mode
/// (vs. composing a new comment), all three fields are populated together:
/// `identity` keys the on-disk record by `created_at`, `original_anchor` is
/// the anchor of the comment being edited at open time (load-bearing for
/// delete, which never re-anchors regardless of chord-time scope swaps),
/// and `original` carries the full source `Comment` for paths that can't
/// resolve the record through `App::loaded_comments` (i.e., the stack
/// overview's edit path).
pub(crate) struct EditingContext {
    pub(crate) identity: OffsetDateTime,
    pub(crate) original_anchor: Anchor,
    pub(crate) original: Option<Comment>,
}

/// State for the comment composer modal.
///
/// `*_available` advertises whether the matching scope variant is
/// constructible from current state — `Some(...)` means "constructible,"
/// `None` means the cursor or app context can't supply the required payload.
///
/// When `scope` is the matching variant, `*_available` may or may not also
/// be `Some(...)` — they refer to *different* sources. `scope` carries the
/// payload for the active scope (which may have been built from a stored
/// anchor at edit time or synthesized for the single-change Stack case);
/// `*_available` carries the payload that Alt+<scope> would re-resolve to
/// from the current cursor. They can disagree on values, and Alt+<scope>
/// always re-resolves from `*_available` (even if scope is already that
/// variant).
pub(crate) struct Composer {
    pub(crate) scope: ComposerScope,
    pub(crate) severity: Severity,
    pub(crate) body: TextArea<'static>,
    /// `Some` when the composer is in edit mode; `None` for new comments.
    /// The folded payload keeps the edit-mode fields impossible to drift
    /// out of sync.
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
    /// Empty when the target change is not the current change (the body is
    /// not loaded for non-current changes).
    pub(crate) change_description: String,
    /// Line-scope payload available from current cursor; `None` means cursor is on a non-commentable row.
    pub(crate) line_available: Option<LineTarget>,
    /// Stack-scope payload available from current app state; `None` in single-change mode.
    pub(crate) stack_available: Option<StackContextSnapshot>,
    /// Description-scope payload available from current cursor; `None` when cursor is not on a description line.
    pub(crate) description_available: Option<DescriptionContext>,
}

/// Bundle of fields drawn from a single `Comment` to seed an edit-mode
/// composer. Constructing this in the caller keeps `severity`, `body`, and
/// the edit-mode coupling fields from drifting apart at the `for_edit`
/// boundary. `original_anchor` is mandatory; `original` is `Some` only when
/// the source record lives outside `App::loaded_comments` (the stack
/// overview's edit path).
pub(crate) struct EditedComment {
    pub(crate) init: ComposerInit,
    pub(crate) body: String,
    pub(crate) identity: OffsetDateTime,
    pub(crate) original: Option<Comment>,
    pub(crate) original_anchor: Anchor,
}

impl Composer {
    pub(crate) fn new(init: ComposerInit) -> Self {
        Self {
            scope: init.scope,
            severity: init.severity,
            body: TextArea::default(),
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

/// Result of processing a key inside the composer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ComposerAction {
    /// Key was consumed; composer remains open.
    Continue,
    /// `^X` pressed; caller should save and close.
    Save,
    /// `Esc` pressed; caller should discard and close.
    Cancel,
    /// `^D` pressed while in edit mode; caller should delete the original
    /// comment and close the composer.
    Delete,
    /// Scope chord pressed without a backing availability snapshot. The
    /// payload is the status string the dispatcher should surface; it
    /// matches `composer.refusal_status` for the same keypress.
    RefusedScopeChord(&'static str),
}

/// Handle a key event inside the composer.
///
/// Scope chords (`Alt+L`, `Alt+C`, `Alt+K`, `Alt+D`) and severity chords
/// (`Alt+R`, `Alt+S`, `Alt+N`) are Alt-chorded; save/delete (`^X`, `^D`)
/// remain Ctrl-chorded. The scope chords moved off Ctrl because `^K` is
/// intercepted by tui-textarea ("delete to end of line"), `^C` is
/// inconsistently delivered as SIGINT vs. as a Char key across terminals,
/// and `^L` collides with redraw bindings in some hosts; under raw mode the
/// Alt prefix avoids those collisions entirely. All intercepted keys are
/// consumed before being passed to tui-textarea; everything else flows
/// through.
///
/// Scope chords whose availability snapshot is absent (`Alt+L` without
/// `line_available`, `Alt+K` without `stack_available`, `Alt+D` without
/// `description_available`) return `RefusedScopeChord(status)` so the caller
/// can surface a status hint. The scope itself is left unchanged — the radio
/// never points at a scope without backing context. `Alt+C` (Change) is
/// unconditional because `change_id` lives on the composer directly.
#[expect(
    clippy::wildcard_enum_match_arm,
    reason = "unhandled Alt+/Ctrl+ KeyCode variants are intentionally ignored; forwarded to textarea"
)]
pub(crate) fn handle_composer_key(composer: &mut Composer, key: KeyEvent) -> ComposerAction {
    // Any keypress (including refusal-producing chords below, which overwrite
    // it) clears a stale in-modal refusal hint so it doesn't linger.
    composer.refusal_status = None;

    if key.modifiers == KeyModifiers::CONTROL {
        match key.code {
            KeyCode::Char('x') => {
                return ComposerAction::Save;
            }
            KeyCode::Char('d') if composer.editing.is_some() => {
                return ComposerAction::Delete;
            }
            _ => {}
        }
    }

    if key.modifiers == KeyModifiers::ALT {
        match key.code {
            KeyCode::Char('l' | 'L') => {
                if let Some(line) = composer.line_available.clone() {
                    composer.scope = ComposerScope::Line(line);
                    return ComposerAction::Continue;
                }
                composer.refusal_status = Some(STATUS_LINE_UNAVAILABLE);
                return ComposerAction::RefusedScopeChord(STATUS_LINE_UNAVAILABLE);
            }
            KeyCode::Char('c' | 'C') => {
                composer.scope = ComposerScope::Change;
                return ComposerAction::Continue;
            }
            KeyCode::Char('k' | 'K') => {
                if let Some(stack) = composer.stack_available.clone() {
                    composer.scope = ComposerScope::Stack(stack);
                    return ComposerAction::Continue;
                }
                composer.refusal_status = Some(STATUS_STACK_UNAVAILABLE);
                return ComposerAction::RefusedScopeChord(STATUS_STACK_UNAVAILABLE);
            }
            KeyCode::Char('r' | 'R') => {
                composer.severity = Severity::Required;
                return ComposerAction::Continue;
            }
            KeyCode::Char('s' | 'S') => {
                composer.severity = Severity::Suggestion;
                return ComposerAction::Continue;
            }
            KeyCode::Char('n' | 'N') => {
                composer.severity = Severity::Note;
                return ComposerAction::Continue;
            }
            KeyCode::Char('d' | 'D') => {
                if let Some(desc) = composer.description_available.clone() {
                    composer.scope = ComposerScope::Description(desc);
                    return ComposerAction::Continue;
                }
                composer.refusal_status = Some(STATUS_DESCRIPTION_UNAVAILABLE);
                return ComposerAction::RefusedScopeChord(STATUS_DESCRIPTION_UNAVAILABLE);
            }
            _ => {}
        }
    }

    if key.code == KeyCode::Esc {
        return ComposerAction::Cancel;
    }

    composer.body.input(key);
    ComposerAction::Continue
}

/// Choose the opening severity for a new comment.
///
/// - If the reviewer has made a previous choice this session, reuse it.
/// - Otherwise default to `Suggestion`.
#[must_use]
pub(crate) fn default_severity(last: Option<Severity>) -> Severity {
    last.unwrap_or(Severity::Suggestion)
}

#[must_use]
pub(crate) fn format_age(now: OffsetDateTime, created_at: OffsetDateTime) -> String {
    let secs = (now - created_at).whole_seconds().max(0);
    format_age_secs(secs)
}

// Boundary literals (29, 89, 3599, 86399) are rounding-half-open midpoints:
// e.g. < 30s rounds down to "just now", < 90s rounds to "1 min ago".
#[must_use]
pub(crate) fn format_age_secs(secs: i64) -> String {
    match secs {
        0..=29 => "just now".to_owned(),
        30..=89 => "1 min ago".to_owned(),
        90..=3599 => {
            let mins = (secs + SECS_PER_MIN / 2) / SECS_PER_MIN;
            format!("{mins} min ago")
        }
        3600..=86399 => {
            let hours = (secs + SECS_PER_HOUR / 2) / SECS_PER_HOUR;
            if hours == 1 {
                "1 hour ago".to_owned()
            } else {
                format!("{hours} hours ago")
            }
        }
        _ => {
            let days = secs / SECS_PER_DAY;
            if days == 1 {
                "yesterday".to_owned()
            } else {
                format!("{days} days ago")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::comment::{LineAnchor, Severity, Side};

    fn make_target() -> LineTarget {
        LineTarget {
            file: PathBuf::from("src/client.rs"),
            rendered_index: 5,
            source_line: None,
            target_line: Some(142),
            target_text: ".execute(|| self.inner.request(req.clone()))".to_owned(),
            hunk_header: "@@ -138,8 +138,14 @@ impl Client {".to_owned(),
            context_before: vec![
                "let req = self.prepare(req)?;".to_owned(),
                "let resp = self.retry_wrapper".to_owned(),
            ],
            context_after: vec![".await?;".to_owned()],
        }
    }

    fn make_init(severity: Severity) -> ComposerInit {
        let target = make_target();
        ComposerInit {
            scope: ComposerScope::Line(target.clone()),
            severity,
            change_id: ChangeId::parse("abc12345").unwrap(),
            change_description: "Add retry policy".to_owned(),
            line_available: Some(target),
            stack_available: Some(StackContextSnapshot {
                revset: "trunk()..@".to_owned(),
                revset_hash: RevsetHash::from_revset("trunk()..@"),
            }),
            description_available: None,
        }
    }

    fn make_composer(severity: Severity) -> Composer {
        Composer::new(make_init(severity))
    }

    fn make_line_anchor() -> Anchor {
        Anchor::Line {
            change_id: ChangeId::parse("abc12345").unwrap(),
            location: LineAnchor {
                file: PathBuf::from("src/client.rs"),
                side: Side::New,
                old_line: None,
                new_line: Some(142),
                hunk_header: "@@ -138,8 +138,14 @@ impl Client {".to_owned(),
                target_text: ".execute(|| self.inner.request(req.clone()))".to_owned(),
                context_before: vec![],
                context_after: vec![],
            },
        }
    }

    #[test]
    fn default_severity_uses_last_when_present() {
        assert_eq!(
            default_severity(Some(Severity::Required)),
            Severity::Required
        );
        assert_eq!(default_severity(Some(Severity::Note)), Severity::Note);
    }

    #[test]
    fn default_severity_falls_back_to_suggestion() {
        assert_eq!(default_severity(None), Severity::Suggestion);
    }

    #[test]
    fn format_age_secs_just_now() {
        assert_eq!(format_age_secs(0), "just now");
        assert_eq!(format_age_secs(15), "just now");
        assert_eq!(format_age_secs(29), "just now");
    }

    #[test]
    fn format_age_secs_one_min_boundary() {
        assert_eq!(format_age_secs(30), "1 min ago");
        assert_eq!(format_age_secs(89), "1 min ago");
    }

    #[test]
    fn format_age_secs_minutes() {
        assert_eq!(format_age_secs(90), "2 min ago");
        assert_eq!(format_age_secs(120), "2 min ago");
        assert_eq!(format_age_secs(150), "3 min ago");
        assert_eq!(format_age_secs(600), "10 min ago");
    }

    #[test]
    fn format_age_secs_one_hour_boundary() {
        assert_eq!(format_age_secs(3600), "1 hour ago");
        assert_eq!(format_age_secs(5399), "1 hour ago");
    }

    #[test]
    fn format_age_secs_hours() {
        assert_eq!(format_age_secs(7200), "2 hours ago");
        assert_eq!(format_age_secs(10800), "3 hours ago");
    }

    #[test]
    fn format_age_secs_yesterday() {
        assert_eq!(format_age_secs(86_400), "yesterday");
        assert_eq!(format_age_secs(172_799), "yesterday");
    }

    #[test]
    fn format_age_secs_days() {
        assert_eq!(format_age_secs(172_800), "2 days ago");
        assert_eq!(format_age_secs(864_000), "10 days ago");
    }

    #[test]
    fn handle_composer_key_alt_l_sets_line_scope() {
        let mut c = make_composer(Severity::Suggestion);
        c.scope = ComposerScope::Change;
        let key = KeyEvent::new(KeyCode::Char('l'), KeyModifiers::ALT);
        let action = handle_composer_key(&mut c, key);
        assert_eq!(action, ComposerAction::Continue);
        assert!(matches!(c.scope, ComposerScope::Line(_)));
    }

    #[test]
    fn handle_composer_key_alt_c_sets_change_scope() {
        let mut c = make_composer(Severity::Suggestion);
        let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::ALT);
        let action = handle_composer_key(&mut c, key);
        assert_eq!(action, ComposerAction::Continue);
        assert!(matches!(c.scope, ComposerScope::Change));
    }

    #[test]
    fn handle_composer_key_alt_k_sets_stack_scope() {
        let mut c = make_composer(Severity::Suggestion);
        let key = KeyEvent::new(KeyCode::Char('k'), KeyModifiers::ALT);
        let action = handle_composer_key(&mut c, key);
        assert_eq!(action, ComposerAction::Continue);
        assert!(matches!(c.scope, ComposerScope::Stack(_)));
    }

    // Pin the regression: under CONTROL, L/C/K must NOT trigger a scope
    // switch (Ctrl+K collides with tui-textarea's "delete to end of line",
    // and Ctrl+C is delivered inconsistently across terminals). The keys are
    // forwarded to the textarea; the action is `Continue`.
    #[test]
    fn handle_composer_key_ctrl_k_does_not_switch_scope() {
        let mut c = make_composer(Severity::Suggestion);
        c.scope = ComposerScope::Change;
        let key = KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL);
        let action = handle_composer_key(&mut c, key);
        assert_eq!(action, ComposerAction::Continue);
        assert!(matches!(c.scope, ComposerScope::Change));
    }

    #[test]
    fn handle_composer_key_ctrl_l_does_not_switch_scope() {
        let mut c = make_composer(Severity::Suggestion);
        c.scope = ComposerScope::Change;
        let key = KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL);
        let action = handle_composer_key(&mut c, key);
        assert_eq!(action, ComposerAction::Continue);
        assert!(matches!(c.scope, ComposerScope::Change));
    }

    #[test]
    fn handle_composer_key_ctrl_c_does_not_switch_scope() {
        let mut c = make_composer(Severity::Suggestion);
        // Start with the default (Line); Ctrl+C must not flip to Change.
        let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        let action = handle_composer_key(&mut c, key);
        assert_eq!(action, ComposerAction::Continue);
        assert!(matches!(c.scope, ComposerScope::Line(_)));
    }

    #[test]
    fn handle_composer_key_alt_r_sets_required() {
        let mut c = make_composer(Severity::Note);
        let key = KeyEvent::new(KeyCode::Char('r'), KeyModifiers::ALT);
        let action = handle_composer_key(&mut c, key);
        assert_eq!(action, ComposerAction::Continue);
        assert_eq!(c.severity, Severity::Required);
    }

    #[test]
    fn handle_composer_key_alt_s_sets_suggestion() {
        let mut c = make_composer(Severity::Required);
        let key = KeyEvent::new(KeyCode::Char('s'), KeyModifiers::ALT);
        let action = handle_composer_key(&mut c, key);
        assert_eq!(action, ComposerAction::Continue);
        assert_eq!(c.severity, Severity::Suggestion);
    }

    #[test]
    fn handle_composer_key_alt_n_sets_note() {
        let mut c = make_composer(Severity::Required);
        let key = KeyEvent::new(KeyCode::Char('n'), KeyModifiers::ALT);
        let action = handle_composer_key(&mut c, key);
        assert_eq!(action, ComposerAction::Continue);
        assert_eq!(c.severity, Severity::Note);
    }

    #[test]
    fn handle_composer_key_alt_uppercase_r_also_sets_required() {
        // Some terminals send Alt+Shift+letter as uppercase; accept both.
        let mut c = make_composer(Severity::Note);
        let key = KeyEvent::new(KeyCode::Char('R'), KeyModifiers::ALT);
        let action = handle_composer_key(&mut c, key);
        assert_eq!(action, ComposerAction::Continue);
        assert_eq!(c.severity, Severity::Required);
    }

    #[test]
    fn handle_composer_key_ctrl_x_returns_save() {
        let mut c = make_composer(Severity::Suggestion);
        let key = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL);
        let action = handle_composer_key(&mut c, key);
        assert_eq!(action, ComposerAction::Save);
    }

    #[test]
    fn handle_composer_key_esc_returns_cancel() {
        let mut c = make_composer(Severity::Suggestion);
        let key = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        let action = handle_composer_key(&mut c, key);
        assert_eq!(action, ComposerAction::Cancel);
    }

    #[test]
    fn composer_title_new_mode_line_scope() {
        let c = make_composer(Severity::Suggestion);
        assert_eq!(c.title(), "Comment · src/client.rs:142");
    }

    #[test]
    fn composer_title_new_mode_change_scope() {
        let mut c = make_composer(Severity::Suggestion);
        c.scope = ComposerScope::Change;
        assert_eq!(c.title(), "Comment · change abc12345");
    }

    #[test]
    fn composer_title_new_mode_stack_scope() {
        let mut c = make_composer(Severity::Suggestion);
        let stack = c
            .stack_available
            .clone()
            .expect("test fixture sets stack_available");
        c.scope = ComposerScope::Stack(stack);
        assert_eq!(c.title(), "Comment · stack");
    }

    #[test]
    fn composer_title_edit_mode() {
        let c = Composer::for_edit(EditedComment {
            init: make_init(Severity::Required),
            body: "existing body".to_owned(),
            identity: OffsetDateTime::UNIX_EPOCH,
            original: None,
            original_anchor: make_line_anchor(),
        });
        assert_eq!(c.title(), "Edit comment · src/client.rs:142");
    }

    #[test]
    fn composer_title_edit_mode_change_scope() {
        let mut init = make_init(Severity::Required);
        init.scope = ComposerScope::Change;
        let c = Composer::for_edit(EditedComment {
            init,
            body: "existing body".to_owned(),
            identity: OffsetDateTime::UNIX_EPOCH,
            original: None,
            original_anchor: make_line_anchor(),
        });
        assert_eq!(c.title(), "Edit comment · change abc12345");
    }

    #[test]
    fn composer_title_edit_mode_stack_scope() {
        let mut init = make_init(Severity::Required);
        let stack = init
            .stack_available
            .clone()
            .expect("test fixture sets stack_available");
        init.scope = ComposerScope::Stack(stack);
        let c = Composer::for_edit(EditedComment {
            init,
            body: "existing body".to_owned(),
            identity: OffsetDateTime::UNIX_EPOCH,
            original: None,
            original_anchor: make_line_anchor(),
        });
        assert_eq!(c.title(), "Edit comment · stack");
    }

    #[test]
    fn for_edit_prepopulates_body_and_severity() {
        let c = Composer::for_edit(EditedComment {
            init: make_init(Severity::Required),
            body: "line one\nline two".to_owned(),
            identity: OffsetDateTime::UNIX_EPOCH,
            original: None,
            original_anchor: make_line_anchor(),
        });
        assert_eq!(c.body_text(), "line one\nline two");
        assert_eq!(c.severity, Severity::Required);
        let ctx = c
            .editing
            .as_ref()
            .expect("for_edit-built composer is in edit mode");
        assert_eq!(ctx.identity, OffsetDateTime::UNIX_EPOCH);
        assert!(matches!(c.scope, ComposerScope::Line(_)));
    }

    #[test]
    fn ctrl_d_in_edit_mode_returns_delete() {
        let mut c = Composer::for_edit(EditedComment {
            init: make_init(Severity::Note),
            body: "body".to_owned(),
            identity: OffsetDateTime::UNIX_EPOCH,
            original: None,
            original_anchor: make_line_anchor(),
        });
        let key = KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL);
        let action = handle_composer_key(&mut c, key);
        assert_eq!(action, ComposerAction::Delete);
    }

    #[test]
    fn ctrl_d_outside_edit_mode_forwarded_to_textarea() {
        let mut c = make_composer(Severity::Suggestion);
        let key = KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL);
        let action = handle_composer_key(&mut c, key);
        assert_eq!(action, ComposerAction::Continue);
    }

    fn make_description_context() -> DescriptionContext {
        DescriptionContext {
            change_id: ChangeId::parse("abc12345").unwrap(),
            target_line: Some(1),
            target_text: "summary".to_owned(),
            context_before: vec![],
            context_after: vec![],
        }
    }

    // -- B5: Alt+D switches to Description scope when a description
    //   availability snapshot is present.
    #[test]
    fn handle_composer_key_alt_d_sets_description_scope_when_snapshot_present() {
        let mut c = make_composer(Severity::Suggestion);
        c.description_available = Some(make_description_context());
        let key = KeyEvent::new(KeyCode::Char('d'), KeyModifiers::ALT);
        let action = handle_composer_key(&mut c, key);
        assert_eq!(action, ComposerAction::Continue);
        assert!(matches!(c.scope, ComposerScope::Description(_)));
    }

    // -- U3: Alt+D without a snapshot returns RefusedScopeChord with the
    //   description-unavailable status. Scope is unchanged.
    #[test]
    fn handle_composer_key_alt_d_emits_refusal_status_when_snapshot_absent() {
        let mut c = make_composer(Severity::Suggestion);
        c.scope = ComposerScope::Change;
        assert!(c.description_available.is_none());
        let key = KeyEvent::new(KeyCode::Char('d'), KeyModifiers::ALT);
        let action = handle_composer_key(&mut c, key);
        assert_eq!(
            action,
            ComposerAction::RefusedScopeChord(STATUS_DESCRIPTION_UNAVAILABLE)
        );
        assert!(matches!(c.scope, ComposerScope::Change));
    }

    // -- U3: Alt+K without a stack snapshot returns
    //   RefusedScopeChord(STATUS_STACK_UNAVAILABLE). Single-change mode has
    //   `stack_available: None`.
    #[test]
    fn handle_composer_key_alt_k_emits_refusal_status_when_stack_unavailable() {
        let mut c = make_composer(Severity::Suggestion);
        c.stack_available = None;
        c.scope = ComposerScope::Change;
        let key = KeyEvent::new(KeyCode::Char('k'), KeyModifiers::ALT);
        let action = handle_composer_key(&mut c, key);
        assert_eq!(
            action,
            ComposerAction::RefusedScopeChord(STATUS_STACK_UNAVAILABLE)
        );
        assert!(matches!(c.scope, ComposerScope::Change));
    }

    // -- Alt+L without a line snapshot returns
    //   RefusedScopeChord(STATUS_LINE_UNAVAILABLE). Composers opened from a
    //   non-commentable cursor (overview, description-only) carry
    //   `line_available: None`.
    #[test]
    fn handle_composer_key_alt_l_emits_refusal_status_when_line_unavailable() {
        let mut c = make_composer(Severity::Suggestion);
        c.line_available = None;
        c.scope = ComposerScope::Change;
        let key = KeyEvent::new(KeyCode::Char('l'), KeyModifiers::ALT);
        let action = handle_composer_key(&mut c, key);
        assert_eq!(
            action,
            ComposerAction::RefusedScopeChord(STATUS_LINE_UNAVAILABLE)
        );
        assert!(matches!(c.scope, ComposerScope::Change));
    }

    // -- E2: refusal hint stored on `composer.refusal_status` so the modal can
    //   surface it inline. Cleared on the next keypress.
    #[test]
    fn composer_refusal_status_visible_in_modal_when_alt_d_pressed_without_snapshot() {
        let mut c = make_composer(Severity::Suggestion);
        assert!(c.description_available.is_none());
        assert!(c.refusal_status.is_none(), "starts unset");
        handle_composer_key(&mut c, KeyEvent::new(KeyCode::Char('d'), KeyModifiers::ALT));
        assert_eq!(c.refusal_status, Some(STATUS_DESCRIPTION_UNAVAILABLE));
        // Next non-refusing keypress clears the hint.
        handle_composer_key(
            &mut c,
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE),
        );
        assert_eq!(c.refusal_status, None);
    }

    // -- T-E2-stack-clear: Alt+K refusal pins both set AND clear, symmetric
    //   with the Alt+D test.
    #[test]
    fn composer_refusal_status_set_on_alt_k_without_stack() {
        let mut c = make_composer(Severity::Suggestion);
        c.stack_available = None;
        handle_composer_key(&mut c, KeyEvent::new(KeyCode::Char('k'), KeyModifiers::ALT));
        assert_eq!(c.refusal_status, Some(STATUS_STACK_UNAVAILABLE));
        // Next non-refusing keypress clears the hint.
        handle_composer_key(
            &mut c,
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE),
        );
        assert_eq!(c.refusal_status, None);
    }

    // -- C3: status row contributes 0 rows when `refusal_status` is None and
    //   STATUS_ROWS when set. Pins the "no permanent tax" contract — body
    //   reclaims its row as soon as the hint clears.
    #[test]
    fn status_row_height_zero_when_refusal_status_none_and_reclaims_after_clear() {
        use crate::tui::composer_overlay::{status_row_height, STATUS_ROWS_FOR_TEST};
        let mut c = make_composer(Severity::Suggestion);
        c.stack_available = None;
        // Initial state: no refusal → 0 rows.
        assert_eq!(status_row_height(&c), 0);
        // Alt+K without stack → refusal hint set → STATUS_ROWS_FOR_TEST.
        handle_composer_key(&mut c, KeyEvent::new(KeyCode::Char('k'), KeyModifiers::ALT));
        assert_eq!(status_row_height(&c), STATUS_ROWS_FOR_TEST);
        // Next non-refusing keypress → cleared → 0 rows again.
        handle_composer_key(
            &mut c,
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE),
        );
        assert_eq!(status_row_height(&c), 0);
    }

    // Sum-type guarantees: each scope variant carries the data needed to
    // build its `Anchor` at save time. These tests don't exercise behavior at
    // runtime — they pin that the type forces callers to provide the payload
    // by construction (the test would not compile if a payload were missing).
    #[test]
    fn composer_scope_line_carries_line_target() {
        let target = make_target();
        let scope = ComposerScope::Line(target.clone());
        match scope {
            ComposerScope::Line(carried) => {
                assert_eq!(carried.file, target.file);
                assert_eq!(carried.target_line, target.target_line);
            }
            ComposerScope::Change | ComposerScope::Stack(_) | ComposerScope::Description(_) => {
                unreachable!("constructed Line variant")
            }
        }
    }

    #[test]
    fn composer_scope_stack_carries_stack_context() {
        let snapshot = StackContextSnapshot {
            revset: "trunk()..@".to_owned(),
            revset_hash: RevsetHash::from_revset("trunk()..@"),
        };
        let scope = ComposerScope::Stack(snapshot.clone());
        match scope {
            ComposerScope::Stack(carried) => {
                assert_eq!(carried.revset, snapshot.revset);
                assert_eq!(carried.revset_hash, snapshot.revset_hash);
            }
            ComposerScope::Line(_) | ComposerScope::Change | ComposerScope::Description(_) => {
                unreachable!("constructed Stack variant")
            }
        }
    }

    #[test]
    fn composer_scope_description_carries_description_context() {
        let ctx = make_description_context();
        let scope = ComposerScope::Description(ctx.clone());
        match scope {
            ComposerScope::Description(carried) => {
                assert_eq!(carried.change_id, ctx.change_id);
                assert_eq!(carried.target_line, ctx.target_line);
                assert_eq!(carried.target_text, ctx.target_text);
            }
            ComposerScope::Line(_) | ComposerScope::Change | ComposerScope::Stack(_) => {
                unreachable!("constructed Description variant")
            }
        }
    }
}
