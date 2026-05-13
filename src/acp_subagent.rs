use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use agent_client_protocol::{self as acp, ByteStreams, Client};
use agent_client_protocol::schema::{
    self, CancelNotification, ClientCapabilities, CreateTerminalRequest, CreateTerminalResponse,
    FileSystemCapabilities, Implementation, InitializeRequest, KillTerminalRequest,
    KillTerminalResponse, NewSessionRequest, PermissionOptionKind, PromptRequest,
    ReadTextFileRequest, ReadTextFileResponse, ReleaseTerminalRequest, ReleaseTerminalResponse,
    RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    SelectedPermissionOutcome, SessionUpdate, TerminalExitStatus, TerminalOutputRequest,
    TerminalOutputResponse, WaitForTerminalExitRequest, WaitForTerminalExitResponse,
    WriteTextFileRequest, WriteTextFileResponse,
};
use serde_json::json;
use tokio::io::AsyncReadExt;
use tokio::sync::Mutex;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use crate::config::{Config, ResolvedSubagentAcpTargetConfig};
use crate::tools::{resolve_tool_working_dir, ToolAuthContext};
use microclaw_core::text::floor_char_boundary;
use microclaw_storage::db::{call_blocking, Database};

const ACP_RUNTIME_PROVIDER: &str = "acp";
const ACP_AGENT_STDERR_LIMIT_BYTES: usize = 16 * 1024;
const ACP_TERMINAL_OUTPUT_LIMIT_BYTES: usize = 256 * 1024;
const ACP_CANCEL_POLL_MS: u64 = 350;

pub struct AcpSubagentTaskParams {
    pub config: Config,
    pub db: Arc<Database>,
    pub auth_context: ToolAuthContext,
    pub run_id: String,
    pub task: String,
    pub context: String,
    pub local_cancel: Arc<AtomicBool>,
    pub target: ResolvedSubagentAcpTargetConfig,
}

#[derive(Default)]
struct RetainedText {
    text: String,
    truncated: bool,
}

#[derive(Default)]
struct TranscriptState {
    message_text: String,
    notes: Vec<String>,
}

struct TerminalSession {
    child: Mutex<tokio::process::Child>,
    output: Arc<Mutex<RetainedText>>,
    exit_status: Mutex<Option<TerminalExitStatus>>,
}

impl TerminalSession {
    async fn output_response(&self) -> Result<TerminalOutputResponse, acp::Error> {
        let exit_status = self.try_refresh_exit_status().await?;
        let output = self.output.lock().await;
        Ok(
            TerminalOutputResponse::new(output.text.clone(), output.truncated)
                .exit_status(exit_status),
        )
    }

    async fn try_refresh_exit_status(&self) -> Result<Option<TerminalExitStatus>, acp::Error> {
        if let Some(existing) = self.exit_status.lock().await.clone() {
            return Ok(Some(existing));
        }
        let maybe_status = {
            let mut child = self.child.lock().await;
            child.try_wait().map_err(internal_error)?
        };
        if let Some(status) = maybe_status {
            let mapped = map_exit_status(status);
            *self.exit_status.lock().await = Some(mapped.clone());
            Ok(Some(mapped))
        } else {
            Ok(None)
        }
    }

    async fn wait_for_exit(&self) -> Result<WaitForTerminalExitResponse, acp::Error> {
        if let Some(existing) = self.exit_status.lock().await.clone() {
            return Ok(WaitForTerminalExitResponse::new(existing));
        }
        let status = {
            let mut child = self.child.lock().await;
            child.wait().await.map_err(internal_error)?
        };
        let mapped = map_exit_status(status);
        *self.exit_status.lock().await = Some(mapped.clone());
        Ok(WaitForTerminalExitResponse::new(mapped))
    }

    async fn kill(&self) -> Result<(), acp::Error> {
        if self.exit_status.lock().await.is_some() {
            return Ok(());
        }
        let status = {
            let mut child = self.child.lock().await;
            let _ = child.start_kill();
            child.wait().await.map_err(internal_error)?
        };
        *self.exit_status.lock().await = Some(map_exit_status(status));
        Ok(())
    }
}

