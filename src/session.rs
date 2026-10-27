//! A `Session` is one Claude Code transcript file (`<uuid>.jsonl`). We tail it
//! incrementally — tracking the byte offset already consumed — so refreshes stay
//! cheap no matter how large the log grows.

use std::collections::{BTreeMap, VecDeque};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use chrono::{DateTime, Local};
use serde_json::Value;

/// Keep the in-memory activity feed bounded; older events scroll off.
const MAX_EVENTS: usize = 500;
const TEXT_CLAMP: usize = 200;

#[derive(Default, Clone, Copy)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_creation: u64,
    pub cache_read: u64,
}

impl Usage {
    fn accumulate(&mut self, m: &Value) {
        self.input += m.get("input_tokens").and_then(Value::as_u64).unwrap_or(0);
        self.output += m.get("output_tokens").and_then(Value::as_u64).unwrap_or(0);
        self.cache_creation += m
            .get("cache_creation_input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        self.cache_read += m
            .get("cache_read_input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
    }

    /// Total tokens that flowed through the context (in + out + cache traffic).
    pub fn total(&self) -> u64 {
        self.input + self.output + self.cache_creation + self.cache_read
    }
}

#[derive(Clone)]
pub enum EventKind {
    Prompt,
    Assistant,
    Thinking,
    Tool(String),
    ToolResult { error: bool },
}

#[derive(Clone)]
pub struct Event {
    pub ts: Option<DateTime<Local>>,
    pub kind: EventKind,
    pub text: String,
}

#[derive(Clone)]
pub struct TranscriptRecord {
    pub path: PathBuf,
    pub offset: u64,
    pub value: Value,
}

/// What a session is doing right now, derived from its transcript tail.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// Blocked on a tool_use that never got a result — waiting for you to
    /// approve a permission prompt (or a long tool is still running).
    NeedsApproval,
    /// Actively producing output / a tool just fired.
    Working,
    /// Turn finished cleanly; awaiting your next prompt.
    Done,
    /// Quiet for a while.
    Idle,
}

impl Status {
    /// Lower rank sorts first — surface sessions that need you at the top.
    pub fn rank(self) -> u8 {
        match self {
            Status::NeedsApproval => 0,
            Status::Working => 1,
            Status::Done => 2,
            Status::Idle => 3,
        }
    }
}

pub struct Session {
    pub path: PathBuf,
    pub id: String,
    pub title: Option<String>,
    pub last_prompt: Option<String>,
    pub first_prompt: Option<String>,
    pub cwd: Option<String>,
    pub branch: Option<String>,
    pub model: Option<String>,
    pub usage: Usage,
    pub events: VecDeque<Event>,
    pub tool_counts: BTreeMap<String, u64>,
    pub assistant_turns: u64,
    pub last_ts: Option<DateTime<Local>>,
    pub mtime: SystemTime,

    /// Set while the last assistant turn ended on a tool_use with no result yet
    /// — i.e. the session is blocked (awaiting permission, or the tool is
    /// running). Holds the tool name for display.
    pub pending_tool: Option<String>,
    /// Last assistant turn ended naturally (`end_turn`/`stop_sequence`).
    pub turn_done: bool,

    /// Set by the app each refresh from the hook bridge: a live approval request
    /// exists for this session. This is the authoritative, actionable "waiting on
    /// a human" signal — far more reliable than the transcript heuristic.
    pub live_request: bool,

    // tail bookkeeping
    offset: u64,
    pending: String,
    pending_offset: u64,
    new_records: Vec<TranscriptRecord>,
}

impl Session {
    pub fn new(path: PathBuf) -> Self {
        let id = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("?")
            .to_string();
        Session {
            path,
            id,
            title: None,
            last_prompt: None,
            first_prompt: None,
            cwd: None,
            branch: None,
            model: None,
            usage: Usage::default(),
            events: VecDeque::new(),
            tool_counts: BTreeMap::new(),
            assistant_turns: 0,
            last_ts: None,
            mtime: SystemTime::UNIX_EPOCH,
            pending_tool: None,
            turn_done: false,
            live_request: false,
            offset: 0,
            pending: String::new(),
            pending_offset: 0,
            new_records: Vec::new(),
        }
    }

