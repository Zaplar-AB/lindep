//! Per-issue scratch datastores (ENG-561).
//!
//! lindep stays **engine-agnostic**: a project declares, per `[[scratch]]` entry,
//! the shell `provision`/`teardown` commands and the `env` to hand the agent (see
//! [`crate::registry::ScratchSpec`]). lindep owns only the *lifecycle* — run
//! `provision` at launch on the blocking pool, inject the resolved env into the
//! agent, run `teardown` at discard — exactly the posture it takes with `git`
//! (shelled, never embedded). Postgres uses a template-cloned DB, Neo4j a container,
//! and so on; lindep never knows which.
//!
//! These functions are **synchronous and blocking** (they shell out); a caller on
//! the tokio runtime invokes them via `spawn_blocking`, like the mirror/worktree code.

use std::path::PathBuf;
use std::process::Command;

use crate::registry::ScratchSpec;
use crate::session::ScratchRecord;

/// The per-issue facts a scratch command is resolved against. Owns its data so it
/// can move into a `spawn_blocking` closure.
#[derive(Debug, Clone)]
pub struct Context {
    pub issue: String,
    /// The project *handle* (path-safe), for the `{project}` placeholder.
    pub project: String,
    /// The per-issue workspace dir (the agent's cwd), for `{workspace}`.
    pub workspace: PathBuf,
}

/// A successfully provisioned resource: the record to persist + the env to inject.
#[derive(Debug)]
pub struct Provisioned {
    pub record: ScratchRecord,
    pub env: Vec<(String, String)>,
    /// True when this is a `persist`ed resource that ALREADY existed (a resume
    /// reconnected to it) rather than one this pass created. A rollback must NOT tear
    /// a reused resource down — it pre-dates this launch and the persist/resume
    /// contract keeps it across aborts.
    pub reused: bool,
}

/// A provision that failed. `required` carries the spec's flag so the caller can
/// decide fatal-vs-skip without re-reading the spec.
#[derive(Debug)]
pub struct ProvisionError {
    pub name: String,
    pub required: bool,
    pub message: String,
}

/// Provision every spec, in order and independently (entries are independent in
/// v1.7 — no cross-references). For a `persist`ed resource, reuse the port recorded
/// in `prior` (a resume reconnects to the same instance) instead of minting a fresh
/// one. Each result is the env-bearing [`Provisioned`] or a [`ProvisionError`].
pub fn provision_all(
    specs: &[ScratchSpec],
    ctx: &Context,
    prior: &[ScratchRecord],
) -> Vec<Result<Provisioned, ProvisionError>> {
    specs
        .iter()
        .map(|spec| {
            let prior_rec = prior.iter().find(|r| r.name == spec.name);
            let reuse_port = spec
                .persist
                .then(|| prior_rec.and_then(|r| r.port))
                .flatten();
            // A persisted resource that already has a prior record pre-dates this
            // launch — mark it reused so a rollback won't tear it down.
            let reused = spec.persist && prior_rec.is_some();
            provision_one(spec, ctx, reuse_port).map(|mut p| {
                p.reused = reused;
                p
            })
        })
        .collect()
}

fn provision_one(
    spec: &ScratchSpec,
    ctx: &Context,
    reuse_port: Option<u16>,
) -> Result<Provisioned, ProvisionError> {
    let fail = |message: String| ProvisionError {
        name: spec.name.clone(),
        required: spec.required,
        message,
    };

    let slug = slug(&ctx.issue);
    let port = if spec.needs_port {
        match reuse_port.or_else(mint_port) {
            Some(p) => Some(p),
            None => return Err(fail("could not allocate a free port".to_string())),
        }
    } else {
        None
    };

    // Env from the spec table: values are substituted but NOT shell-quoted — they're
    // handed to the child as process env, never interpreted by a shell.
    let mut env: Vec<(String, String)> = spec
        .env
        .iter()
        .map(|(k, v)| (k.clone(), subst(v, ctx, &slug, port, false)))
        .collect();

    let cmd = subst(&spec.provision, ctx, &slug, port, true);
    let stdout = run_shell(&cmd).map_err(fail)?;
    // A provision command may print `KEY=VALUE` lines on stdout for values lindep
    // can't template (e.g. a container-assigned port). Capture and inject them.
    env.extend(parse_env_lines(&stdout));

    let env_keys = env.iter().map(|(k, _)| k.clone()).collect();
    // Substitute teardown NOW and store it resolved, so a later registry edit can
    // never strand this resource — teardown acts on exactly what was created.
    let teardown = subst(&spec.teardown, ctx, &slug, port, true);
    Ok(Provisioned {
        record: ScratchRecord {
            name: spec.name.clone(),
            teardown,
            env_keys,
            // Only persisted resources keep their port across a resume.
            port: if spec.persist { port } else { None },
        },
        env,
        // Set by `provision_all` when this reused a prior persisted resource.
        reused: false,
    })
}

