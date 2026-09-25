//! Unified incoming-message classifier.
//!
//! Decides who (or what) an incoming piece of text ultimately came from —
//! the owner, a slash command the owner ran, another session/subagent
//! ("peer"), a background-task completion notice, a `[Request interrupted
//! by user]` marker, a compaction-summary replay, or harness noise. Applies
//! to both `user` turns ([`crate::transcript::events::root::UserEvent`]) and mid-turn
//! deliveries ([`crate::transcript::events::attachment::QueuedCommand`]).
//!
//! Decided from **structured fields first** (`isCompactSummary`,
//! `origin.kind` and its shape, `isMeta`), falling back to content tags only
//! when those fields are absent — the case on older transcripts. See
//! `docs/session-restore/research/2026-09-25-claude-transcript-message-taxonomy.md`
//! for the field survey this is built from.

use super::root::OriginInfo;

/// Who (or what) sent an incoming piece of text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IncomingKind {
    /// The owner typed it, as a normal conversation turn.
    Owner(String),
    /// The owner typed it while a turn was already running (delivered as a
    /// `queued_command` attachment rather than becoming its own turn).
    OwnerMidTurn(String),
    /// A slash command the owner ran, e.g. `/model` with argument
    /// `claude-fable-5`, or `/compact` with no arguments.
    OwnerCommand {
        /// Command name, including its leading slash (e.g. `"/model"`)
        name: String,
        /// Raw argument text, possibly empty
        args: String,
    },
    /// Another session, subagent hand-back, or host-injected message.
    Peer {
        /// Message body, tag-wrapper stripped when one was present.
        text: String,
        /// Sender display name (`origin.name`) or identity (`origin.from`),
        /// when either is present.
        sender: Option<String>,
        /// `true` for a subagent's final-report hand-back.
        handback: bool,
    },
    /// A background agent/Bash task finished (`<task-notification>`).
    TaskNotification(String),
    /// A `[Request interrupted by user…]` marker.
    Interrupt(String),
    /// The synthesized replay of a compacted conversation (`isCompactSummary`).
    CompactSummary,
    /// Any other harness-injected content: skill bodies, local-command
    /// echoes, image companions, idle notices, reminders with nothing left
    /// after stripping, tool-result-only turns, non-external turns, …
    Harness,
}

/// Render a slash-command turn for display: `/model claude-fable-5`, or
/// just `/compact` when there are no arguments.
#[must_use]
pub fn render_owner_command(name: &str, args: &str) -> String {
    let args = args.trim();
    if args.is_empty() {
        name.to_string()
    } else {
        format!("{name} {args}")
    }
}

/// Extract the text between `<tag>` and `</tag>` in `text`, trimmed.
fn extract_tag<'a>(text: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = text.find(&open)? + open.len();
    let end = start + text[start..].find(&close)?;
    Some(text[start..end].trim())
}

/// Extract `key="value"` from a tag's attribute string.
fn extract_attr<'a>(attrs: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!("{key}=\"");
    let start = attrs.find(&needle)? + needle.len();
    let end = start + attrs[start..].find('"')?;
    Some(&attrs[start..end])
}

/// Strip every `<system-reminder>…</system-reminder>` block from `text`.
/// Zero-allocation (returns `text` unchanged) when no block is present.
#[must_use]
pub fn strip_system_reminder_blocks(text: &str) -> String {
    const OPEN: &str = "<system-reminder>";
    const CLOSE: &str = "</system-reminder>";
    if !text.contains(OPEN) {
        return text.to_string();
    }
    let mut result = String::with_capacity(text.len());
    let mut rest = text;
    loop {
        let Some(start) = rest.find(OPEN) else {
            result.push_str(rest);
            break;
        };
        result.push_str(&rest[..start]);
        let after_open = &rest[start + OPEN.len()..];
        let Some(end) = after_open.find(CLOSE) else {
            // Unterminated block — keep the rest verbatim rather than lose it.
            result.push_str(&rest[start..]);
            break;
        };
        rest = &after_open[end + CLOSE.len()..];
    }
    result
}

