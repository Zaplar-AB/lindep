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
use crate::workspace::StoreRegistry;

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
    /// v1.6 auto-push / lazy-pull: which repo in the per-issue workspace a
    /// `post-commit` or `request-repo` event concerns (the post-commit hook and
    /// the `request-repo` forwarder set it; Claude's own hooks don't).
    repo_handle: Option<String>,
    /// v1.6 auto-push: the committed branch carried by an `AgentCommitted` event.
    branch: Option<String>,
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
pub async fn serve(events: AppEventTx, stores: StoreRegistry) -> std::io::Result<Endpoint> {
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
                    let stores = stores.clone();
                    let token = Arc::clone(&token);
                    // Bound each connection so a stalled peer can't pin a task +
                    // fd forever; the only legitimate client posts and closes
                    // within milliseconds.
                    tokio::spawn(async move {
                        let _ = tokio::time::timeout(
                            CONN_TIMEOUT,
                            handle_conn(stream, events, stores, &token),
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
    stores: StoreRegistry,
    token: &str,
) {
    if let Some(req) = read_request(&mut stream).await
        // Gate on the bearer token *before* doing anything else, so an
        // unauthenticated/forged request is acked but never routed and can't
        // even trigger a diagnostic line.
        && req.token.as_deref() == Some(token)
    {
        match serde_json::from_slice::<HookPayload>(&req.body) {
            Ok(payload) => route(&payload, &stores, &events).await,
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

/// Map a hook to its `(project_id, issue)` and emit the corresponding event. The
/// one shared loopback endpoint serves the whole workspace: a hook carries only a
/// `session_id`/`cwd`, so we scan every started project's store to find the owner
/// (a hook-supplied project id would be loopback-forgeable, so it's never
/// trusted). An unmapped session still surfaces a footer rather than vanishing.
async fn route(payload: &HookPayload, stores: &StoreRegistry, events: &AppEventTx) {
    let event_name = payload.hook_event_name.as_deref().unwrap_or("");

    // Find the project whose store owns this hook. Snapshot the (project_id, store)
    // candidates under the registry lock, release it, then scan each store — so the
    // registry lock is never held across an inner store lock (no nested-lock order
    // to deadlock on). The per-hook linear scan is cheap at expected agent counts;
    // a session_id index would be premature.
    let candidates: Vec<(String, Arc<Mutex<SessionStore>>)> = match stores.lock() {
        Ok(reg) => reg
            .iter()
            .map(|(k, v)| (k.clone(), Arc::clone(v)))
            .collect(),
        Err(_) => return,
    };
    let n_projects = candidates.len();
    // Resolve the owning issue ONCE here and carry it forward, rather than testing
    // `is_some()` now and re-resolving under the second lock below: between the two
    // locks a concurrent teardown (`forget`) could remove the record, so the re-resolve
    // would return None and mislabel a real hook as "unmapped" even though we just
    // identified its project. (Also halves the per-hook resolution work.)
    let resolved = candidates.into_iter().find_map(|(project_id, store)| {
        let issue = store.lock().ok().and_then(|s| resolve_issue(payload, &s))?;
        Some((project_id, store, issue))
    });
    let Some((project_id, store, resolved_issue)) = resolved else {
        // No project's store recognizes this session — surface one footer line
        // rather than dropping it silently (a forwarder/transport regression would
        // otherwise make every prompt vanish).
        let _ = events.send(AppEvent::Notification(format!(
            "hook {:?} from a session unmapped across {n_projects} project(s)",
            clamp_display(event_name)
        )));
        return;
    };

    // Resolve the issue, capture the transcript path, and apply the hook-implied
    // status under a *single* lock acquisition on the owning project's store, then
    // PERSIST it so the durable state tracks the live fleet: a restart must see
    // NeedsYou/Idle (not the stale Spawning the supervisor last wrote), and the
    // transcript path must survive even if no further status write happens.
    // Snapshot under the lock; the blocking write runs after the guard drops so a
    // rename never stalls another hook. A poisoned lock drops the hook.
    let (issue, snapshot, notif_needs_you, working) = match store.lock() {
        Ok(mut store) => {
            // The issue resolved during owner identification above — reused rather than
            // re-resolved, so a teardown racing between the two locks can't turn a
            // recognised hook into an "unmapped" one.
            let issue = Some(resolved_issue);
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

            // The agent's status before this hook, read once (Copy, so the borrow
            // ends immediately and can't clash with the set_status below).
            let current = issue
                .as_deref()
                .and_then(|i| store.get(i).map(|s| s.status));

            // A `Notification` only means "needs you" for a genuine permission /
            // elicitation prompt; the idle nudge, `auth_success` and the
            // elicitation-complete events Claude also delivers through this one
            // hook must NOT raise the flag (see `notification_implies_needs_you`).
            let notif_needs_you =
                event_name == "Notification" && notification_implies_needs_you(payload, current);

            // Whether this hook is an unambiguous sign the agent is *actively
            // working again*. `UserPromptSubmit` (the user just sent a turn) and
            // `PostToolUse` (a tool ran) are the two; an MCP elicitation that
            // *completes* also means a block was answered. A working signal
            // promotes any live state — including `NeedsYou` — back to `Running`,
            // which is what finally resolves a prompt once the user answers and
            // the agent resumes. (Without this, only the *next* `Stop` cleared
            // `NeedsYou`, so an answered, busy agent stayed flagged for its whole
            // turn — the "needs-you never resolves once you answer" bug.)
            let working = matches!(event_name, "UserPromptSubmit" | "PostToolUse")
                || (event_name == "Notification"
                    && matches!(
                        payload.notification_type.as_deref(),
                        Some("elicitation_complete" | "elicitation_response")
                    ));

            // The durable status this hook implies (None = surface-only).
            let implied = match event_name {
                "Notification" if notif_needs_you => Some(AgentStatus::NeedsYou),
                "Stop" => Some(AgentStatus::Idle),
                // A working signal promotes a live, non-terminal agent to Running.
                // Spawning/NeedsYou/Running all promote (a turn began, or it resumed
                // after you answered). An *Idle* agent is only revived by a genuine
                // new turn — `UserPromptSubmit` — never by a mid-turn `PostToolUse`,
                // so a late or out-of-order tool hook can't flip a settled agent back
                // to Running (A4). Mirrors the runtime rule (UserPromptSubmit routed
                // as a status change; PostToolUse promotes non-idle only). Never
                // revive a terminal agent; a no-op when already Running is absorbed by
                // the `changed` guard.
                _ if working => match current {
                    Some(AgentStatus::Idle) if event_name == "UserPromptSubmit" => {
                        Some(AgentStatus::Running)
                    }
                    Some(
                        AgentStatus::Spawning | AgentStatus::NeedsYou | AgentStatus::Running,
                    ) => Some(AgentStatus::Running),
                    _ => None,
                },
                _ => None,
            };
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
            (issue, snapshot, notif_needs_you, working)
        }
        Err(_) => return,
    };

    if let Some((path, bytes, seq)) = snapshot {
        crate::session::persist_snapshot(events, path, bytes, seq).await;
    }

    // v1.6 auto-push: a `post-commit` git hook fires `AgentCommitted`. Push the
    // committed branch to the repo's true remote OFF the status machinery (a commit
    // is never "needs you"), serialized per repo handle, on the runtime — so the
    // render loop never runs git. The commit's own `cwd` is the specific repo
    // worktree to push from (correct for a multi-repo issue).
    if event_name == "AgentCommitted" {
        if let (Some(issue), Some(cwd)) = (issue.clone(), payload.cwd.clone()) {
            spawn_auto_push(
                events.clone(),
                project_id.clone(),
                issue,
                payload.repo_handle.clone().unwrap_or_default(),
                payload.branch.clone().unwrap_or_default(),
                PathBuf::from(cwd),
            );
        }
        return;
    }

    // v1.6 fenced lazy-pull (ENG-542): the agent ran `lindep request-repo <handle>`.
    // The handle was already fenced to the project's candidate set by the CLI (which
    // exits non-zero out-of-set); the cockpit raises a confirmation modal and, on
    // confirm, re-fences and materialises it. Bypasses the status machinery (a repo
    // request is never "needs you"), exactly like AgentCommitted above.
    if event_name == "RepoRequested" {
        if let (Some(issue), Some(handle)) = (issue.clone(), payload.repo_handle.clone()) {
            let _ = events.send(AppEvent::RepoRequested {
                project_id,
                issue,
                repo_handle: handle,
            });
        }
        return;
    }

    let event = match (issue, event_name) {
        (Some(issue), "Notification") if notif_needs_you => {
            // A real permission / elicitation prompt: raise the flag.
            let reason = payload
                .notification_type
                .as_deref()
                .or(payload.message.as_deref())
                .map_or_else(|| "needs attention".to_string(), clamp_display);
            AppEvent::AgentNeedsYou {
                project_id,
                issue,
                reason,
            }
        }
        // The user submitted a turn: a genuine new-turn status change → Running. It
        // revives even an *Idle* agent (a fresh turn started) and clears a standing
        // NeedsYou (answering a question is precisely a prompt submit). Routed as a
        // status change — not an AgentAction — so that a *mid-turn* PostToolUse can't
        // also revive Idle: only a real new turn does (A4).
        (Some(issue), "UserPromptSubmit") => AppEvent::AgentStatusChanged {
            project_id,
            issue,
            status: AgentStatus::Running,
        },
        (Some(issue), "Notification") => {
            // A non-blocking notification (idle nudge / auth-success /
            // elicitation-complete): surface it quietly. `working` is true only
            // for an elicitation-complete (a block was answered → resumes), so an
            // idle nudge never disturbs a needs-you agent's status.
            let note = payload
                .notification_type
                .as_deref()
                .or(payload.message.as_deref())
                .map_or_else(|| "agent idle".to_string(), clamp_display);
            AppEvent::AgentAction {
                project_id,
                issue,
                action: note,
                working,
            }
        }
        (Some(issue), "Stop") => AppEvent::AgentStatusChanged {
            project_id,
            issue,
            status: AgentStatus::Idle,
        },
        (Some(issue), "PostToolUse") => {
            let action = payload.tool_name.as_deref().map_or_else(
                || "ran a tool".to_string(),
                |t| format!("ran {}", clamp_display(t)),
            );
            AppEvent::AgentAction {
                project_id,
                issue,
                action,
                working: true,
            }
        }
        (Some(issue), other) => AppEvent::AgentAction {
            project_id,
            issue,
            action: format!("hook: {}", clamp_display(other)),
            working: false,
        },
        (None, name) => AppEvent::Notification(format!(
            "hook {:?} from an unmapped session",
            clamp_display(name)
        )),
    };
    let _ = events.send(event);
}

/// Whether a `Notification` hook genuinely needs the human. Claude fires
/// `Notification` for several distinct reasons under one event name: a permission
/// prompt and an MCP `elicitation_dialog` truly block on you; the idle nudge
/// (`idle_prompt`, ~60 s after a turn), `auth_success`, and the
/// `elicitation_complete` / `elicitation_response` follow-ups do not — flagging
/// those is the "needs-you fires when it doesn't need you" bug.
///
/// Discriminate on `notification_type` when Claude supplies it (the docs don't
/// guarantee the field, so treat its absence gracefully); otherwise fall back to
/// the agent's state. A permission prompt arrives **mid-turn** (the agent is
/// still active — Spawning/Running), whereas the idle nudge only fires once the
/// agent has already gone `Idle` after its `Stop` hook. An unmapped/None state
/// stays conservative (treat as needing you) so a real block is never dropped.
fn notification_implies_needs_you(payload: &HookPayload, current: Option<AgentStatus>) -> bool {
    match payload.notification_type.as_deref() {
        Some("permission_prompt" | "elicitation_dialog") => true,
        Some("idle_prompt" | "auth_success" | "elicitation_complete" | "elicitation_response") => {
            false
        }
        _ => current.is_none_or(|s| s.is_animating()),
    }
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

/// Whether `c` is a Unicode bidirectional or format control that could reorder
/// displayed text without being a C0/C1 control. Covers LRM/RLM/ALM, the embedding/
/// override pair (LRE…RLO), and the directional isolates (LRI…PDI) — the Trojan-Source
/// character set. These are category Cf, which [`char::is_control`] does not match.
fn is_bidi_or_format(c: char) -> bool {
    matches!(c,
        '\u{200E}' | '\u{200F}' | '\u{061C}'
        | '\u{202A}'..='\u{202E}'
        | '\u{2066}'..='\u{2069}')
}

/// Clamp a hook-derived display string for the footer: strip control/escape
/// bytes (the hook body is untrusted, locally-forgeable text) and truncate to a
/// sane length so a runaway or adversarial message can't bloat the status line.
/// Truncates on a char boundary, never mid-codepoint.
fn clamp_display(s: &str) -> String {
    // Drop C0/C1 controls (incl. ESC, CR, LF, TAB) AND the Unicode bidi/format
    // controls (RLO/LRO, the directional isolates, LRM/RLM) — `char::is_control`
    // only covers category Cc, so without this a Trojan-Source-style override could
    // reorder the displayed footer even though the single-line invariant holds. Keep
    // at most MAX_DISPLAY chars (truncating on a char boundary, never mid-codepoint).
    s.chars()
        .filter(|ch| !ch.is_control() && !is_bidi_or_format(*ch))
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
    // `UserPromptSubmit` is the leading "the agent is now working" signal: it
    // fires the instant the user sends a turn, *before* any tool runs — so a
    // turn that thinks/streams text (or answers in pure prose) reads WORKING
    // immediately instead of lingering on the trailing first-PostToolUse. It is
    // also the clean "the user answered" signal that clears a standing NeedsYou.
    let settings = json!({
        "hooks": {
            "UserPromptSubmit": [entry],
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

// ── v1.6 auto-push (`--post-commit`) ─────────────────────────────────────────

/// One-shot **post-commit** forwarder (the `--post-commit` mode). git gives a
/// post-commit hook no stdin, so synthesize the `AgentCommitted` payload from the
/// worktree (the hook's cwd) and its current branch, and POST it presenting the
/// per-run bearer `token`. Always `Ok` — a git hook must never block the commit.
pub fn forward_post_commit(port: u16, token: &str, repo_handle: &str) -> std::io::Result<()> {
    let cwd = std::env::current_dir().unwrap_or_default();
    let branch = current_branch(&cwd).unwrap_or_default();
    let payload = json!({
        "cwd": cwd.to_string_lossy(),
        "hook_event_name": "AgentCommitted",
        "repo_handle": repo_handle,
        "branch": branch,
    });
    if let Ok(body) = serde_json::to_vec(&payload) {
        let _ = post_hook(port, token, &body);
    }
    Ok(())
}

/// One-shot **request-repo** forwarder (the `--request-repo` mode, ENG-542). The
/// agent runs `lindep request-repo <handle>` inside its workspace; this synthesizes
/// a `RepoRequested` payload (the `cwd` resolves the issue, even from a repo subdir)
/// and POSTs it presenting the per-run bearer `token`. Always `Ok` — like every
/// forwarder, it must never block the agent; the candidate fence + non-zero exit for
/// an out-of-set handle happen in the CLI front (`run_request_repo`) *before* this.
pub fn forward_request_repo(port: u16, token: &str, repo_handle: &str) -> std::io::Result<()> {
    let cwd = std::env::current_dir().unwrap_or_default();
    let payload = json!({
        "cwd": cwd.to_string_lossy(),
        "hook_event_name": "RepoRequested",
        "repo_handle": repo_handle,
    });
    if let Ok(body) = serde_json::to_vec(&payload) {
        let _ = post_hook(port, token, &body);
    }
    Ok(())
}

/// The worktree's current branch (`git rev-parse --abbrev-ref HEAD`), or `None`.
fn current_branch(cwd: &Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Install (or refresh) the per-(project, repo) `post-commit` git hook in an L2
/// clone's shared hooks dir, so every agent commit auto-pushes. **Rewritten on
/// every plane build** with the CURRENT run's port + token — the stale-port trap:
/// `serve` mints a fresh ephemeral port each run, so a hook left by a prior run
/// would POST to a dead endpoint and be silently dropped. The forwarder runs
/// detached (`&`) so the commit never waits on the network.
pub fn write_post_commit_hook(
    clone_root: &Path,
    exe: &str,
    port: u16,
    token: &str,
    repo_handle: &str,
) -> std::io::Result<()> {
    let hooks = clone_root.join(".git").join("hooks");
    std::fs::create_dir_all(&hooks)?;
    let path = hooks.join("post-commit");
    let script = format!(
        "#!/bin/sh\n# lindep v1.6 auto-push — best-effort, non-blocking; refreshed each run.\n\
         {} --post-commit {port} --hook-token {} --repo-handle {} >/dev/null 2>&1 &\n",
        sh_quote(exe),
        sh_quote(token),
        sh_quote(repo_handle),
    );
    write_executable(&path, script.as_bytes())
}

/// POSIX single-quote a string for safe embedding in the hook shell script,
/// escaping an embedded `'` the standard `'\''` way.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Write `contents` to `path` (truncating) executable (`0o755`) on Unix — unlike
/// the owner-only settings file, a git hook must be executable to run.
fn write_executable(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o755);
    }
    let mut file = opts.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o755))?;
    }
    file.write_all(contents)
}

/// Per-repo-handle push serialization. Two commits in different worktrees of one
/// repo push different branches (no ref conflict), but a **dedicated** per-handle
/// mutex keeps git's index/ref locks from contending and honours the design's
/// "don't couple push latency to worktree-create" rule (it is *not* the worktree
/// `git_lock`).
fn push_mutex(handle: &str) -> Arc<Mutex<()>> {
    static LOCKS: std::sync::LazyLock<Mutex<std::collections::HashMap<String, Arc<Mutex<()>>>>> =
        std::sync::LazyLock::new(|| Mutex::new(std::collections::HashMap::new()));
    let mut locks = LOCKS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    Arc::clone(locks.entry(handle.to_string()).or_default())
}

/// Push a worktree's branch to its true remote while holding the **same**
/// per-handle [`push_mutex`] the background auto-push uses, so a deliberate push
/// (ENG-541 teardown) can't race an in-flight auto-push of the same handle and
/// spuriously fail on git's local ref lock — surfacing as a phantom "push
/// rejected". Blocking; call from a blocking context (e.g. `spawn_blocking`).
pub fn push_head_serialized(
    handle: &str,
    worktree: &std::path::Path,
) -> Result<(), crate::mirror::MirrorError> {
    let lock = push_mutex(handle);
    let _guard = lock
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    crate::mirror::push_head(worktree)
}

/// Push a committed branch to its true remote in the background, then emit the
/// passive [`AppEvent::AgentCommitted`] indicator — and, on a reject, a footer.
/// Never force-pushes; never blocks the hook or the render loop.
fn spawn_auto_push(
    events: AppEventTx,
    project_id: String,
    issue: String,
    repo_handle: String,
    branch: String,
    worktree: PathBuf,
) {
    tokio::spawn(async move {
        let lock = push_mutex(&repo_handle);
        let push = tokio::task::spawn_blocking(move || {
            let _guard = lock
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            crate::mirror::push_head(&worktree)
        })
        .await;
        // The commit happened regardless of the push outcome: always show the
        // passive indicator; add a footer when the push itself was rejected OR the
        // push task panicked. Surfacing the panic (like `session::persist_snapshot`
        // does) keeps a push that never ran from masquerading as a clean success.
        match &push {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                let _ = events.send(AppEvent::Notification(format!(
                    "{issue}: auto-push failed: {}",
                    clamp_display(&e.to_string())
                )));
            }
            Err(join) => {
                let _ = events.send(AppEvent::Notification(format!(
                    "{issue}: auto-push task aborted: {join}"
                )));
            }
        }
        let _ = events.send(AppEvent::AgentCommitted {
            project_id,
            issue,
            repo_handle,
            branch,
        });
    });
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

    /// Wrap a single store in a one-project registry — the workspace shape the
    /// endpoint now serves. The project key is empty (the test stores load via
    /// `SessionStore::load`, which leaves `project_id` unset); the events the bus
    /// emits then carry `project_id: ""`, which the assertions don't inspect.
    fn registry_of(store: Arc<Mutex<SessionStore>>) -> StoreRegistry {
        Arc::new(Mutex::new(std::collections::HashMap::from([(
            String::new(),
            store,
        )])))
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_hook_resolves_to_the_right_project_when_two_share_an_issue_key() {
        // The workspace rekey's core promise: with several projects live behind one
        // endpoint, a hook for ENG-1 must reach the project whose worktree/session
        // it actually came from — never the other project's same-keyed ENG-1.
        let mk = |pid: &str, wt: &str| -> Arc<Mutex<SessionStore>> {
            let path = std::env::temp_dir()
                .join(format!("lindep-notify-{pid}-{}.json", std::process::id()));
            let _ = std::fs::remove_file(&path);
            let mut store = SessionStore::load(&path).unwrap().for_project(pid);
            store.ensure("ENG-1", wt.into(), "b".into());
            Arc::new(Mutex::new(store))
        };
        let a = mk("proj-a", "/wt/a/ENG-1");
        let b = mk("proj-b", "/wt/b/ENG-1");
        let registry: StoreRegistry = Arc::new(Mutex::new(std::collections::HashMap::from([
            ("proj-a".to_string(), Arc::clone(&a)),
            ("proj-b".to_string(), Arc::clone(&b)),
        ])));
        let (tx, mut rx) = crate::event::channel();

        // A hook carrying project B's ENG-1 session id resolves to (proj-b, ENG-1).
        let payload = HookPayload {
            session_id: Some(SessionStore::session_id_for("proj-b", "ENG-1")),
            hook_event_name: Some("Stop".into()),
            ..Default::default()
        };
        route(&payload, &registry, &tx).await;
        match rx.try_recv().expect("an event was emitted") {
            AppEvent::AgentStatusChanged {
                project_id, issue, ..
            } => assert_eq!(
                (project_id.as_str(), issue.as_str()),
                ("proj-b", "ENG-1"),
                "the session id routed to project B"
            ),
            other => panic!("expected AgentStatusChanged, got {other:?}"),
        }

        // And one carrying project A's worktree cwd resolves to (proj-a, ENG-1).
        let payload = HookPayload {
            cwd: Some("/wt/a/ENG-1".into()),
            hook_event_name: Some("Stop".into()),
            ..Default::default()
        };
        route(&payload, &registry, &tx).await;
        match rx.try_recv().expect("an event was emitted") {
            AppEvent::AgentStatusChanged {
                project_id, issue, ..
            } => assert_eq!(
                (project_id.as_str(), issue.as_str()),
                ("proj-a", "ENG-1"),
                "the cwd routed to project A"
            ),
            other => panic!("expected AgentStatusChanged, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_request_repo_hook_emits_repo_requested_for_the_resolved_issue() {
        // ENG-542: `lindep request-repo web` POSTs a RepoRequested payload whose cwd
        // resolves the issue; route() emits AppEvent::RepoRequested carrying the handle.
        let store = seeded_store();
        let registry = registry_of(store);
        let (tx, mut rx) = crate::event::channel();
        let payload = HookPayload {
            cwd: Some("/wt/ENG-1".into()),
            hook_event_name: Some("RepoRequested".into()),
            repo_handle: Some("web".into()),
            ..Default::default()
        };
        route(&payload, &registry, &tx).await;
        let events = drain(&mut rx).await;
        assert!(
            events.iter().any(|e| matches!(
                e,
                AppEvent::RepoRequested { issue, repo_handle, .. }
                    if issue == "ENG-1" && repo_handle == "web"
            )),
            "a request-repo hook emits RepoRequested for the resolved issue: {events:?}"
        );
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
    fn the_post_commit_hook_is_an_executable_forwarder_invocation() {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let clone = std::env::temp_dir().join(format!("lindep-pch-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&clone);
        std::fs::create_dir_all(clone.join(".git")).unwrap();

        write_post_commit_hook(&clone, "/opt/o'brien/lindep", 8765, "tok'en", "api").unwrap();
        let hook = clone.join(".git").join("hooks").join("post-commit");
        let body = std::fs::read_to_string(&hook).unwrap();
        // The forwarder is invoked with this run's port/token/handle, single-quoted
        // (note the escaped quote in the path/token), and detached so it never
        // blocks the commit.
        assert!(body.contains("--post-commit 8765"), "{body}");
        assert!(body.contains("--repo-handle 'api'"), "{body}");
        assert!(
            body.contains(r"'/opt/o'\''brien/lindep'"),
            "exe is shell-escaped: {body}"
        );
        assert!(
            body.contains(r"--hook-token 'tok'\''en'"),
            "token is shell-escaped: {body}"
        );
        assert!(body.trim_end().ends_with('&'), "runs detached: {body}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&hook).unwrap().permissions().mode();
            assert_eq!(mode & 0o111, 0o111, "the hook is executable");
        }
        let _ = std::fs::remove_dir_all(&clone);
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
        // Unicode bidi/format controls (RLO, the isolates) are dropped too — they
        // aren't C0/C1 controls, so without the Cf filter they'd reorder the footer.
        assert_eq!(clamp_display("a\u{202E}b\u{2066}c"), "abc");
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

    #[test]
    fn permission_and_elicitation_prompts_always_need_you() {
        // A genuine block on the human: flag it whatever the agent's state.
        let permission = HookPayload {
            notification_type: Some("permission_prompt".into()),
            ..Default::default()
        };
        assert!(notification_implies_needs_you(
            &permission,
            Some(AgentStatus::Idle)
        ));
        let elicitation = HookPayload {
            notification_type: Some("elicitation_dialog".into()),
            ..Default::default()
        };
        assert!(notification_implies_needs_you(
            &elicitation,
            Some(AgentStatus::Running)
        ));
    }

    #[test]
    fn idle_auth_and_completion_notifications_never_need_you() {
        // The ~60 s idle nudge, auth-success and the elicitation follow-ups all
        // ride the Notification hook but must NOT raise the flag — this is the
        // "needs-you fires when it doesn't need you" regression.
        for kind in [
            "idle_prompt",
            "auth_success",
            "elicitation_complete",
            "elicitation_response",
        ] {
            let payload = HookPayload {
                notification_type: Some(kind.into()),
                ..Default::default()
            };
            assert!(
                !notification_implies_needs_you(&payload, Some(AgentStatus::Running)),
                "{kind} must not need you even mid-run"
            );
        }
    }

    #[test]
    fn an_untyped_notification_falls_back_to_agent_state() {
        // No notification_type (Claude doesn't guarantee the field): a permission
        // prompt fires mid-turn (agent active) while the idle nudge fires only
        // after Stop (agent already Idle). Unknown/None state stays conservative.
        let bare = HookPayload {
            hook_event_name: Some("Notification".into()),
            ..Default::default()
        };
        assert!(notification_implies_needs_you(
            &bare,
            Some(AgentStatus::Running)
        ));
        assert!(!notification_implies_needs_you(
            &bare,
            Some(AgentStatus::Idle)
        ));
        assert!(notification_implies_needs_you(&bare, None));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_idle_nudge_does_not_raise_the_needs_you_flag() {
        let (tx, mut rx) = crate::event::channel();
        let store = seeded_store();
        let sid = SessionStore::session_id_for("", "ENG-1");
        let Endpoint { port, token } = serve(tx, registry_of(store)).await.unwrap();

        // The idle nudge Claude fires ~60 s after a turn: same hook as a real
        // prompt, but it must surface quietly (AgentAction), never AgentNeedsYou.
        let body = json!({
            "session_id": sid, "cwd": "/wt/ENG-1",
            "hook_event_name": "Notification", "notification_type": "idle_prompt"
        })
        .to_string();
        post_hook(port, &token, body.as_bytes()).unwrap();

        let got = drain(&mut rx).await;
        assert!(
            !got.iter()
                .any(|ev| matches!(ev, AppEvent::AgentNeedsYou { .. })),
            "an idle nudge must not raise needs-you; got {got:?}"
        );
        assert!(
            got.iter().any(|ev| matches!(
                ev,
                AppEvent::AgentAction { issue, .. } if issue == "ENG-1"
            )),
            "the idle nudge still surfaces quietly as an action; got {got:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_first_tool_run_promotes_a_starting_agent_to_running() {
        // Spawn no longer fakes Running, so a fresh agent sits Spawning until it
        // does real work; the first PostToolUse is that "working" signal.
        let (tx, mut rx) = crate::event::channel();
        let store = seeded_store();
        store
            .lock()
            .unwrap()
            .set_status("ENG-1", AgentStatus::Spawning);
        let sid = SessionStore::session_id_for("", "ENG-1");
        let Endpoint { port, token } = serve(tx, registry_of(Arc::clone(&store))).await.unwrap();

        let body = json!({
            "session_id": sid, "cwd": "/wt/ENG-1",
            "hook_event_name": "PostToolUse", "tool_name": "Edit"
        })
        .to_string();
        post_hook(port, &token, body.as_bytes()).unwrap();

        let mut promoted = false;
        for _ in 0..50 {
            if store.lock().unwrap().get("ENG-1").map(|r| r.status) == Some(AgentStatus::Running) {
                promoted = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let _ = drain(&mut rx).await;
        assert!(promoted, "the first tool run moves 'starting…' → 'working'");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_tool_run_after_the_user_answers_clears_needs_you() {
        // The core needs-you bug: a permission prompt set NeedsYou, the user
        // answered, and the agent resumed running tools — yet the flag never
        // cleared until the next Stop. A PostToolUse from NeedsYou must now promote
        // to Running (durably) and emit a *working* action so the cockpit resolves
        // the flag the instant work resumes.
        let (tx, mut rx) = crate::event::channel();
        let store = seeded_store();
        store
            .lock()
            .unwrap()
            .set_status("ENG-1", AgentStatus::NeedsYou);
        let sid = SessionStore::session_id_for("", "ENG-1");
        let Endpoint { port, token } = serve(tx, registry_of(Arc::clone(&store))).await.unwrap();

        let body = json!({
            "session_id": sid, "cwd": "/wt/ENG-1",
            "hook_event_name": "PostToolUse", "tool_name": "Bash"
        })
        .to_string();
        post_hook(port, &token, body.as_bytes()).unwrap();

        let mut cleared = false;
        for _ in 0..50 {
            if store.lock().unwrap().get("ENG-1").map(|r| r.status) == Some(AgentStatus::Running) {
                cleared = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let got = drain(&mut rx).await;
        assert!(
            cleared,
            "a tool run after the user answers clears NeedsYou → Running"
        );
        assert!(
            got.iter().any(|ev| matches!(
                ev,
                AppEvent::AgentAction { issue, working, .. } if issue == "ENG-1" && *working
            )),
            "the resumed tool run emits a *working* action that clears the flag; got {got:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn submitting_a_prompt_marks_the_agent_working_and_clears_needs_you() {
        // UserPromptSubmit is the leading "working" signal AND the clean "the user
        // answered" signal: from NeedsYou (or Idle) it promotes to Running before
        // any tool runs, so a text-only / thinking turn no longer reads idle.
        let (tx, mut rx) = crate::event::channel();
        let store = seeded_store();
        store
            .lock()
            .unwrap()
            .set_status("ENG-1", AgentStatus::NeedsYou);
        let sid = SessionStore::session_id_for("", "ENG-1");
        let Endpoint { port, token } = serve(tx, registry_of(Arc::clone(&store))).await.unwrap();

        let body = json!({
            "session_id": sid, "cwd": "/wt/ENG-1", "hook_event_name": "UserPromptSubmit"
        })
        .to_string();
        post_hook(port, &token, body.as_bytes()).unwrap();

        let mut working = false;
        for _ in 0..50 {
            if store.lock().unwrap().get("ENG-1").map(|r| r.status) == Some(AgentStatus::Running) {
                working = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let got = drain(&mut rx).await;
        assert!(working, "submitting a prompt moves the agent to Running");
        assert!(
            got.iter().any(|ev| matches!(
                ev,
                AppEvent::AgentStatusChanged { issue, status, .. }
                    if issue == "ENG-1" && *status == AgentStatus::Running
            )),
            "the prompt submit emits a Running status change (a new turn); got {got:?}"
        );
    }

    #[test]
    fn hook_settings_register_the_user_prompt_submit_working_signal() {
        // The leading working signal must actually be wired, or a thinking/
        // text-only turn keeps reading idle until its first tool.
        let json = hook_settings_json("/opt/lindep", 8765, "t");
        assert!(json.contains("UserPromptSubmit"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn hooks_map_to_the_right_issue_across_concurrent_agents() {
        let (tx, mut rx) = crate::event::channel();
        let store = seeded_store();
        let sid1 = SessionStore::session_id_for("", "ENG-1");
        let sid2 = SessionStore::session_id_for("", "ENG-2");
        let Endpoint { port, token } = serve(tx, registry_of(store)).await.unwrap();

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
                    AppEvent::AgentNeedsYou { issue, reason, .. } => needs = Some((issue, reason)),
                    AppEvent::AgentStatusChanged { issue, status, .. } => {
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
        let Endpoint { port, token } = serve(tx, registry_of(store)).await.unwrap();
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
        let Endpoint { port, token } = serve(tx, registry_of(store)).await.unwrap();

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
        let sid = SessionStore::session_id_for("", "ENG-1");
        let Endpoint { port, token } = serve(tx, registry_of(store)).await.unwrap();
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
        let sid = SessionStore::session_id_for("", "ENG-1");
        let Endpoint { port, token } = serve(tx, registry_of(store)).await.unwrap();

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
        let sid = SessionStore::session_id_for("", "ENG-1");
        let Endpoint { port, token } = serve(tx, registry_of(store)).await.unwrap();

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
                AppEvent::AgentAction { issue, action, .. } if issue == "ENG-1" && action == "ran Edit"
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
        let sid = SessionStore::session_id_for("", "ENG-1");
        let Endpoint { port, token } = serve(tx, registry_of(Arc::clone(&store))).await.unwrap();

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
