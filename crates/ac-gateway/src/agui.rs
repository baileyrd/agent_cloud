//! A job as an AG-UI run (#471): the mapping from the registry's job state and the agent's
//! stream-json log to AG-UI events, framed as Server-Sent Events. No I/O here; the stream that
//! polls the registry and the log is [`crate::jobs::EventStream`].
//!
//! | Source | AG-UI |
//! |---|---|
//! | stream opens | `RUN_STARTED` (thread = agent id, run = job id), then `STATE_SNAPSHOT` (the job as `GET .../jobs/{job}` renders it) |
//! | registry state changes | `STATE_SNAPSHOT` |
//! | log: `assistant` text block | `TEXT_MESSAGE_START`/`CONTENT`/`END` |
//! | log: `assistant` `tool_use` block | `TOOL_CALL_START`/`ARGS`/`END` (args = the input as JSON) |
//! | log: `user` `tool_result` block | `TOOL_CALL_RESULT` |
//! | log: anything else (`system`, `result`, thinking, unparsable lines) | nothing |
//! | job `succeeded` | `RUN_FINISHED` (result = the job) |
//! | job `failed`, `cancelled`, `timed_out`, `interrupted` | `RUN_ERROR` (code = the state) |
//!
//! The outcome comes from the registry, never from the log: the log is agent-written, so a
//! compromised agent can put any text in the messages but cannot make a failed job look finished.

use ac_domain::job::{Job, JobState};
pub use rusty_agui::EventKind;
use rusty_agui::{Event, Role, Verifier};
use rusty_json::Value;

/// Longest log line translated; a longer one is skipped (a tool result holding a whole file).
pub const MAX_LINE: usize = 1024 * 1024;
/// Longest text passed on in one event; the rest is cut with [`CUT`].
pub const MAX_TEXT: usize = 64 * 1024;
const CUT: &str = "\n[cut by agent_cloud]";

/// `RUN_STARTED` for `job`.
pub fn started(job: &Job) -> EventKind {
    EventKind::RunStarted {
        thread_id: job.agent.to_string(),
        run_id: job.id.to_string(),
        parent_run_id: None,
        input: None,
    }
}

/// `STATE_SNAPSHOT` of `job`, rendered as the job API renders it.
pub fn snapshot(rendered: &str) -> EventKind {
    EventKind::StateSnapshot {
        snapshot: Value::parse(rendered).unwrap_or(Value::Null),
    }
}

/// `CUSTOM` `log_incomplete`, sent before the run's last event when the stream could not read
/// the log to its end: `why` is `read_failed` or `read_limit`. The job's outcome is unchanged.
pub fn log_incomplete(why: &str) -> EventKind {
    EventKind::Custom {
        name: "log_incomplete".to_owned(),
        value: Value::String(why.to_owned()),
    }
}

/// The run's last event, once `job` has ended; `None` while it is queued or running.
pub fn ended(job: &Job, rendered: &str) -> Option<EventKind> {
    match job.state {
        JobState::Queued | JobState::Running => None,
        JobState::Succeeded => Some(EventKind::RunFinished {
            thread_id: job.agent.to_string(),
            run_id: job.id.to_string(),
            result: Value::parse(rendered).ok(),
            outcome: None,
        }),
        state => Some(EventKind::RunError {
            message: match &job.reason {
                Some(why) => format!("job {}: {why}", state.name()),
                None => format!("job {}", state.name()),
            },
            code: Some(state.name().to_owned()),
        }),
    }
}

/// The events one stream-json log line stands for; none for a line that is not one of the
/// mapped kinds, too long, or not JSON.
pub fn line(text: &str) -> Vec<EventKind> {
    if text.len() > MAX_LINE {
        return Vec::new();
    }
    let Ok(v) = Value::parse(text) else {
        return Vec::new();
    };
    let message = v.get("message");
    let blocks = message
        .and_then(|m| m.get("content"))
        .and_then(Value::as_array);
    let (Some(kind), Some(blocks)) = (v.get("type").and_then(Value::as_str), blocks) else {
        return Vec::new();
    };
    let message_id = message
        .and_then(|m| m.get("id"))
        .and_then(Value::as_str)
        .unwrap_or("message");
    let mut out = Vec::new();
    for (i, block) in blocks.iter().enumerate() {
        match kind {
            "assistant" => assistant(block, &format!("{message_id}:{i}"), message_id, &mut out),
            "user" => tool_result(block, &mut out),
            _ => {}
        }
    }
    out
}

