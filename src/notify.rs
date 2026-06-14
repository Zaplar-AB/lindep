//! Notification bus: Claude hooks → a local endpoint → [`AppEvent`]s.
//!
//! When an agent wants the human (a permission prompt or an idle nudge), or
//! finishes a turn, or runs a tool, a Claude **hook** fires. We register hooks
//! that POST their JSON to a tiny loopback HTTP endpoint the cockpit hosts; the
//! endpoint maps each hook back to an issue — by `session_id`, falling back to
//! `cwd` (the worktree) — and emits the matching `AppEvent` so the issue's node
//! can light up live.
//!
//! The hook command is `lindep --hook-forward <port>` (this very binary in a
//! one-shot forwarder mode), so there's **no dependency on `curl`** and the
//! forwarder always knows where to POST. Hooks are injected with
//! `claude --settings <file>`, which layers onto the repo's settings rather than
//! overwriting a checked-in `.claude/settings.json`.
//!
//! The endpoint binds `127.0.0.1` only and speaks the minimal slice of HTTP/1.1
//! our own forwarder uses (a `Content-Length` POST); it is not a general server.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Deserialize;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::event::{AppEvent, AppEventTx};
use crate::session::{AgentStatus, SessionStore};

/// Cap on a hook request we'll read, so a misbehaving client can't grow memory
/// unbounded. Hook payloads are a few hundred bytes.
const MAX_BODY: usize = 256 * 1024;

/// Whole-connection budget. The legitimate forwarder posts and closes in
/// milliseconds; anything slower is dropped so it can't leak a task + fd.
const CONN_TIMEOUT: Duration = Duration::from_secs(5);

/// Pause after a transient `accept()` error before retrying, so a persistent
/// failure can't spin a hot loop.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);

/// The subset of a Claude hook's stdin JSON we care about. Every field is
/// optional so an unexpected or evolving payload degrades gracefully rather
/// than failing to parse.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct HookPayload {
    session_id: Option<String>,
    cwd: Option<String>,
    transcript_path: Option<String>,
    hook_event_name: Option<String>,
    notification_type: Option<String>,
    message: Option<String>,
    tool_name: Option<String>,
}

/// Start the hook endpoint on an ephemeral loopback port and return that port
/// (to hand to agents via the hook settings). The accept loop runs as a
/// detached task on the current runtime for the cockpit's lifetime.
pub async fn serve(events: AppEventTx, store: Arc<Mutex<SessionStore>>) -> std::io::Result<u16> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
    let port = listener.local_addr()?.port();
    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _peer)) => {
                    let events = events.clone();
                    let store = Arc::clone(&store);
                    // Bound each connection so a stalled peer can't pin a task +
                    // fd forever; the only legitimate client posts and closes
                    // within milliseconds.
                    tokio::spawn(async move {
                        let _ =
                            tokio::time::timeout(CONN_TIMEOUT, handle_conn(stream, events, store))
                                .await;
                    });
                }
                // accept() errors are usually transient (fd pressure, an aborted
                // connection); a brief pause then retry keeps the only
                // notification path alive instead of silently dying on a hiccup.
                Err(_) => tokio::time::sleep(ACCEPT_BACKOFF).await,
            }
        }
    });
    Ok(port)
}

/// Read one request, route it, and reply `200 OK`.
async fn handle_conn(mut stream: TcpStream, events: AppEventTx, store: Arc<Mutex<SessionStore>>) {
    if let Some(body) = read_request_body(&mut stream).await
        && let Ok(payload) = serde_json::from_slice::<HookPayload>(&body)
    {
        route(&payload, &store, &events);
    }
    // Ack fast and close; the forwarder doesn't care about the body.
    let _ = stream
        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
        .await;
    let _ = stream.shutdown().await;
}

