//! Notification bus: Claude hooks → a local endpoint → [`AppEvent`]s.
//!
//! When an agent wants the human (a permission prompt or an idle nudge), or
//! finishes a turn, or runs a tool, a Claude **hook** fires. We register hooks
//! that POST their JSON to a tiny loopback HTTP endpoint the cockpit hosts; the
//! endpoint maps each hook back to an issue — by `session_id`, falling back to
//! `cwd` (the worktree) — and emits the matching `AppEvent` so the issue's node
//! can light up live.
//!
//! The hook command is `lindep --hook-forward <port> --hook-token <token>` (this
//! very binary in a one-shot forwarder mode), so there's **no dependency on
//! `curl`** and the forwarder always knows where to POST. Hooks are injected with
//! `claude --settings <file>`, which layers onto the repo's settings rather than
//! overwriting a checked-in `.claude/settings.json`.
//!
//! The endpoint binds `127.0.0.1` only and speaks the minimal slice of HTTP/1.1
//! our own forwarder uses (a `Content-Length` POST); it is **not** a general
//! server and is deliberately not request-smuggling safe (single trusted client).
//! Even on loopback the port is world-discoverable (it's written to a settings
//! file) and the per-issue session id is a deterministic public UUIDv5, so a
//! per-run bearer **token** ([`TOKEN_HEADER`]) gates every routed hook: a local
//! process that can't read the owner-only settings file can't forge one. Hook
//! text is untrusted (it originates from tool output and the endpoint is locally
//! forgeable), so every hook-derived display string is clamped + control-stripped
//! and a hook-supplied `transcript_path` is accepted only inside the issue's own
//! worktree before it's ever persisted.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use serde::Deserialize;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;
use uuid::Uuid;

use crate::event::{AppEvent, AppEventTx};
use crate::session::{AgentStatus, SessionStore};

/// Cap on a hook request we'll read, so a misbehaving client can't grow memory
/// unbounded. Hook payloads are a few hundred bytes.
const MAX_BODY: usize = 256 * 1024;

/// Clamp on any hook-supplied string we render into the footer. Long enough for
/// a real notification reason, short enough that a forged or runaway message
/// can't bloat the status line.
const MAX_DISPLAY: usize = 200;

/// Whole-connection budget. The legitimate forwarder posts and closes in
/// milliseconds; anything slower is dropped so it can't leak a task + fd.
const CONN_TIMEOUT: Duration = Duration::from_secs(5);

/// Pause after a transient `accept()` error before retrying, so a persistent
/// failure can't spin a hot loop.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);

/// Upper bound on the `accept()` backoff: a persistent listener fault backs off
/// geometrically up to this so the loop never spins, but still recovers quickly
/// once the fault clears.
const ACCEPT_BACKOFF_MAX: Duration = Duration::from_secs(2);

/// How many consecutive `accept()` failures before we surface one footer line,
/// so a wedged listener (notifications silently dead) becomes visible to the
/// operator instead of spinning forever in the dark.
const ACCEPT_FAIL_ALERT_AT: u32 = 5;

/// Ceiling on in-flight connection-handler tasks. The only legitimate client is
/// our own one-shot forwarder, which posts and closes in milliseconds, so a
/// handful of permits is plenty; the cap turns "any local process can open
/// sockets faster than 5s handlers retire" into a constant fd/task bound.
const MAX_INFLIGHT: usize = 32;

/// HTTP header the forwarder stamps with the per-run bearer token. The endpoint
/// is loopback-only but world-discoverable (port lives in a settings file), and
/// the per-issue session id is a deterministic public UUIDv5 — so without this a
/// local process could forge any hook. The token gates every routed request.
const TOKEN_HEADER: &str = "x-lindep-token";

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

/// What [`serve`] hands back: the ephemeral loopback `port` agents POST to, and
/// the per-run bearer `token` the forwarder must present (see [`TOKEN_HEADER`]).
/// Both flow into the generated hook settings so the forwarder subprocess can
/// authenticate; a hook missing/mismatching the token is dropped unrouted.
pub struct Endpoint {
    pub port: u16,
    pub token: String,
}