    /// A short human label for the list pane.
    pub fn label(&self) -> String {
        if let Some(t) = &self.title {
            return clamp(t, 60);
        }
        if let Some(p) = self.last_prompt.as_ref().or(self.first_prompt.as_ref()) {
            return clamp(p, 60);
        }
        format!("session {}", &self.id[..self.id.len().min(8)])
    }

    /// Seconds since the transcript was last written.
    pub fn age_secs(&self) -> u64 {
        SystemTime::now()
            .duration_since(self.mtime)
            .map(|d| d.as_secs())
            .unwrap_or(u64::MAX)
    }

    /// Current activity state. A blocked tool_use that has sat quiet for a few
    /// seconds is almost certainly waiting on a human (tools finish fast;
    /// permission prompts wait forever).
    pub fn status(&self) -> Status {
        let age = self.age_secs();
        // A live hook request is the only reliable, *actionable* "needs
        // approval" signal — and the only thing a/d in iris can act on. Driving
        // the red state purely off it means an abandoned tool_use can never get
        // stuck reading as NEEDS APPROVAL forever.
        if self.live_request {
            return Status::NeedsApproval;
        }
        if self.pending_tool.is_some() {
            // Ended on a tool_use with no result yet: the tool is running, or the
            // session was quietly abandoned mid-call. Neither needs you.
            return if age <= 20 {
                Status::Working
            } else {
                Status::Idle
            };
        }
        if self.turn_done {
            return if age <= 8 { Status::Done } else { Status::Idle };
        }
        if age <= 20 {
            Status::Working
        } else {
            Status::Idle
        }
    }

    /// Last path component of the working directory, e.g. `iris`.
    pub fn project(&self) -> &str {
        self.cwd
            .as_deref()
            .map(|c| c.rsplit('/').next().unwrap_or(c))
            .unwrap_or("?")
    }

    /// Build a compact text snapshot of the session to send to the summarizer.
    /// Header fields plus the recent activity feed, length-bounded.
    pub fn digest(&self) -> String {
        let status = match self.status() {
            Status::NeedsApproval => "waiting for tool approval",
            Status::Working => "working",
            Status::Done => "turn finished, awaiting user",
            Status::Idle => "idle",
        };
        let mut out = String::new();
        out.push_str(&format!("Title: {}\n", self.label()));
        out.push_str(&format!(
            "Project dir: {}\n",
            self.cwd.as_deref().unwrap_or("?")
        ));
        if let Some(b) = &self.branch {
            out.push_str(&format!("Git branch: {b}\n"));
        }
        out.push_str(&format!(
            "Model: {}\n",
            self.model.as_deref().unwrap_or("?")
        ));
        out.push_str(&format!("Status: {status}\n"));
        if let Some(t) = &self.pending_tool {
            out.push_str(&format!("Blocked on tool: {t}\n"));
        }
        out.push_str(&format!(
            "Tokens: in {} / out {}\n\n",
            self.usage.input, self.usage.output
        ));
        out.push_str("Recent activity (oldest to newest):\n");

        // Take the tail of the feed, then bound total size.
        let start = self.events.len().saturating_sub(60);
        for e in self.events.iter().skip(start) {
            let line = match &e.kind {
                EventKind::Prompt => format!("[user] {}", e.text),
                EventKind::Assistant => format!("[claude] {}", e.text),
                EventKind::Thinking => format!("[thinking] {}", e.text),
                EventKind::Tool(name) => format!("[tool {name}] {}", e.text),
                EventKind::ToolResult { error } => {
                    let tag = if *error { "result error" } else { "result" };
                    format!("[{tag}] {}", e.text)
                }
            };
            out.push_str(&line);
            out.push('\n');
            if out.len() > 6000 {
                break;
            }
        }
        out
    }