#[derive(Clone)]
struct AcpSessionClient {
    working_dir: PathBuf,
    auto_approve: bool,
    db: Arc<Database>,
    run_id: String,
    transcript: Arc<Mutex<TranscriptState>>,
    terminals: Arc<Mutex<HashMap<String, Arc<TerminalSession>>>>,
}

impl AcpSessionClient {
    fn new(working_dir: PathBuf, auto_approve: bool, db: Arc<Database>, run_id: String) -> Self {
        Self {
            working_dir,
            auto_approve,
            db,
            run_id,
            transcript: Arc::new(Mutex::new(TranscriptState::default())),
            terminals: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    async fn final_text(&self) -> String {
        let transcript = self.transcript.lock().await;
        if transcript.message_text.trim().is_empty() {
            transcript.notes.join("\n")
        } else if transcript.notes.is_empty() {
            transcript.message_text.clone()
        } else {
            format!(
                "{}\n\n{}",
                transcript.message_text,
                transcript.notes.join("\n")
            )
        }
    }

    async fn kill_all_terminals(&self) {
        let sessions = {
            let guard = self.terminals.lock().await;
            guard.values().cloned().collect::<Vec<_>>()
        };
        for session in sessions {
            let _ = session.kill().await;
        }
    }

    async fn append_note(&self, note: String) {
        if note.trim().is_empty() {
            return;
        }
        self.transcript.lock().await.notes.push(note);
    }

    async fn log_event(&self, event_type: &str, detail: Option<String>) {
        let db = self.db.clone();
        let run_id = self.run_id.clone();
        let event_type = event_type.to_string();
        let _ = call_blocking(db, move |db| {
            db.append_subagent_event(&run_id, &event_type, detail.as_deref())
        })
        .await;
    }

    async fn request_permission(
        &self,
        args: RequestPermissionRequest,
    ) -> Result<RequestPermissionResponse, acp::Error> {
        let title = args
            .tool_call
            .fields
            .title
            .clone()
            .unwrap_or_else(|| "ACP tool call".to_string());
        self.append_note(format!("[acp] permission requested: {title}"))
            .await;
        self.log_event("acp_permission_requested", Some(format!("title={title}")))
            .await;

        let preferred = if self.auto_approve {
            args.options
                .iter()
                .find(|opt| {
                    matches!(
                        opt.kind,
                        PermissionOptionKind::AllowOnce | PermissionOptionKind::AllowAlways
                    )
                })
                .or_else(|| args.options.first())
        } else {
            args.options.iter().find(|opt| {
                matches!(
                    opt.kind,
                    PermissionOptionKind::RejectOnce | PermissionOptionKind::RejectAlways
                )
            })
        };

        let Some(selected) = preferred else {
            return Ok(RequestPermissionResponse::new(
                RequestPermissionOutcome::Cancelled,
            ));
        };
        self.log_event(
            "acp_permission_selected",
            Some(format!("option_id={}", selected.option_id.0)),
        )
        .await;

        Ok(RequestPermissionResponse::new(
            RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
                selected.option_id.clone(),
            )),
        ))
    }

    async fn session_notification(&self, args: schema::SessionNotification) {
        match args.update {
            SessionUpdate::AgentMessageChunk(chunk) => {
                if let schema::ContentBlock::Text(text) = chunk.content {
                    self.transcript
                        .lock()
                        .await
                        .message_text
                        .push_str(&text.text);
                }
            }
            SessionUpdate::ToolCall(tool_call) => {
                let title = if tool_call.title.trim().is_empty() {
                    format!("{:?}", tool_call.kind)
                } else {
                    tool_call.title
                };
                self.append_note(format!("[acp] tool call: {title}")).await;
                self.log_event("acp_tool_call", Some(format!("title={title}")))
                    .await;
            }
            SessionUpdate::ToolCallUpdate(update) => {
                let title = update
                    .fields
                    .title
                    .clone()
                    .unwrap_or_else(|| update.tool_call_id.0.to_string());
                if let Some(status) = update.fields.status {
                    self.append_note(format!("[acp] tool update: {title} ({status:?})"))
                        .await;
                    self.log_event(
                        "acp_tool_update",
                        Some(format!("title={title} status={status:?}")),
                    )
                    .await;
                }
            }
            SessionUpdate::Plan(plan) => {
                if !plan.entries.is_empty() {
                    let items = plan
                        .entries
                        .iter()
                        .map(|entry| entry.content.as_str())
                        .collect::<Vec<_>>()
                        .join(" | ");
                    self.append_note(format!("[acp] plan: {items}")).await;
                    self.log_event("acp_plan", Some(items)).await;
                }
            }
            _ => {}
        }
    }