/// Unwrap `<pasted_content id="…">…</pasted_content>` wrappers in `text`,
/// keeping the inner text and prefixing it `[pasted] `.
#[must_use]
pub fn unwrap_pasted_content(text: &str) -> String {
    const OPEN_MARKER: &str = "<pasted_content";
    const CLOSE_TAG: &str = "</pasted_content>";
    if !text.contains(OPEN_MARKER) {
        return text.to_string();
    }
    let mut result = String::with_capacity(text.len());
    let mut rest = text;
    loop {
        let Some(start) = rest.find(OPEN_MARKER) else {
            result.push_str(rest);
            break;
        };
        result.push_str(&rest[..start]);
        let after = &rest[start..];
        let Some(gt) = after.find('>') else {
            result.push_str(after);
            break;
        };
        result.push_str("[pasted] ");
        rest = &after[gt + 1..];
    }
    result.replace(CLOSE_TAG, "")
}

/// Extract a `<cross-session-message …>`/`<agent-message …>` tag's sender
/// and inner body text from freeform content — the fallback used when no
/// structured `origin` is available.
fn extract_peer_from_content(text: &str) -> IncomingKind {
    let cross = text.find("<cross-session-message");
    let agent = text.find("<agent-message");
    let Some(start) = (match (cross, agent) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }) else {
        return IncomingKind::Owner(text.trim().to_string());
    };

    let after_tag = &text[start..];
    let is_agent_message = after_tag.starts_with("<agent-message");
    let Some(gt) = after_tag.find('>') else {
        return IncomingKind::Peer { text: text.trim().to_string(), sender: None, handback: is_agent_message };
    };
    let attrs = &after_tag[..gt];
    let sender = extract_attr(attrs, "name").or_else(|| extract_attr(attrs, "from")).map(str::to_string);

    let mut body = after_tag[gt + 1..].trim();
    for close_tag in ["</cross-session-message>", "</agent-message>"] {
        if let Some(stripped) = body.strip_suffix(close_tag) {
            body = stripped.trim();
        }
    }
    let handback = is_agent_message || body.starts_with("[Subagent hand-back]");
    IncomingKind::Peer { text: body.to_string(), sender, handback }
}

/// Build a `Peer` kind from a structured `origin`, preferring `origin.body`
/// (already the clean message text) over re-parsing content tags.
fn peer_from_origin(origin: &OriginInfo, raw_text: &str) -> IncomingKind {
    let sender = origin.name().or_else(|| origin.from_field()).map(str::to_string);
    let handback = origin.is_handback();
    let text = origin.body().map(|body| body.trim().to_string()).unwrap_or_else(|| {
        match extract_peer_from_content(raw_text.trim_start()) {
            IncomingKind::Peer { text, .. } => text,
            _ => raw_text.trim().to_string(),
        }
    });
    IncomingKind::Peer { text, sender, handback }
}

/// Classify text with no structured `origin` field to lean on — the
/// fallback path for older transcripts, and for `queue-operation` content
/// (which never carries `origin` on the wire). `plain_owner` builds the
/// variant used for ordinary owner text (`Owner` for a `user` turn,
/// `OwnerMidTurn` for a queued mid-turn delivery); `is_meta` gates the
/// harness-notice check `UserEvent` carries but `QueueOperation` does not.
fn classify_by_content(text: &str, is_meta: bool, plain_owner: fn(String) -> IncomingKind) -> IncomingKind {
    if is_meta {
        return IncomingKind::Harness;
    }

    let trimmed = text.trim_start();

    if trimmed.starts_with("<command-name>") {
        if let (Some(name), Some(args)) = (extract_tag(trimmed, "command-name"), extract_tag(trimmed, "command-args")) {
            return IncomingKind::OwnerCommand { name: name.to_string(), args: args.to_string() };
        }
    }

    const HARNESS_ECHO_PREFIXES: [&str; 3] =
        ["<local-command-caveat>", "<local-command-stdout>", "<local-command-stderr>"];
    if HARNESS_ECHO_PREFIXES.iter().any(|prefix| trimmed.starts_with(prefix)) {
        return IncomingKind::Harness;
    }

    if trimmed.starts_with("[Request interrupted by user") {
        return IncomingKind::Interrupt(text.trim().to_string());
    }

    if trimmed.starts_with("<system-reminder>") {
        let stripped = strip_system_reminder_blocks(text);
        let remainder = stripped.trim().to_string();
        if remainder.is_empty() {
            return IncomingKind::Harness;
        }
        // Re-classify the remainder — it can itself be a peer message
        // (`turnOrigin: "sdk"` case in the survey) or plain owner text.
        return classify_by_content(&remainder, false, plain_owner);
    }

    if trimmed.starts_with("<task-notification>") {
        return IncomingKind::TaskNotification(text.trim().to_string());
    }

    if trimmed.starts_with("<cross-session-message") || trimmed.starts_with("<agent-message") {
        return extract_peer_from_content(trimmed);
    }

    let owned = unwrap_pasted_content(text.trim());
    plain_owner(owned)
}