fn assistant(block: &Value, id: &str, parent: &str, out: &mut Vec<EventKind>) {
    let str_of = |k: &str| block.get(k).and_then(Value::as_str);
    match str_of("type") {
        Some("text") => {
            let Some(text) = str_of("text").filter(|t| !t.is_empty()) else {
                return;
            };
            out.push(EventKind::TextMessageStart {
                message_id: id.to_owned(),
                role: Role::Assistant,
            });
            out.push(EventKind::TextMessageContent {
                message_id: id.to_owned(),
                delta: capped(text),
            });
            out.push(EventKind::TextMessageEnd {
                message_id: id.to_owned(),
            });
        }
        Some("tool_use") => {
            let (Some(call), Some(name)) = (str_of("id"), str_of("name")) else {
                return;
            };
            out.push(EventKind::ToolCallStart {
                tool_call_id: call.to_owned(),
                tool_call_name: name.to_owned(),
                parent_message_id: Some(parent.to_owned()),
            });
            let args = block
                .get("input")
                .map_or_else(|| "{}".to_owned(), Value::to_json_string);
            out.push(EventKind::ToolCallArgs {
                tool_call_id: call.to_owned(),
                delta: capped(&args),
            });
            out.push(EventKind::ToolCallEnd {
                tool_call_id: call.to_owned(),
            });
        }
        _ => {}
    }
}

fn tool_result(block: &Value, out: &mut Vec<EventKind>) {
    if block.get("type").and_then(Value::as_str) != Some("tool_result") {
        return;
    }
    let Some(call) = block.get("tool_use_id").and_then(Value::as_str) else {
        return;
    };
    // A string, or an array of blocks whose text parts are joined.
    let content = match block.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    };
    out.push(EventKind::ToolCallResult {
        message_id: format!("result:{call}"),
        tool_call_id: call.to_owned(),
        content: capped(&content),
        role: Some(Role::Tool),
    });
}

/// `text` cut to [`MAX_TEXT`] bytes at a character boundary.
fn capped(text: &str) -> String {
    if text.len() <= MAX_TEXT {
        return text.to_owned();
    }
    let mut end = MAX_TEXT;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{CUT}", &text[..end])
}

/// One run's SSE frames, checked against AG-UI's ordering rules as they are made.
#[derive(Debug, Default)]
pub struct Frames {
    verifier: Verifier,
}

impl Frames {
    /// A run with nothing sent yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// `kind` as SSE text, with `id: <id>` when given (the log offset a client resumes after).
    /// An event that breaks the ordering rules is an error naming the rule; the run should then
    /// end with [`Frames::error`].
    pub fn frame(&mut self, kind: EventKind, id: Option<u64>) -> Result<String, String> {
        let events = self
            .verifier
            .push(Event::from(kind))
            .map_err(|e| e.to_string())?;
        let mut text = String::new();
        if let Some(id) = id {
            text.push_str(&format!("id: {id}\n"));
        }
        for event in &events {
            text.push_str(&rusty_agui::sse::encode(event));
        }
        Ok(text)
    }

    /// `RUN_ERROR` with `message`, which the rules allow at any point of a started run.
    pub fn error(&mut self, message: &str, code: &str) -> String {
        let kind = EventKind::RunError {
            message: message.to_owned(),
            code: Some(code.to_owned()),
        };
        self.frame(kind, None).unwrap_or_default()
    }
}

/// An SSE comment: keeps the connection open and lets the gateway notice a revoked token.
pub const KEEP_ALIVE: &str = ": keep-alive\n\n";

#[cfg(test)]
mod tests {
    use super::*;
    use ac_domain::{AgentId, JobId};