/// Map a hook to an issue and emit the corresponding event. An unmapped session
/// still surfaces a footer notification rather than vanishing.
fn route(payload: &HookPayload, store: &Arc<Mutex<SessionStore>>, events: &AppEventTx) {
    let issue = resolve_issue(payload, store);

    // Remember where this session's transcript lives (kept as a path, never
    // inlined). Persisted lazily on the next status write to avoid per-hook I/O.
    if let (Some(issue), Some(transcript)) = (issue.as_ref(), payload.transcript_path.as_ref())
        && let Ok(mut store) = store.lock()
    {
        store.set_transcript(issue, Some(PathBuf::from(transcript)));
    }

    let event_name = payload.hook_event_name.as_deref().unwrap_or("");
    let event = match (issue, event_name) {
        (Some(issue), "Notification") => {
            // permission_prompt | idle_prompt both mean "the human is needed".
            let reason = payload
                .notification_type
                .clone()
                .or_else(|| payload.message.clone())
                .unwrap_or_else(|| "needs attention".to_string());
            AppEvent::AgentNeedsYou { issue, reason }
        }
        (Some(issue), "Stop") => AppEvent::AgentStatusChanged {
            issue,
            status: AgentStatus::Idle,
        },
        (Some(issue), "PostToolUse") => {
            let action = payload
                .tool_name
                .clone()
                .map_or_else(|| "ran a tool".to_string(), |t| format!("ran {t}"));
            AppEvent::AgentAction { issue, action }
        }
        (Some(issue), other) => AppEvent::AgentAction {
            issue,
            action: format!("hook: {other}"),
        },
        (None, name) => AppEvent::Notification(format!("hook {name:?} from an unmapped session")),
    };
    let _ = events.send(event);
}

/// Resolve a hook to an issue: prefer the durable `session_id`, fall back to the
/// `cwd` (worktree path). Both come straight from Claude's hook payload, so the
/// mapping is correct even with many agents running at once.
fn resolve_issue(payload: &HookPayload, store: &Arc<Mutex<SessionStore>>) -> Option<String> {
    let store = store.lock().ok()?;
    if let Some(sid) = payload.session_id.as_deref()
        && let Some(issue) = store.issue_for_session_id(sid)
    {
        return Some(issue.to_string());
    }
    if let Some(cwd) = payload.cwd.as_deref()
        && let Some(issue) = store.issue_for_cwd(Path::new(cwd))
    {
        return Some(issue.to_string());
    }
    None
}

/// Read an HTTP request body, given the minimal `Content-Length` POST our
/// forwarder sends. Returns the body bytes, or `None` on a malformed/oversized
/// request.
async fn read_request_body(stream: &mut TcpStream) -> Option<Vec<u8>> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        // Find the end of the header block.
        if let Some(header_end) = find(&buf, b"\r\n\r\n") {
            let content_len = content_length(&buf[..header_end])
                .unwrap_or(0)
                .min(MAX_BODY);
            let body_start = header_end + 4;
            while buf.len() < body_start + content_len {
                let n = stream.read(&mut chunk).await.ok()?;
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
                if buf.len() > MAX_BODY {
                    return None;
                }
            }
            let end = (body_start + content_len).min(buf.len());
            return Some(buf[body_start..end].to_vec());
        }
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None; // connection closed before headers completed
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.len() > MAX_BODY {
            return None;
        }
    }
}

/// First index of `needle` in `haystack`.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Parse a `Content-Length` header value (case-insensitive) from the header block.
fn content_length(headers: &[u8]) -> Option<usize> {
    let text = std::str::from_utf8(headers).ok()?;
    for line in text.split("\r\n") {
        if let Some((name, value)) = line.split_once(':')
            && name.trim().eq_ignore_ascii_case("content-length")
        {
            return value.trim().parse().ok();
        }
    }
    None
}

// ── Hook settings generation ─────────────────────────────────────────────────

/// The `settings.json` that registers our forwarder for the events we care
/// about. `exe` is the absolute path to this binary; `port` is the endpoint.
pub fn hook_settings_json(exe: &str, port: u16) -> String {
    // Single-quote the path so spaces don't split the shell command claude runs,
    // escaping any embedded single quote the POSIX way (`'\''`) so an exe path
    // like /home/o'brien/bin/lindep doesn't break the command.
    let escaped = exe.replace('\'', "'\\''");
    let command = format!("'{escaped}' --hook-forward {port}");
    let entry = json!({ "hooks": [{ "type": "command", "command": command }] });
    let matched = json!({ "matcher": "*", "hooks": [{ "type": "command", "command": command }] });
    let settings = json!({
        "hooks": {
            "Notification": [entry],
            "Stop": [entry],
            "PostToolUse": [matched],
        }
    });
    serde_json::to_string_pretty(&settings).unwrap_or_else(|_| "{}".to_string())
}

/// Write the hook settings to `path` (creating parents) and return it, for
/// passing to `claude --settings <path>`.
pub fn write_settings(path: &Path, exe: &str, port: u16) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, hook_settings_json(exe, port))
}

// ── Forwarder (the `--hook-forward` mode) ────────────────────────────────────

