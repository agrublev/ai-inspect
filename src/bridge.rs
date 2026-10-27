//! The approval bridge between a running Claude Code session and iris.
//!
//! iris cannot reach into a session's interactive permission prompt directly,
//! so we use a supported `PreToolUse` hook. The hook IS this same binary
//! (`iris hook`): on every tool call Claude Code pipes the request JSON to it,
//! the hook writes a request file and blocks-polls for a decision file that the
//! iris TUI writes when you press a key, then emits the `hookSpecificOutput`
//! decision Claude Code expects.
//!
//! iris touches a heartbeat file while running; if it's stale (iris not up),
//! the hook immediately defers to the normal interactive prompt rather than
//! pausing the session.

use std::io::Read;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::trace::TraceStore;

/// Heartbeat older than this ⇒ treat iris as not running.
const HEARTBEAT_STALE_SECS: u64 = 8;
/// Max time the hook will wait for a decision before deferring to "ask".
const POLL_TIMEOUT: Duration = Duration::from_secs(25);
const POLL_INTERVAL: Duration = Duration::from_millis(200);
/// A request older than this is dead: the hook has already hit POLL_TIMEOUT and
/// deferred to the native prompt, or its process was killed and orphaned the
/// file. Either way iris can no longer act on it, so we GC it rather than show a
/// phantom "needs approval" forever. Kept a hair above POLL_TIMEOUT.
const REQUEST_STALE_SECS: u64 = 30;
/// Passive lifecycle events that make transcript activity into a timeline.
/// PreToolUse stays synchronous because it also carries opt-in approval gating;
/// all other recorders run asynchronously so agent execution does not wait.
const TRACE_HOOK_EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "PostToolUseFailure",
    "PostToolBatch",
    "SubagentStart",
    "SubagentStop",
    "TaskCreated",
    "TaskCompleted",
    "InstructionsLoaded",
    "Stop",
    "StopFailure",
    "PreCompact",
    "PostCompact",
    "ConfigChange",
    "Notification",
    "SessionEnd",
];

pub fn base_dir() -> PathBuf {
    // Tests set IRIS_BASE_DIR to an isolated temp dir so they never touch the
    // real ~/.claude/iris. Unset in normal use ⇒ behavior is unchanged.
    if let Ok(dir) = std::env::var("IRIS_BASE_DIR") {
        return PathBuf::from(dir);
    }
    dirs::home_dir()
        .unwrap_or_default()
        .join(".claude")
        .join("iris")
}

pub fn trace_db_path() -> PathBuf {
    base_dir().join("traces.db")
}
fn requests_dir() -> PathBuf {
    base_dir().join("requests")
}
fn decisions_dir() -> PathBuf {
    base_dir().join("decisions")
}
fn heartbeat_path() -> PathBuf {
    base_dir().join("heartbeat")
}
/// Presence of this file ⇒ iris is "armed": the hook intercepts tool calls and
/// waits for a dashboard decision. Absent ⇒ iris is passive and tool calls flow
/// through Claude Code's normal permission system with no delay.
fn gating_path() -> PathBuf {
    base_dir().join("gating")
}

/// Is approval gating currently armed? Only meaningful while iris is live.
pub fn gating_armed() -> bool {
    gating_path().exists()
}

/// Arm or disarm approval gating (iris side).
pub fn set_gating(armed: bool) {
    if armed {
        let _ = std::fs::create_dir_all(base_dir());
        let _ = std::fs::write(gating_path(), b"1");
    } else {
        let _ = std::fs::remove_file(gating_path());
    }
}

