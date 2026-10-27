//! Import and export for the Stash canonical trace format.

use std::collections::HashSet;
use std::io;

use chrono::{DateTime, Duration, SecondsFormat, Utc};
use serde_json::{json, Map, Value};

#[derive(Debug, Clone)]
pub struct StashTrace {
    pub id: String,
    pub title: String,
    pub metadata: Value,
    pub steps: Vec<Value>,
}

pub fn parse_jsonl(input: &str) -> io::Result<Vec<StashTrace>> {
    let mut traces = Vec::new();
    let mut ids = HashSet::new();

    for (line_index, line) in input.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let value: Value = serde_json::from_str(line).map_err(|error| {
            invalid(format!(
                "Stash trace line {} is invalid JSON: {error}",
                line_index + 1
            ))
        })?;
        let object = value.as_object().ok_or_else(|| {
            invalid(format!(
                "Stash trace line {} must be an object",
                line_index + 1
            ))
        })?;

        let id = match object.get("id") {
            Some(Value::String(id)) if !id.is_empty() => id.clone(),
            Some(_) => {
                return Err(invalid(format!(
                    "Stash trace line {} id must be a non-empty string",
                    line_index + 1
                )))
            }
            None => format!("auto-{}-{}", line_index + 1, stable_id(line)),
        };
        if !ids.insert(id.clone()) {
            return Err(invalid(format!(
                "Stash import contains duplicate trace id {id:?}"
            )));
        }

        let provided_title = match object.get("title") {
            Some(Value::String(title)) => Some(title.clone()),
            Some(_) => {
                return Err(invalid(format!(
                    "Stash trace {id:?} title must be a string"
                )))
            }
            None => None,
        };
        let metadata = match object.get("metadata") {
            Some(Value::Object(metadata)) => Value::Object(metadata.clone()),
            Some(_) => {
                return Err(invalid(format!(
                    "Stash trace {id:?} metadata must be an object"
                )))
            }
            None => json!({}),
        };
        let steps = object
            .get("steps")
            .and_then(Value::as_array)
            .ok_or_else(|| invalid(format!("Stash trace {id:?} steps must be an array")))?;
        if steps.is_empty() {
            return Err(invalid(format!(
                "Stash trace {id:?} must contain at least one step"
            )));
        }

        let mut has_non_system_step = false;
        let mut first_user_content = None;
        for (step_index, step) in steps.iter().enumerate() {
            let step_object = step.as_object().ok_or_else(|| {
                invalid(format!(
                    "Stash trace {id:?} step {} must be an object",
                    step_index + 1
                ))
            })?;
            let role = step_object
                .get("role")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    invalid(format!(
                        "Stash trace {id:?} step {} role must be a string",
                        step_index + 1
                    ))
                })?;
            if !matches!(role, "system" | "user" | "assistant" | "tool") {
                return Err(invalid(format!(
                    "Stash trace {id:?} step {} has unsupported role {role:?}",
                    step_index + 1
                )));
            }
            let content = step_object
                .get("content")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    invalid(format!(
                        "Stash trace {id:?} step {} content must be a string",
                        step_index + 1
                    ))
                })?;
            if role != "system" {
                has_non_system_step = true;
            }
            if role == "user" && first_user_content.is_none() {
                first_user_content = Some(content);
            }
            for field in ["tool_name", "tool_call_id"] {
                if step_object
                    .get(field)
                    .is_some_and(|value| !value.is_string())
                {
                    return Err(invalid(format!(
                        "Stash trace {id:?} step {} {field} must be a string",
                        step_index + 1
                    )));
                }
            }
            if step_object
                .get("tool_input")
                .is_some_and(|value| !value.is_object())
            {
                return Err(invalid(format!(
                    "Stash trace {id:?} step {} tool_input must be an object",
                    step_index + 1
                )));
            }
            if step_object
                .get("metadata")
                .is_some_and(|value| !value.is_object())
            {
                return Err(invalid(format!(
                    "Stash trace {id:?} step {} metadata must be an object",
                    step_index + 1
                )));
            }
        }
        if !has_non_system_step {
            return Err(invalid(format!(
                "Stash trace {id:?} must contain a non-system step"
            )));
        }
        let title = provided_title
            .or_else(|| first_user_content.map(|content| content.chars().take(80).collect()))
            .ok_or_else(|| invalid(format!("Stash trace {id:?} needs a title or a user step")))?;

        traces.push(StashTrace {
            id,
            title,
            metadata,
            steps: steps.clone(),
        });
    }

    if traces.is_empty() {
        return Err(invalid("Stash import contains no traces"));
    }
    Ok(traces)
}