    async fn write_text_file(
        &self,
        args: WriteTextFileRequest,
    ) -> Result<WriteTextFileResponse, acp::Error> {
        let path = resolve_client_path(&self.working_dir, &args.path)?;
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(internal_error)?;
        }
        tokio::fs::write(path, args.content)
            .await
            .map_err(internal_error)?;
        self.log_event(
            "acp_write_text_file",
            Some(format!("path={}", args.path.display())),
        )
        .await;
        Ok(WriteTextFileResponse::new())
    }

    async fn read_text_file(
        &self,
        args: ReadTextFileRequest,
    ) -> Result<ReadTextFileResponse, acp::Error> {
        let path = resolve_client_path(&self.working_dir, &args.path)?;
        let content = tokio::fs::read_to_string(path)
            .await
            .map_err(internal_error)?;
        let selected = slice_lines(&content, args.line, args.limit);
        self.log_event(
            "acp_read_text_file",
            Some(format!("path={}", args.path.display())),
        )
        .await;
        Ok(ReadTextFileResponse::new(selected))
    }

    async fn create_terminal(
        &self,
        args: CreateTerminalRequest,
    ) -> Result<CreateTerminalResponse, acp::Error> {
        let cwd = match args.cwd {
            Some(path) => resolve_client_path(&self.working_dir, &path)?,
            None => self.working_dir.clone(),
        };
        let terminal_id = format!("acp-term-{}", uuid::Uuid::new_v4());
        let output_limit = args
            .output_byte_limit
            .map(|value| value as usize)
            .unwrap_or(ACP_TERMINAL_OUTPUT_LIMIT_BYTES)
            .clamp(1024, ACP_TERMINAL_OUTPUT_LIMIT_BYTES);
        let mut cmd = tokio::process::Command::new(&args.command);
        cmd.args(&args.args)
            .current_dir(&cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        for env in args.env {
            cmd.env(env.name, env.value);
        }
        let mut child = cmd.spawn().map_err(internal_error)?;
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let session = Arc::new(TerminalSession {
            child: Mutex::new(child),
            output: Arc::new(Mutex::new(RetainedText::default())),
            exit_status: Mutex::new(None),
        });

        if let Some(stdout) = stdout {
            tokio::spawn(read_pipe_into_output(
                stdout,
                session.output.clone(),
                output_limit,
            ));
        }
        if let Some(stderr) = stderr {
            tokio::spawn(read_pipe_into_output(
                stderr,
                session.output.clone(),
                output_limit,
            ));
        }

        self.terminals
            .lock()
            .await
            .insert(terminal_id.clone(), session);
        self.log_event(
            "acp_create_terminal",
            Some(format!("command={} cwd={}", args.command, cwd.display())),
        )
        .await;
        Ok(CreateTerminalResponse::new(terminal_id))
    }

    async fn terminal_output(
        &self,
        args: TerminalOutputRequest,
    ) -> Result<TerminalOutputResponse, acp::Error> {
        let session = self
            .terminals
            .lock()
            .await
            .get(args.terminal_id.0.as_ref())
            .cloned()
            .ok_or_else(acp::Error::invalid_params)?;
        session.output_response().await
    }

    async fn release_terminal(
        &self,
        args: ReleaseTerminalRequest,
    ) -> Result<ReleaseTerminalResponse, acp::Error> {
        if let Some(session) = self
            .terminals
            .lock()
            .await
            .remove(args.terminal_id.0.as_ref())
        {
            let _ = session.kill().await;
        }
        self.log_event(
            "acp_release_terminal",
            Some(format!("terminal_id={}", args.terminal_id.0)),
        )
        .await;
        Ok(ReleaseTerminalResponse::new())
    }

    async fn wait_for_terminal_exit(
        &self,
        args: WaitForTerminalExitRequest,
    ) -> Result<WaitForTerminalExitResponse, acp::Error> {
        let session = self
            .terminals
            .lock()
            .await
            .get(args.terminal_id.0.as_ref())
            .cloned()
            .ok_or_else(acp::Error::invalid_params)?;
        let response = session.wait_for_exit().await?;
        self.log_event(
            "acp_wait_terminal_exit",
            Some(format!("terminal_id={}", args.terminal_id.0)),
        )
        .await;
        Ok(response)
    }

    async fn kill_terminal(
        &self,
        args: KillTerminalRequest,
    ) -> Result<KillTerminalResponse, acp::Error> {
        let session = self
            .terminals
            .lock()
            .await
            .get(args.terminal_id.0.as_ref())
            .cloned()
            .ok_or_else(acp::Error::invalid_params)?;
        session.kill().await?;
        self.log_event(
            "acp_kill_terminal",
            Some(format!("terminal_id={}", args.terminal_id.0)),
        )
        .await;
        Ok(KillTerminalResponse::new())
    }
}

