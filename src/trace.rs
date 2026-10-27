//! Local trace, span, and event persistence for Claude Code activity.
//!
//! Prompts and tool payloads can contain private data. The SQLite store never
//! syncs to a service and is created under an owner-only directory with a
//! 0600 database file.

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use chrono::{SecondsFormat, Utc};
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde_json::{json, Value};

use crate::session::TranscriptRecord;

type TraceContext = (String, Option<String>, Option<String>);

pub struct TraceStore {
    connection: Connection,
}

impl TraceStore {
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
            fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
        }
        let connection = Connection::open(path).map_err(io_error)?;
        connection
            .busy_timeout(std::time::Duration::from_millis(500))
            .map_err(io_error)?;
        connection
            .pragma_update(None, "journal_mode", "WAL")
            .map_err(io_error)?;
        connection
            .pragma_update(None, "synchronous", "NORMAL")
            .map_err(io_error)?;
        connection
            .pragma_update(None, "foreign_keys", "ON")
            .map_err(io_error)?;
        connection
            .execute_batch(
                "
                CREATE TABLE IF NOT EXISTS events (
                    id INTEGER PRIMARY KEY,
                    source_id TEXT NOT NULL UNIQUE,
                    session_id TEXT NOT NULL,
                    trace_id TEXT NOT NULL,
                    span_id TEXT,
                    parent_span_id TEXT,
                    timestamp TEXT NOT NULL,
                    event_type TEXT NOT NULL,
                    name TEXT NOT NULL,
                    status TEXT,
                    input_json TEXT,
                    output_json TEXT,
                    metadata_json TEXT NOT NULL,
                    event_json TEXT NOT NULL
                );
                CREATE INDEX IF NOT EXISTS events_trace_time ON events(trace_id, timestamp, id);
                CREATE INDEX IF NOT EXISTS events_session_time ON events(session_id, timestamp, id);
                CREATE INDEX IF NOT EXISTS events_type_time ON events(event_type, timestamp, id);

                CREATE TABLE IF NOT EXISTS traces (
                    trace_id TEXT PRIMARY KEY,
                    session_id TEXT NOT NULL,
                    name TEXT NOT NULL,
                    started_at TEXT NOT NULL,
                    ended_at TEXT,
                    status TEXT NOT NULL,
                    input_json TEXT,
                    output_json TEXT,
                    metadata_json TEXT NOT NULL DEFAULT '{}'
                );
                CREATE INDEX IF NOT EXISTS traces_started ON traces(started_at DESC);

                CREATE TABLE IF NOT EXISTS spans (
                    session_id TEXT NOT NULL,
                    span_id TEXT NOT NULL,
                    trace_id TEXT NOT NULL,
                    parent_span_id TEXT,
                    kind TEXT NOT NULL,
                    name TEXT NOT NULL,
                    started_at TEXT NOT NULL,
                    ended_at TEXT,
                    status TEXT NOT NULL,
                    input_json TEXT,
                    output_json TEXT,
                    metadata_json TEXT NOT NULL DEFAULT '{}',
                    PRIMARY KEY(session_id, span_id)
                );
                CREATE INDEX IF NOT EXISTS spans_trace_time ON spans(trace_id, started_at, session_id);

                CREATE TABLE IF NOT EXISTS current_traces (
                    session_id TEXT PRIMARY KEY,
                    trace_id TEXT NOT NULL,
                    prompt_hash TEXT,
                    parent_span_id TEXT
                );
                CREATE TABLE IF NOT EXISTS agent_sessions (
                    agent_id TEXT PRIMARY KEY,
                    parent_session_id TEXT NOT NULL,
                    parent_trace_id TEXT NOT NULL,
                    parent_span_id TEXT
                );

                CREATE TABLE IF NOT EXISTS datasets (
                    dataset_id INTEGER PRIMARY KEY,
                    name TEXT NOT NULL UNIQUE,
                    description TEXT NOT NULL DEFAULT '',
                    created_at TEXT NOT NULL
                );
                CREATE TABLE IF NOT EXISTS dataset_items (
                    item_id INTEGER PRIMARY KEY,
                    dataset_id INTEGER NOT NULL REFERENCES datasets(dataset_id) ON DELETE CASCADE,
                    input_json TEXT NOT NULL,
                    expected_json TEXT,
                    metadata_json TEXT NOT NULL DEFAULT '{}',
                    created_at TEXT NOT NULL
                );
                CREATE TABLE IF NOT EXISTS prompts (
                    prompt_id INTEGER PRIMARY KEY,
                    name TEXT NOT NULL,
                    version INTEGER NOT NULL,
                    template TEXT NOT NULL,
                    metadata_json TEXT NOT NULL DEFAULT '{}',
                    created_at TEXT NOT NULL,
                    UNIQUE(name, version)
                );
                CREATE TABLE IF NOT EXISTS evaluations (
                    evaluation_id INTEGER PRIMARY KEY,
                    dataset_id INTEGER REFERENCES datasets(dataset_id),
                    dataset_item_id INTEGER REFERENCES dataset_items(item_id),
                    trace_id TEXT REFERENCES traces(trace_id),
                    scorer TEXT NOT NULL,
                    score REAL NOT NULL,
                    reason TEXT NOT NULL DEFAULT '',
                    created_at TEXT NOT NULL
                );
                ",
            )
            .map_err(io_error)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        Ok(Self { connection })
    }

    pub fn ingest_transcript(&mut self, record: TranscriptRecord) -> io::Result<()> {
        let session_id = record
            .path
            .file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or("unknown")
            .to_string();
        let value = record.value;
        let value_json = value.to_string();
        let source_id = format!(
            "transcript:{}:{}:{}",
            record.path.display(),
            record.offset,
            stable_id(&value_json)
        );
        if self.has_source(&source_id)? {
            return Ok(());
        }

        let record_type = value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("record");
        let timestamp = value
            .get("timestamp")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(now_timestamp);
        let content = value.pointer("/message/content").unwrap_or(&Value::Null);
        let prompt = record_type == "user" && is_prompt(content);
        let inherited = self.current_trace(&session_id)?;
        let parent_span_id = inherited.as_ref().and_then(|context| context.2.clone());
        let agent_child = self.is_agent_session(&session_id)?;

        let trace_id = if prompt && !agent_child {
            let prompt_hash = fingerprint_prompt(content);
            match inherited {
                Some((trace_id, Some(existing_hash), _)) if existing_hash == prompt_hash => {
                    trace_id
                }
                _ => stable_id(&format!("{session_id}:{source_id}")),
            }
        } else {
            inherited
                .map(|context| context.0)
                .unwrap_or_else(|| stable_id(&format!("{session_id}:root")))
        };
        let prompt_hash = prompt.then(|| fingerprint_prompt(content));
        self.set_current_trace(
            &session_id,
            &trace_id,
            prompt_hash.as_deref(),
            parent_span_id.as_deref(),
        )?;

        match record_type {
            "user" if prompt && agent_child => {
                let span_id = stable_id(&format!("agent-prompt:{session_id}:{source_id}"));
                self.append(json!({
                    "source_id": source_id,
                    "session_id": session_id,
                    "trace_id": trace_id,
                    "span_id": span_id,
                    "parent_span_id": parent_span_id,
                    "timestamp": timestamp,
                    "event_type": "agent.prompt",
                    "name": "Subagent prompt",
                    "status": "ok",
                    "input": content,
                    "output": Value::Null,
                    "metadata": common_metadata(&value, "transcript"),
                }))?;
            }
            "user" if prompt => {
                self.append(json!({
                    "source_id": source_id,
                    "session_id": session_id,
                    "trace_id": trace_id,
                    "span_id": trace_id,
                    "parent_span_id": Value::Null,
                    "timestamp": timestamp,
                    "event_type": "trace.input",
                    "name": "User prompt",
                    "status": "running",
                    "input": content,
                    "output": Value::Null,
                    "metadata": common_metadata(&value, "transcript"),
                }))?;
            }
            "user" => {
                self.append(json!({
                    "source_id": format!("{source_id}:record"),
                    "session_id": session_id,
                    "trace_id": trace_id,
                    "span_id": Value::Null,
                    "parent_span_id": parent_span_id,
                    "timestamp": timestamp,
                    "event_type": "transcript.user",
                    "name": "User transcript record",
                    "status": "ok",
                    "input": content,
                    "output": Value::Null,
                    "metadata": common_metadata(&value, "transcript"),
                }))?;
                if let Some(blocks) = content.as_array() {
                    for (index, block) in blocks.iter().enumerate() {
                        if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                            continue;
                        }
                        let tool_use_id = block
                            .get("tool_use_id")
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        let span_id = if tool_use_id.is_empty() {
                            stable_id(&format!("{source_id}:{index}"))
                        } else {
                            tool_use_id.to_string()
                        };
                        let tool_name = self
                            .connection
                            .query_row(
                                "SELECT name FROM spans WHERE session_id=?1 AND span_id=?2",
                                params![session_id, span_id],
                                |row| row.get::<_, String>(0),
                            )
                            .optional()
                            .map_err(io_error)?
                            .unwrap_or_else(|| "Tool result".to_string());
                        let failed = block
                            .get("is_error")
                            .and_then(Value::as_bool)
                            .unwrap_or(false);
                        self.append(json!({
                            "source_id": format!("{source_id}:result:{index}"),
                            "session_id": session_id,
                            "trace_id": trace_id,
                            "span_id": span_id,
                            "parent_span_id": parent_span_id,
                            "timestamp": timestamp,
                            "event_type": "tool.result",
                            "name": tool_name,
                            "status": if failed { "error" } else { "ok" },
                            "input": { "tool_use_id": tool_use_id },
                            "output": block.get("content").cloned().unwrap_or(Value::Null),
                            "metadata": common_metadata(block, "transcript"),
                        }))?;
                    }
                }
            }
            "assistant" => {
                let message = value.get("message").unwrap_or(&Value::Null);
                let message_id = message
                    .get("id")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .unwrap_or_else(|| stable_id(&source_id));
                let model = message.get("model").cloned().unwrap_or(Value::Null);
                let usage = message.get("usage").cloned().unwrap_or(Value::Null);
                self.append(json!({
                    "source_id": format!("{source_id}:model"),
                    "session_id": session_id,
                    "trace_id": trace_id,
                    "span_id": message_id,
                    "parent_span_id": parent_span_id,
                    "timestamp": timestamp,
                    "event_type": "model.response",
                    "name": model.as_str().unwrap_or("Assistant model call"),
                    "status": "ok",
                    "input": Value::Null,
                    "output": message.get("content").cloned().unwrap_or(Value::Null),
                    "metadata": {
                        "source": "transcript",
                        "model": model,
                        "usage": usage,
                        "stop_reason": message.get("stop_reason").cloned().unwrap_or(Value::Null),
                    },
                }))?;

                if let Some(blocks) = message.get("content").and_then(Value::as_array) {
                    for (index, block) in blocks.iter().enumerate() {
                        match block.get("type").and_then(Value::as_str).unwrap_or("") {
                            "tool_use" => {
                                let span_id = block
                                    .get("id")
                                    .and_then(Value::as_str)
                                    .map(str::to_owned)
                                    .unwrap_or_else(|| stable_id(&format!("{source_id}:{index}")));
                                self.append(json!({
                                    "source_id": format!("{source_id}:tool:{index}"),
                                    "session_id": session_id,
                                    "trace_id": trace_id,
                                    "span_id": span_id,
                                    "parent_span_id": message_id,
                                    "timestamp": timestamp,
                                    "event_type": "tool.start",
                                    "name": block.get("name").and_then(Value::as_str).unwrap_or("tool"),
                                    "status": "running",
                                    "input": block.get("input").cloned().unwrap_or(Value::Null),
                                    "output": Value::Null,
                                    "metadata": common_metadata(block, "transcript"),
                                }))?;
                            }
                            "thinking" | "text" => {
                                let kind = block["type"].as_str().unwrap_or("text");
                                self.append(json!({
                                    "source_id": format!("{source_id}:{kind}:{index}"),
                                    "session_id": session_id,
                                    "trace_id": trace_id,
                                    "span_id": stable_id(&format!("{message_id}:{kind}:{index}")),
                                    "parent_span_id": message_id,
                                    "timestamp": timestamp,
                                    "event_type": format!("assistant.{kind}"),
                                    "name": if kind == "thinking" { "Model reasoning" } else { "Assistant response" },
                                    "status": "ok",
                                    "input": Value::Null,
                                    "output": block.get("thinking").or_else(|| block.get("text")).cloned().unwrap_or(Value::Null),
                                    "metadata": common_metadata(block, "transcript"),
                                }))?;
                            }
                            _ => {}
                        }
                    }
                }
                if message.get("stop_reason").and_then(Value::as_str) == Some("end_turn") {
                    self.append(json!({
                        "source_id": format!("{source_id}:end"),
                        "session_id": session_id,
                        "trace_id": trace_id,
                        "span_id": trace_id,
                        "parent_span_id": parent_span_id,
                        "timestamp": timestamp,
                        "event_type": "trace.end",
                        "name": "Turn complete",
                        "status": "ok",
                        "input": Value::Null,
                        "output": message.get("content").cloned().unwrap_or(Value::Null),
                        "metadata": common_metadata(message, "transcript"),
                    }))?;
                }
            }
            _ => {
                self.append(json!({
                    "source_id": source_id,
                    "session_id": session_id,
                    "trace_id": trace_id,
                    "span_id": Value::Null,
                    "parent_span_id": value.get("parentUuid").cloned().unwrap_or(Value::Null),
                    "timestamp": timestamp,
                    "event_type": format!("transcript.{record_type}"),
                    "name": record_type,
                    "status": "ok",
                    "input": value,
                    "output": Value::Null,
                    "metadata": { "source": "transcript" },
                }))?;
            }
        }
        Ok(())
    }

    /// Records hook payloads locally. Errors are handled by the hook caller as
    /// best effort, so storage problems never alter Claude Code's decisions.
    pub fn ingest_hook(&mut self, value: &Value) -> io::Result<()> {
        let session_id = value
            .get("session_id")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        let event_name = value
            .get("hook_event_name")
            .and_then(Value::as_str)
            .unwrap_or("HookEvent");
        let prompt = value.get("prompt").and_then(Value::as_str);
        let prompt_hash = prompt.map(stable_id);
        let existing = self.current_trace(&session_id)?;
        let trace_id = if event_name == "UserPromptSubmit" {
            match (&existing, &prompt_hash) {
                (Some((trace_id, Some(old_hash), _)), Some(new_hash)) if old_hash == new_hash => {
                    trace_id.clone()
                }
                _ => stable_id(&format!("{session_id}:{}", now_timestamp())),
            }
        } else {
            existing
                .as_ref()
                .map(|context| context.0.clone())
                .unwrap_or_else(|| stable_id(&format!("{session_id}:root")))
        };
        if event_name == "UserPromptSubmit" {
            self.set_current_trace(&session_id, &trace_id, prompt_hash.as_deref(), None)?;
        }

        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        let source_id = format!(
            "hook:{session_id}:{event_name}:{nanos}:{}",
            std::process::id()
        );
        let tool_use_id = value
            .get("tool_use_id")
            .or_else(|| value.get("toolUseId"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let event_type = match event_name {
            "PreToolUse" => "tool.pre",
            "PostToolUse" => "tool.post",
            "PostToolUseFailure" => "tool.failure",
            "SubagentStart" => "agent.start",
            "SubagentStop" => "agent.end",
            other => other,
        };
        let name = value
            .get("tool_name")
            .or_else(|| value.get("agent_type"))
            .and_then(Value::as_str)
            .unwrap_or(event_name);
        let input = value
            .get("tool_input")
            .cloned()
            .or_else(|| value.get("prompt").cloned())
            .unwrap_or_else(|| value.clone());
        let output = value
            .get("tool_response")
            .or_else(|| value.get("agent_transcript_path"))
            .cloned()
            .unwrap_or(Value::Null);
        let span_id = if !tool_use_id.is_empty() {
            tool_use_id.to_string()
        } else {
            value
                .get("agent_id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        };
        let status = match event_name {
            "PreToolUse" | "SubagentStart" => "running",
            "PostToolUseFailure" => "error",
            "PostToolUse" | "SubagentStop" => "ok",
            _ => "ok",
        };
        self.append(json!({
            "source_id": source_id,
            "session_id": session_id,
            "trace_id": trace_id,
            "span_id": if span_id.is_empty() { Value::Null } else { json!(span_id) },
            "parent_span_id": value.get("parent_tool_use_id").cloned().unwrap_or(Value::Null),
            "timestamp": value.get("timestamp").cloned().unwrap_or_else(|| json!(now_timestamp())),
            "event_type": event_type,
            "name": name,
            "status": status,
            "input": input,
            "output": output,
            "metadata": common_metadata(value, "hook"),
        }))?;

        if event_name == "SubagentStart" {
            if let Some(agent_id) = value.get("agent_id").and_then(Value::as_str) {
                self.connection
                    .execute(
                        "INSERT INTO agent_sessions(agent_id,parent_session_id,parent_trace_id,parent_span_id)
                         VALUES (?1,?2,?3,?4)
                         ON CONFLICT(agent_id) DO UPDATE SET
                           parent_session_id=excluded.parent_session_id,
                           parent_trace_id=excluded.parent_trace_id,
                           parent_span_id=excluded.parent_span_id",
                        params![
                            agent_id,
                            session_id,
                            trace_id,
                            value.get("parent_tool_use_id").and_then(Value::as_str)
                        ],
                    )
                    .map_err(io_error)?;
            }
        }
        Ok(())
    }

    pub fn list_traces(&self) -> io::Result<Vec<Value>> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT t.trace_id,t.session_id,t.name,t.started_at,t.ended_at,t.status,
                        t.input_json,t.output_json,COUNT(e.id),GROUP_CONCAT(DISTINCT e.event_type)
                 FROM traces t LEFT JOIN events e ON e.trace_id=t.trace_id
                 GROUP BY t.trace_id ORDER BY t.started_at DESC",
            )
            .map_err(io_error)?;
        let rows = statement
            .query_map([], |row| {
                let input: Option<String> = row.get(6)?;
                let output: Option<String> = row.get(7)?;
                let event_types: Option<String> = row.get(9)?;
                Ok(json!({
                    "trace_id": row.get::<_, String>(0)?,
                    "session_id": row.get::<_, String>(1)?,
                    "name": row.get::<_, String>(2)?,
                    "started_at": row.get::<_, String>(3)?,
                    "ended_at": row.get::<_, Option<String>>(4)?,
                    "status": row.get::<_, String>(5)?,
                    "input": input.and_then(|text| serde_json::from_str::<Value>(&text).ok()),
                    "output": output.and_then(|text| serde_json::from_str::<Value>(&text).ok()),
                    "event_count": row.get::<_, u64>(8)?,
                    "event_types": event_types
                        .map(|types| types.split(',').map(str::to_owned).collect::<Vec<_>>())
                        .unwrap_or_default(),
                }))
            })
            .map_err(io_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(io_error)
    }

    pub fn spans_for_trace(&self, trace_id: &str) -> io::Result<Vec<Value>> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT session_id,span_id,parent_span_id,kind,name,started_at,ended_at,status,
                        input_json,output_json,metadata_json
                 FROM spans WHERE trace_id=?1 ORDER BY started_at,session_id,span_id",
            )
            .map_err(io_error)?;
        let rows = statement
            .query_map(params![trace_id], |row| {
                let input: Option<String> = row.get(8)?;
                let output: Option<String> = row.get(9)?;
                let metadata: String = row.get(10)?;
                Ok(json!({
                    "session_id": row.get::<_, String>(0)?,
                    "span_id": row.get::<_, String>(1)?,
                    "parent_span_id": row.get::<_, Option<String>>(2)?,
                    "kind": row.get::<_, String>(3)?,
                    "name": row.get::<_, String>(4)?,
                    "started_at": row.get::<_, String>(5)?,
                    "ended_at": row.get::<_, Option<String>>(6)?,
                    "status": row.get::<_, String>(7)?,
                    "input": input.and_then(|text| serde_json::from_str::<Value>(&text).ok()),
                    "output": output.and_then(|text| serde_json::from_str::<Value>(&text).ok()),
                    "metadata": serde_json::from_str::<Value>(&metadata).unwrap_or_else(|_| json!({})),
                }))
            })
            .map_err(io_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(io_error)
    }

    pub fn create_dataset(&self, name: &str, description: &str) -> io::Result<()> {
        self.connection
            .execute(
                "INSERT INTO datasets(name,description,created_at) VALUES (?1,?2,?3)
                 ON CONFLICT(name) DO UPDATE SET description=excluded.description",
                params![name, description, now_timestamp()],
            )
            .map_err(io_error)?;
        Ok(())
    }

    pub fn list_datasets(&self) -> io::Result<Vec<Value>> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT d.name,d.description,d.created_at,COUNT(i.item_id)
                 FROM datasets d LEFT JOIN dataset_items i USING(dataset_id)
                 GROUP BY d.dataset_id ORDER BY d.name COLLATE NOCASE",
            )
            .map_err(io_error)?;
        let rows = statement
            .query_map([], |row| {
                Ok(json!({
                    "name": row.get::<_, String>(0)?,
                    "description": row.get::<_, String>(1)?,
                    "created_at": row.get::<_, String>(2)?,
                    "items": row.get::<_, u64>(3)?,
                }))
            })
            .map_err(io_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(io_error)
    }

    pub fn update_dataset(
        &self,
        current_name: &str,
        name: &str,
        description: &str,
    ) -> io::Result<()> {
        let dataset_id = self
            .connection
            .query_row(
                "SELECT dataset_id FROM datasets WHERE name=?1",
                params![current_name],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .map_err(io_error)?
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("dataset not found: {current_name}"),
                )
            })?;
        let duplicate = self
            .connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM datasets WHERE name=?1 AND dataset_id<>?2)",
                params![name, dataset_id],
                |row| row.get::<_, bool>(0),
            )
            .map_err(io_error)?;
        if duplicate {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("dataset already exists: {name}"),
            ));
        }
        self.connection
            .execute(
                "UPDATE datasets SET name=?1,description=?2 WHERE dataset_id=?3",
                params![name, description, dataset_id],
            )
            .map_err(io_error)?;
        Ok(())
    }

    pub fn add_dataset_item(
        &self,
        dataset: &str,
        input: &Value,
        expected: Option<&Value>,
    ) -> io::Result<()> {
        let dataset_id = self
            .connection
            .query_row(
                "SELECT dataset_id FROM datasets WHERE name=?1",
                params![dataset],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .map_err(io_error)?
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("dataset not found: {dataset}"),
                )
            })?;
        self.connection
            .execute(
                "INSERT INTO dataset_items(dataset_id,input_json,expected_json,created_at)
                 VALUES (?1,?2,?3,?4)",
                params![
                    dataset_id,
                    input.to_string(),
                    expected.map(Value::to_string),
                    now_timestamp()
                ],
            )
            .map_err(io_error)?;
        Ok(())
    }

    pub fn dataset_items(&self, dataset: &str) -> io::Result<Vec<Value>> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT i.item_id,i.input_json,i.expected_json,i.metadata_json,i.created_at
                 FROM dataset_items i JOIN datasets d USING(dataset_id)
                 WHERE d.name=?1 ORDER BY i.item_id",
            )
            .map_err(io_error)?;
        let rows = statement
            .query_map(params![dataset], |row| {
                let input: String = row.get(1)?;
                let expected: Option<String> = row.get(2)?;
                let metadata: String = row.get(3)?;
                Ok(json!({
                    "item_id": row.get::<_, i64>(0)?,
                    "input": serde_json::from_str::<Value>(&input).unwrap_or(Value::Null),
                    "expected": expected.and_then(|text| serde_json::from_str::<Value>(&text).ok()),
                    "metadata": serde_json::from_str::<Value>(&metadata).unwrap_or_else(|_| json!({})),
                    "created_at": row.get::<_, String>(4)?,
                }))
            })
            .map_err(io_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(io_error)
    }

    pub fn update_dataset_item(
        &self,
        dataset: &str,
        item_id: i64,
        input: &Value,
        expected: Option<&Value>,
    ) -> io::Result<()> {
        let changed = self
            .connection
            .execute(
                "UPDATE dataset_items
                 SET input_json=?1,expected_json=?2
                 WHERE item_id=?3 AND dataset_id=(SELECT dataset_id FROM datasets WHERE name=?4)",
                params![
                    input.to_string(),
                    expected.map(Value::to_string),
                    item_id,
                    dataset
                ],
            )
            .map_err(io_error)?;
        if changed == 0 {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("dataset item {item_id} not found in {dataset}"),
            ));
        }
        Ok(())
    }

    pub fn save_prompt(&self, name: &str, template: &str) -> io::Result<u32> {
        let version: u32 = self
            .connection
            .query_row(
                "SELECT COALESCE(MAX(version),0)+1 FROM prompts WHERE name=?1",
                params![name],
                |row| row.get(0),
            )
            .map_err(io_error)?;
        self.connection
            .execute(
                "INSERT INTO prompts(name,version,template,created_at) VALUES (?1,?2,?3,?4)",
                params![name, version, template, now_timestamp()],
            )
            .map_err(io_error)?;
        Ok(version)
    }

    pub fn list_prompts(&self) -> io::Result<Vec<Value>> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT name,version,template,created_at FROM prompts ORDER BY name,version DESC",
            )
            .map_err(io_error)?;
        let rows = statement
            .query_map([], |row| {
                Ok(json!({
                    "name": row.get::<_, String>(0)?,
                    "version": row.get::<_, u32>(1)?,
                    "template": row.get::<_, String>(2)?,
                    "created_at": row.get::<_, String>(3)?,
                }))
            })
            .map_err(io_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(io_error)
    }

    pub fn get_prompt(&self, name: &str, version: Option<u32>) -> io::Result<Option<Value>> {
        self.connection
            .query_row(
                "SELECT name,version,template,created_at FROM prompts
                 WHERE name=?1 AND (?2 IS NULL OR version=?2)
                 ORDER BY version DESC LIMIT 1",
                params![name, version],
                |row| {
                    Ok(json!({
                        "name": row.get::<_, String>(0)?,
                        "version": row.get::<_, u32>(1)?,
                        "template": row.get::<_, String>(2)?,
                        "created_at": row.get::<_, String>(3)?,
                    }))
                },
            )
            .optional()
            .map_err(io_error)
    }

    pub fn record_evaluation(&self, trace_id: &str, score: f64, reason: &str) -> io::Result<()> {
        self.connection
            .execute(
                "INSERT INTO evaluations(trace_id,scorer,score,reason,created_at)
                 VALUES (?1,'manual',?2,?3,?4)",
                params![trace_id, score, reason, now_timestamp()],
            )
            .map_err(io_error)?;
        Ok(())
    }

    pub fn evaluate_dataset(&self, dataset: &str, scorer: &str) -> io::Result<(u64, f64)> {
        if !matches!(scorer, "exact" | "contains") {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "dataset scorer must be exact or contains",
            ));
        }
        let dataset_id = self
            .connection
            .query_row(
                "SELECT dataset_id FROM datasets WHERE name=?1",
                params![dataset],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .map_err(io_error)?
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("dataset not found: {dataset}"),
                )
            })?;
        let items = {
            let mut statement = self
                .connection
                .prepare(
                    "SELECT item_id,input_json,expected_json FROM dataset_items
                     WHERE dataset_id=?1 ORDER BY item_id",
                )
                .map_err(io_error)?;
            let rows = statement
                .query_map(params![dataset_id], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                    ))
                })
                .map_err(io_error)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(io_error)?
        };

        let mut scored = 0u64;
        let mut total = 0.0;
        for (item_id, input_json, expected_json) in items {
            let Some(expected_json) = expected_json else {
                continue;
            };
            let trace = self
                .connection
                .query_row(
                    "SELECT trace_id,output_json FROM traces
                     WHERE input_json=?1 ORDER BY started_at DESC LIMIT 1",
                    params![input_json],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
                )
                .optional()
                .map_err(io_error)?;
            let expected: Value = serde_json::from_str(&expected_json).map_err(io_error)?;
            let (trace_id, score, reason) = match trace {
                Some((trace_id, Some(actual_json))) => {
                    let actual: Value = serde_json::from_str(&actual_json).map_err(io_error)?;
                    let expected_text = text_value(&expected);
                    let actual_text = text_value(&actual);
                    let matched = if scorer == "exact" {
                        expected_text.trim() == actual_text.trim()
                    } else {
                        actual_text.contains(expected_text.trim())
                    };
                    (
                        Some(trace_id),
                        if matched { 1.0 } else { 0.0 },
                        if matched {
                            "matched"
                        } else {
                            "output did not match expected"
                        },
                    )
                }
                _ => (None, 0.0, "no completed trace matched this input"),
            };
            self.connection
                .execute(
                    "INSERT INTO evaluations(dataset_id,dataset_item_id,trace_id,scorer,score,reason,created_at)
                     VALUES (?1,?2,?3,?4,?5,?6,?7)",
                    params![dataset_id, item_id, trace_id, scorer, score, reason, now_timestamp()],
                )
                .map_err(io_error)?;
            scored += 1;
            total += score;
        }
        Ok((
            scored,
            if scored == 0 {
                0.0
            } else {
                total / scored as f64
            },
        ))
    }

    pub fn evaluations(&self, trace_id: Option<&str>) -> io::Result<Vec<Value>> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT evaluation_id,trace_id,scorer,score,reason,created_at FROM evaluations
                 WHERE (?1 IS NULL OR trace_id=?1) ORDER BY evaluation_id DESC",
            )
            .map_err(io_error)?;
        let rows = statement
            .query_map(params![trace_id], |row| {
                Ok(json!({
                    "id": row.get::<_, i64>(0)?,
                    "trace_id": row.get::<_, Option<String>>(1)?,
                    "scorer": row.get::<_, String>(2)?,
                    "score": row.get::<_, f64>(3)?,
                    "reason": row.get::<_, String>(4)?,
                    "created_at": row.get::<_, String>(5)?,
                }))
            })
            .map_err(io_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(io_error)
    }

    pub fn events_for_trace(&self, trace_id: &str) -> io::Result<Vec<Value>> {
        self.query_events(
            "SELECT event_json FROM events WHERE trace_id=?1 ORDER BY timestamp,id",
            params![trace_id],
        )
    }

    pub fn search(&self, query: &str) -> io::Result<Vec<Value>> {
        let pattern = format!("%{}%", query.replace('%', "\\%").replace('_', "\\_"));
        let mut statement = self
            .connection
            .prepare(
                "SELECT event_json FROM events WHERE event_json LIKE ?1 ESCAPE '\\' COLLATE NOCASE
                 ORDER BY timestamp DESC,id DESC LIMIT 500",
            )
            .map_err(io_error)?;
        let rows = statement
            .query_map(params![pattern], |row| row.get::<_, String>(0))
            .map_err(io_error)?;
        rows.map(|row| {
            let text = row.map_err(io_error)?;
            serde_json::from_str(&text).map_err(io_error)
        })
        .collect()
    }

    fn query_events(&self, sql: &str, parameters: impl rusqlite::Params) -> io::Result<Vec<Value>> {
        let mut statement = self.connection.prepare(sql).map_err(io_error)?;
        let rows = statement
            .query_map(parameters, |row| row.get::<_, String>(0))
            .map_err(io_error)?;
        rows.map(|row| {
            let text = row.map_err(io_error)?;
            serde_json::from_str(&text).map_err(io_error)
        })
        .collect()
    }

    fn current_trace(&self, session_id: &str) -> io::Result<Option<TraceContext>> {
        let direct = self
            .connection
            .query_row(
                "SELECT trace_id,prompt_hash,parent_span_id FROM current_traces WHERE session_id=?1",
                params![session_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .map_err(io_error)?;
        if direct.is_some() {
            return Ok(direct);
        }
        let agent_id = session_id.strip_prefix("agent-").unwrap_or(session_id);
        self.connection
            .query_row(
                "SELECT parent_trace_id,NULL,parent_span_id FROM agent_sessions WHERE agent_id=?1",
                params![agent_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .map_err(io_error)
    }

    fn is_agent_session(&self, session_id: &str) -> io::Result<bool> {
        let agent_id = session_id.strip_prefix("agent-").unwrap_or(session_id);
        self.connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM agent_sessions WHERE agent_id=?1)",
                params![agent_id],
                |row| row.get(0),
            )
            .map_err(io_error)
    }

    fn set_current_trace(
        &self,
        session_id: &str,
        trace_id: &str,
        prompt_hash: Option<&str>,
        parent_span_id: Option<&str>,
    ) -> io::Result<()> {
        self.connection
            .execute(
                "INSERT INTO current_traces(session_id,trace_id,prompt_hash,parent_span_id)
                 VALUES (?1,?2,?3,?4)
                 ON CONFLICT(session_id) DO UPDATE SET
                   trace_id=excluded.trace_id,
                   prompt_hash=excluded.prompt_hash,
                   parent_span_id=excluded.parent_span_id",
                params![session_id, trace_id, prompt_hash, parent_span_id],
            )
            .map_err(io_error)?;
        Ok(())
    }

    fn has_source(&self, source_id: &str) -> io::Result<bool> {
        self.connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM events WHERE source_id=?1)",
                params![source_id],
                |row| row.get(0),
            )
            .map_err(io_error)
    }

    fn append(&mut self, event: Value) -> io::Result<()> {
        let source_id = field_str(&event, "source_id").unwrap_or("");
        let session_id = field_str(&event, "session_id").unwrap_or("unknown");
        let trace_id = field_str(&event, "trace_id").unwrap_or("unknown");
        let timestamp = field_str(&event, "timestamp").unwrap_or("");
        let event_type = field_str(&event, "event_type").unwrap_or("event");
        let name = field_str(&event, "name").unwrap_or(event_type);
        let status = field_str(&event, "status");
        let event_json = event.to_string();
        let input_json = json_field(&event, "input")?;
        let output_json = json_field(&event, "output")?;
        let metadata_json = event
            .get("metadata")
            .cloned()
            .unwrap_or_else(|| json!({}))
            .to_string();
        let span_id = field_str(&event, "span_id");
        let parent_span_id = field_str(&event, "parent_span_id");

        let transaction = self.connection.transaction().map_err(io_error)?;
        let inserted = transaction
            .execute(
                "INSERT OR IGNORE INTO events
                 (source_id,session_id,trace_id,span_id,parent_span_id,timestamp,event_type,name,status,input_json,output_json,metadata_json,event_json)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
                params![
                    source_id,
                    session_id,
                    trace_id,
                    span_id,
                    parent_span_id,
                    timestamp,
                    event_type,
                    name,
                    status,
                    input_json,
                    output_json,
                    metadata_json,
                    event_json
                ],
            )
            .map_err(io_error)?;
        if inserted > 0 {
            update_projection(&transaction, &event)?;
        }
        transaction.commit().map_err(io_error)
    }
}