    /// Read any bytes appended since the last refresh. Returns true if new
    /// lines were processed.
    pub fn refresh(&mut self) -> std::io::Result<bool> {
        let meta = std::fs::metadata(&self.path)?;
        self.mtime = meta.modified().unwrap_or(SystemTime::now());
        let len = meta.len();

        if len < self.offset {
            // File was truncated or rotated — re-read from the top.
            self.offset = 0;
            self.pending.clear();
            self.pending_offset = 0;
        }
        if len == self.offset {
            return Ok(false);
        }

        let read_from = self.offset;
        let mut f = File::open(&self.path)?;
        f.seek(SeekFrom::Start(self.offset))?;
        let mut bytes = Vec::new();
        f.take(len - self.offset).read_to_end(&mut bytes)?;
        self.offset = len;
        if self.pending.is_empty() {
            self.pending_offset = read_from;
        }
        self.pending.push_str(&String::from_utf8_lossy(&bytes));

        while let Some(idx) = self.pending.find('\n') {
            let line: String = self.pending.drain(..=idx).collect();
            let record_offset = self.pending_offset;
            self.pending_offset += line.len() as u64;
            let line = line.trim();
            if !line.is_empty() {
                if let Ok(v) = serde_json::from_str::<Value>(line) {
                    self.process(&v);
                    self.new_records.push(TranscriptRecord {
                        path: self.path.clone(),
                        offset: record_offset,
                        value: v,
                    });
                }
            }
        }
        Ok(true)
    }

    pub fn take_new_records(&mut self) -> Vec<TranscriptRecord> {
        std::mem::take(&mut self.new_records)
    }

    pub fn restore_new_records(&mut self, mut records: Vec<TranscriptRecord>) {
        records.append(&mut self.new_records);
        self.new_records = records;
    }

    fn process(&mut self, v: &Value) {
        let ts = v
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|d| d.with_timezone(&Local));
        if ts.is_some() {
            self.last_ts = ts;
        }
        if let Some(c) = v.get("cwd").and_then(Value::as_str) {
            self.cwd = Some(c.to_string());
        }
        if let Some(b) = v.get("gitBranch").and_then(Value::as_str) {
            if !b.is_empty() {
                self.branch = Some(b.to_string());
            }
        }

        match v.get("type").and_then(Value::as_str).unwrap_or("") {
            "ai-title" => {
                if let Some(t) = v.get("aiTitle").and_then(Value::as_str) {
                    self.title = Some(t.to_string());
                }
            }
            "last-prompt" => {
                if let Some(p) = v.get("lastPrompt").and_then(Value::as_str) {
                    self.last_prompt = Some(p.to_string());
                }
            }
            "user" => self.process_user(v, ts),
            "assistant" => self.process_assistant(v, ts),
            _ => {}
        }
    }

    fn process_user(&mut self, v: &Value, ts: Option<DateTime<Local>>) {
        // Any user line (a fresh prompt or a tool_result) unblocks the session.
        self.pending_tool = None;
        self.turn_done = false;
        let content = match v.get("message").and_then(|m| m.get("content")) {
            Some(c) => c,
            None => return,
        };
        match content {
            Value::String(s) => self.add_prompt(s, ts),
            Value::Array(blocks) => {
                let mut text = String::new();
                let mut has_tool_result = false;
                for b in blocks {
                    match b.get("type").and_then(Value::as_str) {
                        Some("tool_result") => {
                            has_tool_result = true;
                            let error = b.get("is_error").and_then(Value::as_bool).unwrap_or(false);
                            let brief = result_brief(b.get("content"));
                            self.push(Event {
                                ts,
                                kind: EventKind::ToolResult { error },
                                text: brief,
                            });
                        }
                        Some("text") => {
                            if let Some(t) = b.get("text").and_then(Value::as_str) {
                                text.push_str(t);
                            }
                        }
                        _ => {}
                    }
                }
                if !has_tool_result && !text.trim().is_empty() {
                    self.add_prompt(&text, ts);
                }
            }
            _ => {}
        }
    }

    fn add_prompt(&mut self, s: &str, ts: Option<DateTime<Local>>) {
        let s = s.trim();
        if s.is_empty() {
            return;
        }
        if self.first_prompt.is_none() {
            self.first_prompt = Some(s.to_string());
        }
        self.push(Event {
            ts,
            kind: EventKind::Prompt,
            text: clamp(s, TEXT_CLAMP),
        });
    }

