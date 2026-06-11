//! AppleScript helpers — building + executing osascript commands.
//!
//! All user-controlled values must go through [`quote_applescript_string`]
//! before interpolation. Never pass raw user input into the script body.

use crate::error::IMessageError;

/// AppleScript-quote a string value. Escapes backslashes and double-quotes,
/// then wraps in double-quotes. This is the minimal safe quoting for
/// AppleScript string literals.
pub fn quote_applescript_string(s: &str) -> String {
    let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

/// Run an osascript expression and return stdout on success.
/// Returns `IMessageError::AutomationDenied` when stderr indicates TCC denial.
/// Returns `IMessageError::ChatNotFound` when stderr indicates -1728 (cache miss).
pub async fn run_osascript(script: &str, timeout_ms: u64) -> Result<String, IMessageError> {
    use tokio::process::Command;
    use tokio::time::{Duration, timeout};

    let fut = async {
        Command::new("osascript")
            .args(["-e", script])
            .output()
            .await
    };

    let output = timeout(Duration::from_millis(timeout_ms), fut)
        .await
        .map_err(|_| IMessageError::AppleScript {
            exit_code: -1,
            stderr: "osascript timed out".to_string(),
        })?
        .map_err(|e| IMessageError::AppleScript {
            exit_code: -1,
            stderr: e.to_string(),
        })?;

    let exit_code = output.status.code().unwrap_or(-1);
    if exit_code != 0 {
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        if is_automation_denied(&stderr) {
            return Err(IMessageError::AutomationDenied);
        }
        if is_chat_not_found(&stderr) {
            return Err(IMessageError::ChatNotFound);
        }
        return Err(IMessageError::AppleScript { exit_code, stderr });
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn is_automation_denied(stderr: &str) -> bool {
    stderr.contains("not allowed to send Apple events")
        || stderr.contains("-1743")
        || stderr.contains("AppleScript")
}

fn is_chat_not_found(stderr: &str) -> bool {
    stderr.contains("-1728") || stderr.contains("Can't get chat id")
}

/// True when `id` is a chat.db chat GUID rather than a bare 1:1 handle.
///
/// Messages.app / chat.db address conversations two ways:
/// - **Service-prefixed GUID** — `iMessage;+;chat<id>` / `SMS;+;chat<id>` for
///   groups and `iMessage;-;<handle>` / `SMS;-;<handle>` for 1:1s. Any such id
///   contains the `;` service delimiter.
/// - **Legacy bare group form** — `chat<hex>`.
///
/// A bare 1:1 handle (`+15551234567`, `foo@bar.com`) contains neither marker.
/// Both GUID forms are addressed opaquely by `chat id`; only bare handles go
/// through `buddy`. Note this never byte-indexes the string (a fixed offset can
/// fall inside a multi-byte char and panic) and never assumes a hex shape.
fn is_chat_guid(id: &str) -> bool {
    id.contains(';') || id.starts_with("chat")
}

/// Build an osascript to send a plain-text iMessage.
///
/// - Existing conversations (groups and 1:1s) surface as a chat.db chat GUID
///   (`iMessage;+;chat…`, `iMessage;-;<handle>`, or legacy `chat<hex>`) and are
///   addressed opaquely by `chat id` — the service is encoded in the GUID
///   prefix, so SMS/iMessage routing is exact.
/// - A bare phone/email handle (conversation_id fell back to the sender handle
///   when no chat GUID was present) is addressed by `buddy`; `service_name`
///   ("iMessage" / "SMS") then picks the service so green-bubble recipients
///   don't silently queue on the wrong one.
pub fn build_send_script(chat_id: &str, text: &str, service_name: Option<&str>) -> String {
    let quoted_text = quote_applescript_string(text);

    // chat.db chat GUID (group or service-prefixed 1:1): address opaquely.
    if is_chat_guid(chat_id) {
        let quoted_guid = quote_applescript_string(chat_id);
        return format!(
            "tell application \"Messages\"\n  \
               set targetChat to chat id {quoted_guid}\n  \
               send {quoted_text} to targetChat\n\
             end tell"
        );
    }

    // Bare 1:1 handle.
    let quoted_handle = quote_applescript_string(chat_id);
    let use_sms = service_name
        .map(|s| s.to_uppercase() == "SMS")
        .unwrap_or(false);
    let service_type = if use_sms { "SMS" } else { "iMessage" };
    format!(
        "tell application \"Messages\"\n  \
           set targetService to 1st service whose service type = {service_type}\n  \
           set targetBuddy to buddy {quoted_handle} of targetService\n  \
           send {quoted_text} to targetBuddy\n\
         end tell"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quote_escapes_double_quote_and_backslash() {
        assert_eq!(
            quote_applescript_string(r#"say "hello""#),
            r#""say \"hello\"""#
        );
        assert_eq!(
            quote_applescript_string(r"path\to\file"),
            r#""path\\to\\file""#
        );
    }

    #[test]
    fn build_send_script_group_chat() {
        let script = build_send_script("chatdeadbeef", "hello", None);
        assert!(script.contains("chat id"), "should use chat id idiom");
        assert!(script.contains("\"chatdeadbeef\""));
        assert!(script.contains("\"hello\""));
    }

    #[test]
    fn build_send_script_real_group_guid_uses_chat_id() {
        // chat.db surfaces a group's conversation_id as the service-prefixed
        // c.guid (`iMessage;+;chat<digits>`). It must be addressed by `chat id`
        // opaquely, NOT passed to `buddy` (the old hex-shape heuristic dropped
        // it into the buddy branch, producing an unresolvable -1728 send).
        let script = build_send_script("iMessage;+;chat8675309", "hi team", None);
        assert!(script.contains("chat id"), "group guid must use chat id");
        assert!(script.contains("\"iMessage;+;chat8675309\""));
        assert!(
            !script.contains("buddy"),
            "group guid must not fall through to the buddy branch"
        );
    }

    #[test]
    fn build_send_script_service_prefixed_dm_guid_uses_chat_id() {
        // A 1:1 conversation_id can also arrive as a service-prefixed c.guid
        // (`iMessage;-;<handle>`); `buddy "iMessage;-;..."` is unresolvable, so
        // it too is addressed by `chat id`.
        let script = build_send_script("iMessage;-;+15551234567", "yo", None);
        assert!(script.contains("chat id"));
        assert!(script.contains("\"iMessage;-;+15551234567\""));
        assert!(!script.contains("buddy"));
    }

    #[test]
    fn build_send_script_multibyte_guid_does_not_panic() {
        // The old code byte-indexed chat_id[4..]; a 4-byte emoji straddling
        // byte 4 aborts the process. The classifier must never panic on it.
        let script = build_send_script("😀;+;chat1", "hello", None);
        assert!(script.contains("chat id"));
        assert!(script.contains("hello"));
    }

    #[test]
    fn build_send_script_one_to_one_imessage() {
        let script = build_send_script("+15551234567", "hi", Some("iMessage"));
        assert!(script.contains("service type = iMessage"));
        assert!(script.contains("buddy"));
    }

    #[test]
    fn build_send_script_one_to_one_sms() {
        let script = build_send_script("+15551234567", "hi", Some("SMS"));
        assert!(script.contains("service type = SMS"));
    }
}