pub async fn run_acp_subagent_task(
    params: AcpSubagentTaskParams,
) -> Result<(String, String, i64, i64), String> {
    let working_dir = resolve_session_working_dir(&params.config, &params.auth_context)
        .map_err(|e| format!("Failed resolving ACP working directory: {e}"))?;
    tokio::fs::create_dir_all(&working_dir)
        .await
        .map_err(|e| format!("Failed creating ACP working directory: {e}"))?;

    let command = params.target.command.clone();
    if command.trim().is_empty() {
        return Err("ACP runtime target command is empty".into());
    }

    let mut child_cmd = tokio::process::Command::new(&command);
    child_cmd
        .args(&params.target.args)
        .current_dir(&working_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .env("MICROCLAW_SUBAGENT_RUN_ID", &params.run_id)
        .env(
            "MICROCLAW_SUBAGENT_CHAT_ID",
            params.auth_context.caller_chat_id.to_string(),
        )
        .env(
            "MICROCLAW_SUBAGENT_WORKDIR",
            working_dir.display().to_string(),
        );
    if let Some(target_name) = params.target.name.as_deref() {
        child_cmd.env("MICROCLAW_SUBAGENT_RUNTIME_TARGET", target_name);
    }
    for (key, value) in &params.target.env {
        child_cmd.env(key, value);
    }

    let mut child = child_cmd
        .spawn()
        .map_err(|e| format!("Failed spawning ACP agent command '{command}': {e}"))?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| "ACP agent stdin pipe unavailable".to_string())?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "ACP agent stdout pipe unavailable".to_string())?;
    let stderr = child.stderr.take();
    let stderr_capture = Arc::new(Mutex::new(RetainedText::default()));
    if let Some(stderr) = stderr {
        tokio::spawn(read_pipe_into_output(
            stderr,
            stderr_capture.clone(),
            ACP_AGENT_STDERR_LIMIT_BYTES,
        ));
    }

    let client = Arc::new(AcpSessionClient::new(
        working_dir.clone(),
        params.target.auto_approve,
        params.db.clone(),
        params.run_id.clone(),
    ));
    let prompt_text = if params.context.is_empty() {
        params.task.clone()
    } else {
        format!("Context: {}\n\nTask: {}", params.context, params.task)
    };

    let transport = ByteStreams::new(stdin.compat_write(), stdout.compat());
    let db = params.db.clone();
    let run_id = params.run_id.clone();
    let local_cancel = params.local_cancel.clone();

    let h_perm = client.clone();
    let h_write = client.clone();
    let h_read = client.clone();
    let h_create = client.clone();
    let h_out = client.clone();
    let h_release = client.clone();
    let h_wait = client.clone();
    let h_kill = client.clone();
    let h_notif = client.clone();

    let prompt_client = client.clone();
    let connection_result = Client
        .builder()
        .name("microclaw-acp-subagent")
        .on_receive_request(
            async move |req: RequestPermissionRequest, responder, _cx| {
                match h_perm.request_permission(req).await {
                    Ok(r) => responder.respond(r),
                    Err(err) => responder.respond_with_error(err),
                }
            },
            acp::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: WriteTextFileRequest, responder, _cx| {
                match h_write.write_text_file(req).await {
                    Ok(r) => responder.respond(r),
                    Err(err) => responder.respond_with_error(err),
                }
            },
            acp::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: ReadTextFileRequest, responder, _cx| {
                match h_read.read_text_file(req).await {
                    Ok(r) => responder.respond(r),
                    Err(err) => responder.respond_with_error(err),
                }
            },
            acp::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: CreateTerminalRequest, responder, _cx| {
                match h_create.create_terminal(req).await {
                    Ok(r) => responder.respond(r),
                    Err(err) => responder.respond_with_error(err),
                }
            },
            acp::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: TerminalOutputRequest, responder, _cx| {
                match h_out.terminal_output(req).await {
                    Ok(r) => responder.respond(r),
                    Err(err) => responder.respond_with_error(err),
                }
            },
            acp::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: ReleaseTerminalRequest, responder, _cx| {
                match h_release.release_terminal(req).await {
                    Ok(r) => responder.respond(r),
                    Err(err) => responder.respond_with_error(err),
                }
            },
            acp::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: WaitForTerminalExitRequest, responder, _cx| {
                match h_wait.wait_for_terminal_exit(req).await {
                    Ok(r) => responder.respond(r),
                    Err(err) => responder.respond_with_error(err),
                }
            },
            acp::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: KillTerminalRequest, responder, _cx| {
                match h_kill.kill_terminal(req).await {
                    Ok(r) => responder.respond(r),
                    Err(err) => responder.respond_with_error(err),
                }
            },
            acp::on_receive_request!(),
        )
        .on_receive_notification(
            async move |notif: schema::SessionNotification, _cx| {
                h_notif.session_notification(notif).await;
                Ok::<_, acp::Error>(())
            },
            acp::on_receive_notification!(),
        )
        .connect_with(transport, async move |cx| {
            cx.send_request(
                InitializeRequest::new(schema::ProtocolVersion::V1)
                    .client_info(
                        Implementation::new(
                            "microclaw-acp-client",
                            env!("CARGO_PKG_VERSION"),
                        )
                        .title("MicroClaw ACP Client"),
                    )
                    .client_capabilities(
                        ClientCapabilities::new()
                            .fs(FileSystemCapabilities::new()
                                .read_text_file(true)
                                .write_text_file(true))
                            .terminal(true),
                    ),
            )
            .block_task()
            .await?;

            let new_session = cx
                .send_request(NewSessionRequest::new(working_dir.clone()))
                .block_task()
                .await?;
            let session_id = new_session.session_id.clone();

            let prompt_future = cx
                .send_request(PromptRequest::new(
                    session_id.clone(),
                    vec![prompt_text.into()],
                ))
                .block_task();
            tokio::pin!(prompt_future);

            loop {
                tokio::select! {
                    res = &mut prompt_future => {
                        res?;
                        return Ok(());
                    }
                    _ = tokio::time::sleep(std::time::Duration::from_millis(ACP_CANCEL_POLL_MS)) => {
                        let cancelled = crate::tools::subagents::is_cancelled(
                            db.clone(),
                            &run_id,
                            &local_cancel,
                        )
                        .await
                        .map_err(internal_error)?;
                        if cancelled {
                            let _ = cx.send_notification(CancelNotification::new(session_id.clone()));
                            prompt_client.kill_all_terminals().await;
                            let mut err = acp::Error::internal_error();
                            err.message = "cancelled".to_string();
                            return Err(err);
                        }
                    }
                }
            }
        })
        .await;

    client.kill_all_terminals().await;
    let _ = terminate_child_process(&mut child).await;

    match connection_result {
        Ok(()) => {}
        Err(err) => {
            if err.message == "cancelled" {
                return Err("cancelled".to_string());
            }
            return Err(format!("ACP connection failed: {err}"));
        }
    }

    let source = client.final_text().await;
    let stderr_text = stderr_capture.lock().await.text.clone();
    if source.trim().is_empty() && !stderr_text.trim().is_empty() {
        return Err(format!(
            "ACP agent produced no response. stderr:\n{stderr_text}"
        ));
    }
    let source = if source.trim().is_empty() {
        "(sub-agent produced no output)".to_string()
    } else {
        source
    };
    let (final_text, artifact_json) =
        crate::tools::subagents::normalize_subagent_artifact_payload(&source);
    Ok((final_text, artifact_json, 0, 0))
}