/// Run a recorded resource's already-substituted `teardown`. A no-op for an empty
/// command (a resource with nothing to undo).
pub fn teardown(record: &ScratchRecord) -> Result<(), String> {
    if record.teardown.trim().is_empty() {
        return Ok(());
    }
    run_shell(&record.teardown).map(|_| ())
}

/// Derive a path-safe, shell-inert, DB-identifier-safe token from an issue id (the
/// `{slug}` placeholder). The charset `[a-z0-9_]` starting with a letter is the
/// intersection of a Postgres unquoted identifier, a Docker name, a URL component,
/// and a shell-inert word — so one slug works as all of them AND can never break out
/// of a `provision` shell string. Lowercases, collapses every run of other chars to a
/// single `_`, trims, and prefixes `s` if it would start with a digit.
///
/// **Collision-free, always.** Every one of those transforms is many-to-one
/// (`ENG-1`, `eng-1`, `ENG--1`, `ENG-1-` all canonicalise to `eng_1`), so the bare
/// canonical form is only used verbatim when the raw id ALREADY equals it (a no-op
/// round-trip); any id that was changed at all — or is over-long — keeps the readable
/// canonical form as a prefix plus a stable hash of the RAW id, guaranteeing distinct
/// issues never share a slug (e.g. `ENG-812` → `eng_812_1f3c…`, `api` → `api`).
pub fn slug(issue: &str) -> String {
    let mut out = String::with_capacity(issue.len());
    let mut prev_underscore = false;
    for ch in issue.chars() {
        let c = ch.to_ascii_lowercase();
        if c.is_ascii_alphanumeric() {
            out.push(c);
            prev_underscore = false;
        } else if !prev_underscore {
            out.push('_');
            prev_underscore = true;
        }
    }
    let mut slug = out.trim_matches('_').to_string();
    if slug.is_empty() || slug.starts_with(|c: char| c.is_ascii_digit()) {
        slug.insert(0, 's');
    }
    // The no-hash fast path is sound ONLY when canonicalisation was a true no-op — the
    // raw id already equals its slug. Anything that changed (case-fold, `-`/punct
    // collapse, trim, digit-prefix) or is over-long MUST carry a stable hash of the RAW
    // id, or two distinct ids could alias to one resource (the non-collision invariant).
    if slug != issue || slug.len() > 40 {
        slug.truncate(31);
        let base = slug.trim_end_matches('_');
        return format!("{base}_{:08x}", stable_hash(issue));
    }
    slug
}

/// Substitute `{issue}/{slug}/{project}/{workspace}/{port}` into `template`. In
/// command context (`shell = true`) every value is shell-inert: `{slug}` and
/// `{project}` are inert by construction, `{port}` is numeric, and `{issue}` /
/// `{workspace}` (arbitrary text / paths with spaces) are single-quote-escaped. In
/// env-value context (`shell = false`) values are inserted verbatim — env is passed
/// to the child process, never through a shell.
fn subst(template: &str, ctx: &Context, slug: &str, port: Option<u16>, shell: bool) -> String {
    let quote = |s: &str| if shell { sh_quote(s) } else { s.to_string() };
    let workspace = ctx.workspace.to_string_lossy();
    let port_s = port.map(|p| p.to_string()).unwrap_or_default();
    // `{workspace}` is substituted LAST: it's the only value that could itself contain
    // literal placeholder text (the user-configurable `$LINDEP_HOME` root), and with no
    // `.replace` after it, such text can't be re-expanded. The other values are a
    // validated issue id, an inert slug, a safe project handle, and a number — none can
    // contain a `{token}`.
    template
        .replace("{issue}", &quote(&ctx.issue))
        .replace("{slug}", slug)
        .replace("{project}", &ctx.project)
        .replace("{port}", &port_s)
        .replace("{workspace}", &quote(&workspace))
}

/// POSIX single-quote a value into one inert shell word: wrap in `'…'`, rendering any
/// embedded `'` as `'\''`.
fn sh_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

/// Parse `KEY=VALUE` lines from a provision command's stdout (other lines ignored).
/// `KEY` must be a valid env identifier (`[A-Za-z_][A-Za-z0-9_]*`); the value is the
/// rest of the line as-is (the newline already stripped by `lines()`).
fn parse_env_lines(stdout: &str) -> Vec<(String, String)> {
    stdout
        .lines()
        .filter_map(|line| {
            let (key, value) = line.split_once('=')?;
            let key = key.trim();
            let valid = !key.is_empty()
                && key.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
                && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
            valid.then(|| (key.to_string(), value.to_string()))
        })
        .collect()
}