pub fn imported_events(trace: &StashTrace) -> Vec<Value> {
    let trace_id = format!("stash:{}", trace.id);
    let session_id = trace_id.clone();
    let base = Utc::now();
    let stash_metadata = trace.metadata.clone();
    let root_metadata = json!({
        "source": "stash",
        "format": "stash",
        "stash_id": trace.id,
        "stash_metadata": stash_metadata,
    });
    let initial_input = trace
        .steps
        .iter()
        .find(|step| step["role"] == "user")
        .map(|step| step["content"].clone())
        .unwrap_or(Value::Null);
    let mut events = vec![json!({
        "source_id": format!("stash:{}:trace.input", trace.id),
        "session_id": session_id,
        "trace_id": trace_id,
        "span_id": trace_id,
        "parent_span_id": null,
        "timestamp": timestamp(base, 0),
        "event_type": "trace.input",
        "name": trace.title,
        "status": "running",
        "input": initial_input,
        "output": null,
        "metadata": root_metadata,
    })];

    let mut final_output = Value::Null;
    for (index, step) in trace.steps.iter().enumerate() {
        let offset = (index as i64 + 1) * 3;
        let role = step["role"].as_str().unwrap_or("assistant");
        let tool_call_id = step["tool_call_id"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| format!("step-{index}"));
        let tool_span_id = format!("stash-tool:{tool_call_id}");
        let step_span_id = format!("stash-step-{index}");
        let mut metadata = Map::new();
        metadata.insert("source".into(), json!("stash"));
        metadata.insert("step_index".into(), json!(index));
        events.push(json!({
            "source_id": format!("stash:{}:step:{index}", trace.id),
            "session_id": session_id,
            "trace_id": trace_id,
            "span_id": step_span_id,
            "parent_span_id": trace_id,
            "timestamp": timestamp(base, offset),
            "event_type": "conversation.step",
            "name": format!("{} step {}", role, index + 1),
            "status": "ok",
            "input": null,
            "output": step,
            "metadata": metadata,
        }));

        if role == "assistant" && step.get("tool_name").and_then(Value::as_str).is_some() {
            let tool_name = step["tool_name"].as_str().unwrap_or("tool");
            events.push(json!({
                "source_id": format!("stash:{}:tool.start:{index}", trace.id),
                "session_id": session_id,
                "trace_id": trace_id,
                "span_id": tool_span_id,
                "parent_span_id": trace_id,
                "timestamp": timestamp(base, offset + 1),
                "event_type": "tool.start",
                "name": tool_name,
                "status": "running",
                "input": step.get("tool_input").cloned().unwrap_or_else(|| json!({})),
                "output": null,
                "metadata": { "source": "stash", "tool_call_id": tool_call_id },
            }));
        } else if role == "tool" {
            let is_error = step
                .pointer("/metadata/is_error")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            events.push(json!({
                "source_id": format!("stash:{}:tool.result:{index}", trace.id),
                "session_id": session_id,
                "trace_id": trace_id,
                "span_id": tool_span_id,
                "parent_span_id": trace_id,
                "timestamp": timestamp(base, offset + 1),
                "event_type": if is_error { "tool.failure" } else { "tool.result" },
                "name": step.get("tool_name").and_then(Value::as_str).unwrap_or("Tool result"),
                "status": if is_error { "error" } else { "ok" },
                "input": null,
                "output": step["content"],
                "metadata": { "source": "stash", "tool_call_id": tool_call_id },
            }));
        }
        if role == "assistant" {
            final_output = step["content"].clone();
        }
    }

    events.push(json!({
        "source_id": format!("stash:{}:trace.end", trace.id),
        "session_id": session_id,
        "trace_id": trace_id,
        "span_id": trace_id,
        "parent_span_id": null,
        "timestamp": timestamp(base, trace.steps.len() as i64 * 3 + 3),
        "event_type": "trace.end",
        "name": trace.title,
        "status": "ok",
        "input": null,
        "output": final_output,
        "metadata": root_metadata,
    }));
    events
}