/// Start the hook endpoint on an ephemeral loopback port, minting a per-run
/// bearer token, and return both (to hand to agents via the hook settings). The
/// accept loop runs as a detached task on the current runtime for the cockpit's
/// lifetime.
pub async fn serve(
    events: AppEventTx,
    store: Arc<Mutex<SessionStore>>,
) -> std::io::Result<Endpoint> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
    let port = listener.local_addr()?.port();
    // An opaque, unguessable per-run secret. v4 UUIDs are CSPRNG-backed
    // (getrandom), so a forger can't predict it even knowing the port/issue.
    let token = Uuid::new_v4().simple().to_string();
    let token_for_loop = token.clone();
    // Cap concurrent handlers so no inbound rate can accumulate live tasks/fds;
    // the permit is held by each spawned handler and released when it returns.
    let permits = Arc::new(Semaphore::new(MAX_INFLIGHT));
    tokio::spawn(async move {
        let token = Arc::new(token_for_loop);
        // Consecutive accept() failures; reset on every success. Used to back off
        // geometrically and to surface one footer line on a sustained fault.
        let mut fails: u32 = 0;
        loop {
            match listener.accept().await {
                Ok((stream, _peer)) => {
                    fails = 0;
                    // No permit free → too many handlers in flight already; drop
                    // this connection rather than unboundedly spawning. The
                    // legitimate forwarder retries are best-effort anyway.
                    let Ok(permit) = Arc::clone(&permits).try_acquire_owned() else {
                        continue;
                    };
                    let events = events.clone();
                    let store = Arc::clone(&store);
                    let token = Arc::clone(&token);
                    // Bound each connection so a stalled peer can't pin a task +
                    // fd forever; the only legitimate client posts and closes
                    // within milliseconds.
                    tokio::spawn(async move {
                        let _ = tokio::time::timeout(
                            CONN_TIMEOUT,
                            handle_conn(stream, events, store, &token),
                        )
                        .await;
                        drop(permit); // release the in-flight slot
                    });
                }
                // accept() errors are usually transient (fd pressure, an aborted
                // connection); back off then retry so the only notification path
                // survives a hiccup. But a *persistent* fault must not spin
                // silently with notifications dead — so escalate the backoff and,
                // once, tell the operator.
                Err(e) => {
                    fails = fails.saturating_add(1);
                    if fails == ACCEPT_FAIL_ALERT_AT {
                        let _ = events.send(AppEvent::Notification(format!(
                            "hook endpoint degraded ({e}); live notifications may be paused"
                        )));
                    }
                    // Geometric backoff capped so a wedged listener idles instead
                    // of retrying ~10x/sec forever.
                    let backoff = (ACCEPT_BACKOFF * 2u32.saturating_pow(fails.min(5)))
                        .min(ACCEPT_BACKOFF_MAX);
                    tokio::time::sleep(backoff).await;
                }
            }
        }
    });
    Ok(Endpoint { port, token })
}

/// Read one request, route it, and reply `200 OK`. Requests missing or
/// mismatching the bearer token are acked but never routed.
async fn handle_conn(
    mut stream: TcpStream,
    events: AppEventTx,
    store: Arc<Mutex<SessionStore>>,
    token: &str,
) {
    if let Some(req) = read_request(&mut stream).await
        // Gate on the bearer token *before* doing anything else, so an
        // unauthenticated/forged request is acked but never routed and can't
        // even trigger a diagnostic line.
        && req.token.as_deref() == Some(token)
    {
        match serde_json::from_slice::<HookPayload>(&req.body) {
            Ok(payload) => route(&payload, &store, &events).await,
            // An authenticated but unparseable hook is dropped silently *unless*
            // it lacked a Content-Length — the likeliest cause of a future
            // forwarder/transport regression (e.g. chunked encoding), which
            // would otherwise make every permission prompt vanish invisibly.
            // Surface that one case so it's diagnosable in the field.
            Err(_) if !req.had_content_length => {
                let _ = events.send(AppEvent::Notification(
                    "hook dropped: no Content-Length (forwarder/transport regression?)".to_string(),
                ));
            }
            Err(_) => {}
        }
    }
    // Ack fast and close; the forwarder doesn't care about the body.
    let _ = stream
        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
        .await;
    let _ = stream.shutdown().await;
}