pub fn acp_runtime_model(target: &ResolvedSubagentAcpTargetConfig) -> String {
    target.model_label()
}

pub fn acp_runtime_provider(target_name: Option<&str>) -> String {
    match target_name.map(str::trim).filter(|name| !name.is_empty()) {
        Some(name) => format!("{ACP_RUNTIME_PROVIDER}:{name}"),
        None => ACP_RUNTIME_PROVIDER.to_string(),
    }
}

pub fn acp_runtime_target_from_provider(provider: &str) -> Option<String> {
    provider
        .strip_prefix(&format!("{ACP_RUNTIME_PROVIDER}:"))
        .map(ToOwned::to_owned)
        .filter(|name| !name.trim().is_empty())
}

fn resolve_session_working_dir(
    config: &Config,
    auth_context: &ToolAuthContext,
) -> Result<PathBuf, std::io::Error> {
    let input = json!({
        "__microclaw_auth": {
            "caller_channel": auth_context.caller_channel,
            "caller_chat_id": auth_context.caller_chat_id,
            "control_chat_ids": auth_context.control_chat_ids,
            "env_files": auth_context.env_files,
        }
    });
    let path = resolve_tool_working_dir(
        Path::new(&config.working_dir),
        config.working_dir_isolation,
        &input,
    );
    std::fs::create_dir_all(&path)?;
    Ok(path)
}