pub fn export_trace(trace: &Value, events: &[Value]) -> Value {
    let root = events
        .iter()
        .find(|event| event["event_type"] == "trace.input");
    if root
        .and_then(|event| event.pointer("/metadata/source"))
        .and_then(Value::as_str)
        == Some("stash")
    {
        let metadata = root
            .and_then(|event| event.pointer("/metadata/stash_metadata"))
            .cloned()
            .unwrap_or_else(|| json!({}));
        let id = root
            .and_then(|event| event.pointer("/metadata/stash_id"))
            .and_then(Value::as_str)
            .unwrap_or_else(|| trace["trace_id"].as_str().unwrap_or("trace"));
        let steps = events
            .iter()
            .filter(|event| event["event_type"] == "conversation.step")
            .map(|event| event["output"].clone())
            .collect::<Vec<_>>();
        return json!({ "id": id, "title": trace["name"], "metadata": metadata, "steps": steps });
    }

    let mut steps = Vec::new();
    for event in events {
        match event["event_type"].as_str().unwrap_or("") {
            "trace.input" => append_request_steps(&mut steps, &event["input"]),
            "agent.prompt" | "transcript.user" => {
                push_step(&mut steps, "user", &event["input"], json!({}))
            }
            "assistant.text" => push_step(&mut steps, "assistant", &event["output"], json!({})),
            "assistant.thinking" => push_step(
                &mut steps,
                "assistant",
                &event["output"],
                json!({ "thinking": true }),
            ),
            "tool.start" | "tool.pre" => {
                let tool_id = event["span_id"].as_str().unwrap_or("tool-call");
                let mut step = json!({
                    "role": "assistant",
                    "content": "",
                    "tool_name": event["name"],
                    "tool_input": object_or_wrap(&event["input"]),
                    "tool_call_id": tool_id,
                });
                if let Some(metadata) = step.as_object_mut() {
                    metadata.insert(
                        "metadata".into(),
                        json!({ "iris_event_type": event["event_type"] }),
                    );
                }
                steps.push(step);
            }
            "tool.result" | "tool.post" | "tool.failure" => {
                let mut metadata = Map::new();
                if event["status"] == "error" || event["event_type"] == "tool.failure" {
                    metadata.insert("is_error".into(), json!(true));
                }
                steps.push(json!({
                    "role": "tool",
                    "content": content_string(&event["output"]),
                    "tool_name": event["name"],
                    "tool_call_id": event["span_id"],
                    "metadata": metadata,
                }));
            }
            "model.response"
                if event.pointer("/metadata/source").and_then(Value::as_str)
                    == Some("openrouter-agent") =>
            {
                append_response_steps(&mut steps, &event["output"]);
            }
            _ => {}
        }
    }
    if !steps.iter().any(|step| step["role"] != "system") {
        steps.push(
            json!({ "role": "assistant", "content": "", "metadata": { "iris_event_only": true } }),
        );
    }

    let title = trace["name"]
        .as_str()
        .filter(|title| {
            !title.is_empty() && !matches!(*title, "User prompt" | "OpenRouter agent run")
        })
        .map(str::to_owned)
        .or_else(|| {
            steps
                .iter()
                .find(|step| step["role"] == "user")
                .and_then(|step| step["content"].as_str())
                .map(|content| content.chars().take(80).collect())
        })
        .unwrap_or_else(|| "Iris trace".to_owned());
    let raw_events = Value::Array(events.to_vec());
    let mut metadata = Map::new();
    metadata.insert("iris_events".into(), raw_events);
    metadata.insert("iris_session_id".into(), trace["session_id"].clone());
    metadata.insert("iris_status".into(), trace["status"].clone());
    json!({
        "id": trace["trace_id"],
        "title": title,
        "metadata": metadata,
        "steps": steps,
    })
}

fn append_request_steps(steps: &mut Vec<Value>, input: &Value) {
    if let Some(messages) = input.as_array() {
        append_messages(steps, messages);
        return;
    }
    if let Some(system) = input.get("system").and_then(Value::as_str) {
        push_step(steps, "system", &json!(system), json!({}));
    }
    if let Some(messages) = input.get("messages").and_then(Value::as_array) {
        append_messages(steps, messages);
    } else if let Some(prompt) = input.get("input") {
        push_step(steps, "user", prompt, json!({}));
    } else if !input.is_null() {
        push_step(steps, "user", input, json!({}));
    }
}

fn append_messages(steps: &mut Vec<Value>, messages: &[Value]) {
    for message in messages {
        if let Some(role) = message.get("role").and_then(Value::as_str) {
            let role = if role == "developer" { "system" } else { role };
            if matches!(role, "system" | "user" | "assistant" | "tool") {
                push_step(steps, role, &message["content"], json!({}));
            }
        }
    }
}

fn append_response_steps(steps: &mut Vec<Value>, output: &Value) {
    let response = output.get("response").unwrap_or(output);
    let items = response.get("output").and_then(Value::as_array);
    if let Some(items) = items {
        for item in items {
            if item.get("type").and_then(Value::as_str) != Some("message") {
                continue;
            }
            if let Some(content) = item.get("content") {
                if let Some(parts) = content.as_array() {
                    for part in parts {
                        if let Some(text) = part.get("text") {
                            push_step(steps, "assistant", text, json!({}));
                        } else if let Some(thinking) = part.get("thinking") {
                            push_step(steps, "assistant", thinking, json!({ "thinking": true }));
                        }
                    }
                } else if content.is_string() {
                    push_step(steps, "assistant", content, json!({}));
                }
            }
        }
    } else if let Some(text) = response.get("text") {
        push_step(steps, "assistant", text, json!({}));
    }
}

fn push_step(steps: &mut Vec<Value>, role: &str, content: &Value, metadata: Value) {
    let content = content_string(content);
    steps.push(json!({ "role": role, "content": content, "metadata": metadata }));
}

fn object_or_wrap(value: &Value) -> Value {
    if value.is_object() {
        value.clone()
    } else if let Some(text) = value.as_str() {
        serde_json::from_str::<Value>(text)
            .ok()
            .filter(Value::is_object)
            .unwrap_or_else(|| json!({ "raw": text }))
    } else {
        json!({})
    }
}

fn content_string(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        _ => value.to_string(),
    }
}

fn timestamp(base: DateTime<Utc>, microseconds: i64) -> String {
    (base + Duration::microseconds(microseconds)).to_rfc3339_opts(SecondsFormat::Nanos, true)
}

fn stable_id(value: &str) -> String {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in value.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}