/// One-shot forwarder: read Claude's hook JSON from stdin and POST it to the
/// cockpit's endpoint. Always returns `Ok` — a hook must never block or fail the
/// agent, so delivery is best-effort.
pub fn forward(port: u16) -> std::io::Result<()> {
    use std::io::Read;
    // Both the read and the POST are best-effort: the forwarder must always exit
    // 0, since a non-zero hook exit can block the agent's operation.
    let mut body = Vec::new();
    let _ = std::io::stdin().read_to_end(&mut body);
    let _ = post_hook(port, &body);
    Ok(())
}

/// Synchronous loopback POST of `body` to the endpoint. Used by the forwarder
/// and the tests; short timeouts keep a hook from ever hanging.
fn post_hook(port: u16, body: &[u8]) -> std::io::Result<()> {
    use std::io::{Read, Write};
    let addr: SocketAddr = format!("127.0.0.1:{port}")
        .parse()
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "bad loopback addr"))?;
    let mut stream = std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(750))?;
    stream.set_write_timeout(Some(Duration::from_millis(750)))?;
    stream.set_read_timeout(Some(Duration::from_millis(750)))?;
    let head = format!(
        "POST /hook HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()?;
    let mut sink = Vec::new();
    let _ = stream.read_to_end(&mut sink); // drain the 200, ignore timeout
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seeded_store() -> Arc<Mutex<SessionStore>> {
        let path = std::env::temp_dir().join(format!("lindep-notify-{}.json", std::process::id()));
        let mut store = SessionStore::load(&path).unwrap();
        store.ensure("ENG-1", "/wt/ENG-1".into(), "b1".into());
        store.ensure("ENG-2", "/wt/ENG-2".into(), "b2".into());
        Arc::new(Mutex::new(store))
    }

    #[test]
    fn hook_settings_json_registers_the_forwarder() {
        let json = hook_settings_json("/opt/lindep", 8765);
        assert!(json.contains("'/opt/lindep' --hook-forward 8765"));
        assert!(
            json.contains("Notification") && json.contains("Stop") && json.contains("PostToolUse")
        );
    }

    #[test]
    fn content_length_is_parsed_case_insensitively() {
        let headers = b"POST /hook HTTP/1.1\r\nHost: x\r\nCONTENT-LENGTH: 42\r\n";
        assert_eq!(content_length(headers), Some(42));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn hooks_map_to_the_right_issue_across_concurrent_agents() {
        let (tx, mut rx) = crate::event::channel();
        let store = seeded_store();
        let sid1 = SessionStore::session_id_for("ENG-1");
        let sid2 = SessionStore::session_id_for("ENG-2");
        let port = serve(tx, store).await.unwrap();

        // ENG-1 raises a permission prompt; ENG-2 stops. Posted back to back.
        let body1 = json!({
            "session_id": sid1, "cwd": "/wt/ENG-1",
            "hook_event_name": "Notification", "notification_type": "permission_prompt"
        })
        .to_string();
        let body2 = json!({ "session_id": sid2, "cwd": "/wt/ENG-2", "hook_event_name": "Stop" })
            .to_string();
        post_hook(port, body1.as_bytes()).unwrap();
        post_hook(port, body2.as_bytes()).unwrap();

        let mut needs: Option<(String, String)> = None;
        let mut stopped: Option<String> = None;
        for _ in 0..100 {
            while let Ok(ev) = rx.try_recv() {
                match ev {
                    AppEvent::AgentNeedsYou { issue, reason } => needs = Some((issue, reason)),
                    AppEvent::AgentStatusChanged { issue, status } => {
                        assert_eq!(status, AgentStatus::Idle);
                        stopped = Some(issue);
                    }
                    _ => {}
                }
            }
            if needs.is_some() && stopped.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(
            needs,
            Some(("ENG-1".to_string(), "permission_prompt".to_string())),
            "the permission prompt mapped to ENG-1 via its session id"
        );
        assert_eq!(
            stopped,
            Some("ENG-2".to_string()),
            "the Stop mapped to ENG-2"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_unmapped_session_still_surfaces_a_notification() {
        let (tx, mut rx) = crate::event::channel();
        let store = seeded_store();
        let port = serve(tx, store).await.unwrap();
        let body =
            json!({ "session_id": "unknown", "hook_event_name": "Notification" }).to_string();
        post_hook(port, body.as_bytes()).unwrap();

        let mut got = None;
        for _ in 0..100 {
            while let Ok(ev) = rx.try_recv() {
                if let AppEvent::Notification(text) = ev {
                    got = Some(text);
                }
            }
            if got.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(got.unwrap_or_default().contains("unmapped"));
    }
}