    fn process_assistant(&mut self, v: &Value, ts: Option<DateTime<Local>>) {
        let msg = match v.get("message") {
            Some(m) => m,
            None => return,
        };
        self.assistant_turns += 1;
        if let Some(model) = msg.get("model").and_then(Value::as_str) {
            self.model = Some(model.to_string());
        }
        if let Some(u) = msg.get("usage") {
            self.usage.accumulate(u);
        }
        let mut last_tool: Option<String> = None;
        if let Some(blocks) = msg.get("content").and_then(Value::as_array) {
            for b in blocks {
                match b.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        if let Some(t) = b.get("text").and_then(Value::as_str) {
                            if !t.trim().is_empty() {
                                self.push(Event {
                                    ts,
                                    kind: EventKind::Assistant,
                                    text: clamp(t, TEXT_CLAMP),
                                });
                            }
                        }
                    }
                    Some("thinking") => {
                        let t = b.get("thinking").and_then(Value::as_str).unwrap_or("");
                        self.push(Event {
                            ts,
                            kind: EventKind::Thinking,
                            text: clamp(t, TEXT_CLAMP),
                        });
                    }
                    Some("tool_use") => {
                        let name = b
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("tool")
                            .to_string();
                        *self.tool_counts.entry(name.clone()).or_insert(0) += 1;
                        let brief = tool_brief(&name, b.get("input"));
                        last_tool = Some(name.clone());
                        self.push(Event {
                            ts,
                            kind: EventKind::Tool(name),
                            text: brief,
                        });
                    }
                    _ => {}
                }
            }
        }

        // If the turn ended on a tool call, the session is now blocked waiting
        // for that tool's result (permission approval or execution). Otherwise
        // the turn is complete.
        if let Some(tool) = last_tool {
            self.pending_tool = Some(tool);
            self.turn_done = false;
        } else {
            self.pending_tool = None;
            let stop = msg.get("stop_reason").and_then(Value::as_str).unwrap_or("");
            self.turn_done = matches!(stop, "end_turn" | "stop_sequence");
        }
    }

    fn push(&mut self, e: Event) {
        self.events.push_back(e);
        while self.events.len() > MAX_EVENTS {
            self.events.pop_front();
        }
    }
}

/// Summarize a `tool_use` input into one readable line.
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
        "Agent" | "Task" => {
            let d = s("description");
            if d.is_empty() {
                s("subagent_type")
            } else {
                d
            }
        }
        "Skill" => s("skill"),
        "WebFetch" => s("url"),
        "WebSearch" => s("query"),
        "ToolSearch" => s("query"),
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
    clamp(picked, 100)
}

fn result_brief(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => clamp(s, 100),
        Some(Value::Array(a)) => {
            let mut out = String::new();
            for b in a {
                if let Some(t) = b.get("text").and_then(Value::as_str) {
                    out.push_str(t);
                }
            }
            clamp(&out, 100)
        }
        _ => String::new(),
    }
}

/// Clamp to `max` chars on a single line, appending `…` when truncated.
fn clamp(s: &str, max: usize) -> String {
    let one = s.replace(['\n', '\r', '\t'], " ");
    let one = one.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() <= max {
        one
    } else {
        let cut: String = one.chars().take(max.saturating_sub(1)).collect();
        format!("{cut}…")
    }
}