fn resolve_client_path(root: &Path, requested: &Path) -> Result<PathBuf, acp::Error> {
    let raw_root = normalize_path(root);
    let root = std::fs::canonicalize(root).map_err(internal_error)?;
    let requested = normalize_path(requested);
    let candidate = if requested.is_absolute() {
        if let Ok(relative) = requested.strip_prefix(&root) {
            root.join(relative)
        } else if let Ok(relative) = requested.strip_prefix(&raw_root) {
            root.join(relative)
        } else {
            return Err(acp::Error::invalid_params());
        }
    } else {
        root.join(requested)
    };
    let resolved = std::fs::canonicalize(&candidate).unwrap_or_else(|_| normalize_path(&candidate));
    if !resolved.starts_with(&root) {
        return Err(acp::Error::invalid_params());
    }
    let path_str = resolved.to_string_lossy().to_string();
    if let Err(msg) = microclaw_tools::path_guard::check_path(&path_str) {
        let mut err = acp::Error::invalid_request();
        err.message = msg;
        return Err(err);
    }
    Ok(resolved)
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            std::path::Component::RootDir => normalized.push(Path::new("/")),
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            std::path::Component::Normal(part) => normalized.push(part),
        }
    }
    normalized
}

fn slice_lines(content: &str, line: Option<u32>, limit: Option<u32>) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let start = line.unwrap_or(1).saturating_sub(1) as usize;
    let max = limit.unwrap_or(lines.len() as u32).max(1) as usize;
    let end = (start + max).min(lines.len());
    lines[start.min(lines.len())..end].join("\n")
}

async fn read_pipe_into_output<R>(mut reader: R, output: Arc<Mutex<RetainedText>>, limit: usize)
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut buf = [0_u8; 4096];
    loop {
        let read = match reader.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => break,
        };
        let chunk = String::from_utf8_lossy(&buf[..read]).into_owned();
        let mut retained = output.lock().await;
        retained.text.push_str(&chunk);
        truncate_retained_text(&mut retained, limit);
    }
}

fn truncate_retained_text(output: &mut RetainedText, limit: usize) {
    if output.text.len() <= limit {
        return;
    }
    output.truncated = true;
    let overflow = output.text.len().saturating_sub(limit);
    let boundary = floor_char_boundary(&output.text, overflow);
    output.text.replace_range(..boundary, "");
}

fn map_exit_status(status: std::process::ExitStatus) -> TerminalExitStatus {
    #[cfg(unix)]
    let mapped = {
        use std::os::unix::process::ExitStatusExt;
        TerminalExitStatus::new().signal(status.signal().map(|signal| signal.to_string()))
    };
    #[cfg(not(unix))]
    let mapped = TerminalExitStatus::new();
    mapped.exit_code(status.code().map(|code| code as u32))
}

async fn terminate_child_process(child: &mut tokio::process::Child) -> Result<(), std::io::Error> {
    match child.try_wait()? {
        Some(_) => Ok(()),
        None => {
            let _ = child.start_kill();
            let _ = child.wait().await?;
            Ok(())
        }
    }
}