/// Classify a `user` turn's content (spec section A). Skips tool-result-only
/// turns and non-`isSidechain: false` filtering is the caller's job (main
/// chain vs sidechain is orthogonal to who sent the text).
#[must_use]
pub fn classify_user_content(
    text: &str,
    is_compact_summary: bool,
    origin: Option<&OriginInfo>,
    is_meta: bool,
) -> IncomingKind {
    if is_compact_summary {
        return IncomingKind::CompactSummary;
    }

    if let Some(origin) = origin {
        return match origin.kind.as_str() {
            "human" => {
                let stripped = strip_system_reminder_blocks(text);
                let remainder = stripped.trim();
                if remainder.is_empty() {
                    IncomingKind::Harness
                } else {
                    IncomingKind::Owner(unwrap_pasted_content(remainder))
                }
            }
            "task-notification" => IncomingKind::TaskNotification(text.trim().to_string()),
            "peer" => peer_from_origin(origin, text),
            // Unknown/future origin.kind values (e.g. "coordinator") fall
            // back to harness rather than risk misfiling them as owner text.
            _ => IncomingKind::Harness,
        };
    }

    classify_by_content(text, is_meta, IncomingKind::Owner)
}

/// Classify a `queued_command` attachment's `prompt` (spec section B).
#[must_use]
pub fn classify_queued_command_content(
    prompt: &str,
    command_mode: Option<&str>,
    origin: Option<&OriginInfo>,
    is_meta: bool,
) -> IncomingKind {
    if command_mode == Some("task-notification") {
        return IncomingKind::TaskNotification(prompt.trim().to_string());
    }

    if let Some(origin) = origin {
        return match origin.kind.as_str() {
            "human" => {
                let stripped = strip_system_reminder_blocks(prompt);
                let remainder = stripped.trim();
                if remainder.is_empty() {
                    IncomingKind::Harness
                } else {
                    IncomingKind::OwnerMidTurn(unwrap_pasted_content(remainder))
                }
            }
            "peer" => peer_from_origin(origin, prompt),
            _ => IncomingKind::Harness,
        };
    }

    classify_by_content(prompt, is_meta, IncomingKind::OwnerMidTurn)
}