fn ensure_dirs() {
    let _ = std::fs::create_dir_all(requests_dir());
    let _ = std::fs::create_dir_all(decisions_dir());
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Append a diagnostic line to `~/.claude/iris/iris.log`. Cheap and best-effort.
pub fn log(msg: &str) {
    use std::io::Write;
    let _ = std::fs::create_dir_all(base_dir());
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(base_dir().join("iris.log"))
    {
        let _ = writeln!(f, "{} [{}] {}", now_secs(), std::process::id(), msg);
    }
}

/// Heuristic check: is an `iris … hook` PreToolUse hook present in settings.json?
/// Checks both the global and project settings files.
pub fn hook_installed() -> bool {
    let mut paths = Vec::new();
    if let Some(home) = dirs::home_dir() {
        paths.push(home.join(".claude").join("settings.json"));
    }
    paths.push(PathBuf::from(".claude").join("settings.json"));

    for p in paths {
        let v: Value = match std::fs::read_to_string(&p)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
        {
            Some(v) => v,
            None => continue,
        };
        let pre = v
            .get("hooks")
            .and_then(|h| h.get("PreToolUse"))
            .and_then(Value::as_array);
        if let Some(arr) = pre {
            for entry in arr {
                if let Some(hs) = entry.get("hooks").and_then(Value::as_array) {
                    for h in hs {
                        if let Some(cmd) = h.get("command").and_then(Value::as_str) {
                            if cmd.contains("iris") && cmd.trim_end().ends_with("hook") {
                                return true;
                            }
                        }
                    }
                }
            }
        }
    }
    false
}

/// A tool call waiting for a human decision.
pub struct Pending {
    pub id: String,
    pub session_id: String,
    pub tool_name: String,
    pub brief: String,
    /// Full tool input, pretty-printed, for the detail view.
    pub input: String,
    pub cwd: String,
    pub ts: u64,
}

// ---- iris (TUI) side -------------------------------------------------------

/// Mark iris as alive so hooks know to route approvals here.
pub fn touch_heartbeat() {
    let _ = std::fs::create_dir_all(base_dir());
    let _ = std::fs::write(heartbeat_path(), now_secs().to_string());
}

/// Load all pending approval requests from disk.
pub fn load_pending() -> Vec<Pending> {
    gc_stale_decisions();
    let mut out = Vec::new();
    let rd = match std::fs::read_dir(requests_dir()) {
        Ok(rd) => rd,
        Err(_) => return out,
    };
    for entry in rd.flatten() {
        let p = entry.path();
        if p.extension().is_some_and(|x| x == "json") {
            // A request the hook can no longer act on is litter — reap it. We use
            // the request's own `ts`, falling back to the file mtime for
            // empty/corrupt files (a hook killed before it finished writing).
            let file_age = std::fs::metadata(&p)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| SystemTime::now().duration_since(t).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);

            let parsed = std::fs::read_to_string(&p)
                .ok()
                .and_then(|text| serde_json::from_str::<Value>(&text).ok());

            let ts = parsed.as_ref().and_then(|v| v["ts"].as_u64()).unwrap_or(0);
            let age = if ts != 0 {
                now_secs().saturating_sub(ts)
            } else {
                file_age
            };
            if age > REQUEST_STALE_SECS {
                let _ = std::fs::remove_file(&p);
                log(&format!(
                    "load_pending: GC stale request {} (age {age}s)",
                    p.display()
                ));
                continue;
            }

            if let Some(v) = parsed {
                let input = v
                    .get("tool_input")
                    .map(|ti| serde_json::to_string_pretty(ti).unwrap_or_default())
                    .unwrap_or_default();
                out.push(Pending {
                    id: v["id"].as_str().unwrap_or("").to_string(),
                    session_id: v["session_id"].as_str().unwrap_or("").to_string(),
                    tool_name: v["tool_name"].as_str().unwrap_or("tool").to_string(),
                    brief: v["brief"].as_str().unwrap_or("").to_string(),
                    input,
                    cwd: v["cwd"].as_str().unwrap_or("").to_string(),
                    ts,
                });
            }
        }
    }
    out
}

/// Reap decision files a dead/timed-out hook never consumed. The hook removes
/// its own decision once read; anything left past the stale window is an orphan.
fn gc_stale_decisions() {
    let rd = match std::fs::read_dir(decisions_dir()) {
        Ok(rd) => rd,
        Err(_) => return,
    };
    for entry in rd.flatten() {
        let p = entry.path();
        let age = std::fs::metadata(&p)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| SystemTime::now().duration_since(t).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        if age > REQUEST_STALE_SECS {
            let _ = std::fs::remove_file(&p);
        }
    }
}

/// Write a decision for a pending request. The hook is polling for this file.
pub fn write_decision(id: &str, allow: bool, reason: &str) {
    ensure_dirs();
    let decision = if allow { "allow" } else { "deny" };
    log(&format!("iris: write_decision id={id} decision={decision}"));
    let body = json!({ "decision": decision, "reason": reason }).to_string();
    let _ = std::fs::write(decisions_dir().join(format!("{id}.json")), body);
    // Drop the request immediately so the TUI stops showing it.
    let _ = std::fs::remove_file(requests_dir().join(format!("{id}.json")));
}

