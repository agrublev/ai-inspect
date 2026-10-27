//! iris — a live supervisor for all active Claude Code sessions.
//!
//! Reads the transcript files Claude Code writes under
//! `~/.claude/projects/<slug>/<uuid>.jsonl`, tails the active ones, and renders
//! a dashboard of what every running session is currently doing.

mod anthropic;
mod app;
mod bridge;
mod cost;
mod session;
mod trace;
mod ui;
mod web;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};

use app::App;

struct Args {
    window: Duration,
    interval: Duration,
    dir: Option<PathBuf>,
    list_once: bool,
    traces: bool,
    web: bool,
    port: u16,
    trace_id: Option<String>,
    search: Option<String>,
    management: Option<Vec<String>>,
    hook: bool,
    install_hook: bool,
    uninstall_hook: bool,
    project: bool,
}

const HELP: &str = "\
iris — supervise all active Claude Code sessions

USAGE:
    iris [OPTIONS] [COMMAND]

COMMANDS:
    ls            Print a one-shot table of active sessions and exit
    traces        List saved traces, newest first
    trace <ID>    Show the event timeline and full payloads for a trace
    web           Open the local web trace explorer
    search <TEXT> Search captured event data
    dataset list | create <NAME> [DESCRIPTION] | add <NAME> <INPUT_JSON> [EXPECTED_JSON]
    dataset items <NAME>  Show examples in a local evaluation dataset
    prompt list | save <NAME> <TEMPLATE> | show <NAME> [VERSION]
    eval run <DATASET> [exact|contains]  Score saved traces against expectations
    eval <TRACE_ID> <SCORE> [REASON]  Add a manual 0..1 evaluation score
    evals [TRACE_ID]  List evaluation scores
    install-hook  Register trace lifecycle hooks (and optional approval hook)
    uninstall-hook  Remove the iris lifecycle hooks from settings.json.
    hook          Hook event recorder and PreToolUse approval bridge.
                  Register it in settings.json, not run by hand.
    (default)     Launch the live TUI dashboard

OPTIONS:
    -w, --window <MIN>      Treat a session active if touched within MIN minutes [default: 3]
                            (sessions waiting for approval stay shown regardless)
    -i, --interval <SEC>    Data refresh interval in seconds [default: 1]
    -d, --dir <PATH>        Override the projects directory [default: ~/.claude/projects]
    -p, --port <PORT>       Web UI port [default: 4318; binds to 127.0.0.1]
    -h, --help              Show this help
";

fn parse_args() -> Result<Args> {
    let mut a = Args {
        window: Duration::from_secs(3 * 60),
        interval: Duration::from_secs(1),
        dir: None,
        list_once: false,
        traces: false,
        web: false,
        port: 4318,
        trace_id: None,
        search: None,
        management: None,
        hook: false,
        install_hook: false,
        uninstall_hook: false,
        project: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                print!("{HELP}");
                std::process::exit(0);
            }
            "ls" | "list" => a.list_once = true,
            "traces" => a.traces = true,
            "web" => a.web = true,
            "trace" => a.trace_id = Some(it.next().context("trace needs an ID")?),
            "search" => a.search = Some(it.next().context("search needs text")?),
            "dataset" | "prompt" | "eval" | "evals" => {
                let mut command = vec![arg];
                command.extend(it);
                a.management = Some(command);
                break;
            }
            "hook" => a.hook = true,
            "install-hook" => a.install_hook = true,
            "uninstall-hook" => a.uninstall_hook = true,
            "--project" => a.project = true,
            "-w" | "--window" => {
                let v: u64 = it.next().context("--window needs a value")?.parse()?;
                a.window = Duration::from_secs(v * 60);
            }
            "-i" | "--interval" => {
                let v: u64 = it.next().context("--interval needs a value")?.parse()?;
                a.interval = Duration::from_secs(v.max(1));
            }
            "-p" | "--port" => {
                a.port = it.next().context("--port needs a value")?.parse()?;
            }
            "-d" | "--dir" => {
                a.dir = Some(PathBuf::from(it.next().context("--dir needs a value")?));
            }
            other => anyhow::bail!("unknown argument: {other}\n\n{HELP}"),
        }
    }
    Ok(a)
}

fn projects_dir(arg: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(d) = arg {
        return Ok(d);
    }
    let home = dirs::home_dir().context("cannot resolve home directory")?;
    Ok(home.join(".claude").join("projects"))
}