fn update_projection(transaction: &Transaction<'_>, event: &Value) -> io::Result<()> {
    let trace_id = field_str(event, "trace_id").unwrap_or("unknown");
    let session_id = field_str(event, "session_id").unwrap_or("unknown");
    let timestamp = field_str(event, "timestamp").unwrap_or("");
    let event_type = field_str(event, "event_type").unwrap_or("");
    let name = field_str(event, "name").unwrap_or(event_type);
    let status = field_str(event, "status").unwrap_or("ok");
    let span_id = field_str(event, "span_id");
    let parent_span_id = field_str(event, "parent_span_id");
    let input = json_field(event, "input")?;
    let output = json_field(event, "output")?;
    let metadata = event
        .get("metadata")
        .cloned()
        .unwrap_or_else(|| json!({}))
        .to_string();

    if event_type == "trace.input" {
        transaction
            .execute(
                "INSERT OR IGNORE INTO traces(trace_id,session_id,name,started_at,status,input_json,metadata_json)
                 VALUES (?1,?2,?3,?4,'running',?5,?6)",
                params![trace_id, session_id, name, timestamp, input, metadata],
            )
            .map_err(io_error)?;
    } else if event_type == "trace.end" {
        transaction
            .execute(
                "UPDATE traces SET ended_at=?2,status=?3,output_json=COALESCE(?4,output_json)
                 WHERE trace_id=?1",
                params![trace_id, timestamp, status, output],
            )
            .map_err(io_error)?;
    }

    let Some(span_id) = span_id else {
        return Ok(());
    };
    let (kind, ended_at, span_status) = match event_type {
        "model.response" => ("model", Some(timestamp), "ok"),
        "tool.start" | "tool.pre" => ("tool", None, "running"),
        "tool.result" | "tool.post" => ("tool", Some(timestamp), "ok"),
        "tool.failure" => ("tool", Some(timestamp), "error"),
        "agent.start" => ("agent", None, "running"),
        "agent.end" => ("agent", Some(timestamp), "ok"),
        "agent.prompt" => ("agent", Some(timestamp), "ok"),
        _ => return Ok(()),
    };
    transaction
        .execute(
            "INSERT INTO spans(session_id,span_id,trace_id,parent_span_id,kind,name,started_at,ended_at,status,input_json,output_json,metadata_json)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)
             ON CONFLICT(session_id,span_id) DO UPDATE SET
                trace_id=excluded.trace_id,
                parent_span_id=COALESCE(excluded.parent_span_id,spans.parent_span_id),
                kind=excluded.kind,
                name=excluded.name,
                ended_at=COALESCE(excluded.ended_at,spans.ended_at),
                status=CASE WHEN excluded.ended_at IS NULL THEN spans.status ELSE excluded.status END,
                input_json=COALESCE(excluded.input_json,spans.input_json),
                output_json=COALESCE(excluded.output_json,spans.output_json),
                metadata_json=excluded.metadata_json",
            params![
                session_id,
                span_id,
                trace_id,
                parent_span_id,
                kind,
                name,
                timestamp,
                ended_at,
                span_status,
                input,
                output,
                metadata
            ],
        )
        .map_err(io_error)?;
    Ok(())
}

