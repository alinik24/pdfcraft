//! Opt-in ACP stdio transport for an independently confined agent launcher.
//!
//! This module supplies no executable, credentials, model or sandbox. `cwd` is
//! a working directory, **not** filesystem confinement. The caller must enforce
//! backend tool roots and descendant lifetime separately. Client-side file and
//! terminal capabilities are deliberately not advertised or implemented.

use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::thread;
use std::time::{Duration, Instant};

const FRAME_BYTES: usize = 256 * 1024;
const QUEUE_SIZE: usize = 64;
const MAX_PENDING: usize = 16;
const MAX_TURN_BYTES: usize = 2 * 1024 * 1024;

/// Private launch configuration. Intentionally does not implement `Debug`.
pub struct Launch {
    pub executable: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    /// The entire child environment. Nothing from the parent is inherited.
    pub environment: BTreeMap<String, String>,
    /// Known secret values to redact from agent frames, in addition to secret
    /// environment entries. Never include these in saved document metadata.
    pub secrets: Vec<String>,
    /// Hard request deadline, including time waiting for a permission decision.
    pub timeout: Duration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("the agent launch settings are incomplete")]
    Configuration,
    #[error("the agent could not be started")]
    Start,
    #[error("the agent connection is closed")]
    Closed,
    #[error("the agent connection is busy")]
    Busy,
    #[error("the agent message exceeds its size limit")]
    Limit,
    #[error("the agent returned an invalid protocol message")]
    Protocol,
    #[error("the agent did not finish before the deadline")]
    Timeout,
}

/// Only per-operation approval is supported. No remembered or default approval.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    AllowOnce,
    Reject,
}

/// Bounded protocol events. All strings are untrusted and must be rendered as
/// text, never commands or markup. Permission `params` contains the proposed
/// operation for the caller's explicit review UI.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    Response { id: u64, result: Value },
    RequestFailed { id: u64 },
    Text { session_id: String, text: String },
    Tool { session_id: String, update: Value },
    Permission { request_id: Value, params: Value },
    Update { session_id: String, update: Value },
    Failed(Error),
    Exited,
}