fn main() -> Result<()> {
    let mut args = parse_args()?;

    // The hook bridge runs as a Claude Code subprocess: read stdin, print the
    // decision, exit. It must not touch the projects dir or print anything else.
    if args.hook {
        std::process::exit(bridge::run_hook());
    }

    if args.install_hook {
        match bridge::install_hook(args.project) {
            Ok(msg) => {
                println!("{msg}");
                return Ok(());
            }
            Err(e) => anyhow::bail!("install-hook failed: {e}"),
        }
    }

    if args.uninstall_hook {
        match bridge::uninstall_hook(args.project) {
            Ok(msg) => {
                println!("{msg}");
                return Ok(());
            }
            Err(e) => anyhow::bail!("uninstall-hook failed: {e}"),
        }
    }

    let dir = projects_dir(args.dir.take())?;
    if args.web {
        web::run(args.port, &dir)?;
        return Ok(());
    }
    if args.traces || args.trace_id.is_some() || args.search.is_some() || args.management.is_some()
    {
        let mut store = trace::TraceStore::open(bridge::trace_db_path())?;
        if dir.exists() {
            for path in session::discover(&dir) {
                let mut reader = session::Session::new(path);
                reader.refresh()?;
                for record in reader.take_new_records() {
                    store.ingest_transcript(record)?;
                }
            }
        }
        if let Some(command) = args.management {
            run_management(&store, &command)?;
        } else if let Some(query) = args.search {
            print_events(store.search(&query)?);
        } else if let Some(requested_id) = args.trace_id {
            let trace_id = store
                .list_traces()?
                .into_iter()
                .filter_map(|trace| trace["trace_id"].as_str().map(str::to_owned))
                .find(|trace_id| trace_id.starts_with(&requested_id))
                .with_context(|| format!("no trace found matching {requested_id}"))?;
            print_events(store.events_for_trace(&trace_id)?);
        } else {
            print_traces(store.list_traces()?);
        }
        return Ok(());
    }
    if !dir.exists() {
        anyhow::bail!("projects directory not found: {}", dir.display());
    }

    let app = App::new(dir, args.window, args.interval);

    if args.list_once {
        print_list(&app);
        return Ok(());
    }

    run_tui(app)
}

fn run_management(store: &trace::TraceStore, command: &[String]) -> Result<()> {
    let verb = command.first().map(String::as_str).unwrap_or("");
    match verb {
        "dataset" => match command.get(1).map(String::as_str).unwrap_or("list") {
            "list" => {
                for dataset in store.list_datasets()? {
                    println!(
                        "{}  {} items  {}",
                        dataset["name"].as_str().unwrap_or(""),
                        dataset["items"].as_u64().unwrap_or(0),
                        dataset["description"].as_str().unwrap_or("")
                    );
                }
            }
            "create" => {
                let name = command.get(2).context("dataset create needs a name")?;
                let description = command.get(3).map(String::as_str).unwrap_or("");
                store.create_dataset(name, description)?;
                println!("saved dataset {name}");
            }
            "add" => {
                let name = command.get(2).context("dataset add needs a name")?;
                let input: serde_json::Value =
                    serde_json::from_str(command.get(3).context("dataset add needs input JSON")?)?;
                let expected = command
                    .get(4)
                    .map(|value| serde_json::from_str(value))
                    .transpose()?;
                store.add_dataset_item(name, &input, expected.as_ref())?;
                println!("added example to {name}");
            }
            "items" => {
                let name = command.get(2).context("dataset items needs a name")?;
                for item in store.dataset_items(name)? {
                    println!("{}", serde_json::to_string_pretty(&item)?);
                }
            }
            other => anyhow::bail!("unknown dataset command: {other}"),
        },
        "prompt" => match command.get(1).map(String::as_str).unwrap_or("list") {
            "list" => {
                for prompt in store.list_prompts()? {
                    println!(
                        "{} v{}  {}",
                        prompt["name"].as_str().unwrap_or(""),
                        prompt["version"].as_u64().unwrap_or(0),
                        truncate(prompt["template"].as_str().unwrap_or(""), 100)
                    );
                }
            }
            "save" => {
                let name = command.get(2).context("prompt save needs a name")?;
                let template = command.get(3).context("prompt save needs a template")?;
                let version = store.save_prompt(name, template)?;
                println!("saved {name} v{version}");
            }
            "show" => {
                let name = command.get(2).context("prompt show needs a name")?;
                let version = command.get(3).map(|value| value.parse()).transpose()?;
                let prompt = store
                    .get_prompt(name, version)?
                    .with_context(|| format!("prompt not found: {name}"))?;
                println!("{}", serde_json::to_string_pretty(&prompt)?);
            }
            other => anyhow::bail!("unknown prompt command: {other}"),
        },
        "eval" => {
            if command.get(1).map(String::as_str) == Some("run") {
                let dataset = command.get(2).context("eval run needs a dataset name")?;
                let scorer = command.get(3).map(String::as_str).unwrap_or("exact");
                let (count, average) = store.evaluate_dataset(dataset, scorer)?;
                println!("scored {count} examples with {scorer}; average score {average:.3}");
                return Ok(());
            }
            let requested_id = command.get(1).context("eval needs a trace ID")?;
            let score: f64 = command
                .get(2)
                .context("eval needs a score between 0 and 1")?
                .parse()?;
            anyhow::ensure!(
                (0.0..=1.0).contains(&score),
                "score must be between 0 and 1"
            );
            let trace_id = resolve_trace_id(store, requested_id)?;
            let reason = command.get(3).map(String::as_str).unwrap_or("");
            store.record_evaluation(&trace_id, score, reason)?;
            println!("recorded score {score:.2} for {trace_id}");
        }
        "evals" => {
            let trace_id = command
                .get(1)
                .map(|value| resolve_trace_id(store, value))
                .transpose()?;
            for evaluation in store.evaluations(trace_id.as_deref())? {
                println!("{}", serde_json::to_string_pretty(&evaluation)?);
            }
        }
        _ => anyhow::bail!("unknown local management command: {verb}"),
    }
    Ok(())
}