fn internal_error(err: impl std::fmt::Display) -> acp::Error {
    let mut error = acp::Error::internal_error();
    error.message = err.to_string();
    error
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::{
        PermissionOption, SessionId, ToolCallUpdate, ToolCallUpdateFields,
    };

    fn test_db() -> Arc<Database> {
        let dir = std::env::temp_dir().join(format!(
            "microclaw_acp_subagent_test_{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Arc::new(Database::new(dir.to_str().unwrap()).unwrap())
    }

    #[cfg(unix)]
    fn test_terminal_command() -> (&'static str, Vec<String>) {
        ("/bin/sh", vec!["-c".into(), "printf 'hello'".into()])
    }

    #[cfg(windows)]
    fn test_terminal_command() -> (&'static str, Vec<String>) {
        ("cmd.exe", vec!["/C".into(), "echo hello".into()])
    }

    #[tokio::test]
    async fn test_acp_client_read_write_stays_inside_workdir() {
        let root =
            std::env::temp_dir().join(format!("microclaw_acp_client_{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&root).await.unwrap();
        let client = AcpSessionClient::new(root.clone(), true, test_db(), "run-1".into());
        let session_id = SessionId::new("test-session");

        client
            .write_text_file(WriteTextFileRequest::new(
                session_id.clone(),
                root.join("notes/todo.txt"),
                "hello",
            ))
            .await
            .unwrap();
        let read = client
            .read_text_file(ReadTextFileRequest::new(
                session_id.clone(),
                root.join("notes/todo.txt"),
            ))
            .await
            .unwrap();
        assert_eq!(read.content, "hello");

        let err = client
            .read_text_file(ReadTextFileRequest::new(
                session_id,
                PathBuf::from("/etc/hosts"),
            ))
            .await
            .unwrap_err();
        assert_eq!(err.code, acp::Error::invalid_params().code);
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn test_acp_client_terminal_lifecycle() {
        let root =
            std::env::temp_dir().join(format!("microclaw_acp_term_{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&root).await.unwrap();
        let client = AcpSessionClient::new(root.clone(), true, test_db(), "run-2".into());
        let session_id = SessionId::new("test-session");
        let (command, args) = test_terminal_command();

        let created = client
            .create_terminal(CreateTerminalRequest::new(session_id.clone(), command).args(args))
            .await
            .unwrap();
        let waited = client
            .wait_for_terminal_exit(WaitForTerminalExitRequest::new(
                session_id.clone(),
                created.terminal_id.clone(),
            ))
            .await
            .unwrap();
        assert_eq!(waited.exit_status.exit_code, Some(0));

        let output = client
            .terminal_output(TerminalOutputRequest::new(
                session_id.clone(),
                created.terminal_id.clone(),
            ))
            .await
            .unwrap();
        assert!(output.output.contains("hello"));

        client
            .release_terminal(ReleaseTerminalRequest::new(
                session_id,
                created.terminal_id,
            ))
            .await
            .unwrap();
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn test_permission_auto_approve_prefers_allow() {
        let client = AcpSessionClient::new(std::env::temp_dir(), true, test_db(), "run-3".into());
        let session_id = SessionId::new("test-session");
        let response = client
            .request_permission(RequestPermissionRequest::new(
                session_id,
                ToolCallUpdate::new(
                    "tool-1",
                    ToolCallUpdateFields::new().title("Run terminal".to_string()),
                ),
                vec![
                    PermissionOption::new("reject", "Reject", PermissionOptionKind::RejectOnce),
                    PermissionOption::new("allow", "Allow", PermissionOptionKind::AllowOnce),
                ],
            ))
            .await
            .unwrap();
        let RequestPermissionOutcome::Selected(selected) = response.outcome else {
            panic!("expected selected permission outcome");
        };
        assert_eq!(selected.option_id.0.as_ref(), "allow");
    }

    #[test]
    fn test_acp_runtime_provider_round_trips_target_name() {
        let provider = acp_runtime_provider(Some("worker"));
        assert_eq!(provider, "acp:worker");
        assert_eq!(
            acp_runtime_target_from_provider(&provider).as_deref(),
            Some("worker")
        );
        assert_eq!(acp_runtime_target_from_provider("acp"), None);
    }
}