/// Run `sh -c <cmd>`, returning stdout on success or a one-line error (exit code +
/// trimmed, clamped stderr) on failure. `GIT_TERMINAL_PROMPT=0` mirrors the mirror
/// layer so a provision that shells git can't hang on a credential prompt.
fn run_shell(cmd: &str) -> Result<String, String> {
    let output = Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .map_err(|e| format!("could not run `sh`: {e}"))?;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
    }
    let code = output
        .status
        .code()
        .map(|c| c.to_string())
        .unwrap_or_else(|| "signal".to_string());
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(format!(
        "`{}` failed (exit {code}): {}",
        clamp(cmd, 80),
        clamp(stderr.trim(), 400)
    ))
}

/// Mint a free loopback TCP port by binding to port 0, reading the assignment, then
/// dropping the listener. The brief race (another process could grab it before the
/// provision command binds) is acceptable for scratch — the same ephemeral-bind
/// approach the hook endpoint uses.
fn mint_port() -> Option<u16> {
    std::net::TcpListener::bind(("127.0.0.1", 0))
        .ok()
        .and_then(|l| l.local_addr().ok())
        .map(|addr| addr.port())
}

/// FNV-1a (32-bit) — a small, dependency-free, deterministic hash used only to
/// disambiguate a lossy/truncated slug. Not cryptographic; collision-resistance for
/// short ids is more than enough to keep two issues' resources apart.
fn stable_hash(s: &str) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for byte in s.as_bytes() {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

/// Truncate `s` to at most `max` characters, appending `…` when shortened — keeps a
/// command/stderr from blowing out the footer.
fn clamp(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max).collect();
        format!("{head}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn ctx() -> Context {
        Context {
            issue: "ENG-812".to_string(),
            project: "lindep".to_string(),
            workspace: PathBuf::from("/tmp/ws/ENG-812"),
        }
    }

    #[test]
    fn slug_passes_through_an_already_clean_token_unchanged() {
        // A raw id that already equals its canonical form round-trips with no hash.
        assert_eq!(slug("eng_812"), "eng_812");
        assert_eq!(slug("api"), "api");
    }

    #[test]
    fn slug_hashes_a_key_that_isnt_already_its_own_slug() {
        // A canonicalised id (uppercase, dashes) keeps the readable prefix + a stable
        // hash so it can't alias another id.
        let s = slug("ENG-812");
        assert!(s.starts_with("eng_812_"), "{s}");
        assert_eq!(s, slug("ENG-812"), "the hash is stable");
        assert!(
            s.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        );
    }

    #[test]
    fn distinct_issue_ids_never_alias_to_one_slug() {
        // The collision class the review caught: case-fold, repeated `-`, and a
        // trailing `-` all canonicalise to the same base `eng_1`, so each distinct id
        // must still get a distinct slug via the hash.
        let ids = ["ENG-1", "eng-1", "ENG--1", "ENG-1-", "Eng-1"];
        let slugs: std::collections::HashSet<_> = ids.iter().map(|i| slug(i)).collect();
        assert_eq!(
            slugs.len(),
            ids.len(),
            "every distinct id maps to a distinct slug: {slugs:?}"
        );
    }

    #[test]
    fn slug_prefixes_a_leading_digit_so_its_a_valid_identifier() {
        assert!(slug("123").starts_with('s'));
        assert!(!slug("123").starts_with(|c: char| c.is_ascii_digit()));
    }

    #[test]
    fn slug_caps_length_under_the_postgres_identifier_limit() {
        let long = "ENG-".to_string() + &"x".repeat(100);
        assert!(slug(&long).len() <= 40);
    }

    #[test]
    fn sh_quote_neutralises_an_embedded_single_quote() {
        assert_eq!(sh_quote("a'b"), "'a'\\''b'");
        assert_eq!(sh_quote("plain"), "'plain'");
    }

    #[test]
    fn subst_shell_quotes_a_workspace_path_with_spaces() {
        let c = Context {
            issue: "ENG-1".to_string(),
            project: "p".to_string(),
            workspace: PathBuf::from("/tmp/a b/ENG-1"),
        };
        let cmd = subst(
            "cd {workspace} && createdb scratch_{slug}",
            &c,
            "eng_1",
            None,
            true,
        );
        assert_eq!(cmd, "cd '/tmp/a b/ENG-1' && createdb scratch_eng_1");
    }

    #[test]
    fn subst_env_value_is_verbatim_not_quoted() {
        let url = subst("postgres:///scratch_{slug}", &ctx(), "eng_812", None, false);
        assert_eq!(url, "postgres:///scratch_eng_812");
    }

    #[test]
    fn subst_fills_the_minted_port() {
        let s = subst(
            "bolt://localhost:{port}",
            &ctx(),
            "eng_812",
            Some(7687),
            false,
        );
        assert_eq!(s, "bolt://localhost:7687");
    }

    #[test]
    fn parse_env_lines_keeps_valid_assignments_and_drops_noise() {
        let out = "DATABASE_URL=postgres:///x\nnoise line\n=bad\n1BAD=x\nPORT=5432\n";
        let env = parse_env_lines(out);
        assert_eq!(
            env,
            vec![
                ("DATABASE_URL".to_string(), "postgres:///x".to_string()),
                ("PORT".to_string(), "5432".to_string()),
            ]
        );
    }

    #[test]
    fn provision_runs_the_command_injects_env_and_captures_stdout() {
        let spec = ScratchSpec {
            name: "db".to_string(),
            provision: "echo CONTAINER_PORT=6000".to_string(),
            teardown: "true".to_string(),
            env: BTreeMap::from([(
                "DATABASE_URL".to_string(),
                "postgres:///scratch_{slug}".to_string(),
            )]),
            needs_port: false,
            required: false,
            persist: false,
        };
        let p = provision_one(&spec, &ctx(), None).expect("provision succeeds");
        // The static env value is substituted ({slug} → the collision-free slug)…
        assert!(p.env.contains(&(
            "DATABASE_URL".to_string(),
            format!("postgres:///scratch_{}", slug("ENG-812"))
        )));
        // …and the stdout KEY=VALUE is captured.
        assert!(
            p.env
                .contains(&("CONTAINER_PORT".to_string(), "6000".to_string()))
        );
        assert_eq!(p.record.teardown, "true");
        assert_eq!(p.record.port, None);
    }

    #[test]
    fn a_failing_required_provision_reports_required() {
        let spec = ScratchSpec {
            name: "db".to_string(),
            provision: "exit 3".to_string(),
            teardown: String::new(),
            env: BTreeMap::new(),
            needs_port: false,
            required: true,
            persist: false,
        };
        let err = provision_one(&spec, &ctx(), None).expect_err("provision fails");
        assert!(err.required);
        assert_eq!(err.name, "db");
        assert!(err.message.contains("exit 3"));
    }

    #[test]
    fn a_persisted_resource_keeps_its_port_and_reuses_it_on_resume() {
        let spec = ScratchSpec {
            name: "graph".to_string(),
            provision: "true".to_string(),
            teardown: "true".to_string(),
            env: BTreeMap::new(),
            needs_port: true,
            required: false,
            persist: true,
        };
        let first = provision_one(&spec, &ctx(), None).expect("first provision");
        let port = first
            .record
            .port
            .expect("a persisted resource records its port");
        // A resume passes the prior record's port back; it must be reused, not re-minted.
        let again = provision_one(&spec, &ctx(), Some(port)).expect("resume provision");
        assert_eq!(again.record.port, Some(port));
    }

    #[test]
    fn provision_all_flags_a_reused_persisted_resource_but_not_a_fresh_one() {
        let persist = ScratchSpec {
            name: "graph".to_string(),
            provision: "true".to_string(),
            teardown: "true".to_string(),
            env: BTreeMap::new(),
            needs_port: true,
            required: false,
            persist: true,
        };
        let fresh = ScratchSpec {
            name: "db".to_string(),
            ..persist.clone()
        };
        // A prior record for `graph` means a resume reconnects to it → reused.
        let prior = vec![ScratchRecord {
            name: "graph".to_string(),
            teardown: "true".to_string(),
            env_keys: vec![],
            port: Some(7000),
        }];
        let out = provision_all(&[persist, fresh], &ctx(), &prior);
        let graph = out[0].as_ref().expect("graph provisions");
        let db = out[1].as_ref().expect("db provisions");
        assert!(
            graph.reused,
            "a persisted resource with a prior record is reused"
        );
        assert_eq!(
            graph.record.port,
            Some(7000),
            "and reuses its recorded port"
        );
        assert!(
            !db.reused,
            "a resource with no prior record is freshly created"
        );
    }

    #[test]
    fn teardown_is_a_noop_for_an_empty_command() {
        let rec = ScratchRecord {
            name: "db".to_string(),
            teardown: String::new(),
            env_keys: vec![],
            port: None,
        };
        assert!(teardown(&rec).is_ok());
    }
}