fn resolve_trace_id(store: &trace::TraceStore, requested: &str) -> Result<String> {
    store
        .list_traces()?
        .into_iter()
        .filter_map(|trace| trace["trace_id"].as_str().map(str::to_owned))
        .find(|trace_id| trace_id.starts_with(requested))
        .with_context(|| format!("no trace found matching {requested}"))
}

fn print_traces(traces: Vec<serde_json::Value>) {
    if traces.is_empty() {
        println!("no traces recorded yet (start iris and install the Claude Code hooks)");
        return;
    }
    println!(
        "{:<18} {:<18} {:<25} {:>7}  INPUT",
        "TRACE", "SESSION", "STARTED", "EVENTS"
    );
    for trace in traces {
        let id = trace["trace_id"].as_str().unwrap_or("");
        let session = trace["session_id"].as_str().unwrap_or("");
        let started = trace["started_at"].as_str().unwrap_or("");
        let input = brief(&trace["input"]);
        println!(
            "{:<18} {:<18} {:<25} {:>7}  {}",
            truncate(id, 16),
            truncate(session, 16),
            truncate(started, 24),
            trace["event_count"].as_u64().unwrap_or(0),
            input
        );
    }
}

fn print_events(events: Vec<serde_json::Value>) {
    if events.is_empty() {
        println!("no matching trace events");
        return;
    }
    for event in events {
        let span_id = event["span_id"].as_str().unwrap_or("");
        let parent_span_id = event["parent_span_id"].as_str().unwrap_or("");
        let relation = match (span_id.is_empty(), parent_span_id.is_empty()) {
            (false, false) => format!(
                " span={} parent={}",
                truncate(span_id, 12),
                truncate(parent_span_id, 12)
            ),
            (false, true) => format!(" span={}", truncate(span_id, 12)),
            (true, false) => format!(" parent={}", truncate(parent_span_id, 12)),
            (true, true) => String::new(),
        };
        println!(
            "{}  {}  {}  [{}]{}",
            event["timestamp"].as_str().unwrap_or(""),
            event["event_type"].as_str().unwrap_or("event"),
            event["name"].as_str().unwrap_or(""),
            event["status"].as_str().unwrap_or("?"),
            relation
        );
        for field in ["input", "output", "metadata"] {
            if !event[field].is_null() {
                let body = serde_json::to_string_pretty(&event[field]).unwrap_or_default();
                println!("  {field}:\n{}", indent(&body, "    "));
            }
        }
    }
}

fn brief(value: &serde_json::Value) -> String {
    let text = value
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| value.to_string());
    truncate(&text.replace('\n', " "), 80)
}