/// Map a hook to an issue and emit the corresponding event. An unmapped session
/// still surfaces a footer notification rather than vanishing.
async fn route(payload: &HookPayload, store: &Arc<Mutex<SessionStore>>, events: &AppEventTx) {
    let event_name = payload.hook_event_name.as_deref().unwrap_or("");
    // The durable status this hook implies (None = surface-only, e.g. PostToolUse
    // — too frequent to persist, and Running is already set at spawn).
    let implied = match event_name {
        "Notification" => Some(AgentStatus::NeedsYou),
        "Stop" => Some(AgentStatus::Idle),
        _ => None,
    };

    // Resolve the issue, capture the transcript path, and apply the hook-implied
    // status under a *single* lock acquisition, then PERSIST the durable store so
    // it tracks the live fleet: a restart must see NeedsYou/Idle (not the stale
    // Spawning the supervisor last wrote), and the transcript path must survive
    // even if no further status write happens. Snapshot under the lock; the
    // blocking write runs after the guard drops so a rename never stalls another
    // hook on the mutex. A poisoned lock drops the hook rather than panicking.
    let (issue, snapshot) = match store.lock() {
        Ok(mut store) => {
            let issue = resolve_issue(payload, &store);
            let mut dirty = false;

            // Transcript path: kept as a path, never inlined. Accept it only if
            // absolute and under the issue's own worktree — a forged hook must not
            // plant an arbitrary path a viewer would later open as a read
            // primitive. Compute against an immutable borrow, drop it before the
            // mutable write, and only persist when the value actually changes.
            let safe = issue
                .as_deref()
                .zip(payload.transcript_path.as_deref())
                .and_then(|(issue, transcript)| {
                    let worktree = &store.get(issue)?.worktree_path;
                    sanitize_transcript_path(transcript, worktree)
                });
            if let (Some(issue), Some(safe)) = (issue.as_deref(), safe) {
                let changed = store
                    .get(issue)
                    .is_some_and(|s| s.transcript_path.as_deref() != Some(safe.as_path()));
                if changed {
                    store.set_transcript(issue, Some(safe));
                    dirty = true;
                }
            }

            if let (Some(issue), Some(status)) = (issue.as_deref(), implied) {
                let changed = store.get(issue).is_some_and(|s| s.status != status);
                if changed {
                    store.set_status(issue, status);
                    dirty = true;
                }
            }

            // Persist only when this hook actually changed durable state, so the
            // frequent surface-only hooks never touch the disk.
            let snapshot = if dirty {
                store
                    .snapshot_with_seq()
                    .ok()
                    .map(|(b, seq)| (store.path().to_path_buf(), b, seq))
            } else {
                None
            };
            (issue, snapshot)
        }
        Err(_) => return,
    };

    if let Some((path, bytes, seq)) = snapshot {
        crate::session::persist_snapshot(events, path, bytes, seq).await;
    }

    let event = match (issue, event_name) {
        (Some(issue), "Notification") => {
            // permission_prompt | idle_prompt both mean "the human is needed".
            let reason = payload
                .notification_type
                .as_deref()
                .or(payload.message.as_deref())
                .map_or_else(|| "needs attention".to_string(), clamp_display);
            AppEvent::AgentNeedsYou { issue, reason }
        }
        (Some(issue), "Stop") => AppEvent::AgentStatusChanged {
            issue,
            status: AgentStatus::Idle,
        },
        (Some(issue), "PostToolUse") => {
            let action = payload.tool_name.as_deref().map_or_else(
                || "ran a tool".to_string(),
                |t| format!("ran {}", clamp_display(t)),
            );
            AppEvent::AgentAction { issue, action }
        }
        (Some(issue), other) => AppEvent::AgentAction {
            issue,
            action: format!("hook: {}", clamp_display(other)),
        },
        (None, name) => AppEvent::Notification(format!(
            "hook {:?} from an unmapped session",
            clamp_display(name)
        )),
    };
    let _ = events.send(event);
}