enum Input {
    Request { id: u64, method: &'static str, params: Value },
    Cancel(String),
    Permission(Value, Decision),
}

/// One owned ACP process and its bounded queues. Dropping it requests immediate
/// shutdown (the supervisor checks within 20 ms), closes pending permissions and
/// kills/reaps the direct child. The confined launcher owns descendant cleanup.
pub struct Client {
    commands: mpsc::SyncSender<Input>,
    events: mpsc::Receiver<Event>,
    stop: Arc<AtomicBool>,
    next_id: u64,
    cwd: String,
}

impl Client {
    pub fn spawn(launch: Launch) -> Result<Self, Error> {
        if !launch.executable.is_absolute()
            || !launch.cwd.is_absolute()
            || !launch.cwd.is_dir()
            || launch.timeout.is_zero()
            || launch.timeout > Duration::from_secs(600)
        {
            return Err(Error::Configuration);
        }
        let cwd = launch.cwd.to_str().ok_or(Error::Configuration)?.to_owned();
        let mut command = Command::new(&launch.executable);
        command
            .args(&launch.args)
            .current_dir(&launch.cwd)
            .env_clear()
            .envs(&launch.environment)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        // Do not create a console window for a background copilot on Windows.
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x0800_0000);
        }
        let mut child = OwnedChild(command.spawn().map_err(|_| Error::Start)?);
        let Some(stdin) = child.0.stdin.take() else {
            return Err(Error::Start);
        };
        let Some(stdout) = child.0.stdout.take() else {
            return Err(Error::Start);
        };
        let (commands, incoming) = mpsc::sync_channel(QUEUE_SIZE);
        let (events_tx, events) = mpsc::sync_channel(QUEUE_SIZE);
        let (frames_tx, frames) = mpsc::sync_channel(QUEUE_SIZE);
        let (wire, writes) = mpsc::sync_channel::<Value>(QUEUE_SIZE);
        let stop = Arc::new(AtomicBool::new(false));
        let writer_stop = stop.clone();
        // A blocked pipe write cannot block supervision or deadline enforcement.
        thread::Builder::new()
            .name("pdfcraft-acp-write".into())
            .spawn(move || {
                let mut stdin = stdin;
                while let Ok(value) = writes.recv() {
                    let result = serde_json::to_writer(&mut stdin, &value)
                        .map_err(std::io::Error::other)
                        .and_then(|()| stdin.write_all(b"\n"))
                        .and_then(|()| stdin.flush());
                    if result.is_err() {
                        writer_stop.store(true, Ordering::Release);
                        break;
                    }
                }
            })
            .map_err(|_| Error::Start)?;
        thread::Builder::new()
            .name("pdfcraft-acp-read".into())
            .spawn(move || {
                let mut reader = BufReader::new(stdout);
                loop {
                    let frame = read_frame(&mut reader);
                    let done = frame.is_err();
                    if frames_tx.send(frame).is_err() || done {
                        break;
                    }
                }
            })
            .map_err(|_| Error::Start)?;
        let mut secrets = launch.secrets;
        secrets.extend(
            launch
                .environment
                .iter()
                .filter(|(name, _)| {
                    let name = name.to_ascii_uppercase();
                    ["KEY", "TOKEN", "SECRET", "PASSWORD", "CREDENTIAL"].iter().any(|part| name.contains(part))
                })
                .map(|(_, value)| value.clone()),
        );
        secrets.retain(|value| !value.is_empty());
        secrets.sort_by_key(|value| std::cmp::Reverse(value.len()));
        let worker_stop = stop.clone();
        thread::Builder::new()
            .name("pdfcraft-acp".into())
            .spawn(move || supervise(child, incoming, frames, wire, events_tx, worker_stop, launch.timeout, secrets))
            .map_err(|_| Error::Start)?;
        Ok(Self { commands, events, stop, next_id: 1, cwd })
    }

    fn request(&mut self, method: &'static str, params: Value) -> Result<u64, Error> {
        let id = self.next_id;
        self.next_id = self.next_id.checked_add(1).ok_or(Error::Limit)?;
        if serde_json::to_vec(&params).map_err(|_| Error::Protocol)?.len() > FRAME_BYTES - 1024 {
            return Err(Error::Limit);
        }
        self.send(Input::Request { id, method, params })?;
        Ok(id)
    }

    fn send(&self, input: Input) -> Result<(), Error> {
        if self.stop.load(Ordering::Acquire) {
            return Err(Error::Closed);
        }
        self.commands.try_send(input).map_err(|error| match error {
            mpsc::TrySendError::Full(_) => Error::Busy,
            mpsc::TrySendError::Disconnected(_) => Error::Closed,
        })
    }

    /// Call first and wait for its successful response before opening a session.
    pub fn initialize(&mut self) -> Result<u64, Error> {
        self.request(
            "initialize",
            json!({"protocolVersion":1,"clientCapabilities":{},
            "clientInfo":{"name":"pdfcraft","version":env!("CARGO_PKG_VERSION")}}),
        )
    }

    pub fn new_session(&mut self) -> Result<u64, Error> {
        let cwd = self.cwd.clone();
        self.new_session_at(&cwd)
    }

    /// Open a session at a path interpreted by the agent, which may be a
    /// container path distinct from the host launch directory. This validates
    /// the protocol field only; the launcher must enforce its filesystem roots.
    pub fn new_session_at(&mut self, cwd: &str) -> Result<u64, Error> {
        self.request("session/new", session_params(None, cwd)?)
    }

    pub fn load_session(&mut self, session_id: &str) -> Result<u64, Error> {
        let cwd = self.cwd.clone();
        self.load_session_at(session_id, &cwd)
    }

    /// Resume a session using the agent's path namespace, without resolving it
    /// against the host filesystem. See [`Self::new_session_at`].
    pub fn load_session_at(&mut self, session_id: &str, cwd: &str) -> Result<u64, Error> {
        self.request("session/load", session_params(Some(session_id), cwd)?)
    }

    /// Prompt only after a session response. Document context is untrusted data;
    /// the caller supplies it as part of the text and enforces its copy permission.
    pub fn prompt(&mut self, session_id: &str, text: &str) -> Result<u64, Error> {
        validate_id(session_id)?;
        if text.len() > FRAME_BYTES - 2048 {
            return Err(Error::Limit);
        }
        if text.trim().is_empty() {
            return Err(Error::Configuration);
        }
        self.request("session/prompt", json!({"sessionId":session_id,"prompt":[{"type":"text","text":text}]}))
    }

    /// ACP cancellation is a notification. A five-second grace deadline then
    /// kills the process if the outstanding prompt does not complete.
    pub fn cancel(&self, session_id: &str) -> Result<(), Error> {
        validate_id(session_id)?;
        self.send(Input::Cancel(session_id.to_owned()))
    }

    pub fn permission(&self, request_id: Value, decision: Decision) -> Result<(), Error> {
        if !valid_rpc_id(&request_id) {
            return Err(Error::Protocol);
        }
        self.send(Input::Permission(request_id, decision))
    }

    /// Nonblocking; drain regularly. A stalled consumer closes the process
    /// rather than allocating an unbounded transcript.
    pub fn try_event(&self) -> Result<Option<Event>, Error> {
        match self.events.try_recv() {
            Ok(event) => Ok(Some(event)),
            Err(mpsc::TryRecvError::Empty) => Ok(None),
            Err(mpsc::TryRecvError::Disconnected) => Err(Error::Closed),
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}

fn validate_id(id: &str) -> Result<(), Error> {
    if id.is_empty() || id.len() > 512 || id.chars().any(char::is_control) { Err(Error::Protocol) } else { Ok(()) }
}

fn session_params(session_id: Option<&str>, cwd: &str) -> Result<Value, Error> {
    if cwd.trim().is_empty() || cwd.len() > 8192 || cwd.chars().any(char::is_control) {
        return Err(Error::Configuration);
    }
    let mut params = json!({"cwd":cwd,"mcpServers":[]});
    if let Some(session_id) = session_id {
        validate_id(session_id)?;
        let object = params.as_object_mut().ok_or(Error::Protocol)?;
        object.insert("sessionId".to_owned(), Value::String(session_id.to_owned()));
    }
    Ok(params)
}

fn valid_rpc_id(id: &Value) -> bool {
    id.as_u64().is_some() || id.as_str().is_some_and(|s| !s.is_empty() && s.len() <= 512)
}

fn terminate(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

struct OwnedChild(Child);

impl Drop for OwnedChild {
    fn drop(&mut self) {
        terminate(&mut self.0);
    }
}

/// Read one bounded NDJSON frame. Truncated final lines are rejected.
fn read_frame(reader: &mut impl BufRead) -> Result<Value, Error> {
    let mut bytes = Vec::new();
    loop {
        let available = reader.fill_buf().map_err(|_| Error::Closed)?;
        if available.is_empty() {
            return Err(Error::Closed);
        }
        let count = available.iter().position(|byte| *byte == b'\n').map_or(available.len(), |n| n.saturating_add(1));
        if bytes.len().saturating_add(count) > FRAME_BYTES {
            return Err(Error::Limit);
        }
        let chunk = available.get(..count).ok_or(Error::Protocol)?;
        let complete = chunk.last() == Some(&b'\n');
        bytes.extend_from_slice(chunk);
        reader.consume(count);
        if complete {
            break;
        }
    }
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| Error::Protocol)?;
    if value.get("jsonrpc").and_then(Value::as_str) != Some("2.0") || !value.is_object() {
        return Err(Error::Protocol);
    }
    Ok(value)
}

fn scrub(value: &mut Value, secrets: &[String]) {
    match value {
        Value::String(text) => {
            for secret in secrets {
                *text = text.replace(secret, "[redacted]");
            }
        }
        Value::Array(values) => {
            for value in values {
                scrub(value, secrets);
            }
        }
        Value::Object(values) => {
            let entries = std::mem::take(values);
            for (mut key, mut value) in entries {
                for secret in secrets {
                    key = key.replace(secret, "[redacted]");
                }
                scrub(&mut value, secrets);
                values.insert(key, value);
            }
        }
        _ => {}
    }
}

/// Withhold a possible secret prefix at a text-chunk boundary. Otherwise a
/// credential echoed as two individually harmless ACP chunks could escape
/// per-frame redaction. Incomplete prefixes are discarded at turn completion.
#[derive(Default)]
struct TextRedactor {
    tails: HashMap<String, String>,
}

impl TextRedactor {
    fn push(&mut self, session: &str, text: &str, secrets: &[String]) -> Result<String, Error> {
        if !self.tails.contains_key(session) && self.tails.len() >= MAX_PENDING {
            return Err(Error::Limit);
        }
        let mut combined = self.tails.remove(session).unwrap_or_default();
        combined.push_str(text);
        for secret in secrets {
            combined = combined.replace(secret, "[redacted]");
        }
        let max = secrets.iter().map(String::len).max().unwrap_or(0);
        let earliest = combined.len().saturating_sub(max);
        let split = combined
            .char_indices()
            .filter(|(index, _)| *index >= earliest)
            .find_map(|(index, _)| {
                let tail = combined.get(index..)?;
                secrets.iter().any(|secret| secret.starts_with(tail)).then_some(index)
            })
            .unwrap_or(combined.len());
        let visible = combined.get(..split).ok_or(Error::Protocol)?.to_owned();
        let tail = combined.get(split..).ok_or(Error::Protocol)?.to_owned();
        if !tail.is_empty() {
            self.tails.insert(session.to_owned(), tail);
        }
        Ok(visible)
    }
}

fn update_event(params: &Value) -> Result<Event, Error> {
    let session = params.get("sessionId").and_then(Value::as_str).ok_or(Error::Protocol)?;
    validate_id(session)?;
    let update = params.get("update").filter(|v| v.is_object()).ok_or(Error::Protocol)?;
    let kind = update.get("sessionUpdate").and_then(Value::as_str).ok_or(Error::Protocol)?;
    Ok(match kind {
        "agent_message_chunk" => {
            let text = update
                .get("content")
                .filter(|v| v.get("type").and_then(Value::as_str) == Some("text"))
                .and_then(|v| v.get("text"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            Event::Text { session_id: session.to_owned(), text: text.to_owned() }
        }
        "tool_call" | "tool_call_update" => Event::Tool { session_id: session.to_owned(), update: update.clone() },
        // Do not forward hidden model reasoning to the document or transcript.
        "agent_thought_chunk" => Event::Update { session_id: session.to_owned(), update: json!({"sessionUpdate":"working"}) },
        _ => Event::Update { session_id: session.to_owned(), update: update.clone() },
    })
}

struct Permission {
    id: Value,
    allow_once: Option<String>,
    reject_once: Option<String>,
    deadline: Instant,
}

impl Permission {
    fn parse(frame: &Value) -> Result<Self, Error> {
        let id = frame.get("id").filter(|id| valid_rpc_id(id)).ok_or(Error::Protocol)?.clone();
        let options = frame.get("params").and_then(|v| v.get("options")).and_then(Value::as_array).ok_or(Error::Protocol)?;
        let find = |kind: &str| {
            options
                .iter()
                .find(|option| option.get("kind").and_then(Value::as_str) == Some(kind))
                .and_then(|option| option.get("optionId"))
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty() && id.len() <= 512)
                .map(str::to_owned)
        };
        Ok(Self { id, allow_once: find("allow_once"), reject_once: find("reject_once"), deadline: Instant::now() + Duration::from_secs(60) })
    }

    fn response(&self, decision: Decision) -> Value {
        let selected = match decision {
            Decision::AllowOnce => self.allow_once.as_ref(),
            Decision::Reject => self.reject_once.as_ref(),
        };
        let outcome = selected.map_or_else(|| json!({"outcome":"cancelled"}), |id| json!({"outcome":"selected","optionId":id}));
        json!({"jsonrpc":"2.0","id":self.id,"result":{"outcome":outcome}})
    }
}

#[allow(clippy::too_many_arguments)]
fn supervise(
    mut child: OwnedChild,
    commands: mpsc::Receiver<Input>,
    frames: mpsc::Receiver<Result<Value, Error>>,
    wire: mpsc::SyncSender<Value>,
    events: mpsc::SyncSender<Event>,
    stop: Arc<AtomicBool>,
    timeout: Duration,
    secrets: Vec<String>,
) {
    let result = drive(&mut child.0, &commands, &frames, &wire, &events, &stop, timeout, &secrets);
    stop.store(true, Ordering::Release);
    terminate(&mut child.0);
    if let Err(error) = result {
        let _ = events.try_send(Event::Failed(error));
    }
    let _ = events.try_send(Event::Exited);
}

#[allow(clippy::too_many_arguments)]
fn drive(
    child: &mut Child,
    commands: &mpsc::Receiver<Input>,
    frames: &mpsc::Receiver<Result<Value, Error>>,
    wire: &mpsc::SyncSender<Value>,
    events: &mpsc::SyncSender<Event>,
    stop: &AtomicBool,
    timeout: Duration,
    secrets: &[String],
) -> Result<(), Error> {
    let mut pending = HashMap::<u64, Instant>::new();
    let mut permissions: Vec<Permission> = Vec::new();
    let mut cancel_deadline = None;
    let mut output_bytes = 0usize;
    let mut prompt_id = None;
    let mut redactor = TextRedactor::default();
    let send = |frame| wire.try_send(frame).map_err(|_| Error::Busy);
    let emit = |event| events.try_send(event).map_err(|_| Error::Limit);
    while !stop.load(Ordering::Acquire) {
        let now = Instant::now();
        if pending.values().any(|deadline| now >= *deadline) || cancel_deadline.is_some_and(|deadline| now >= deadline) {
            return Err(Error::Timeout);
        }
        let mut position = 0;
        while position < permissions.len() {
            if permissions.get(position).is_some_and(|permission| now >= permission.deadline) {
                let permission = permissions.remove(position);
                send(permission.response(Decision::Reject))?;
            } else {
                position = position.saturating_add(1);
            }
        }
        // Drain queued stdout even if the process has already exited; its last
        // response must reach the caller before the EOF/exit event.
        let _ = child.try_wait().map_err(|_| Error::Closed)?;
        for _ in 0..QUEUE_SIZE {
            let Ok(input) = commands.try_recv() else {
                break;
            };
            match input {
                Input::Request { id, method, params } => {
                    if pending.len() >= MAX_PENDING {
                        return Err(Error::Limit);
                    }
                    if method == "session/prompt" {
                        if prompt_id.is_some() {
                            emit(Event::RequestFailed { id })?;
                            continue;
                        }
                        prompt_id = Some(id);
                        output_bytes = 0;
                    }
                    pending.insert(id, Instant::now() + timeout);
                    send(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))?;
                }
                Input::Cancel(session) => {
                    send(json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":session}}))?;
                    for permission in permissions.drain(..) {
                        send(permission.response(Decision::Reject))?;
                    }
                    cancel_deadline = Some(Instant::now() + Duration::from_secs(5));
                }
                Input::Permission(id, decision) => {
                    let position = permissions.iter().position(|permission| permission.id == id).ok_or(Error::Protocol)?;
                    let permission = permissions.remove(position);
                    send(permission.response(decision))?;
                }
            }
        }
        match frames.recv_timeout(Duration::from_millis(20)) {
            Ok(frame) => {
                let mut frame = frame?;
                output_bytes = output_bytes.saturating_add(serde_json::to_vec(&frame).map_err(|_| Error::Protocol)?.len());
                if output_bytes > MAX_TURN_BYTES {
                    return Err(Error::Limit);
                }
                scrub(&mut frame, secrets);
                match frame.get("method").and_then(Value::as_str) {
                    Some("session/update") => {
                        let mut event = update_event(frame.get("params").ok_or(Error::Protocol)?)?;
                        if let Event::Text { session_id, text } = &mut event {
                            *text = redactor.push(session_id, text, secrets)?;
                            if text.is_empty() {
                                continue;
                            }
                        }
                        emit(event)?;
                    }
                    Some("session/request_permission") => {
                        if permissions.len() >= MAX_PENDING {
                            return Err(Error::Limit);
                        }
                        let permission = Permission::parse(&frame)?;
                        if permissions.iter().any(|existing| existing.id == permission.id) {
                            return Err(Error::Protocol);
                        }
                        emit(Event::Permission { request_id: permission.id.clone(), params: frame.get("params").cloned().ok_or(Error::Protocol)? })?;
                        permissions.push(permission);
                    }
                    Some(_) => {
                        if let Some(id) = frame.get("id").filter(|id| valid_rpc_id(id)) {
                            send(json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"Client capability not available"}}))?;
                        }
                    }
                    None => {
                        let id = frame.get("id").and_then(Value::as_u64).ok_or(Error::Protocol)?;
                        if pending.remove(&id).is_none() {
                            return Err(Error::Protocol);
                        }
                        if prompt_id == Some(id) {
                            prompt_id = None;
                            redactor.tails.clear();
                        }
                        if pending.is_empty() {
                            cancel_deadline = None;
                            permissions.clear();
                        }
                        if frame.get("error").is_some() {
                            emit(Event::RequestFailed { id })?;
                        } else {
                            emit(Event::Response { id, result: frame.get("result").cloned().ok_or(Error::Protocol)? })?;
                        }
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return Err(Error::Closed),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn session_payload_preserves_agent_container_path_namespace() -> Result<(), Error> {
        assert_eq!(session_params(None, "/workspace")?, json!({"cwd":"/workspace","mcpServers":[]}));
        assert_eq!(
            session_params(Some("document-session"), "/workspace")?,
            json!({"sessionId":"document-session","cwd":"/workspace","mcpServers":[]})
        );
        let host_path = r"C:\Documents\Document workspace";
        assert_eq!(session_params(None, host_path)?.get("cwd").and_then(Value::as_str), Some(host_path));
        for invalid in ["", "   ", "/workspace\nelsewhere", "/workspace\0elsewhere"] {
            assert_eq!(session_params(None, invalid), Err(Error::Configuration));
        }
        assert_eq!(session_params(None, &"x".repeat(8193)), Err(Error::Configuration));
        assert_eq!(session_params(Some("bad\nsession"), "/workspace"), Err(Error::Protocol));
        Ok(())
    }

    #[test]
    fn frames_are_bounded_and_require_complete_jsonrpc_lines() {
        assert_eq!(read_frame(&mut Cursor::new(vec![b'x'; FRAME_BYTES + 1])), Err(Error::Limit));
        assert_eq!(read_frame(&mut Cursor::new(b"{bad}\n")), Err(Error::Protocol));
        assert_eq!(read_frame(&mut Cursor::new(b"{}\n")), Err(Error::Protocol));
        assert_eq!(read_frame(&mut Cursor::new(b"{\"jsonrpc\":\"2.0\"}")), Err(Error::Closed));
        assert!(read_frame(&mut Cursor::new(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\r\n")).is_ok());
    }

    #[test]
    fn permission_never_selects_persistent_or_unoffered_approval() -> Result<(), Error> {
        let frame = json!({"id":7,"params":{"options":[{"optionId":"forever","kind":"allow_always"},{"optionId":"no","kind":"reject_once"}]}});
        let permission = Permission::parse(&frame)?;
        assert_eq!(permission.response(Decision::AllowOnce)["result"]["outcome"]["outcome"], "cancelled");
        assert_eq!(permission.response(Decision::Reject)["result"]["outcome"]["optionId"], "no");
        let frame = json!({"id":"request","params":{"options":[{"optionId":"yes","kind":"allow_once"}]}});
        let permission = Permission::parse(&frame)?;
        assert_eq!(permission.response(Decision::AllowOnce)["result"]["outcome"]["optionId"], "yes");
        assert_eq!(permission.response(Decision::Reject)["result"]["outcome"]["outcome"], "cancelled");
        Ok(())
    }

    #[test]
    fn streams_text_and_tool_updates_without_hidden_reasoning() {
        let params = json!({"sessionId":"doc","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"Hello"}}});
        assert_eq!(update_event(&params), Ok(Event::Text { session_id: "doc".into(), text: "Hello".into() }));
        let params = json!({"sessionId":"doc","update":{"sessionUpdate":"tool_call_update","title":"Review","status":"completed"}});
        assert!(matches!(update_event(&params), Ok(Event::Tool { .. })));
        let params = json!({"sessionId":"doc","update":{"sessionUpdate":"agent_thought_chunk","content":{"text":"private reasoning"}}});
        assert!(!format!("{:?}", update_event(&params)).contains("private reasoning"));
    }

    #[test]
    fn redacts_nested_secret_echoes() {
        let mut value = json!({"result":[{"text":"abc-secret"}],"error":{"message":"abc-secret"},"abc-secret":"key"});
        scrub(&mut value, &["abc-secret".into()]);
        assert!(!value.to_string().contains("abc-secret"));
        assert!(value.to_string().contains("[redacted]"));
    }

    #[test]
    fn a_secret_split_across_text_chunks_is_not_emitted() -> Result<(), Error> {
        let mut redactor = TextRedactor::default();
        let secrets = vec!["abc-secret".to_owned()];
        assert_eq!(redactor.push("session", "Hello abc-", &secrets)?, "Hello ");
        assert_eq!(redactor.push("session", "secret. Done", &secrets)?, "[redacted]. Done");
        assert_eq!(redactor.push("session", "😀a", &secrets)?, "😀");
        assert_eq!(redactor.push("other-session", "different", &secrets)?, "different");
        Ok(())
    }
}