pub fn discover(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(_) => return out,
    };
    for entry in rd.flatten() {
        let p = entry.path();
        if !p.is_dir() {
            continue;
        }
        if let Ok(inner) = std::fs::read_dir(&p) {
            for e2 in inner.flatten() {
                let p2 = e2.path();
                if p2.extension().is_some_and(|x| x == "jsonl") {
                    out.push(p2);
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    // Each test writes its own transcript into its own TempDir (auto-deleted on
    // drop), so tests are fully isolated, parallel-safe, and never touch the
    // real ~/.claude/projects.

    /// Create a transcript file containing `lines` and a Session tailing it.
    fn session_with(dir: &TempDir, lines: &[&str]) -> Session {
        let path = dir.path().join("test-session.jsonl");
        let mut f = File::create(&path).unwrap();
        for l in lines {
            writeln!(f, "{l}").unwrap();
        }
        let mut s = Session::new(path);
        s.refresh().unwrap();
        s
    }

    fn append(s: &Session, lines: &[&str]) {
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&s.path)
            .unwrap();
        for l in lines {
            writeln!(f, "{l}").unwrap();
        }
    }

    // ---- Format tolerance: garbage in, no panic, good lines still parse ----

    #[test]
    fn malformed_lines_are_skipped_without_breaking_good_ones() {
        let dir = TempDir::new().unwrap();
        let s = session_with(
            &dir,
            &[
                "not json at all {{{",
                "",
                r#"{"type":"user","message":{"content":"hello iris"}}"#,
                r#"{"type": 42, "message": null}"#,
                r#"{"unknown_type_entirely": true}"#,
                r#"[1,2,3]"#,
                r#""just a string""#,
            ],
        );
        // The one valid prompt survived; the garbage was ignored.
        assert_eq!(s.first_prompt.as_deref(), Some("hello iris"));
        assert_eq!(s.events.len(), 1);
    }

    #[test]
    fn empty_file_parses_to_empty_session() {
        let dir = TempDir::new().unwrap();
        let s = session_with(&dir, &[]);
        assert!(s.events.is_empty());
        assert!(s.first_prompt.is_none());
        assert_eq!(s.usage.total(), 0);
    }

    #[test]
    fn missing_optional_fields_are_tolerated() {
        let dir = TempDir::new().unwrap();
        // assistant with no model, no usage, no content; user with no message.
        let s = session_with(
            &dir,
            &[r#"{"type":"assistant","message":{}}"#, r#"{"type":"user"}"#],
        );
        assert_eq!(s.assistant_turns, 1);
        assert!(s.model.is_none());
        assert_eq!(s.usage.total(), 0);
    }

    // ---- Metadata extraction ----

    #[test]
    fn extracts_cwd_branch_model_title_and_usage() {
        let dir = TempDir::new().unwrap();
        let s = session_with(
            &dir,
            &[
                r#"{"type":"ai-title","aiTitle":"Fixing the parser"}"#,
                r#"{"type":"user","cwd":"/home/me/proj","gitBranch":"main","message":{"content":"do it"}}"#,
                r#"{"type":"assistant","message":{"model":"claude-opus-4-8","usage":{"input_tokens":100,"output_tokens":50,"cache_creation_input_tokens":10,"cache_read_input_tokens":5},"content":[{"type":"text","text":"done"}],"stop_reason":"end_turn"}}"#,
            ],
        );
        assert_eq!(s.title.as_deref(), Some("Fixing the parser"));
        assert_eq!(s.cwd.as_deref(), Some("/home/me/proj"));
        assert_eq!(s.project(), "proj");
        assert_eq!(s.branch.as_deref(), Some("main"));
        assert_eq!(s.model.as_deref(), Some("claude-opus-4-8"));
        assert_eq!(s.usage.input, 100);
        assert_eq!(s.usage.output, 50);
        assert_eq!(s.usage.cache_creation, 10);
        assert_eq!(s.usage.cache_read, 5);
        assert_eq!(s.usage.total(), 165);
    }

    #[test]
    fn usage_accumulates_across_turns() {
        let dir = TempDir::new().unwrap();
        let turn = r#"{"type":"assistant","message":{"usage":{"input_tokens":10,"output_tokens":5},"content":[]}}"#;
        let s = session_with(&dir, &[turn, turn, turn]);
        assert_eq!(s.assistant_turns, 3);
        assert_eq!(s.usage.input, 30);
        assert_eq!(s.usage.output, 15);
    }

    // ---- Blocked / done state machine ----

    #[test]
    fn turn_ending_on_tool_use_sets_pending_tool() {
        let dir = TempDir::new().unwrap();
        let s = session_with(
            &dir,
            &[
                r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"ls"}}]}}"#,
            ],
        );
        assert_eq!(s.pending_tool.as_deref(), Some("Bash"));
        assert!(!s.turn_done);
        assert_eq!(s.tool_counts.get("Bash"), Some(&1));
        // Fresh file (age ~0) + pending tool ⇒ Working, not NeedsApproval:
        // only a live hook request may show the red state.
        assert_eq!(s.status().rank(), Status::Working.rank());
    }

    #[test]
    fn tool_result_unblocks_the_session() {
        let dir = TempDir::new().unwrap();
        let mut s = session_with(
            &dir,
            &[
                r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"ls"}}]}}"#,
            ],
        );
        assert!(s.pending_tool.is_some());
        append(
            &s,
            &[
                r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"file1\nfile2"}]}}"#,
            ],
        );
        s.refresh().unwrap();
        assert!(
            s.pending_tool.is_none(),
            "a tool_result must clear the block"
        );
    }

    #[test]
    fn end_turn_marks_done() {
        let dir = TempDir::new().unwrap();
        let s = session_with(
            &dir,
            &[
                r#"{"type":"assistant","message":{"content":[{"type":"text","text":"all done"}],"stop_reason":"end_turn"}}"#,
            ],
        );
        assert!(s.turn_done);
        assert!(s.pending_tool.is_none());
        assert_eq!(s.status().rank(), Status::Done.rank());
    }

    // ---- Incremental tailing ----

    #[test]
    fn refresh_picks_up_only_appended_lines() {
        let dir = TempDir::new().unwrap();
        let mut s = session_with(&dir, &[r#"{"type":"user","message":{"content":"first"}}"#]);
        assert_eq!(s.events.len(), 1);

        // No new bytes ⇒ refresh reports nothing to do.
        assert!(!s.refresh().unwrap());

        append(&s, &[r#"{"type":"user","message":{"content":"second"}}"#]);
        assert!(s.refresh().unwrap());
        assert_eq!(s.events.len(), 2, "old lines must not be re-processed");
        assert_eq!(s.first_prompt.as_deref(), Some("first"));
    }

    #[test]
    fn partial_line_is_buffered_until_newline_arrives() {
        let dir = TempDir::new().unwrap();
        let mut s = session_with(&dir, &[]);

        // Write half a JSON line with NO trailing newline (a writer mid-flush).
        let half = r#"{"type":"user","message":{"content":"split"#;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&s.path)
            .unwrap();
        write!(f, "{half}").unwrap();
        drop(f);
        s.refresh().unwrap();
        assert!(s.events.is_empty(), "half a line must not be parsed");

        // Complete the line.
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&s.path)
            .unwrap();
        writeln!(f, r#" prompt"}}}}"#).unwrap();
        drop(f);
        s.refresh().unwrap();
        assert_eq!(
            s.events.len(),
            1,
            "completed line parses on the next refresh"
        );
        assert_eq!(s.first_prompt.as_deref(), Some("split prompt"));
    }

    #[test]
    fn captured_records_keep_byte_offsets_across_partial_writes() {
        let dir = TempDir::new().unwrap();
        let mut s = session_with(&dir, &[]);
        let first = r#"{"type":"user","message":{"content":"first"}}"#;
        let split = first.len() / 2;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&s.path)
            .unwrap();
        file.write_all(&first.as_bytes()[..split]).unwrap();
        drop(file);
        s.refresh().unwrap();
        assert!(s.take_new_records().is_empty());

        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&s.path)
            .unwrap();
        file.write_all(&first.as_bytes()[split..]).unwrap();
        file.write_all(b"\n").unwrap();
        drop(file);
        s.refresh().unwrap();
        let first_record = s.take_new_records().pop().unwrap();
        assert_eq!(first_record.offset, 0);

        let second = r#"{"type":"user","message":{"content":"second"}}"#;
        append(&s, &[second]);
        s.refresh().unwrap();
        let second_record = s.take_new_records().pop().unwrap();
        assert_eq!(second_record.offset, (first.len() + 1) as u64);
    }

    #[test]
    fn truncated_file_is_reread_from_the_top() {
        let dir = TempDir::new().unwrap();
        let mut s = session_with(
            &dir,
            &[r#"{"type":"user","message":{"content":"old long line that makes the file big"}}"#],
        );
        assert_eq!(s.events.len(), 1);

        // Rotate: replace with a shorter file.
        std::fs::write(
            &s.path,
            "{\"type\":\"user\",\"message\":{\"content\":\"new\"}}\n",
        )
        .unwrap();
        s.refresh().unwrap();
        assert_eq!(
            s.events.back().unwrap().text,
            "new",
            "after truncation the parser must restart from offset 0"
        );
    }

    // ---- Pure helpers ----

    #[test]
    fn clamp_shortens_and_flattens_whitespace() {
        assert_eq!(clamp("a\nb\tc", 100), "a b c");
        assert_eq!(clamp("  spaced   out  ", 100), "spaced out");
        let long = "x".repeat(300);
        let out = clamp(&long, 10);
        assert_eq!(out.chars().count(), 10);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn discover_finds_jsonl_files_one_level_deep() {
        let dir = TempDir::new().unwrap();
        let proj = dir.path().join("-home-me-proj");
        std::fs::create_dir(&proj).unwrap();
        std::fs::write(proj.join("abc.jsonl"), "").unwrap();
        std::fs::write(proj.join("notes.txt"), "").unwrap();
        std::fs::write(dir.path().join("toplevel.jsonl"), "").unwrap(); // wrong depth

        let found = discover(dir.path());
        assert_eq!(found.len(), 1);
        assert!(found[0].ends_with("abc.jsonl"));
    }
}