/// Resolve a hook to an issue: prefer the durable `session_id`, fall back to the
/// `cwd` (worktree path). Both come straight from Claude's hook payload, so the
/// mapping is correct even with many agents running at once. Takes the live
/// store guard so the caller can do issue resolution and the transcript write in
/// one lock acquisition.
fn resolve_issue(payload: &HookPayload, store: &MutexGuard<'_, SessionStore>) -> Option<String> {
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

/// Validate a hook-supplied transcript path before we durably store it. The path
/// is attacker-influencable (the hook body originates from tool output and the
/// loopback endpoint is locally forgeable), so we accept it only if it is
/// absolute and lexically contained within the issue's own worktree — no `..`
/// escape, no absolute path elsewhere on disk. Lexical (not canonicalizing) so a
/// not-yet-created transcript file still validates and we never touch the FS on
/// the hook path. Returns the path to store, or `None` to reject it.
fn sanitize_transcript_path(transcript: &str, worktree: &Path) -> Option<PathBuf> {
    let path = Path::new(transcript);
    if !path.is_absolute() {
        return None;
    }
    // Reject any `..` component outright rather than trying to resolve it; a
    // purely-lexical containment check is only sound without parent traversal.
    if path
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return None;
    }
    path.starts_with(worktree).then(|| path.to_path_buf())
}

/// Clamp a hook-derived display string for the footer: strip control/escape
/// bytes (the hook body is untrusted, locally-forgeable text) and truncate to a
/// sane length so a runaway or adversarial message can't bloat the status line.
/// Truncates on a char boundary, never mid-codepoint.
fn clamp_display(s: &str) -> String {
    // Drop C0/C1 controls (incl. ESC, CR, LF, TAB) so nothing reaches the
    // terminal that could disturb the footer's single line, and keep at most
    // MAX_DISPLAY chars (truncating on a char boundary, never mid-codepoint).
    s.chars()
        .filter(|ch| !ch.is_control())
        .take(MAX_DISPLAY)
        .collect()
}

/// A parsed hook request: the body bytes, the bearer token the forwarder
/// presented (if any), and whether a usable `Content-Length` framed it. The flag
/// lets the caller tell a deliberately-empty body apart from a forwarder that
/// sent no length (a silent-drop that's otherwise invisible to diagnose).
struct ParsedRequest {
    body: Vec<u8>,
    token: Option<String>,
    had_content_length: bool,
}

/// Read one HTTP request, given the minimal `Content-Length` POST our forwarder
/// sends. Returns the body + headers we care about, or `None` on a
/// malformed/oversized/closed-early request. Not a general HTTP parser: it
/// handles single-CRLF `Content-Length` framing only and is not request-smuggling
/// safe — fine because the only client is our own loopback forwarder.
async fn read_request(stream: &mut TcpStream) -> Option<ParsedRequest> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    // Only rescan the freshly-appended tail (minus a 3-byte overlap so a
    // terminator split across two reads is still found) instead of the whole
    // growing buffer each iteration — avoids the O(n²) rescan on a dribbling peer.
    let mut scan_from = 0usize;
    loop {
        // Find the end of the header block.
        if let Some(rel) = find(&buf[scan_from..], b"\r\n\r\n") {
            let header_end = scan_from + rel;
            let parsed_len = content_length(&buf[..header_end]);
            let token = token_header(&buf[..header_end]);
            let content_len = parsed_len.unwrap_or(0).min(MAX_BODY);
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
            // Drain the headers and trim to the body in place — no second
            // allocation/copy of a buffer we already own.
            let end = (body_start + content_len).min(buf.len());
            buf.truncate(end);
            buf.drain(..body_start);
            return Some(ParsedRequest {
                body: buf,
                token,
                had_content_length: parsed_len.is_some(),
            });
        }
        // Advance the scan cursor to just before the unsearched tail.
        scan_from = buf.len().saturating_sub(3);
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
    header_value(headers, "content-length").and_then(|v| v.parse().ok())
}

/// Parse our bearer-token header (case-insensitive) from the header block.
fn token_header(headers: &[u8]) -> Option<String> {
    header_value(headers, TOKEN_HEADER).map(str::to_string)
}

