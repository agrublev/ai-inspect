//! Application state: owns every known session reader, decides which are
//! "active", and keeps a stable selection across refreshes.

use std::cell::{Cell, RefCell, RefMut};
use std::collections::{HashMap, HashSet};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use ratatui::widgets::ListState;
use serde_json::Value;

use crate::anthropic;
use crate::bridge::{self, Pending};
use crate::session::{discover, Session, Status};
use crate::trace::TraceStore;

/// State of an AI summary for one session.
pub enum SummaryState {
    Loading,
    Done(String),
    Error(String),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TracePane {
    Traces,
    Events,
    Payload,
}

fn key_file() -> PathBuf {
    bridge::base_dir().join("api_key")
}

fn hidden_file() -> PathBuf {
    bridge::base_dir().join("hidden")
}

/// Load the set of session paths the user has hidden from the dashboard.
/// Read-only over transcripts: this never touches the `.jsonl` files, it only
/// records which ones iris should skip rendering.
fn load_hidden() -> HashSet<PathBuf> {
    std::fs::read_to_string(hidden_file())
        .map(|s| {
            s.lines()
                .filter(|l| !l.is_empty())
                .map(PathBuf::from)
                .collect()
        })
        .unwrap_or_default()
}

/// Persist the hidden set, one path per line.
fn save_hidden(hidden: &HashSet<PathBuf>) {
    let _ = std::fs::create_dir_all(bridge::base_dir());
    let body: String = hidden
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join("\n");
    let _ = std::fs::write(hidden_file(), body);
}

/// Resolve the API key: `ANTHROPIC_API_KEY` wins, else the saved key file.
fn load_api_key() -> Option<String> {
    if let Some(k) = std::env::var("ANTHROPIC_API_KEY")
        .ok()
        .filter(|k| !k.is_empty())
    {
        return Some(k);
    }
    std::fs::read_to_string(key_file())
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Persist the key to `~/.claude/iris/api_key` with owner-only (0600) perms.
fn save_api_key(key: &str) -> std::io::Result<()> {
    std::fs::create_dir_all(bridge::base_dir())?;
    let path = key_file();
    std::fs::write(&path, key)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

/// A project with several active sessions collapses to a single header row.
/// The counts let us paint a one-line summary (`⚠12 ●3 ✓1 ○8`) without
/// expanding the group.
#[derive(Clone)]
pub struct GroupInfo {
    pub key: String,
    pub count: usize,
    pub pending: usize,
    pub needs: usize,
    pub working: usize,
    pub done: usize,
    pub idle: usize,
    pub collapsed: bool,
    /// Label of the group's top (highest-priority) session, for collapsed view.
    pub lead_label: String,
}

/// One rendered row in the list pane: either a project header or a session.
/// `grouped` sessions sit under a header and are indented.
#[derive(Clone)]
pub enum Row {
    Group(GroupInfo),
    Session { path: PathBuf, grouped: bool },
}

/// Stable identity of the selected row, so selection survives a regroup even
/// as rows are inserted/removed around it.
#[derive(Clone, PartialEq)]
enum SelKey {
    Group(String),
    Session(PathBuf),
}

/// Projects with at least this many active sessions auto-collapse on first
/// sight, so a noisy watcher (e.g. dozens of "Slack triage" runs) folds into
/// one line instead of flooding the list.
const GROUP_AUTOCOLLAPSE: usize = 4;

fn moved_index(current: usize, len: usize, delta: isize) -> Option<usize> {
    if len == 0 {
        return None;
    }
    let last = len - 1;
    Some(if delta < 0 {
        current.saturating_sub(delta.unsigned_abs())
    } else {
        current.saturating_add(delta as usize).min(last)
    })
}

pub struct App {
    pub projects_dir: PathBuf,
    pub window: Duration,
    pub interval: Duration,
    trace_store: Option<TraceStore>,
    pub trace_browser_open: bool,
    pub trace_pane: TracePane,
    pub trace_rows: Vec<Value>,
    pub trace_selected: usize,
    pub trace_events: Vec<Value>,
    pub trace_event_selected: usize,
    trace_payload_scroll: Cell<u16>,
    trace_payload_max_scroll: Cell<u16>,

    readers: HashMap<PathBuf, Session>,
    /// Paths of currently active sessions in grouped priority order (flat).
    pub visible: Vec<PathBuf>,
    /// What the list pane actually renders: group headers interleaved with the
    /// sessions of expanded groups.
    pub rows: Vec<Row>,
    pub selected: usize,
    selected_key: Option<SelKey>,
    /// Project groups the user (or auto-collapse) has folded shut.
    collapsed: HashSet<String>,
    /// Session transcript paths the user deleted from the dashboard view. The
    /// files on disk are untouched (read-only contract); these are just hidden.
    hidden: HashSet<PathBuf>,
    /// Groups already evaluated for auto-collapse, so a manual expand sticks.
    known_groups: HashSet<String>,

    last_refresh: Instant,
    pub should_quit: bool,

    // AI summaries
    api_key: Option<String>,
    pub summaries: HashMap<PathBuf, SummaryState>,
    pub popup_open: bool,
    summary_tx: Sender<(PathBuf, Result<String, String>)>,
    summary_rx: Receiver<(PathBuf, Result<String, String>)>,

    /// Pending tool-approval requests from the hook bridge, keyed by session id.
    pub pending: HashMap<String, Pending>,

    // API-key entry
    pub editing_key: bool,
    pub key_buffer: String,

    // Approval detail modal + AI risk assessment (keyed by request id)
    pub approve_open: bool,
    pub assessments: HashMap<String, SummaryState>,
    assess_tx: Sender<(String, Result<String, String>)>,
    assess_rx: Receiver<(String, Result<String, String>)>,

    /// Transient one-line status shown in the header.
    pub flash: Option<String>,

    /// True when the detail pane is "entered" for scrolling through the full
    /// activity feed of the selected session.
    pub focused: bool,
    /// Cursor + scroll offset for the feed, vim-style. ratatui keeps the
    /// selected line visible and remembers the offset across frames, so motion
    /// stays smooth. `RefCell` lets the (immutable) draw pass render it.
    feed_state: RefCell<ListState>,
    /// When the cursor sits on the last event, new events keep it pinned to the
    /// bottom — a live `tail -f` feel.
    feed_follow: bool,
    /// First half of a pending `gg` motion.
    pub pending_g: bool,
    /// Last rendered feed viewport height, stashed during draw so half/full-page
    /// motions can size themselves. `Cell` lets the immutable draw pass write it.
    feed_viewport: Cell<u16>,

    /// Whether an `iris hook` is registered in settings.json.
    pub hook_installed: bool,
    /// The install/enable-approvals proposal modal is showing.
    pub install_open: bool,
    last_pending_logged: usize,

    /// Approval gating armed: the hook intercepts other sessions and waits for
    /// dashboard decisions. When disarmed, iris is a passive viewer and tool
    /// calls flow through Claude Code's normal permission system with no delay.
    pub gating: bool,
}

impl App {
    pub fn new(projects_dir: PathBuf, window: Duration, interval: Duration) -> Self {
        let (summary_tx, summary_rx) = channel();
        let (assess_tx, assess_rx) = channel();
        let mut app = App {
            projects_dir,
            window,
            interval,
            trace_store: None,
            trace_browser_open: false,
            trace_pane: TracePane::Traces,
            trace_rows: Vec::new(),
            trace_selected: 0,
            trace_events: Vec::new(),
            trace_event_selected: 0,
            trace_payload_scroll: Cell::new(0),
            trace_payload_max_scroll: Cell::new(0),
            readers: HashMap::new(),
            visible: Vec::new(),
            rows: Vec::new(),
            selected: 0,
            selected_key: None,
            collapsed: HashSet::new(),
            hidden: load_hidden(),
            known_groups: HashSet::new(),
            last_refresh: Instant::now(),
            should_quit: false,
            api_key: load_api_key(),
            summaries: HashMap::new(),
            popup_open: false,
            summary_tx,
            summary_rx,
            pending: HashMap::new(),
            editing_key: false,
            key_buffer: String::new(),
            approve_open: false,
            assessments: HashMap::new(),
            assess_tx,
            assess_rx,
            flash: None,
            focused: false,
            feed_state: RefCell::new(ListState::default()),
            feed_follow: true,
            pending_g: false,
            feed_viewport: Cell::new(0),
            hook_installed: bridge::hook_installed(),
            install_open: false,
            last_pending_logged: 0,
            gating: false,
        };
        // Propose enabling approvals on first launch when the hook isn't set up.
        app.install_open = !app.hook_installed;
        // Start disarmed so a stale flag from a crashed session never silently
        // blocks other sessions; the user arms gating explicitly.
        bridge::set_gating(false);
        bridge::touch_heartbeat();
        bridge::log(&format!(
            "iris start: api_key={} hook_installed={}",
            app.api_key.is_some(),
            app.hook_installed
        ));
        app.refresh();
        app
    }

    pub fn tick(&mut self) {
        self.drain_summaries();
        if self.last_refresh.elapsed() >= self.interval {
            self.refresh();
        }
    }

    pub fn has_api_key(&self) -> bool {
        self.api_key.is_some()
    }

    pub fn open_install(&mut self) {
        self.install_open = true;
    }
    pub fn close_install(&mut self) {
        self.install_open = false;
    }

    pub fn open_trace_browser(&mut self) {
        if self.trace_store.is_none() {
            match TraceStore::open(bridge::trace_db_path()) {
                Ok(store) => self.trace_store = Some(store),
                Err(error) => {
                    self.flash = Some(format!("trace explorer unavailable: {error}"));
                    return;
                }
            }
        }
        self.trace_browser_open = true;
        self.trace_pane = TracePane::Traces;
        self.refresh_trace_browser();
    }

    pub fn close_trace_browser(&mut self) {
        self.trace_browser_open = false;
        self.pending_g = false;
    }

    pub fn trace_browser_focus(&mut self, backwards: bool) {
        self.trace_pane = match (self.trace_pane, backwards) {
            (TracePane::Traces, false) | (TracePane::Payload, true) => TracePane::Events,
            (TracePane::Events, false) | (TracePane::Traces, true) => TracePane::Payload,
            (TracePane::Payload, false) | (TracePane::Events, true) => TracePane::Traces,
        };
    }

    pub fn trace_browser_activate(&mut self) {
        self.trace_browser_focus(false);
    }

    pub fn trace_browser_move(&mut self, delta: isize) {
        match self.trace_pane {
            TracePane::Traces => {
                let Some(next) = moved_index(self.trace_selected, self.trace_rows.len(), delta)
                else {
                    return;
                };
                if next != self.trace_selected {
                    self.trace_selected = next;
                    self.load_trace_events(true);
                }
            }
            TracePane::Events => {
                if let Some(next) =
                    moved_index(self.trace_event_selected, self.trace_events.len(), delta)
                {
                    self.trace_event_selected = next;
                    self.trace_payload_scroll.set(0);
                }
            }
            TracePane::Payload => {
                let offset = (i32::from(self.trace_payload_scroll.get()) + (delta * 3) as i32)
                    .clamp(0, i32::from(self.trace_payload_max_scroll.get()))
                    as u16;
                self.trace_payload_scroll.set(offset);
            }
        }
    }

    pub fn trace_browser_page(&mut self, pages: isize) {
        match self.trace_pane {
            TracePane::Traces => self.trace_browser_move(pages * 5),
            TracePane::Events => self.trace_browser_move(pages * 5),
            TracePane::Payload => {
                let offset = (i32::from(self.trace_payload_scroll.get()) + (pages * 12) as i32)
                    .clamp(0, i32::from(self.trace_payload_max_scroll.get()))
                    as u16;
                self.trace_payload_scroll.set(offset);
            }
        }
    }

    pub fn trace_payload_scroll(&self) -> u16 {
        self.trace_payload_scroll.get()
    }

    pub fn set_trace_payload_max_scroll(&self, max_scroll: u16) {
        self.trace_payload_max_scroll.set(max_scroll);
        self.trace_payload_scroll
            .set(self.trace_payload_scroll.get().min(max_scroll));
    }

    pub fn refresh_trace_browser(&mut self) {
        let selected_id = self
            .trace_rows
            .get(self.trace_selected)
            .and_then(|trace| trace["trace_id"].as_str())
            .map(str::to_owned);
        let traces = match self.trace_store.as_ref().map(TraceStore::list_traces) {
            Some(Ok(traces)) => traces,
            Some(Err(error)) => {
                self.flash = Some(format!("trace refresh failed: {error}"));
                return;
            }
            None => return,
        };
        self.trace_rows = traces;
        self.trace_selected = selected_id
            .and_then(|id| {
                self.trace_rows
                    .iter()
                    .position(|trace| trace["trace_id"].as_str() == Some(id.as_str()))
            })
            .unwrap_or_else(|| {
                self.trace_selected
                    .min(self.trace_rows.len().saturating_sub(1))
            });
        self.load_trace_events(false);
    }

    pub fn selected_trace_event(&self) -> Option<&Value> {
        self.trace_events.get(self.trace_event_selected)
    }

    fn load_trace_events(&mut self, reset_selection: bool) {
        let trace_id = self
            .trace_rows
            .get(self.trace_selected)
            .and_then(|trace| trace["trace_id"].as_str())
            .map(str::to_owned);
        let events = match (self.trace_store.as_ref(), trace_id.as_deref()) {
            (Some(store), Some(trace_id)) => match store.events_for_trace(trace_id) {
                Ok(events) => events,
                Err(error) => {
                    self.flash = Some(format!("trace detail unavailable: {error}"));
                    Vec::new()
                }
            },
            _ => Vec::new(),
        };
        self.trace_events = events;
        if reset_selection {
            self.trace_event_selected = 0;
            self.trace_payload_scroll.set(0);
        } else {
            self.trace_event_selected = self
                .trace_event_selected
                .min(self.trace_events.len().saturating_sub(1));
        }
    }

    /// Install trace capture and the opt-in approval hook.
    pub fn enable_approvals(&mut self) {
        match bridge::install_hook(false) {
            Ok(_) => {
                self.hook_installed = true;
                self.flash =
                    Some("trace capture ON — restart Claude sessions to record events".into());
            }
            Err(e) => self.flash = Some(format!("enable failed: {e}")),
        }
        self.install_open = false;
    }

    /// Remove all Iris lifecycle hooks from Claude Code settings.
    pub fn disable_approvals(&mut self) {
        match bridge::uninstall_hook(false) {
            Ok(_) => {
                self.hook_installed = false;
                self.flash = Some("trace capture OFF — Iris hooks removed".into());
            }
            Err(e) => self.flash = Some(format!("disable failed: {e}")),
        }
        self.install_open = false;
    }

    /// Arm/disarm approval gating. When armed, the hook intercepts other
    /// sessions and waits for your a/d here; when disarmed, iris is passive.
    pub fn toggle_gating(&mut self) {
        if !self.hook_installed {
            self.flash = Some("enable approvals first (the hook isn't installed)".into());
            return;
        }
        self.gating = !self.gating;
        bridge::set_gating(self.gating);
        self.flash = Some(if self.gating {
            "gating ARMED — iris now intercepts tool approvals".into()
        } else {
            "gating off — sessions run via normal permissions".into()
        });
    }

    pub fn start_key_input(&mut self) {
        self.editing_key = true;
        self.key_buffer.clear();
        self.flash = None;
    }

    pub fn cancel_key_input(&mut self) {
        self.editing_key = false;
        self.key_buffer.clear();
    }

    pub fn key_input_push(&mut self, c: char) {
        if !c.is_control() {
            self.key_buffer.push(c);
        }
    }

    pub fn key_input_backspace(&mut self) {
        self.key_buffer.pop();
    }

    /// Save the entered key (persisted 0600) and close the prompt.
    pub fn commit_key_input(&mut self) {
        let key = self.key_buffer.trim().to_string();
        self.editing_key = false;
        self.key_buffer.clear();
        if key.is_empty() {
            return;
        }
        match save_api_key(&key) {
            Ok(()) => {
                self.api_key = Some(key);
                self.flash = Some("API key saved to ~/.claude/iris/api_key".into());
            }
            Err(e) => self.flash = Some(format!("could not save key: {e}")),
        }
    }

    /// The pending request to act on: the selected session's, or — if it has
    /// none — the sole pending request anywhere (so `a`/`d` always do the
    /// obvious thing when only one approval is waiting).
    pub fn current_pending(&self) -> Option<&Pending> {
        if let Some(s) = self.selected_session() {
            if let Some(p) = self.pending.get(&s.id) {
                return Some(p);
            }
        }
        if self.pending.len() == 1 {
            return self.pending.values().next();
        }
        None
    }

    fn current_pending_id(&self) -> Option<String> {
        self.current_pending().map(|p| p.id.clone())
    }

    /// Write an allow/deny decision for the current pending request, with
    /// feedback in the header so the keypress is never silent.
    pub fn approve_selected(&mut self, allow: bool) {
        let target = self
            .current_pending()
            .map(|p| (p.session_id.clone(), p.id.clone(), p.tool_name.clone()));
        bridge::log(&format!(
            "approve_selected(allow={allow}): pending={} target={}",
            self.pending.len(),
            target.as_ref().map(|t| t.1.as_str()).unwrap_or("none")
        ));
        match target {
            Some((session_id, id, tool)) => {
                let verb = if allow { "approved" } else { "denied" };
                bridge::write_decision(&id, allow, &format!("{verb} via iris"));
                self.pending.remove(&session_id);
                self.assessments.remove(&id);
                self.approve_open = false;
                self.flash = Some(format!("{verb} {tool}"));
            }
            None => {
                self.flash = Some("nothing to approve (no pending tool request)".into());
            }
        }
    }

    /// Open the approval detail modal if there's a request to act on.
    pub fn open_approval(&mut self) {
        if self.current_pending().is_some() {
            self.approve_open = true;
        } else {
            self.flash = Some("nothing to approve (no pending tool request)".into());
        }
    }

    pub fn close_approval(&mut self) {
        self.approve_open = false;
    }

    /// Ask the model for a quick risk read on the pending tool call.
    pub fn assess_pending(&mut self) {
        let (id, prompt) = match self.current_pending() {
            Some(p) => (
                p.id.clone(),
                format!(
                    "A Claude Code agent in '{}' wants to run the tool '{}'.\nInput:\n{}\n\n\
In 2-3 short lines: what does this do, and how risky is it (destructive, \
irreversible, network, or secret-touching)? End with a final line exactly: \
RISK: low|medium|high",
                    p.cwd, p.tool_name, p.input
                ),
            ),
            None => return,
        };
        if matches!(self.assessments.get(&id), Some(SummaryState::Loading)) {
            return;
        }
        // Use the API key when present, else the local `claude` CLI.
        let key = self.api_key.clone();
        self.assessments.insert(id.clone(), SummaryState::Loading);
        let tx = self.assess_tx.clone();
        thread::spawn(move || {
            let result = match key {
                Some(k) => anthropic::assess(&k, anthropic::SUMMARY_MODEL, &prompt),
                None => anthropic::assess_cli(anthropic::SUMMARY_MODEL, &prompt),
            };
            let _ = tx.send((id, result));
        });
    }

    /// Assessment state for the current pending request.
    pub fn current_assessment(&self) -> Option<&SummaryState> {
        self.current_pending_id()
            .and_then(|id| self.assessments.get(&id))
    }

    fn drain_summaries(&mut self) {
        while let Ok((path, result)) = self.summary_rx.try_recv() {
            let state = match result {
                Ok(text) => SummaryState::Done(text),
                Err(e) => SummaryState::Error(e),
            };
            self.summaries.insert(path, state);
        }
        while let Ok((id, result)) = self.assess_rx.try_recv() {
            let state = match result {
                Ok(text) => SummaryState::Done(text),
                Err(e) => SummaryState::Error(e),
            };
            self.assessments.insert(id, state);
        }
    }

    /// Open the summary popup for the selected session, kicking off generation
    /// if one isn't already cached or in flight.
    pub fn open_summary(&mut self) {
        let path = match self.selected_path() {
            Some(p) => p,
            None => {
                self.flash = Some("select a session (not a group) for a summary".into());
                return;
            }
        };
        self.popup_open = true;
        match self.summaries.get(&path) {
            Some(SummaryState::Loading) | Some(SummaryState::Done(_)) => {}
            _ => self.request_summary(path),
        }
    }

    pub fn close_summary(&mut self) {
        self.popup_open = false;
    }

    /// Force-regenerate the summary for the selected session.
    pub fn regenerate_summary(&mut self) {
        if let Some(path) = self.selected_path() {
            if !matches!(self.summaries.get(&path), Some(SummaryState::Loading)) {
                self.request_summary(path);
            }
        }
    }

    fn request_summary(&mut self, path: PathBuf) {
        let digest = match self.readers.get(&path) {
            Some(s) => s.digest(),
            None => return,
        };
        // With a key, hit the API directly; without one, fall back to the local
        // `claude` CLI so summaries still work.
        let key = self.api_key.clone();
        self.summaries.insert(path.clone(), SummaryState::Loading);
        let tx = self.summary_tx.clone();
        thread::spawn(move || {
            let result = match key {
                Some(k) => anthropic::summarize(&k, anthropic::SUMMARY_MODEL, &digest),
                None => anthropic::summarize_cli(anthropic::SUMMARY_MODEL, &digest),
            };
            let _ = tx.send((path, result));
        });
    }

    pub fn selected_summary(&self) -> Option<&SummaryState> {
        self.selected_path().and_then(|p| self.summaries.get(&p))
    }

    pub fn refresh(&mut self) {
        self.last_refresh = Instant::now();
        bridge::touch_heartbeat();
        // Keep the on-disk gating flag in sync with our state each tick.
        bridge::set_gating(self.gating);
        self.hook_installed = bridge::hook_installed();

        // Load pending hook approvals, keyed by session id (newest per session).
        self.pending.clear();
        for p in bridge::load_pending() {
            match self.pending.get(&p.session_id) {
                Some(existing) if existing.ts >= p.ts => {}
                _ => {
                    self.pending.insert(p.session_id.clone(), p);
                }
            }
        }
        if self.pending.len() != self.last_pending_logged {
            bridge::log(&format!("pending requests: {}", self.pending.len()));
            self.last_pending_logged = self.pending.len();
        }

        if self.hook_installed && self.trace_store.is_none() {
            match TraceStore::open(bridge::trace_db_path()) {
                Ok(store) => self.trace_store = Some(store),
                Err(error) => self.flash = Some(format!("trace capture unavailable: {error}")),
            }
        }

        for path in discover(&self.projects_dir) {
            let records = {
                let s = self
                    .readers
                    .entry(path.clone())
                    .or_insert_with(|| Session::new(path.clone()));
                let _ = s.refresh();
                s.take_new_records()
            };
            let mut retry_records = None;
            if self.hook_installed {
                if let Some(store) = self.trace_store.as_mut() {
                    let mut pending = records.into_iter();
                    while let Some(record) = pending.next() {
                        if let Err(error) = store.ingest_transcript(record.clone()) {
                            self.flash = Some(format!("trace capture failed: {error}"));
                            let mut retry = vec![record];
                            // The failed source is retried too; SQLite's unique
                            // source key makes a post-commit error harmless.
                            retry.extend(pending);
                            retry_records = Some(retry);
                            break;
                        }
                    }
                } else if !records.is_empty() {
                    retry_records = Some(records);
                }
            }
            if let Some(records) = retry_records {
                if let Some(reader) = self.readers.get_mut(&path) {
                    reader.restore_new_records(records);
                }
            }
        }

        // Annotate every reader with bridge state so status() is precise: a live
        // hook request is the authoritative "needs approval" signal.
        let pending_ids: HashSet<String> = self.pending.keys().cloned().collect();
        for s in self.readers.values_mut() {
            s.live_request = pending_ids.contains(&s.id);
        }

        let cutoff = SystemTime::now()
            .checked_sub(self.window)
            .unwrap_or(SystemTime::UNIX_EPOCH);

        // Show anything touched within the window, plus any session blocked
        // waiting for approval (transcript heuristic OR a live hook request) —
        // those stay pinned however long they wait.
        let mut active: Vec<&Session> = self
            .readers
            .values()
            .filter(|s| !self.hidden.contains(&s.path))
            .filter(|s| {
                s.mtime >= cutoff
                    || s.status() == Status::NeedsApproval
                    || self.pending.contains_key(&s.id)
            })
            .collect();
        // Sort: live hook approvals first, then by status, then most recent.
        let rank = |s: &Session| -> (u8, u8) {
            let pend = if self.pending.contains_key(&s.id) {
                0
            } else {
                1
            };
            (pend, s.status().rank())
        };
        active.sort_by(|a, b| rank(a).cmp(&rank(b)).then(b.mtime.cmp(&a.mtime)));
        self.visible = active.iter().map(|s| s.path.clone()).collect();

        self.regroup();
        self.restore_selection();

        // While entered and parked at the bottom, ride new events like tail -f.
        if self.focused && self.feed_follow {
            let last = self.feed_len().saturating_sub(1);
            self.feed_state.borrow_mut().select(Some(last));
        }
        if self.trace_browser_open {
            self.refresh_trace_browser();
        }
    }

    /// Bucket `visible` sessions by project (preserving their priority order),
    /// auto-collapse newly-seen noisy groups, and flatten into render `rows`.
    fn regroup(&mut self) {
        let mut order: Vec<String> = Vec::new();
        let mut members: HashMap<String, Vec<PathBuf>> = HashMap::new();
        for path in &self.visible {
            let key = self
                .readers
                .get(path)
                .map(|s| s.project().to_string())
                .unwrap_or_else(|| "?".into());
            if !members.contains_key(&key) {
                order.push(key.clone());
            }
            members.entry(key).or_default().push(path.clone());
        }

        // First time we see a sizeable group, fold it shut.
        for key in &order {
            if self.known_groups.insert(key.clone()) && members[key].len() >= GROUP_AUTOCOLLAPSE {
                self.collapsed.insert(key.clone());
            }
        }
        // Forget groups that are no longer active.
        self.known_groups.retain(|k| members.contains_key(k));
        self.collapsed.retain(|k| members.contains_key(k));

        let mut rows = Vec::new();
        for key in &order {
            let paths = &members[key];
            // Singletons render bare — a one-session "group" header is just noise.
            if paths.len() == 1 {
                rows.push(Row::Session {
                    path: paths[0].clone(),
                    grouped: false,
                });
                continue;
            }
            let collapsed = self.collapsed.contains(key);
            let mut info = GroupInfo {
                key: key.clone(),
                count: paths.len(),
                pending: 0,
                needs: 0,
                working: 0,
                done: 0,
                idle: 0,
                collapsed,
                lead_label: String::new(),
            };
            for p in paths {
                if let Some(s) = self.readers.get(p) {
                    if self.pending.contains_key(&s.id) {
                        info.pending += 1;
                    }
                    match s.status() {
                        Status::NeedsApproval => info.needs += 1,
                        Status::Working => info.working += 1,
                        Status::Done => info.done += 1,
                        Status::Idle => info.idle += 1,
                    }
                    if info.lead_label.is_empty() {
                        info.lead_label = s.label();
                    }
                }
            }
            rows.push(Row::Group(info));
            if !collapsed {
                for p in paths {
                    rows.push(Row::Session {
                        path: p.clone(),
                        grouped: true,
                    });
                }
            }
        }
        self.rows = rows;
    }

    fn row_key(row: &Row) -> SelKey {
        match row {
            Row::Group(g) => SelKey::Group(g.key.clone()),
            Row::Session { path, .. } => SelKey::Session(path.clone()),
        }
    }

    fn restore_selection(&mut self) {
        if self.rows.is_empty() {
            self.selected = 0;
            self.selected_key = None;
            return;
        }
        if let Some(key) = &self.selected_key {
            if let Some(i) = self.rows.iter().position(|r| &Self::row_key(r) == key) {
                self.selected = i;
                return;
            }
        }
        self.selected = self.selected.min(self.rows.len() - 1);
        self.selected_key = Some(Self::row_key(&self.rows[self.selected]));
    }

    pub fn select_next(&mut self) {
        if self.rows.is_empty() {
            return;
        }
        self.selected = (self.selected + 1).min(self.rows.len() - 1);
        self.selected_key = Some(Self::row_key(&self.rows[self.selected]));
    }

    pub fn select_prev(&mut self) {
        if self.rows.is_empty() {
            return;
        }
        self.selected = self.selected.saturating_sub(1);
        self.selected_key = Some(Self::row_key(&self.rows[self.selected]));
    }

    /// Move the list cursor by a signed delta (for Ctrl-d/Ctrl-u jumps).
    pub fn select_by(&mut self, delta: isize) {
        if self.rows.is_empty() {
            return;
        }
        let n = self.rows.len() as isize;
        self.selected = ((self.selected as isize) + delta).clamp(0, n - 1) as usize;
        self.selected_key = Some(Self::row_key(&self.rows[self.selected]));
    }

    pub fn select_first(&mut self) {
        self.select_by(isize::MIN / 2);
    }

    pub fn select_last(&mut self) {
        self.select_by(isize::MAX / 2);
    }

    /// Fold/unfold the group under the cursor.
    pub fn toggle_selected_group(&mut self) {
        if let Some(Row::Group(g)) = self.rows.get(self.selected) {
            let key = g.key.clone();
            if !self.collapsed.remove(&key) {
                self.collapsed.insert(key);
            }
            self.regroup();
            self.restore_selection();
        }
    }

    /// Collapse every group if any is open, otherwise expand them all.
    pub fn toggle_all_groups(&mut self) {
        let keys: Vec<String> = self
            .rows
            .iter()
            .filter_map(|r| match r {
                Row::Group(g) => Some(g.key.clone()),
                _ => None,
            })
            .collect();
        let any_open = self
            .rows
            .iter()
            .any(|r| matches!(r, Row::Group(g) if !g.collapsed));
        for k in keys {
            if any_open {
                self.collapsed.insert(k);
            } else {
                self.collapsed.remove(&k);
            }
        }
        self.regroup();
        self.restore_selection();
    }

    /// Batch allow/deny every pending approval in the selected group — the fast
    /// path for clearing a flood of identical automation prompts.
    pub fn approve_group(&mut self, allow: bool) {
        let key = match self.rows.get(self.selected) {
            Some(Row::Group(g)) => g.key.clone(),
            _ => return,
        };
        let targets: Vec<(String, String)> = self
            .visible
            .iter()
            .filter_map(|p| self.readers.get(p))
            .filter(|s| s.project() == key)
            .filter_map(|s| {
                self.pending
                    .get(&s.id)
                    .map(|p| (s.id.clone(), p.id.clone()))
            })
            .collect();
        if targets.is_empty() {
            self.flash = Some(format!("no pending approvals in {key}"));
            return;
        }
        let verb = if allow { "approved" } else { "denied" };
        let n = targets.len();
        for (session_id, id) in targets {
            bridge::write_decision(&id, allow, &format!("{verb} via iris (group)"));
            self.pending.remove(&session_id);
            self.assessments.remove(&id);
        }
        self.regroup();
        self.flash = Some(format!("{verb} {n} in {key}"));
    }

    pub fn sessions(&self) -> impl Iterator<Item = &Session> {
        self.visible.iter().filter_map(move |p| self.readers.get(p))
    }

    /// Estimated USD cost summed over every visible session.
    pub fn visible_cost(&self) -> f64 {
        self.sessions()
            .map(|s| crate::cost::estimate(&s.usage, s.model.as_deref()))
            .sum()
    }

    /// Sessions belonging to a given project group, in priority order.
    pub fn group_sessions(&self, key: &str) -> Vec<&Session> {
        self.visible
            .iter()
            .filter_map(|p| self.readers.get(p))
            .filter(|s| s.project() == key)
            .collect()
    }

    /// Path of the session under the cursor, if a session row is selected.
    fn selected_path(&self) -> Option<PathBuf> {
        match self.rows.get(self.selected) {
            Some(Row::Session { path, .. }) => Some(path.clone()),
            _ => None,
        }
    }

    /// "Delete" the selected session from the dashboard. Read-only contract:
    /// this only hides it from view (persisted across refreshes), the transcript
    /// `.jsonl` file is never touched.
    pub fn hide_selected(&mut self) {
        let Some(path) = self.selected_path() else {
            return;
        };
        self.hidden.insert(path.clone());
        save_hidden(&self.hidden);
        self.flash = Some("session removed from view (transcript on disk untouched)".into());
        self.refresh();
        // Clamp selection in case the last row went away.
        if self.selected >= self.rows.len() {
            self.selected = self.rows.len().saturating_sub(1);
        }
    }

    pub fn selected_group(&self) -> Option<&GroupInfo> {
        match self.rows.get(self.selected) {
            Some(Row::Group(g)) => Some(g),
            _ => None,
        }
    }

    pub fn selected_session(&self) -> Option<&Session> {
        self.selected_path().and_then(|p| self.readers.get(&p))
    }

    pub fn session_at(&self, path: &PathBuf) -> Option<&Session> {
        self.readers.get(path)
    }

    /// True if the selected session has a live approval waiting.
    pub fn selected_has_pending(&self) -> bool {
        self.selected_session()
            .map(|s| self.pending.contains_key(&s.id))
            .unwrap_or(false)
    }

    /// "Enter" the selected session to scroll its full activity feed. Opens at
    /// the latest event (bottom), like opening a log.
    pub fn enter_focus(&mut self) {
        if self.selected_session().is_some() {
            self.focused = true;
            self.feed_follow = true;
            self.pending_g = false;
            let last = self.feed_len().saturating_sub(1);
            self.feed_state.borrow_mut().select(Some(last));
        }
    }

    pub fn exit_focus(&mut self) {
        self.focused = false;
        self.pending_g = false;
    }

    /// Record the feed viewport height during the draw pass (interior mutable).
    pub fn set_feed_viewport(&self, h: u16) {
        self.feed_viewport.set(h);
    }

    /// Mutable handle to the feed's cursor/scroll state for the draw pass.
    pub fn feed_state_mut(&self) -> RefMut<'_, ListState> {
        self.feed_state.borrow_mut()
    }

    fn feed_len(&self) -> usize {
        self.selected_session().map(|s| s.events.len()).unwrap_or(0)
    }

    pub fn feed_cursor(&self) -> usize {
        self.feed_state.borrow().selected().unwrap_or(0)
    }

    /// Place the cursor at absolute event index `i`, clamped, and update the
    /// follow flag (true only when parked on the last line).
    fn feed_set(&mut self, i: usize) {
        let len = self.feed_len();
        if len == 0 {
            self.feed_state.borrow_mut().select(None);
            self.feed_follow = true;
            return;
        }
        let i = i.min(len - 1);
        self.feed_state.borrow_mut().select(Some(i));
        self.feed_follow = i == len - 1;
    }

    /// Move the cursor by a signed delta (negative = toward older events).
    pub fn feed_move(&mut self, delta: isize) {
        let cur = self.feed_cursor() as isize;
        let next = (cur + delta).max(0) as usize;
        self.feed_set(next);
    }

    pub fn feed_top(&mut self) {
        self.feed_set(0);
    }

    pub fn feed_bottom(&mut self) {
        self.feed_set(self.feed_len().saturating_sub(1));
    }

    /// Half a viewport — Ctrl-d / Ctrl-u.
    pub fn feed_half_page(&self) -> isize {
        ((self.feed_viewport.get() as isize) / 2).max(1)
    }

    /// One viewport minus a line of overlap — Ctrl-f / Ctrl-b, PgUp/PgDn.
    pub fn feed_page(&self) -> isize {
        ((self.feed_viewport.get() as isize) - 1).max(1)
    }

    /// Consume a pending first `g` (for the `gg` motion).
    pub fn take_pending_g(&mut self) -> bool {
        std::mem::take(&mut self.pending_g)
    }
}