/// Classify plain `queue-operation` content, which never carries an
/// `origin` field on the wire (spec section C).
#[must_use]
pub fn classify_plain_text(text: &str) -> IncomingKind {
    classify_by_content(text, false, IncomingKind::Owner)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn human_origin() -> OriginInfo {
        serde_json::from_str(r#"{"kind":"human"}"#).expect("origin fixture")
    }

    fn peer_origin(json: &str) -> OriginInfo {
        serde_json::from_str(json).expect("origin fixture")
    }

    #[test]
    fn owner_plain_text_with_human_origin() {
        let kind = classify_user_content("placeholder prompt", false, Some(&human_origin()), false);
        assert_eq!(kind, IncomingKind::Owner("placeholder prompt".to_string()));
    }

    #[test]
    fn owner_text_survives_a_leading_system_reminder_under_human_origin() {
        // Real shape (survey row 696): a genuine question the owner asked,
        // wrapped by a harness reminder about a background task.
        let text = "<system-reminder>\nThe user started your suggested background task task_00000001 (\"Example background task\") in a separate local session. It is running independently. You will be notified here when it ends.\n</system-reminder>\n\nготово? всё проверено?";
        let kind = classify_user_content(text, false, Some(&human_origin()), false);
        assert_eq!(
            kind,
            IncomingKind::Owner("готово? всё проверено?".to_string())
        );
    }

    #[test]
    fn harness_when_nothing_remains_after_stripping_the_reminder() {
        let text = "<system-reminder>\nJust a reminder, no owner text.\n</system-reminder>";
        let kind = classify_user_content(text, false, Some(&human_origin()), false);
        assert_eq!(kind, IncomingKind::Harness);
    }

    #[test]
    fn task_notification_origin() {
        let kind =
            classify_user_content("<task-notification>...</task-notification>", false, Some(&peer_origin(r#"{"kind":"task-notification"}"#)), false);
        assert!(matches!(kind, IncomingKind::TaskNotification(_)));
    }

    #[test]
    fn peer_cross_session_message_with_name_and_body() {
        let origin = peer_origin(
            r#"{"kind":"peer","from":"uds:\\.\\pipe\\LOCAL\\cc-msg-x","msg_id":"m1","name":"nemo-76","fromMode":"prompting","body":"placeholder peer body"}"#,
        );
        let kind = classify_user_content(
            "Another Claude session sent a message:\n<cross-session-message from=\"uds:\\\\.\\pipe\\LOCAL\\cc-msg-x\" name=\"nemo-76\">\nplaceholder peer body\n</cross-session-message>",
            false,
            Some(&origin),
            true,
        );
        assert_eq!(
            kind,
            IncomingKind::Peer { text: "placeholder peer body".to_string(), sender: Some("nemo-76".to_string()), handback: false }
        );
    }

    #[test]
    fn peer_subagent_handback_flagged() {
        let origin = peer_origin(
            r#"{"kind":"peer","from":"a1b2c3d4e5f607182","senderTaskId":"a1b2c3d4e5f607182","body":"[Subagent hand-back] placeholder report","handback":true}"#,
        );
        let kind = classify_user_content(
            "Another Claude session sent a message:\n<agent-message from=\"a1b2c3d4e5f607182\">\n[Subagent hand-back] placeholder report\n</agent-message>",
            false,
            Some(&origin),
            true,
        );
        let IncomingKind::Peer { text, sender, handback } = kind else { panic!("expected Peer") };
        assert_eq!(text, "[Subagent hand-back] placeholder report");
        assert_eq!(sender, Some("a1b2c3d4e5f607182".to_string()));
        assert!(handback);
    }

    #[test]
    fn peer_host_injected_without_origin_field_falls_back_to_content_tag() {
        // Real shape (survey row 1030): no `origin` at all, `turnOrigin: "sdk"`,
        // a leading reminder followed by a cross-session-message tag.
        let text = "<system-reminder>\nThe separate session for background task task_00000001 (\"Example background task\") has ended.\n</system-reminder>\n\n<cross-session-message from=\"local_00000000-0000-0000-0000-000000000000\" name=\"Соседняя сессия\">\nplaceholder answer\n</cross-session-message>";
        let kind = classify_user_content(text, false, None, false);
        let IncomingKind::Peer { text, sender, .. } = kind else { panic!("expected Peer, got {kind:?}") };
        assert_eq!(text, "placeholder answer");
        assert_eq!(sender, Some("Соседняя сессия".to_string()));
    }

    #[test]
    fn interrupt_marker() {
        let kind = classify_user_content("[Request interrupted by user]", false, Some(&human_origin()), false);
        // An interrupt marker under origin.kind human is still owner text —
        // the marker prefix only matters on the no-origin fallback path
        // (legacy transcripts); with a structured human origin it is kept
        // verbatim as Owner, matching "a system-generated notice of a real
        // user action" from the pre-taxonomy design.
        assert_eq!(kind, IncomingKind::Owner("[Request interrupted by user]".to_string()));

        let legacy = classify_user_content("[Request interrupted by user]", false, None, false);
        assert_eq!(legacy, IncomingKind::Interrupt("[Request interrupted by user]".to_string()));
    }

    #[test]
    fn compact_summary_wins_over_everything() {
        let kind = classify_user_content("This session is being continued…", true, Some(&human_origin()), false);
        assert_eq!(kind, IncomingKind::CompactSummary);
    }

    #[test]
    fn legacy_meta_turn_without_origin_is_harness() {
        let kind = classify_user_content(
            "<local-command-caveat>Caveat: placeholder</local-command-caveat>",
            false,
            None,
            true,
        );
        assert_eq!(kind, IncomingKind::Harness);
    }

    #[test]
    fn legacy_slash_command_without_origin() {
        let kind = classify_user_content(
            "<command-name>/model</command-name>\n<command-message>model</command-message>\n<command-args>placeholder-model</command-args>",
            false,
            None,
            false,
        );
        assert_eq!(
            kind,
            IncomingKind::OwnerCommand { name: "/model".to_string(), args: "placeholder-model".to_string() }
        );
    }

    #[test]
    fn legacy_task_notification_without_origin() {
        let kind = classify_user_content(
            "<task-notification>\n<task-id>t</task-id>\n</task-notification>",
            false,
            None,
            false,
        );
        assert!(matches!(kind, IncomingKind::TaskNotification(_)));
    }

    #[test]
    fn legacy_plain_text_falls_back_to_owner() {
        let kind = classify_user_content("placeholder plain text, no origin field at all", false, None, false);
        assert_eq!(kind, IncomingKind::Owner("placeholder plain text, no origin field at all".to_string()));
    }

    #[test]
    fn owner_text_unwraps_pasted_content() {
        let kind = classify_user_content(
            "<pasted_content id=\"f494\">\ninner pasted text\n</pasted_content>\ntrailing text",
            false,
            Some(&human_origin()),
            false,
        );
        assert_eq!(kind, IncomingKind::Owner("[pasted] \ninner pasted text\n\ntrailing text".to_string()));
    }

    #[test]
    fn queued_command_task_notification_by_command_mode() {
        let kind = classify_queued_command_content("<task-notification>...</task-notification>", Some("task-notification"), None, false);
        assert!(matches!(kind, IncomingKind::TaskNotification(_)));
    }

    #[test]
    fn queued_command_owner_mid_turn_with_human_origin() {
        let kind = classify_queued_command_content("placeholder mid-turn message", Some("prompt"), Some(&human_origin()), false);
        assert_eq!(kind, IncomingKind::OwnerMidTurn("placeholder mid-turn message".to_string()));
    }

    #[test]
    fn queued_command_peer_with_origin() {
        let origin = peer_origin(r#"{"kind":"peer","from":"a1","senderTaskId":"a1","body":"placeholder handback","handback":true}"#);
        let kind = classify_queued_command_content(
            "<agent-message from=\"a1\">\nplaceholder handback\n</agent-message>",
            Some("prompt"),
            Some(&origin),
            true,
        );
        assert!(matches!(kind, IncomingKind::Peer { handback: true, .. }));
    }

    #[test]
    fn plain_queue_text_is_owner() {
        assert_eq!(
            classify_plain_text("placeholder queued owner text"),
            IncomingKind::Owner("placeholder queued owner text".to_string())
        );
    }

    #[test]
    fn plain_queue_text_task_notification() {
        assert!(matches!(
            classify_plain_text("<task-notification>\n<task-id>t</task-id>\n</task-notification>"),
            IncomingKind::TaskNotification(_)
        ));
    }

    #[test]
    fn plain_queue_text_cross_session_message() {
        assert!(matches!(classify_plain_text("<cross-session-message from=\"x\">body</cross-session-message>"), IncomingKind::Peer { .. }));
    }

    #[test]
    fn plain_queue_text_agent_message() {
        assert!(matches!(classify_plain_text("<agent-message from=\"x\">[Subagent hand-back] body</agent-message>"), IncomingKind::Peer { handback: true, .. }));
    }

    #[test]
    fn plain_queue_text_reminder_plus_owner_text() {
        let kind = classify_plain_text("<system-reminder>\nreminder\n</system-reminder>\n\nplaceholder owner text");
        assert_eq!(kind, IncomingKind::Owner("placeholder owner text".to_string()));
    }

    #[test]
    fn strip_system_reminder_blocks_removes_every_occurrence() {
        let text = "<system-reminder>a</system-reminder>middle<system-reminder>b</system-reminder>tail";
        assert_eq!(strip_system_reminder_blocks(text), "middletail");
    }

    #[test]
    fn unwrap_pasted_content_keeps_inner_text_with_prefix() {
        let text = "before <pasted_content id=\"x\">inner</pasted_content> after";
        assert_eq!(unwrap_pasted_content(text), "before [pasted] inner after");
    }

    #[test]
    fn render_owner_command_formats_name_and_args() {
        assert_eq!(render_owner_command("/model", "placeholder-model"), "/model placeholder-model");
        assert_eq!(render_owner_command("/compact", ""), "/compact");
        assert_eq!(render_owner_command("/compact", "   "), "/compact");
    }
}