// ---- hook side -------------------------------------------------------------

/// Pure staleness rule: a heartbeat this many seconds old still counts as live.
/// Extracted so the "never hang a session" boundary is unit-testable. Any
/// ambiguity (missing/unreadable heartbeat) is handled by the caller as "not
/// live", so the hook always defers rather than blocks.
fn heartbeat_fresh(age_secs: u64) -> bool {
    age_secs <= HEARTBEAT_STALE_SECS
}

fn iris_live() -> bool {
    std::fs::metadata(heartbeat_path())
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| SystemTime::now().duration_since(t).ok())
        .map(|age| heartbeat_fresh(age.as_secs()))
        .unwrap_or(false)
}

/// Entry point for `iris hook`. Records lifecycle events and, for PreToolUse,
/// preserves the approval bridge behavior. Always exits 0 so telemetry cannot
/// block Claude Code; on any approval problem it defers to normal permissions.
pub fn run_hook() -> i32 {
    let mut input = String::new();
    let _ = std::io::stdin().read_to_string(&mut input);
    let v: Value = serde_json::from_str(&input).unwrap_or(Value::Null);

    let event_name = v
        .get("hook_event_name")
        .and_then(Value::as_str)
        .unwrap_or("PreToolUse");
    if event_name != "PreToolUse" {
        if let Err(error) =
            TraceStore::open(trace_db_path()).and_then(|mut store| store.ingest_hook(&v))
        {
            log(&format!("trace capture failed: {error}"));
        }
        return 0;
    }

    let session_id = v["session_id"].as_str().unwrap_or("");
    let tool_name = v["tool_name"].as_str().unwrap_or("tool");
    let cwd = v["cwd"].as_str().unwrap_or("");
    let brief = tool_brief(tool_name, v.get("tool_input"));

    log(&format!(
        "hook: tool={tool_name} session={session_id} iris_live={}",
        iris_live()
    ));

    // iris not running → don't touch this tool call at all. Emitting "ask" here
    // would force a confirmation prompt even for commands the user's settings
    // auto-allow; emitting nothing lets Claude Code's normal permission flow run.
    if session_id.is_empty() || !iris_live() {
        log("hook: iris not live → defer to normal permission flow");
        return 0;
    }

    // iris is live but not armed → passive mode: never block or gate, just let
    // the normal permission flow run. The user arms gating from the dashboard
    // only when they want to supervise approvals.
    if !gating_armed() {
        log("hook: gating disarmed → defer to normal permission flow");
        return 0;
    }

    if let Err(error) =
        TraceStore::open(trace_db_path()).and_then(|mut store| store.ingest_hook(&v))
    {
        log(&format!("trace capture failed: {error}"));
    }

    ensure_dirs();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let id = format!("{session_id}-{}-{}", nanos, std::process::id());
    let req = json!({
        "id": id,
        "session_id": session_id,
        "tool_name": tool_name,
        "brief": brief,
        "tool_input": v.get("tool_input").cloned().unwrap_or(Value::Null),
        "cwd": cwd,
        "ts": now_secs(),
    });
    let req_path = requests_dir().join(format!("{id}.json"));
    let dec_path = decisions_dir().join(format!("{id}.json"));
    let _ = std::fs::write(&req_path, req.to_string());
    log(&format!("hook: wrote request {id}, waiting for decision"));

    let start = SystemTime::now();
    loop {
        if let Ok(text) = std::fs::read_to_string(&dec_path) {
            let _ = std::fs::remove_file(&dec_path);
            let _ = std::fs::remove_file(&req_path);
            let dv: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
            let decision = dv["decision"].as_str().unwrap_or("");
            let reason = dv["reason"].as_str().unwrap_or("");
            // Only an explicit allow/deny overrides the normal flow. Anything
            // else defers (emit nothing) so we don't force a spurious prompt.
            if decision == "allow" || decision == "deny" {
                emit(decision, reason);
            } else {
                log(&format!(
                    "hook: decision '{decision}' not allow/deny → defer"
                ));
            }
            return 0;
        }
        let elapsed = start.elapsed().unwrap_or(POLL_TIMEOUT);
        if elapsed >= POLL_TIMEOUT || !iris_live() {
            // No decision from iris in time. Defer to the normal permission flow
            // instead of forcing "ask" — that turned auto-allowed commands into
            // manual prompts and flooded sessions with confirmations.
            let _ = std::fs::remove_file(&req_path);
            log("hook: no decision in time → defer to normal permission flow");
            return 0;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

fn settings_path(project: bool) -> Result<PathBuf, String> {
    if project {
        Ok(PathBuf::from(".claude").join("settings.json"))
    } else {
        Ok(dirs::home_dir()
            .ok_or("cannot resolve home directory")?
            .join(".claude")
            .join("settings.json"))
    }
}

fn is_iris_hook(cmd: &str) -> bool {
    cmd.contains("iris") && cmd.trim_end().ends_with("hook")
}

/// Remove iris lifecycle hooks from settings.json (idempotent).
pub fn uninstall_hook(project: bool) -> Result<String, String> {
    let path = settings_path(project)?;
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => return Ok(format!("no settings file at {}", path.display())),
    };
    let mut root: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    let mut removed = 0usize;
    if let Some(hooks) = root.get_mut("hooks").and_then(Value::as_object_mut) {
        for event in TRACE_HOOK_EVENTS {
            if let Some(arr) = hooks.get_mut(*event).and_then(Value::as_array_mut) {
                for entry in arr.iter_mut() {
                    if let Some(hs) = entry.get_mut("hooks").and_then(Value::as_array_mut) {
                        let before = hs.len();
                        hs.retain(|h| {
                            !h.get("command")
                                .and_then(Value::as_str)
                                .map(is_iris_hook)
                                .unwrap_or(false)
                        });
                        removed += before - hs.len();
                    }
                }
                arr.retain(|entry| {
                    entry
                        .get("hooks")
                        .and_then(Value::as_array)
                        .map(|hooks| !hooks.is_empty())
                        .unwrap_or(true)
                });
            }
        }
    }
    if removed == 0 {
        return Ok(format!("no iris hook found in {}", path.display()));
    }
    let out = serde_json::to_string_pretty(&root).map_err(|e| e.to_string())?;
    std::fs::write(&path, out).map_err(|e| e.to_string())?;
    Ok(format!(
        "removed {removed} iris hook(s) from {}",
        path.display()
    ))
}

/// Register iris lifecycle hooks in settings.json (idempotent).
/// `project` targets `./.claude/settings.json` instead of the global one.
pub fn install_hook(project: bool) -> Result<String, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let command = format!("{} hook", exe.display());

    let settings_path = settings_path(project)?;

    let mut root: Value = std::fs::read_to_string(&settings_path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| json!({}));
    if !root.is_object() {
        return Err(format!("{} is not a JSON object", settings_path.display()));
    }

    let hooks = root
        .as_object_mut()
        .unwrap()
        .entry("hooks")
        .or_insert_with(|| json!({}));
    let hooks = hooks.as_object_mut().ok_or("hooks is not an object")?;
    let mut added = 0usize;
    for event in TRACE_HOOK_EVENTS {
        let entries = hooks
            .entry((*event).to_string())
            .or_insert_with(|| json!([]));
        let entries = entries
            .as_array_mut()
            .ok_or_else(|| format!("hooks.{event} is not an array"))?;
        let installed = entries.iter().any(|entry| {
            entry
                .get("hooks")
                .and_then(Value::as_array)
                .is_some_and(|registered| {
                    registered.iter().any(|hook| {
                        hook.get("command").and_then(Value::as_str) == Some(command.as_str())
                    })
                })
        });
        if !installed {
            let hook = if *event == "PreToolUse" {
                json!({ "type": "command", "command": command, "timeout": 30 })
            } else {
                json!({ "type": "command", "command": command, "async": true })
            };
            let entry = if matches!(*event, "PreToolUse" | "PostToolUse" | "PostToolUseFailure") {
                json!({ "matcher": "*", "hooks": [hook] })
            } else {
                json!({ "hooks": [hook] })
            };
            entries.push(entry);
            added += 1;
        }
    }

    if let Some(parent) = settings_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let out = serde_json::to_string_pretty(&root).map_err(|e| e.to_string())?;
    std::fs::write(&settings_path, out).map_err(|e| e.to_string())?;
    let outcome = if added == 0 {
        "already installed"
    } else {
        "installed"
    };
    Ok(format!(
        "{outcome} {} Iris capture hooks in {}\ncommand: {command}\nRestart Claude Code sessions to pick them up.",
        TRACE_HOOK_EVENTS.len(),
        settings_path.display()
    ))
}

fn emit(decision: &str, reason: &str) {
    let v = json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": decision,
            "permissionDecisionReason": reason,
        }
    });
    println!("{v}");
}