fn indent(text: &str, prefix: &str) -> String {
    text.lines()
        .map(|line| format!("{prefix}{line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn print_list(app: &App) {
    use std::io::Write;
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    if app.visible.is_empty() {
        let _ = writeln!(out, "no active sessions");
        return;
    }
    // Ignore write errors (e.g. a broken pipe when piped into `head`).
    let _ = writeln!(
        out,
        "{:<3} {:<22} {:<16} {:<13} {:>8} {:>7}  TITLE",
        "", "STATE", "PROJECT", "MODEL", "TOKENS", "~COST"
    );
    for s in app.sessions() {
        let (icon, _color, state) = ui::status_glyph(s);
        let est = cost::estimate(&s.usage, s.model.as_deref());
        if writeln!(
            out,
            "{:<3} {:<22} {:<16} {:<13} {:>8} {:>7.2}  {}",
            icon,
            truncate(&state, 22),
            truncate(s.project(), 16),
            truncate(&ui::short_model(s.model.as_deref()), 13),
            ui::human_tokens(s.usage.total()),
            est,
            s.label(),
        )
        .is_err()
        {
            break;
        }
    }
    let _ = writeln!(
        out,
        "{:<3} {:<22} {:<16} {:<13} {:>8} {:>7.2}  total (estimate)",
        "",
        "",
        "",
        "",
        "",
        app.visible_cost(),
    );
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max.saturating_sub(1)).chain(['…']).collect()
    }
}

fn run_tui(mut app: App) -> Result<()> {
    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, &mut app);
    ratatui::restore();
    result
}