fn field_str<'a>(value: &'a Value, field: &str) -> Option<&'a str> {
    value.get(field).and_then(Value::as_str)
}

fn json_field(value: &Value, field: &str) -> io::Result<Option<String>> {
    value
        .get(field)
        .filter(|value| !value.is_null())
        .map(serde_json::to_string)
        .transpose()
        .map_err(io_error)
}

fn is_prompt(content: &Value) -> bool {
    match content {
        Value::String(text) => !text.trim().is_empty(),
        Value::Array(blocks) => {
            blocks.iter().any(|block| {
                block.get("type").and_then(Value::as_str) == Some("text")
                    && block
                        .get("text")
                        .and_then(Value::as_str)
                        .is_some_and(|text| !text.trim().is_empty())
            }) && !blocks
                .iter()
                .any(|block| block.get("type").and_then(Value::as_str) == Some("tool_result"))
        }
        _ => false,
    }
}

fn fingerprint_prompt(content: &Value) -> String {
    match content {
        Value::String(text) => stable_id(text),
        Value::Array(blocks) => {
            let text = blocks
                .iter()
                .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|block| block.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n");
            stable_id(&text)
        }
        _ => stable_id(&content.to_string()),
    }
}

fn text_value(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => value.to_string(),
    }
}

fn common_metadata(value: &Value, source: &str) -> Value {
    let mut metadata = serde_json::Map::new();
    metadata.insert("source".into(), json!(source));
    for key in [
        "cwd",
        "gitBranch",
        "agent_id",
        "agent_type",
        "hook_event_name",
    ] {
        if let Some(value) = value.get(key) {
            metadata.insert(key.into(), value.clone());
        }
    }
    Value::Object(metadata)
}