    fn names(events: &[EventKind]) -> Vec<&'static str> {
        events.iter().map(EventKind::type_name).collect()
    }

    fn job(state: JobState, reason: Option<&str>) -> Job {
        let agent = AgentId::parse("a1").unwrap_or_else(|e| panic!("{e}"));
        let id = JobId::parse("0123456789abcdef").unwrap_or_else(|e| panic!("{e}"));
        let mut j = Job::new(agent, id, 1, 60, None);
        j.state = state;
        j.reason = reason.map(str::to_owned);
        j
    }

    #[test]
    fn assistant_blocks_become_messages_and_tool_calls() {
        let line = r#"{"type":"assistant","message":{"id":"m1","content":[{"type":"thinking","thinking":"hm"},{"type":"text","text":"hi"},{"type":"text","text":""},{"type":"tool_use","id":"t1","name":"Read","input":{"path":"a"}}]}}"#;
        let events = line_events(line);
        assert_eq!(
            names(&events),
            [
                "TEXT_MESSAGE_START",
                "TEXT_MESSAGE_CONTENT",
                "TEXT_MESSAGE_END",
                "TOOL_CALL_START",
                "TOOL_CALL_ARGS",
                "TOOL_CALL_END"
            ]
        );
        assert!(
            matches!(&events[0], EventKind::TextMessageStart { message_id, .. } if message_id == "m1:1")
        );
        assert!(
            matches!(&events[4], EventKind::ToolCallArgs { delta, .. } if delta == r#"{"path":"a"}"#)
        );
    }

    fn line_events(text: &str) -> Vec<EventKind> {
        line(text)
    }

    #[test]
    fn tool_results_join_text_parts() {
        let line = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":[{"type":"text","text":"a"},{"type":"image"},{"type":"text","text":"b"}]}]}}"#;
        let events = line_events(line);
        assert!(
            matches!(&events[..], [EventKind::ToolCallResult { content, tool_call_id, .. }] if content == "a\nb" && tool_call_id == "t1"),
            "{events:?}"
        );
    }

    #[test]
    fn other_lines_make_nothing() {
        for text in [
            "",
            "not json",
            r#"{"type":"result","result":"x"}"#,
            r#"{"type":"system","subtype":"init"}"#,
            r#"{"type":"assistant","message":{"content":"text"}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"no id"}]}}"#,
            r#"[1,2]"#,
        ] {
            assert!(line_events(text).is_empty(), "{text}");
        }
        let long = format!(
            r#"{{"type":"assistant","message":{{"content":[{{"type":"text","text":"{}"}}]}}}}"#,
            "x".repeat(MAX_LINE)
        );
        assert!(line_events(&long).is_empty());
    }

    #[test]
    fn long_text_is_cut_at_a_character() {
        let text = "é".repeat(MAX_TEXT);
        let cut = capped(&text);
        assert!(cut.ends_with(CUT));
        assert!(cut.len() <= MAX_TEXT + CUT.len());
        assert_eq!(capped("short"), "short");
    }

    #[test]
    fn the_registry_decides_how_the_run_ends() {
        assert!(ended(&job(JobState::Running, None), "{}").is_none());
        assert!(ended(&job(JobState::Queued, None), "{}").is_none());
        assert!(matches!(
            ended(&job(JobState::Succeeded, None), r#"{"state":"succeeded"}"#),
            Some(EventKind::RunFinished {
                result: Some(_),
                ..
            })
        ));
        let Some(EventKind::RunError { message, code }) =
            ended(&job(JobState::TimedOut, Some("over 60 s")), "{}")
        else {
            panic!("not an error");
        };
        assert_eq!(code.as_deref(), Some("timed_out"));
        assert_eq!(message, "job timed_out: over 60 s");
    }

    #[test]
    fn frames_follow_the_ordering_rules() {
        let mut frames = Frames::new();
        let early = EventKind::TextMessageEnd {
            message_id: "m".into(),
        };
        assert!(
            frames.frame(early, None).is_err(),
            "nothing before RUN_STARTED"
        );
        let mut frames = Frames::new();
        let start = frames.frame(started(&job(JobState::Running, None)), Some(7));
        let start = start.unwrap_or_else(|e| panic!("{e}"));
        assert!(start.starts_with("id: 7\ndata: {"), "{start}");
        assert!(start.ends_with("\n\n"));
        let end = frames.error("gone", "gone");
        assert!(end.contains("RUN_ERROR"), "{end}");
    }
}