/// One-line summary of a tool's input for display in the approval prompt.
fn tool_brief(name: &str, input: Option<&Value>) -> String {
    let input = match input {
        Some(v) => v,
        None => return String::new(),
    };
    let s = |k: &str| input.get(k).and_then(Value::as_str).unwrap_or("");
    let picked = match name {
        "Bash" => s("command"),
        "Read" | "Edit" | "Write" | "NotebookEdit" => s("file_path"),
        "Grep" | "Glob" => s("pattern"),
        "WebFetch" => s("url"),
        "WebSearch" => s("query"),
        _ => "",
    };
    let picked = if picked.is_empty() {
        input
            .as_object()
            .and_then(|o| o.values().find_map(Value::as_str))
            .unwrap_or("")
    } else {
        picked
    };
    let one = picked.split_whitespace().collect::<Vec<_>>().join(" ");
    one.chars().take(120).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::sync::Mutex;
    use tempfile::TempDir;

    // The filesystem tests share one process-wide env var (IRIS_BASE_DIR), so
    // they must not run concurrently. This lock serializes them; the pure tests
    // below take no lock and run freely in parallel. `unwrap_or_else(into_inner)`
    // shrugs off a poisoned lock so one panicking test can't cascade-fail the rest.
    static FS_LOCK: Mutex<()> = Mutex::new(());

    fn fs_lock() -> std::sync::MutexGuard<'static, ()> {
        FS_LOCK.lock().unwrap_or_else(|p| p.into_inner())
    }

    struct HomeGuard(Option<OsString>);

    impl Drop for HomeGuard {
        fn drop(&mut self) {
            if let Some(home) = self.0.take() {
                std::env::set_var("HOME", home);
            } else {
                std::env::remove_var("HOME");
            }
        }
    }

    /// Point iris at a fresh, empty temp dir for the duration of a test. The
    /// returned `TempDir` deletes itself (and everything in it) the moment it is
    /// dropped at the end of the test, so nothing leaks into /tmp and no test
    /// ever sees another's files. Keep the guard bound (`let _dir = ...`) so it
    /// lives for the whole test.
    fn use_temp_base() -> TempDir {
        let dir = TempDir::new().expect("create temp dir");
        std::env::set_var("IRIS_BASE_DIR", dir.path());
        dir
    }

    // ---- The safety contract: "never hang a session" --------------------

    #[test]
    fn heartbeat_fresh_at_and_below_threshold_is_live() {
        assert!(heartbeat_fresh(0), "a brand-new heartbeat must be live");
        assert!(
            heartbeat_fresh(HEARTBEAT_STALE_SECS),
            "exactly at the threshold still counts as live"
        );
    }

    #[test]
    fn heartbeat_past_threshold_is_not_live() {
        assert!(
            !heartbeat_fresh(HEARTBEAT_STALE_SECS + 1),
            "one second past the threshold must read as NOT live, so the hook defers"
        );
        assert!(!heartbeat_fresh(9999), "an ancient heartbeat is never live");
    }

    #[test]
    fn missing_heartbeat_reads_as_not_live() {
        let _guard = fs_lock();
        let _dir = use_temp_base();
        // No heartbeat file exists in the fresh temp dir.
        assert!(
            !iris_live(),
            "with no heartbeat file the hook must treat iris as down and defer"
        );
    }

    #[test]
    fn fresh_heartbeat_reads_as_live() {
        let _guard = fs_lock();
        let _dir = use_temp_base();
        touch_heartbeat();
        assert!(iris_live(), "a heartbeat just written must read as live");
    }

    // ---- Gating (opt-in interception) -----------------------------------

    #[test]
    fn gating_round_trips() {
        let _guard = fs_lock();
        let _dir = use_temp_base();
        assert!(!gating_armed(), "gating starts disarmed");
        set_gating(true);
        assert!(gating_armed(), "set_gating(true) arms it");
        set_gating(false);
        assert!(!gating_armed(), "set_gating(false) disarms it");
    }

    // ---- Request lifecycle / GC -----------------------------------------

    fn write_request(id: &str, ts: u64) {
        ensure_dirs();
        let req = json!({
            "id": id,
            "session_id": "sess",
            "tool_name": "Bash",
            "brief": "echo hi",
            "tool_input": { "command": "echo hi" },
            "cwd": "/tmp",
            "ts": ts,
        });
        std::fs::write(requests_dir().join(format!("{id}.json")), req.to_string()).unwrap();
    }

    #[test]
    fn load_pending_keeps_fresh_and_reaps_stale() {
        let _guard = fs_lock();
        let _dir = use_temp_base();
        write_request("fresh", now_secs());
        write_request("old", now_secs().saturating_sub(REQUEST_STALE_SECS + 5));

        let pending = load_pending();
        let ids: Vec<_> = pending.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(pending.len(), 1, "only the fresh request should survive");
        assert!(ids.contains(&"fresh"));
        assert!(
            !requests_dir().join("old.json").exists(),
            "the stale request file must be deleted, not left as a phantom"
        );
    }

    #[test]
    fn write_decision_records_choice_and_drops_request() {
        let _guard = fs_lock();
        let _dir = use_temp_base();
        write_request("abc", now_secs());
        write_decision("abc", true, "looks safe");

        assert!(
            !requests_dir().join("abc.json").exists(),
            "the request must be removed so the TUI stops showing it"
        );
        let body = std::fs::read_to_string(decisions_dir().join("abc.json")).unwrap();
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["decision"], "allow");
        assert_eq!(v["reason"], "looks safe");
    }

    // ---- Pure helpers ---------------------------------------------------

    #[test]
    fn tool_brief_picks_the_meaningful_field() {
        let input = json!({ "command": "cargo test --all" });
        assert_eq!(tool_brief("Bash", Some(&input)), "cargo test --all");

        let input = json!({ "file_path": "/etc/hosts" });
        assert_eq!(tool_brief("Read", Some(&input)), "/etc/hosts");

        assert_eq!(tool_brief("Bash", None), "");
    }

    #[test]
    fn is_iris_hook_matches_only_the_iris_hook_command() {
        assert!(is_iris_hook("/home/me/.cargo/bin/iris hook"));
        assert!(is_iris_hook("iris hook"));
        assert!(!is_iris_hook("iris ls"));
        assert!(!is_iris_hook("some-other-tool hook"));
    }

    #[test]
    fn hook_installation_is_idempotent_and_uninstall_preserves_other_hooks() {
        let _guard = fs_lock();
        let home = TempDir::new().unwrap();
        let _home_guard = HomeGuard(std::env::var_os("HOME"));
        std::env::set_var("HOME", home.path());
        let settings = home.path().join(".claude/settings.json");
        std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
        std::fs::write(
            &settings,
            r#"{"hooks":{"PostToolUse":[{"matcher":"Write","hooks":[{"type":"command","command":"other-recorder"}]}]}}"#,
        )
        .unwrap();

        assert!(install_hook(false).unwrap().starts_with("installed"));
        let installed: Value =
            serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
        for event in TRACE_HOOK_EVENTS {
            let entries = installed["hooks"][*event].as_array().unwrap();
            assert_eq!(
                entries
                    .iter()
                    .flat_map(|entry| entry["hooks"].as_array().into_iter().flatten())
                    .filter(|hook| hook["command"].as_str().is_some_and(is_iris_hook))
                    .count(),
                1,
                "exactly one Iris hook should be installed for {event}"
            );
        }
        assert_eq!(
            installed["hooks"]["PostToolUse"][1]["hooks"][0]["async"],
            true
        );
        assert!(install_hook(false)
            .unwrap()
            .starts_with("already installed"));
        assert!(uninstall_hook(false).unwrap().starts_with("removed"));

        let uninstalled: Value =
            serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
        assert_eq!(
            uninstalled["hooks"]["PostToolUse"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            uninstalled["hooks"]["PostToolUse"][0]["hooks"][0]["command"],
            "other-recorder"
        );
        assert!(uninstalled["hooks"]["PreToolUse"]
            .as_array()
            .unwrap()
            .is_empty());
    }
}