fn stable_id(value: &str) -> String {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in value.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

fn now_timestamp() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true)
}

fn io_error(error: impl std::error::Error + Send + Sync + 'static) -> io::Error {
    io::Error::other(error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn record(path: &Path, offset: u64, value: Value) -> TranscriptRecord {
        TranscriptRecord {
            path: path.to_path_buf(),
            offset,
            value,
        }
    }

    #[test]
    fn persists_prompt_tool_call_and_correlated_result() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("session.jsonl");
        let mut store = TraceStore::open(dir.path().join("traces.db")).unwrap();
        for (offset, value) in [
            (
                0,
                json!({"type":"user","timestamp":"2026-10-27T10:00:00Z","message":{"content":"hello"}}),
            ),
            (
                100,
                json!({"type":"assistant","timestamp":"2026-10-27T10:00:01Z","message":{"id":"msg-1","model":"claude-test","content":[{"type":"tool_use","id":"call-1","name":"Read","input":{"file_path":"secret.txt"}}]}}),
            ),
            (
                200,
                json!({"type":"user","timestamp":"2026-10-27T10:00:02Z","message":{"content":[{"type":"tool_result","tool_use_id":"call-1","content":"file contents"}]}}),
            ),
            (
                300,
                json!({"type":"assistant","timestamp":"2026-10-27T10:00:03Z","message":{"id":"msg-2","model":"claude-test","content":[{"type":"text","text":"done"}],"stop_reason":"end_turn"}}),
            ),
        ] {
            store
                .ingest_transcript(record(&path, offset, value))
                .unwrap();
        }

        let traces = store.list_traces().unwrap();
        assert_eq!(traces.len(), 1);
        assert!(traces[0]["event_types"]
            .as_array()
            .unwrap()
            .contains(&json!("tool.start")));
        let trace_id = traces[0]["trace_id"].as_str().unwrap();
        assert_eq!(traces[0]["output"][0]["text"], "done");
        let events = store.events_for_trace(trace_id).unwrap();
        let call = events
            .iter()
            .find(|event| event["event_type"] == "tool.start")
            .unwrap();
        let result = events
            .iter()
            .find(|event| event["event_type"] == "tool.result")
            .unwrap();
        assert_eq!(call["span_id"], result["span_id"]);
        assert_eq!(call["input"]["file_path"], "secret.txt");
        assert_eq!(result["output"], "file contents");
        let spans = store.spans_for_trace(trace_id).unwrap();
        let tool_span = spans.iter().find(|span| span["name"] == "Read").unwrap();
        assert_eq!(tool_span["kind"], "tool");
        assert_eq!(tool_span["span_id"], call["span_id"]);
    }

    #[test]
    fn replay_is_idempotent_and_database_is_owner_only() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("session.jsonl");
        let db = dir.path().join("traces.db");
        let value = json!({"type":"user","message":{"content":"persist me"}});
        {
            let mut store = TraceStore::open(&db).unwrap();
            store
                .ingest_transcript(record(&path, 42, value.clone()))
                .unwrap();
            store.ingest_transcript(record(&path, 42, value)).unwrap();
            assert_eq!(store.list_traces().unwrap().len(), 1);
        }
        assert_eq!(
            fs::metadata(&db).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let store = TraceStore::open(&db).unwrap();
        assert_eq!(store.list_traces().unwrap().len(), 1);
    }

    #[test]
    fn prompts_start_new_turn_traces_and_search_finds_payloads() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("session.jsonl");
        let mut store = TraceStore::open(dir.path().join("traces.db")).unwrap();
        for (offset, prompt) in [(0, "first"), (50, "second")] {
            store
                .ingest_transcript(record(
                    &path,
                    offset,
                    json!({"type":"user","message":{"content":prompt}}),
                ))
                .unwrap();
        }
        let traces = store.list_traces().unwrap();
        assert_eq!(traces.len(), 2);
        assert_eq!(store.search("second").unwrap().len(), 1);
    }

    #[test]
    fn prompt_hook_and_transcript_share_one_trace() {
        let dir = TempDir::new().unwrap();
        let db = dir.path().join("traces.db");
        let path = dir.path().join("session.jsonl");
        let mut store = TraceStore::open(&db).unwrap();
        store
            .ingest_hook(&json!({
                "hook_event_name":"UserPromptSubmit",
                "session_id":"session",
                "prompt":"hello"
            }))
            .unwrap();
        let hook_trace = store.current_trace("session").unwrap().unwrap().0;
        store
            .ingest_transcript(record(
                &path,
                0,
                json!({"type":"user","message":{"content":"hello"}}),
            ))
            .unwrap();
        assert_eq!(store.list_traces().unwrap()[0]["trace_id"], hook_trace);
    }

    #[test]
    fn subagent_prompt_is_nested_in_parent_task_trace() {
        let dir = TempDir::new().unwrap();
        let parent_path = dir.path().join("session.jsonl");
        let child_path = dir.path().join("agent-agent-42.jsonl");
        let mut store = TraceStore::open(dir.path().join("traces.db")).unwrap();
        store
            .ingest_transcript(record(
                &parent_path,
                0,
                json!({"type":"user","message":{"content":"delegate this"}}),
            ))
            .unwrap();
        store
            .ingest_transcript(record(
                &parent_path,
                50,
                json!({"type":"assistant","message":{"content":[{"type":"tool_use","id":"task-call","name":"Task","input":{"prompt":"child task"}}]}}),
            ))
            .unwrap();
        let parent_trace = store.list_traces().unwrap()[0]["trace_id"]
            .as_str()
            .unwrap()
            .to_string();
        store
            .ingest_hook(&json!({
                "hook_event_name":"SubagentStart",
                "session_id":"session",
                "agent_id":"agent-42",
                "agent_type":"Explore",
                "parent_tool_use_id":"task-call"
            }))
            .unwrap();
        store
            .ingest_transcript(record(
                &child_path,
                0,
                json!({"type":"user","message":{"content":"child prompt"}}),
            ))
            .unwrap();
        let child = store
            .events_for_trace(&parent_trace)
            .unwrap()
            .into_iter()
            .find(|event| event["event_type"] == "agent.prompt")
            .unwrap();
        assert_eq!(child["parent_span_id"], "task-call");
        assert_eq!(child["trace_id"], parent_trace);
    }

    #[test]
    fn dataset_edits_preserve_items_and_prompt_edits_append_versions() {
        let dir = TempDir::new().unwrap();
        let store = TraceStore::open(dir.path().join("traces.db")).unwrap();
        store.create_dataset("support", "old description").unwrap();
        store
            .add_dataset_item("support", &json!({"question":"old"}), Some(&json!("old")))
            .unwrap();
        let item_id = store.dataset_items("support").unwrap()[0]["item_id"]
            .as_i64()
            .unwrap();

        store
            .update_dataset("support", "support-v2", "updated description")
            .unwrap();
        store
            .update_dataset_item(
                "support-v2",
                item_id,
                &json!({"question":"new"}),
                Some(&json!("new")),
            )
            .unwrap();
        let datasets = store.list_datasets().unwrap();
        assert_eq!(datasets[0]["name"], "support-v2");
        assert_eq!(datasets[0]["description"], "updated description");
        assert_eq!(datasets[0]["items"], 1);
        let items = store.dataset_items("support-v2").unwrap();
        assert_eq!(items[0]["input"]["question"], "new");
        assert_eq!(items[0]["expected"], "new");
        assert_eq!(
            store
                .update_dataset_item("support", item_id, &json!("x"), None)
                .unwrap_err()
                .kind(),
            io::ErrorKind::NotFound
        );

        assert_eq!(store.save_prompt("assistant", "draft").unwrap(), 1);
        assert_eq!(store.save_prompt("assistant", "final").unwrap(), 2);
        assert_eq!(
            store.get_prompt("assistant", Some(1)).unwrap().unwrap()["template"],
            "draft"
        );
        assert_eq!(
            store.get_prompt("assistant", None).unwrap().unwrap()["template"],
            "final"
        );
    }

    #[test]
    fn exact_dataset_evaluation_scores_completed_trace() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("session.jsonl");
        let mut store = TraceStore::open(dir.path().join("traces.db")).unwrap();
        store
            .create_dataset("greeting", "greeting quality")
            .unwrap();
        store
            .add_dataset_item("greeting", &json!("hello"), Some(&json!("hi")))
            .unwrap();
        store
            .ingest_transcript(record(
                &path,
                0,
                json!({"type":"user","message":{"content":"hello"}}),
            ))
            .unwrap();
        store
            .ingest_transcript(record(
                &path,
                50,
                json!({"type":"assistant","message":{"content":[{"type":"text","text":"hi"}],"stop_reason":"end_turn"}}),
            ))
            .unwrap();
        let (count, average) = store.evaluate_dataset("greeting", "exact").unwrap();
        assert_eq!(count, 1);
        assert_eq!(average, 1.0);
        assert_eq!(store.evaluations(None).unwrap()[0]["score"], 1.0);
    }
}