/// First value of header `name` (case-insensitive) in a CRLF-delimited block.
/// Deliberately minimal: returns the first match and doesn't reject duplicates —
/// loopback single-client, so request smuggling has nowhere to land.
fn header_value<'a>(headers: &'a [u8], name: &str) -> Option<&'a str> {
    let text = std::str::from_utf8(headers).ok()?;
    for line in text.split("\r\n") {
        if let Some((hname, value)) = line.split_once(':')
            && hname.trim().eq_ignore_ascii_case(name)
        {
            return Some(value.trim());
        }
    }
    None
}

// ── Hook settings generation ─────────────────────────────────────────────────

/// The `settings.json` that registers our forwarder for the events we care
/// about. `exe` is the absolute path to this binary; `port` is the endpoint;
/// `token` is the per-run bearer secret the forwarder must present so a local
/// process can't forge hooks (see [`Endpoint`]).
pub fn hook_settings_json(exe: &str, port: u16, token: &str) -> String {
    // Single-quote the path so spaces don't split the shell command claude runs,
    // escaping any embedded single quote the POSIX way (`'\''`) so an exe path
    // like /home/o'brien/bin/lindep doesn't break the command. The token is our
    // own hex UUID (no shell metacharacters) but is quoted on the same principle.
    let escaped = exe.replace('\'', "'\\''");
    let escaped_token = token.replace('\'', "'\\''");
    let command = format!("'{escaped}' --hook-forward {port} --hook-token '{escaped_token}'");
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
/// passing to `claude --settings <path>`. The file carries the per-run bearer
/// token, so on Unix it's created `0o600` (owner-only) — the endpoint port is no
/// longer enough to forge a hook, and the token shouldn't be world-readable.
pub fn write_settings(path: &Path, exe: &str, port: u16, token: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let contents = hook_settings_json(exe, port, token);
    write_owner_only(path, contents.as_bytes())
}

/// Write `contents` to `path`, truncating, with owner-only permissions on Unix.
/// `OpenOptions::mode` only applies on *create*, so we also `set_permissions`
/// when the file already existed, ensuring an old `0o644` settings file from a
/// pre-token build is tightened on the next launch.
fn write_owner_only(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(contents)
}

// ── Forwarder (the `--hook-forward` mode) ────────────────────────────────────

/// One-shot forwarder: read Claude's hook JSON from stdin and POST it to the
/// cockpit's endpoint, presenting the per-run bearer `token`. Always returns
/// `Ok` — a hook must never block or fail the agent, so delivery is best-effort.
pub fn forward(port: u16, token: &str) -> std::io::Result<()> {
    use std::io::Read;
    // Both the read and the POST are best-effort: the forwarder must always exit
    // 0, since a non-zero hook exit can block the agent's operation.
    let mut body = Vec::new();
    // If stdin can't be read in full we have no real payload — skip the POST
    // rather than forwarding a truncated/empty body that the endpoint would just
    // fail to parse and drop, making a malformed hook indistinguishable from a
    // real one.
    if std::io::stdin().read_to_end(&mut body).is_ok() {
        let _ = post_hook(port, token, &body);
    }
    Ok(())
}

/// Synchronous loopback POST of `body` to the endpoint, with the bearer `token`.
/// Used by the forwarder and the tests; short timeouts keep a hook from ever
/// hanging.
fn post_hook(port: u16, token: &str, body: &[u8]) -> std::io::Result<()> {
    use std::io::{Read, Write};
    let addr: SocketAddr = format!("127.0.0.1:{port}")
        .parse()
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "bad loopback addr"))?;
    let mut stream = std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(750))?;
    stream.set_write_timeout(Some(Duration::from_millis(750)))?;
    stream.set_read_timeout(Some(Duration::from_millis(750)))?;
    let head = format!(
        "POST /hook HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\n\
         {TOKEN_HEADER}: {token}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
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

    /// Drain every queued event into a vec, polling briefly so the off-thread
    /// handler has a chance to route before we look.
    async fn drain(rx: &mut crate::event::AppEventRx) -> Vec<AppEvent> {
        let mut out = Vec::new();
        for _ in 0..50 {
            while let Ok(ev) = rx.try_recv() {
                out.push(ev);
            }
            if !out.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        // One more sweep for anything that landed during the last sleep.
        while let Ok(ev) = rx.try_recv() {
            out.push(ev);
        }
        out
    }

    /// Raw loopback POST: lets a test send arbitrary bytes (a bogus token,
    /// junk body, missing Content-Length) the typed `post_hook` won't produce.
    /// Writes are best-effort — on an oversized body the server stops reading and
    /// closes early, which can break the pipe before we finish writing; that's
    /// the behaviour under test, not a test failure, so write errors are ignored.
    fn raw_post(port: u16, request: &[u8]) {
        use std::io::{Read, Write};
        let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        let mut stream =
            std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(750)).unwrap();
        let _ = stream.set_write_timeout(Some(Duration::from_millis(750)));
        let _ = stream.set_read_timeout(Some(Duration::from_millis(750)));
        let _ = stream.write_all(request);
        let _ = stream.flush();
        let mut sink = Vec::new();
        let _ = stream.read_to_end(&mut sink);
    }

    #[test]
    fn hook_settings_json_registers_the_forwarder() {
        let json = hook_settings_json("/opt/lindep", 8765, "secrettoken");
        assert!(json.contains("'/opt/lindep' --hook-forward 8765 --hook-token 'secrettoken'"));
        assert!(
            json.contains("Notification") && json.contains("Stop") && json.contains("PostToolUse")
        );
    }

    #[test]
    fn content_length_is_parsed_case_insensitively() {
        let headers = b"POST /hook HTTP/1.1\r\nHost: x\r\nCONTENT-LENGTH: 42\r\n";
        assert_eq!(content_length(headers), Some(42));
    }

    #[test]
    fn token_header_is_parsed_case_insensitively() {
        let headers = b"POST /hook HTTP/1.1\r\nHost: x\r\nX-Lindep-Token: abc123\r\n";
        assert_eq!(token_header(headers).as_deref(), Some("abc123"));
    }

    #[test]
    fn clamp_display_strips_controls_and_truncates() {
        // Embedded ESC / CR / LF are dropped, the rest preserved in order.
        assert_eq!(clamp_display("ok\x1b[2J\rhi\n!"), "ok[2Jhi!");
        // Over-long input is cut to MAX_DISPLAY chars.
        let long = "x".repeat(MAX_DISPLAY * 3);
        assert_eq!(clamp_display(&long).chars().count(), MAX_DISPLAY);
        // Multi-byte chars are never split mid-codepoint.
        let wide = "é".repeat(MAX_DISPLAY + 10);
        let clamped = clamp_display(&wide);
        assert_eq!(clamped.chars().count(), MAX_DISPLAY);
        assert!(clamped.is_char_boundary(clamped.len()));
    }

    #[test]
    fn transcript_path_must_be_absolute_and_inside_the_worktree() {
        let wt = Path::new("/wt/ENG-1");
        // Inside the worktree → accepted verbatim.
        assert_eq!(
            sanitize_transcript_path("/wt/ENG-1/transcripts/a.ndjson", wt),
            Some(PathBuf::from("/wt/ENG-1/transcripts/a.ndjson"))
        );
        // Absolute but elsewhere on disk → rejected (the stored-path-injection).
        assert_eq!(sanitize_transcript_path("/etc/passwd", wt), None);
        // A sibling worktree that merely shares a prefix → rejected.
        assert_eq!(
            sanitize_transcript_path("/wt/ENG-12/secret.ndjson", wt),
            None
        );
        // Relative path → rejected.
        assert_eq!(sanitize_transcript_path("transcripts/a.ndjson", wt), None);
        // `..` traversal back out of the worktree → rejected.
        assert_eq!(
            sanitize_transcript_path("/wt/ENG-1/../ENG-2/x.ndjson", wt),
            None
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn hooks_map_to_the_right_issue_across_concurrent_agents() {
        let (tx, mut rx) = crate::event::channel();
        let store = seeded_store();
        let sid1 = SessionStore::session_id_for("ENG-1");
        let sid2 = SessionStore::session_id_for("ENG-2");
        let Endpoint { port, token } = serve(tx, store).await.unwrap();

        // ENG-1 raises a permission prompt; ENG-2 stops. Posted back to back.
        let body1 = json!({
            "session_id": sid1, "cwd": "/wt/ENG-1",
            "hook_event_name": "Notification", "notification_type": "permission_prompt"
        })
        .to_string();
        let body2 = json!({ "session_id": sid2, "cwd": "/wt/ENG-2", "hook_event_name": "Stop" })
            .to_string();
        post_hook(port, &token, body1.as_bytes()).unwrap();
        post_hook(port, &token, body2.as_bytes()).unwrap();

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
        let Endpoint { port, token } = serve(tx, store).await.unwrap();
        let body =
            json!({ "session_id": "unknown", "hook_event_name": "Notification" }).to_string();
        post_hook(port, &token, body.as_bytes()).unwrap();

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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_hook_without_a_session_id_routes_by_cwd_worktree() {
        // The documented fallback in `resolve_issue`: when a hook carries no
        // usable `session_id`, the `cwd` (the agent's worktree) maps it to an
        // issue. Every other test wins on `session_id`, so this is the only thing
        // exercising the cwd arm end to end.
        let (tx, mut rx) = crate::event::channel();
        let store = seeded_store();
        let Endpoint { port, token } = serve(tx, store).await.unwrap();

        // No `session_id` at all → resolution must fall back to cwd = /wt/ENG-2.
        let body = json!({
            "cwd": "/wt/ENG-2",
            "hook_event_name": "Notification", "notification_type": "permission_prompt"
        })
        .to_string();
        post_hook(port, &token, body.as_bytes()).unwrap();
        let got = drain(&mut rx).await;
        assert!(
            got.iter().any(|ev| matches!(
                ev,
                AppEvent::AgentNeedsYou { issue, .. } if issue == "ENG-2"
            )),
            "a hook with only a cwd routes via the worktree fallback; got {got:?}"
        );

        // A cwd matching no worktree (and still no session_id) resolves to the
        // unmapped-session notification — the negative arm of the same fallback.
        let unmapped = json!({ "cwd": "/nope", "hook_event_name": "Notification" }).to_string();
        post_hook(port, &token, unmapped.as_bytes()).unwrap();
        let got = drain(&mut rx).await;
        assert!(
            got.iter()
                .any(|ev| matches!(ev, AppEvent::Notification(t) if t.contains("unmapped"))),
            "an unmatched cwd surfaces the unmapped-session notification; got {got:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_hook_with_a_bad_or_missing_token_is_dropped() {
        let (tx, mut rx) = crate::event::channel();
        let store = seeded_store();
        let sid = SessionStore::session_id_for("ENG-1");
        let Endpoint { port, token } = serve(tx, store).await.unwrap();
        let body = json!({
            "session_id": sid, "hook_event_name": "Notification",
            "notification_type": "permission_prompt"
        })
        .to_string();

        // Wrong token → no event routed.
        post_hook(port, "not-the-token", body.as_bytes()).unwrap();
        // No token header at all → also dropped.
        let no_token = format!(
            "POST /hook HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: {}\r\n\
             Connection: close\r\n\r\n{body}",
            body.len()
        );
        raw_post(port, no_token.as_bytes());
        assert!(
            drain(&mut rx).await.is_empty(),
            "forged hooks (wrong/missing token) must not route any event"
        );

        // The correct token still works → liveness after rejected requests.
        post_hook(port, &token, body.as_bytes()).unwrap();
        let got = drain(&mut rx).await;
        assert!(
            got.iter().any(|ev| matches!(
                ev,
                AppEvent::AgentNeedsYou { issue, .. } if issue == "ENG-1"
            )),
            "an authenticated hook still routes after forged ones were dropped"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn malformed_bodies_are_dropped_and_the_server_stays_live() {
        let (tx, mut rx) = crate::event::channel();
        let store = seeded_store();
        let sid = SessionStore::session_id_for("ENG-1");
        let Endpoint { port, token } = serve(tx, store).await.unwrap();

        // (1) Authenticated but non-JSON body → no AgentNeedsYou; server lives.
        post_hook(port, &token, b"not json at all").unwrap();
        // (2) Oversized body (> MAX_BODY) → read_request returns None, dropped
        //     without growth or panic. Frame it honestly so the endpoint actually
        //     tries to read the whole thing.
        let big = vec![b'x'; MAX_BODY + 4096];
        let oversized = {
            let mut req = format!(
                "POST /hook HTTP/1.1\r\nHost: 127.0.0.1\r\n{TOKEN_HEADER}: {token}\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n",
                big.len()
            )
            .into_bytes();
            req.extend_from_slice(&big);
            req
        };
        raw_post(port, &oversized);
        let after_junk = drain(&mut rx).await;
        assert!(
            !after_junk
                .iter()
                .any(|ev| matches!(ev, AppEvent::AgentNeedsYou { .. })),
            "junk/oversized bodies must not route a needs-you event"
        );

        // (3) Liveness: a valid POST after the junk still routes.
        let body = json!({
            "session_id": sid, "hook_event_name": "Notification",
            "notification_type": "permission_prompt"
        })
        .to_string();
        post_hook(port, &token, body.as_bytes()).unwrap();
        let got = drain(&mut rx).await;
        assert!(
            got.iter().any(|ev| matches!(
                ev,
                AppEvent::AgentNeedsYou { issue, .. } if issue == "ENG-1"
            )),
            "the endpoint still answers a valid POST after malformed ones"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn posttooluse_maps_to_an_action_with_the_tool_name() {
        let (tx, mut rx) = crate::event::channel();
        let store = seeded_store();
        let sid = SessionStore::session_id_for("ENG-1");
        let Endpoint { port, token } = serve(tx, store).await.unwrap();

        // With a tool_name → "ran <tool>".
        let with_tool = json!({
            "session_id": sid, "hook_event_name": "PostToolUse", "tool_name": "Edit"
        })
        .to_string();
        post_hook(port, &token, with_tool.as_bytes()).unwrap();
        let got = drain(&mut rx).await;
        assert!(
            got.iter().any(|ev| matches!(
                ev,
                AppEvent::AgentAction { issue, action } if issue == "ENG-1" && action == "ran Edit"
            )),
            "PostToolUse with a tool_name maps to 'ran <tool>'; got {got:?}"
        );

        // Without a tool_name → the "ran a tool" fallback.
        let no_tool = json!({ "session_id": sid, "hook_event_name": "PostToolUse" }).to_string();
        post_hook(port, &token, no_tool.as_bytes()).unwrap();
        let got = drain(&mut rx).await;
        assert!(
            got.iter().any(|ev| matches!(
                ev,
                AppEvent::AgentAction { action, .. } if action == "ran a tool"
            )),
            "PostToolUse without a tool_name falls back to 'ran a tool'; got {got:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_transcript_path_inside_the_worktree_is_persisted_lazily() {
        let (tx, mut rx) = crate::event::channel();
        let store = seeded_store();
        let sid = SessionStore::session_id_for("ENG-1");
        let Endpoint { port, token } = serve(tx, Arc::clone(&store)).await.unwrap();

        // A PostToolUse carrying a transcript path *inside* ENG-1's worktree.
        let inside = json!({
            "session_id": sid, "hook_event_name": "PostToolUse", "tool_name": "Read",
            "transcript_path": "/wt/ENG-1/transcripts/eng-1.ndjson"
        })
        .to_string();
        post_hook(port, &token, inside.as_bytes()).unwrap();
        let _ = drain(&mut rx).await; // let the handler run

        // set_transcript is in-memory until a status write; assert it landed on
        // the live record (the lazy-persist contract).
        {
            let store = store.lock().unwrap();
            assert_eq!(
                store.get("ENG-1").and_then(|s| s.transcript_path.clone()),
                Some(PathBuf::from("/wt/ENG-1/transcripts/eng-1.ndjson")),
                "an in-worktree transcript path is captured on the record"
            );
        }

        // A forged path *outside* the worktree must be rejected, leaving the
        // previously-captured good path untouched.
        let outside = json!({
            "session_id": sid, "hook_event_name": "PostToolUse", "tool_name": "Read",
            "transcript_path": "/etc/shadow"
        })
        .to_string();
        post_hook(port, &token, outside.as_bytes()).unwrap();
        let _ = drain(&mut rx).await;
        {
            let store = store.lock().unwrap();
            assert_eq!(
                store.get("ENG-1").and_then(|s| s.transcript_path.clone()),
                Some(PathBuf::from("/wt/ENG-1/transcripts/eng-1.ndjson")),
                "a forged out-of-worktree transcript path is rejected, not stored"
            );
        }
    }
}