fn event_loop(terminal: &mut ratatui::DefaultTerminal, app: &mut App) -> Result<()> {
    // A short poll keeps the live feed and key response feeling immediate.
    let poll = Duration::from_millis(40);
    loop {
        app.tick();
        terminal.draw(|f| ui::draw(f, app))?;

        if event::poll(poll)? {
            if let Event::Key(k) = event::read()? {
                if k.kind != KeyEventKind::Press {
                    continue;
                }
                if k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL) {
                    break;
                }
                if app.editing_key {
                    // Modal text entry for the API key.
                    match k.code {
                        KeyCode::Enter => app.commit_key_input(),
                        KeyCode::Esc => app.cancel_key_input(),
                        KeyCode::Backspace => app.key_input_backspace(),
                        KeyCode::Char(c) => app.key_input_push(c),
                        _ => {}
                    }
                } else if app.install_open {
                    // Proposal: enable/disable approval interception.
                    match k.code {
                        KeyCode::Char('a') | KeyCode::Char('y') | KeyCode::Enter
                            if !app.hook_installed =>
                        {
                            app.enable_approvals()
                        }
                        KeyCode::Char('u') | KeyCode::Char('x') if app.hook_installed => {
                            app.disable_approvals()
                        }
                        KeyCode::Char('r') | KeyCode::Char('n') | KeyCode::Esc => {
                            app.close_install()
                        }
                        KeyCode::Char('q') => break,
                        _ => {}
                    }
                } else if app.approve_open {
                    // Approval modal: a/d decide, x risk-checks, Esc closes.
                    match k.code {
                        KeyCode::Char('a') => app.approve_selected(true),
                        KeyCode::Char('d') => app.approve_selected(false),
                        KeyCode::Char('x') => app.assess_pending(),
                        KeyCode::Esc => app.close_approval(),
                        KeyCode::Char('q') => break,
                        _ => {}
                    }
                } else if app.popup_open {
                    // Popup-mode keys: Esc/s close, g regenerates, q quits.
                    match k.code {
                        KeyCode::Esc | KeyCode::Char('s') => app.close_summary(),
                        KeyCode::Char('g') => app.regenerate_summary(),
                        KeyCode::Char('q') => break,
                        _ => {}
                    }
                } else if app.trace_browser_open {
                    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
                    match k.code {
                        KeyCode::Esc | KeyCode::Char('q') => app.close_trace_browser(),
                        KeyCode::Tab => app.trace_browser_focus(false),
                        KeyCode::BackTab => app.trace_browser_focus(true),
                        KeyCode::Enter => app.trace_browser_activate(),
                        KeyCode::Char('j') | KeyCode::Down => app.trace_browser_move(1),
                        KeyCode::Char('k') | KeyCode::Up => app.trace_browser_move(-1),
                        KeyCode::PageDown => app.trace_browser_page(1),
                        KeyCode::PageUp => app.trace_browser_page(-1),
                        KeyCode::Char('d') if ctrl => app.trace_browser_page(1),
                        KeyCode::Char('u') if ctrl => app.trace_browser_page(-1),
                        KeyCode::Char('r') => app.refresh(),
                        _ => {}
                    }
                } else if app.focused {
                    // Scroll mode: vim motions drive the activity feed of the
                    // entered session. Esc/h leaves; a/d/s still act on it.
                    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
                    let g_pending = app.take_pending_g();
                    match k.code {
                        KeyCode::Char('q') => break,
                        KeyCode::Esc | KeyCode::Char('h') | KeyCode::Left => app.exit_focus(),
                        KeyCode::Char('d') if ctrl => app.feed_move(app.feed_half_page()),
                        KeyCode::Char('u') if ctrl => app.feed_move(-app.feed_half_page()),
                        KeyCode::Char('f') if ctrl => app.feed_move(app.feed_page()),
                        KeyCode::Char('b') if ctrl => app.feed_move(-app.feed_page()),
                        KeyCode::Char('j') | KeyCode::Down => app.feed_move(1),
                        KeyCode::Char('k') | KeyCode::Up => app.feed_move(-1),
                        KeyCode::PageDown => app.feed_move(app.feed_page()),
                        KeyCode::PageUp => app.feed_move(-app.feed_page()),
                        KeyCode::Char('g') => {
                            if g_pending {
                                app.feed_top()
                            } else {
                                app.pending_g = true
                            }
                        }
                        KeyCode::Home => app.feed_top(),
                        KeyCode::Char('G') | KeyCode::End => app.feed_bottom(),
                        KeyCode::Char('s') => app.open_summary(),
                        KeyCode::Char('t') => app.open_trace_browser(),
                        KeyCode::Char('a') => app.approve_selected(true),
                        KeyCode::Char('d') => app.approve_selected(false),
                        KeyCode::Char('r') => app.refresh(),
                        _ => {}
                    }
                } else {
                    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
                    let g_pending = app.take_pending_g();
                    match k.code {
                        KeyCode::Char('q') | KeyCode::Esc => break,
                        KeyCode::Char('d') if ctrl => app.select_by(5),
                        KeyCode::Char('u') if ctrl => app.select_by(-5),
                        KeyCode::Char('j') | KeyCode::Down => app.select_next(),
                        KeyCode::Char('k') | KeyCode::Up => app.select_prev(),
                        KeyCode::Char('g') => {
                            if g_pending {
                                app.select_first()
                            } else {
                                app.pending_g = true
                            }
                        }
                        KeyCode::Home => app.select_first(),
                        KeyCode::Char('G') | KeyCode::End => app.select_last(),
                        KeyCode::Char('r') => app.refresh(),
                        KeyCode::Char('s') => app.open_summary(),
                        // a/d act on a whole group when a header is selected,
                        // else on the single selected session.
                        KeyCode::Char('a') => {
                            if app.selected_group().is_some() {
                                app.approve_group(true)
                            } else {
                                app.approve_selected(true)
                            }
                        }
                        KeyCode::Char('d') => {
                            if app.selected_group().is_some() {
                                app.approve_group(false)
                            } else {
                                app.approve_selected(false)
                            }
                        }
                        // Enter: fold a group header; open the approval modal if
                        // the session has one waiting; otherwise enter scroll mode.
                        KeyCode::Enter | KeyCode::Char('l') | KeyCode::Right => {
                            if app.selected_group().is_some() {
                                app.toggle_selected_group()
                            } else if app.selected_has_pending() && k.code == KeyCode::Enter {
                                app.open_approval()
                            } else {
                                app.enter_focus()
                            }
                        }
                        // Capital D: delete (hide) the selected session from the
                        // dashboard. Transcript file on disk is left untouched.
                        KeyCode::Char('D') => app.hide_selected(),
                        KeyCode::Char(' ') => app.toggle_selected_group(),
                        KeyCode::Char('z') => app.toggle_all_groups(),
                        KeyCode::Char('i') => app.open_install(),
                        KeyCode::Char('t') => app.open_trace_browser(),
                        KeyCode::Char('A') => app.toggle_gating(),
                        KeyCode::Char('K') => app.start_key_input(),
                        _ => {}
                    }
                }
            }
        }
        if app.should_quit {
            break;
        }
    }
    // Disarm on exit so no session keeps blocking once the dashboard is gone.
    bridge::set_gating(false);
    Ok(())
}
