use crate::{
    config::{self, Environment},
    execution,
    process::ProcessIdentity,
    project_sources, prompts,
    providers::{self, HttpProvider},
    sessions::{self, SessionStore},
};
use anyhow::{Context, Result, bail};
use rmcp::{ErrorData, RoleServer, ServerHandler, ServiceExt, model::*, service::RequestContext};
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    sync::atomic::{AtomicU8, Ordering},
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;

tokio::task_local! {
    /// The calling request's MCP log channel (reference `context.mcpReq.log`).
    static LOG: tokio::sync::mpsc::UnboundedSender<String>;
}
/// The client's `logging/setLevel` as LoggingLevel order plus one; 0 until a client sets one.
static LOG_LEVEL: AtomicU8 = AtomicU8::new(0);
/// Sends one info-level log line to the calling MCP client, as the reference sendLog does.
fn notify(line: &str) {
    if LOG_LEVEL.load(Ordering::Relaxed) <= 2 {
        let _ = LOG.try_with(|log| log.send(line.to_owned()));
    }
}
/// Streams new session-log lines to the MCP client whilst `run` executes.
async fn follow_log<T>(store: &SessionStore, id: &str, run: impl Future<Output = T>) -> T {
    if LOG.try_with(|_| ()).is_err() {
        return run.await;
    }
    tokio::pin!(run);
    let mut sent = 0;
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    loop {
        tokio::select! {
            output = &mut run => {
                forward_log(store, id, &mut sent, true);
                return output;
            }
            _ = tick.tick() => forward_log(store, id, &mut sent, false),
        }
    }
}
// shortcut: rereads the composed session log each tick, which is linear in the log size; one
// consult's log is small, and per-file offset readers are the upgrade path.
fn forward_log(store: &SessionStore, id: &str, sent: &mut usize, done: bool) {
    let Ok(log) = store.log(id) else { return };
    let Some(fresh) = log.get(*sent..) else {
        return;
    };
    let end = if done {
        fresh.len()
    } else {
        fresh.rfind('\n').map_or(0, |index| index + 1)
    };
    for line in fresh[..end].lines() {
        notify(line);
    }
    *sent += end;
}

#[derive(Clone)]
pub struct AlcuinMcp {
    pub store: SessionStore,
    pub cwd: PathBuf,
    pub env: Environment,
}
impl AlcuinMcp {
    pub fn tools() -> Vec<Tool> {
        serde_json::from_str::<ListToolsResult>(include_str!("../fixtures/mcp-tools.json"))
            .expect("immutable MCP tool fixture")
            .tools
    }
    pub async fn invoke(
        &self,
        name: &str,
        arguments: Value,
        cancel: &CancellationToken,
    ) -> Result<CallToolResult> {
        let tool = Self::tools()
            .into_iter()
            .find(|t| t.name == name)
            .context("Unknown MCP tool")?;
        let schema = Value::Object((*tool.input_schema).clone());
        validate_input(&arguments, &schema, "input")?;
        if let Some(id) = arguments["id"].as_str() {
            sessions::assert_id(id)?;
        }
        match name {
            "sessions" => self.sessions(&arguments),
            "wait" => self.wait(&arguments, cancel).await,
            "consult" => self.consult(arguments, cancel).await,
            "project_sources" => {
                // Reference buildProjectSourcesBrowserConfig: the environment names apply here too.
                let mut overrides = json!({});
                if let Some(names) = project_sources::cookie_names(None, &self.env) {
                    overrides["cookieNames"] = names;
                }
                self.project_sources(&arguments, &overrides, cancel).await
            }
            _ => bail!("Unknown MCP tool"),
        }
    }
    /// MCP callers see only sessions started in or below the server's working directory unless
    /// the user opts in; a session without a recorded directory counts as outside.
    fn in_project(&self, meta: &Value) -> bool {
        self.opted_in("ORACLE_MCP_ALL_SESSIONS")
            || meta["cwd"].as_str().is_some_and(|cwd| {
                prompts::within(
                    &prompts::absolute(std::path::Path::new(cwd), "."),
                    &prompts::absolute(&self.cwd, "."),
                )
            })
    }
    fn project_session(&self, id: &str) -> Result<Value> {
        let meta = self.store.require(id)?;
        anyhow::ensure!(
            self.in_project(&meta),
            "Session \"{id}\" was started outside this MCP server's working directory. Set ORACLE_MCP_ALL_SESSIONS=1 to read sessions from every project."
        );
        Ok(meta)
    }
    fn sessions(&self, input: &Value) -> Result<CallToolResult> {
        if let Some(id) = input["id"].as_str() {
            let meta = self.store.reported(id, self.project_session(id)?)?;
            if input["detail"] == true {
                let log = self.store.log(id)?;
                let mut session = json!({"metadata":meta,"log":log});
                if let Some(request) = self.store.request(id)? {
                    session["request"] = request;
                }
                return tool_result(log, json!({"session":session}));
            }
            return tool_result(
                session_line(&meta),
                json!({"entries":[session_entry(&meta)],"total":1,"truncated":false}),
            );
        }
        let entries = self
            .store
            .list(true)?
            .into_iter()
            .filter(|entry| !crate::browser_attempt::internal(entry) && self.in_project(entry))
            .map(|mut entry| {
                entry["status"] = self.store.listed_status(&entry);
                entry
            })
            .collect();
        let (entries, total, truncated) = sessions::filter_range(
            entries,
            input["hours"].as_f64().unwrap_or(24.0),
            input["includeAll"] == true,
            input["limit"].as_f64().unwrap_or(100.0).max(0.0) as usize,
        );
        let text = entries
            .iter()
            .map(session_line)
            .collect::<Vec<_>>()
            .join("\n");
        tool_result(
            text,
            json!({"entries":entries.iter().map(session_entry).collect::<Vec<_>>(),"total":total,"truncated":truncated}),
        )
    }
    async fn wait(&self, input: &Value, cancel: &CancellationToken) -> Result<CallToolResult> {
        let id = input["id"].as_str().context("Session id is required.")?;
        self.project_session(id)?;
        let timeout = input["timeoutMs"]
            .as_f64()
            .map(|n| Duration::from_secs_f64(n / 1000.0));
        let (meta, timed_out) = self.store.wait(id, timeout, cancel).await?;
        let tail = log_tail(&self.store.log(id)?, 4000);
        let wait_status = if timed_out { "timed_out" } else { "terminal" };
        let text = format!(
            "Session {} ({}; wait={wait_status})\n{}",
            meta["id"].as_str().unwrap_or(id),
            meta["status"].as_str().unwrap_or("unknown"),
            if tail.is_empty() {
                "(log empty)"
            } else {
                &tail
            }
        );
        let mut structured = json!({"sessionId":meta["id"],"status":meta["status"],"waitStatus":wait_status,"timedOut":timed_out,"cancelled":meta["status"]=="cancelled","output":tail});
        add_summaries(&mut structured, &meta);
        tool_result(text, structured)
    }
    pub(crate) fn configured(&self) -> Value {
        config::load(
            self.store.home(),
            &self.cwd,
            self.env
                .get(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
                .map(PathBuf::from)
                .as_deref(),
            true,
        )
        .config
    }
    fn opted_in(&self, name: &str) -> bool {
        self.env.get(name).is_some_and(|s| {
            matches!(
                s.trim().to_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
    }
    /// MCP callers read files only inside the server's working directory unless the user opts in.
    fn confine_files(&self, input: &Value) -> Result<()> {
        if self.opted_in("ORACLE_MCP_ALLOW_EXTERNAL_INPUT") {
            return Ok(());
        }
        let files = input["files"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect::<Vec<_>>();
        prompts::confine_inputs(&files, &self.cwd)
    }
    async fn consult(
        &self,
        mut input: Value,
        cancel: &CancellationToken,
    ) -> Result<CallToolResult> {
        if input["preset"] == "chatgpt-pro-heavy" {
            for (key, value) in [
                ("engine", "browser"),
                ("model", "gpt-6-pro"),
                ("browserThinkingTime", "pro"),
            ] {
                if input.get(key).is_none() {
                    input[key] = json!(value);
                }
            }
        }
        self.confine_files(&input)?;
        let config = self.configured();
        input["waitPreference"] = json!(input["waitForCompletion"] != false);
        let request = execution::resolve_mcp(&input, &config, &self.env, &self.cwd)?;
        if input["dryRun"] == true {
            let resolved = execution::dry_run_resolved(&request);
            let mut lines = dry_run_summary(&request)?;
            lines.extend(execution::dry_run_lines(&resolved));
            for line in &lines {
                notify(line);
            }
            let output = lines.join("\n").trim().to_owned();
            return tool_result(
                output.clone(),
                json!({"status":"dry-run","output":output,"dryRun":true,"resolved":resolved}),
            );
        }
        let remote = (request.engine == "browser")
            .then(|| crate::remote_session::Connection::resolve(&request.browser, &self.env))
            .flatten();
        if remote.is_some() && input["waitForCompletion"] == false {
            bail!(
                "Detached MCP consults are not supported with a remote browser service yet. Keep waitForCompletion:true for this run."
            );
        }
        if input["waitForCompletion"] == false
            && self
                .env
                .get("ORACLE_NO_DETACH")
                .is_some_and(|value| value == "1")
        {
            bail!(
                "Detached MCP consults are disabled by ORACLE_NO_DETACH=1. Remove it or keep waitForCompletion:true."
            );
        }
        // Reference ensureBrowserAvailable, before any session exists.
        if request.engine == "browser"
            && remote.is_none()
            && self.env.get("CHROME_PATH").is_none_or(String::is_empty)
            && crate::browser_detect::chrome(&self.env).is_none()
        {
            bail!(
                "Browser engine unavailable: no Chrome installation found and CHROME_PATH is unset."
            );
        }
        if let Some(connection) = &remote
            && connection.token.is_none()
        {
            bail!(
                "Remote host configured ({}) but remote token is missing. Run `alcuin bridge client --connect <...>` or set ORACLE_REMOTE_TOKEN.",
                connection.host
            );
        }
        let (session, provider) = if let Some(connection) = &remote {
            (
                crate::remote_session::prepare(&self.store, &request, &self.cwd, connection)?,
                None,
            )
        } else if request.engine == "browser" {
            (
                crate::browser_runner::prepare(&self.store, &request, &self.cwd)?,
                None,
            )
        } else {
            let (route, key) = providers::resolve_route(
                request.options["model"].as_str().context("Missing model")?,
                &request.options,
                &self.env,
            )?;
            let key = key.with_context(|| providers::missing_key_message(&route))?;
            let session = execution::prepare(&self.store, &request, &self.cwd, route.clone())?;
            let id = session["id"].as_str().context("Missing session ID")?;
            let header = crate::cli_execution::api_header(
                &request,
                &route,
                &key,
                &execution::committed_request(&self.store, id)?.body,
                &self.cwd,
            )?;
            let provider =
                HttpProvider::new(route, key, execution::http_timeout(&request.options)?)?;
            (session, Some((provider, header)))
        };
        let id = session["id"].as_str().context("Missing session ID")?;
        if input["waitForCompletion"] == false {
            let executable = crate::process::cli_executable()?;
            let worker_env = remote
                .as_ref()
                .map(|connection| connection.worker_environment(&self.env));
            // The session stays for recovery rather than becoming an error: a worker that
            // started before the failure may already have dispatched.
            execution::launch_detached_with_bootstrap(
                &self.store,
                id,
                &executable,
                worker_env.as_ref().unwrap_or(&self.env),
                request.cookie_bootstrap.as_ref(),
            )
            .await
            .map_err(|error| anyhow::anyhow!("Unable to start detached session {id}: {error}"))?;
            let current = self.store.require(id)?;
            let mut structured =
                json!({"sessionId":id,"status":current["status"],"output":"","detached":true});
            add_summaries(&mut structured, &current);
            return tool_result(
                format!(
                    "Session {id} ({}; detached)\nUse the wait tool with id={} to wait without polling.",
                    current["status"].as_str().unwrap_or("running"),
                    json!(id)
                ),
                structured,
            );
        }
        let identity = ProcessIdentity::current()?;
        execution::start_owner(&self.store, id, &identity, false)?;
        // shortcut: the perf trace is process-global, so concurrent traced consults share one
        // trace; a per-run trace handle is the upgrade path.
        let traced =
            crate::perf::detached_value(self.env.get("ORACLE_PERF_TRACE").map(String::as_str), id);
        let tracing = traced.is_some();
        if tracing {
            crate::perf::start(traced, &[], &self.cwd);
        }
        let run = async {
            if let Some(connection) = &remote {
                crate::remote_session::execute(&self.store, id, connection, &identity, cancel).await
            } else if request.engine == "browser" {
                crate::browser_runner::run_native_authenticated(
                    &self.store,
                    id,
                    &self.env,
                    &identity,
                    cancel,
                    request.cookie_bootstrap.as_ref(),
                )
                .await
            } else {
                let (provider, header) = provider.as_ref().context("Missing API provider")?;
                // Reference performSessionRun log: the preamble, answer and finish lines.
                self.store.append_log(
                    id,
                    Some(&provider.route().model),
                    &format!("{}\n", header.join("\n")),
                )?;
                execution::execute_logged(&self.store, id, provider, &identity, cancel, None).await
            }
        };
        let result = follow_log(&self.store, id, run).await;
        if tracing {
            crate::perf::flush(i32::from(result.is_err()));
        }
        let meta = self.store.require(id)?;
        if let Err(error) = result {
            let line = format!("Run failed: {error}");
            notify(&line);
            self.store
                .append_log(id, meta["model"].as_str(), &format!("{line}\n"))?;
            return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                "Session {id} failed: {error}"
            ))]));
        }
        let output = log_tail(&self.store.log(id)?, 4000);
        let mut structured = json!({"sessionId":id,"status":meta["status"],"output":output});
        add_summaries(&mut structured, &meta);
        let mut response = tool_result(
            format!(
                "Session {id} ({})\n{}",
                meta["status"].as_str().unwrap_or("unknown"),
                if output.is_empty() {
                    "(log empty)"
                } else {
                    &output
                }
            )
            .trim()
            .to_owned(),
            structured,
        )?;
        if meta["status"] == "completed"
            && (meta["cleanupWarning"].is_string()
                || (meta["recoveryRequired"] == true
                    && meta["browser"]["runtime"]["targetDispositionPending"] == true))
        {
            response.content.push(ContentBlock::text(
                "Answer saved. Owned browser tab cleanup is pending.",
            ));
        }
        Ok(response)
    }
    pub(crate) async fn project_sources(
        &self,
        input: &Value,
        browser_overrides: &Value,
        cancel: &CancellationToken,
    ) -> Result<CallToolResult> {
        let started = Instant::now();
        let operation = input["operation"]
            .as_str()
            .context("Missing Project Sources operation")?;
        anyhow::ensure!(
            operation != "add" || input["dryRun"] == true,
            "Temporary Chat policy forbids persistent Project Sources uploads; use dryRun to preview locally."
        );
        let mut config = self.configured();
        if !config["browser"].is_object() {
            config["browser"] = json!({});
        }
        for (key, value) in browser_overrides.as_object().into_iter().flatten() {
            config["browser"][key] = value.clone();
        }
        if crate::remote_session::Connection::resolve(&config["browser"], &self.env).is_some() {
            bail!(
                "project_sources v1 must run on the signed-in browser host; remote alcuin serve support is not enabled yet."
            );
        }
        let url = project_sources::normalise_url(
            input["chatgptUrl"]
                .as_str()
                .or(config["browser"]["chatgptUrl"].as_str())
                .or(config["browser"]["url"].as_str())
                .unwrap_or(""),
        )?;
        let files = if operation == "add" {
            self.confine_files(input)?;
            let inputs = input["files"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|v| v.as_str().unwrap_or("").to_owned())
                .collect::<Vec<_>>();
            prompts::resolve_files(
                &inputs,
                &self.cwd,
                project_sources::maximum_file_bytes(input, &config, &self.env)?,
                false,
            )?
        } else {
            vec![]
        };
        anyhow::ensure!(
            operation != "add" || !files.is_empty(),
            "Project Sources add requires at least one file."
        );
        if input["dryRun"] != true {
            let mut browser =
                project_sources::browser_config(&config["browser"], input, &self.env)?;
            let bootstrap =
                crate::browser_auth::prepare_config(input, &mut browser, &self.env, &self.cwd)?;
            let result = project_sources::run(
                project_sources::Request {
                    operation,
                    project_url: &url,
                    files: &files,
                    config: browser,
                },
                &self.env,
                &self.cwd,
                cancel,
                bootstrap.as_ref(),
                // Node prints these on stdout; stderr keeps `--json` and MCP stdout parseable.
                &|line| eprintln!("{line}"),
            )
            .await?;
            return tool_result(
                format!(
                    "Project Sources {operation} completed: {} source(s).",
                    result["sourcesAfter"].as_array().map_or(0, Vec::len)
                ),
                result,
            );
        }
        tool_result(
            format!(
                "Project Sources {operation} dry run: {} planned upload(s).",
                files.len()
            ),
            json!({"status":"dry-run","operation":operation,"projectUrl":url,"dryRun":true,"plannedUploads":project_sources::upload_plan(&files),"warnings":[],"tookMs":started.elapsed().as_millis() as u64}),
        )
    }
    pub fn resource(&self, uri: &str) -> Result<Value> {
        let tail = uri
            .strip_prefix("oracle-session://")
            .context("Unsupported resource URI")?;
        let (id, kind) = tail.split_once('/').context("Missing id or kind")?;
        let id = decode_component(id)?;
        let kind = decode_component(kind)?;
        sessions::assert_id(&id)?;
        let text = match kind.as_str() {
            "metadata" => serde_json::to_string_pretty(&self.store.require(&id)?)?,
            "log" => self.store.log(&id)?,
            "request" => match self.store.request(&id)? {
                Some(value) => serde_json::to_string_pretty(&value)?,
                None => std::fs::read_to_string(self.store.directory(&id)?.join("request.json"))?,
            },
            _ => bail!("Unsupported resource kind: {kind}"),
        };
        Ok(json!({"contents":[{"uri":uri,"text":text}]}))
    }
}
/// The initialise fixture with the reference serverInfo.version: the package version.
fn initialize_info(fixture: &str) -> Value {
    let mut info: Value = serde_json::from_str(fixture).expect("immutable MCP initialise fixture");
    info["serverInfo"]["version"] = json!(env!("CARGO_PKG_VERSION"));
    info
}
impl ServerHandler for AlcuinMcp {
    fn get_info(&self) -> ServerConfig {
        serde_json::from_value(initialize_info(include_str!(
            "../fixtures/mcp-initialize.json"
        )))
        .expect("immutable MCP initialise fixture")
    }
    async fn list_tools(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> std::result::Result<ListToolsResult, ErrorData> {
        Ok(
            serde_json::from_str(include_str!("../fixtures/mcp-tools.json"))
                .expect("immutable tool fixture"),
        )
    }
    async fn list_resource_templates(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> std::result::Result<ListResourceTemplatesResult, ErrorData> {
        Ok(
            serde_json::from_str(include_str!("../fixtures/mcp-resources.json"))
                .expect("immutable resource fixture"),
        )
    }
    async fn list_resources(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> std::result::Result<ListResourcesResult, ErrorData> {
        Ok(ListResourcesResult::default())
    }
    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _: RequestContext<RoleServer>,
    ) -> std::result::Result<ReadResourceResponse, ErrorData> {
        let value = self
            .resource(&request.uri)
            .map_err(|e| ErrorData::invalid_params(e.to_string(), None))?;
        let result: ReadResourceResult = serde_json::from_value(value)
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        Ok(result.into())
    }
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> std::result::Result<CallToolResponse, ErrorData> {
        if !Self::tools().iter().any(|t| t.name == request.name) {
            return Err(ErrorData::invalid_params("Unknown MCP tool", None));
        }
        let arguments = Value::Object(request.arguments.unwrap_or_default());
        let (log, mut lines) = tokio::sync::mpsc::unbounded_channel::<String>();
        let peer = context.peer.clone();
        // The maintained 2025 MCP surface still carries logging notifications.
        #[allow(deprecated)]
        let forward = tokio::spawn(async move {
            while let Some(text) = lines.recv().await {
                let bytes = text.len();
                let _ = peer
                    .notify_logging_message(LoggingMessageNotificationParam::new(
                        LoggingLevel::Info,
                        json!({"text":text,"bytes":bytes}),
                    ))
                    .await;
            }
        });
        let result = match LOG
            .scope(log, self.invoke(&request.name, arguments, &context.ct))
            .await
        {
            Ok(value) => value,
            Err(error) => CallToolResult::error(vec![ContentBlock::text(error.to_string())]),
        };
        // Log lines precede the result, as the reference awaits each send in order.
        let _ = forward.await;
        Ok(result.into())
    }
    // The maintained 2025 MCP surface still advertises logging.
    #[allow(deprecated)]
    async fn set_level(
        &self,
        request: SetLevelRequestParams,
        _: RequestContext<RoleServer>,
    ) -> std::result::Result<(), ErrorData> {
        LOG_LEVEL.store(request.level as u8 + 1, Ordering::Relaxed);
        Ok(())
    }
}
struct EofReader<T> {
    inner: T,
    closed: CancellationToken,
}

impl<T: tokio::io::AsyncRead + Unpin> tokio::io::AsyncRead for EofReader<T> {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
        buffer: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let this = self.get_mut();
        let before = buffer.filled().len();
        let has_room = buffer.remaining() > 0;
        let result = std::pin::Pin::new(&mut this.inner).poll_read(context, buffer);
        if has_room
            && matches!(&result, std::task::Poll::Ready(Ok(())))
            && buffer.filled().len() == before
        {
            this.closed.cancel();
        }
        result
    }
}

/// Serves MCP over stdio for `alcuin-mcp` and the `alcuin oracle-mcp` alias.
pub async fn serve_stdio() -> Result<()> {
    let env = crate::config::runtime_environment();
    let home = crate::config::oracle_home(&env)?;
    serve(AlcuinMcp {
        store: SessionStore::new(home),
        cwd: std::env::current_dir()?,
        env,
    })
    .await
}
pub async fn serve(server: AlcuinMcp) -> Result<()> {
    // Inline browser consultations and Project Sources own Chrome in this process.
    crate::browser_signal::install();
    crate::browser_signal::exit_on_ctrl_c();
    let closed = CancellationToken::new();
    let reader = EofReader {
        inner: tokio::io::stdin(),
        closed: closed.clone(),
    };
    let running = server.serve((reader, tokio::io::stdout())).await?;
    let cancellation = running.cancellation_token();
    let watcher = tokio::spawn(async move {
        closed.cancelled().await;
        cancellation.cancel();
    });
    let result = running.waiting().await;
    watcher.abort();
    let _ = watcher.await;
    result?;
    Ok(())
}
/// Reference runDryRunSummary lines, before the resolved-request lines.
fn dry_run_summary(request: &execution::ResolvedRequest) -> Result<Vec<String>> {
    if request.engine != "browser" {
        return crate::cli::api_preview_lines(request);
    }
    let mut prompt = crate::browser_prompt::BrowserPrompt::assemble(
        &request.files,
        request.options["prompt"].as_str().unwrap_or(""),
        request.options["system"].as_str(),
        &request.options,
        &std::env::temp_dir(),
    )?;
    let lines = prompt.preview_lines(
        &request.browser_preview,
        request.options["model"].as_str().unwrap_or("gpt-6-pro"),
        "dry-run",
        request.options["browserFollowUps"]
            .as_array()
            .map_or(&[], Vec::as_slice),
    );
    prompt.cleanup()?;
    lines
}
fn tool_result(text: impl Into<String>, structured: Value) -> Result<CallToolResult> {
    // Parsing the source wire shape avoids constructor-only fields on older MCP peers.
    Ok(serde_json::from_value(
        json!({"content":[{"type":"text","text":text.into()}],"structuredContent":structured}),
    )?)
}
fn session_entry(meta: &Value) -> Value {
    select(meta, &["id", "createdAt", "status", "model", "mode"])
}
fn session_line(meta: &Value) -> String {
    format!(
        "{} | {} | {} | {}",
        meta["createdAt"].as_str().unwrap_or(""),
        meta["status"].as_str().unwrap_or("unknown"),
        meta["model"].as_str().unwrap_or("n/a"),
        meta["id"].as_str().unwrap_or("")
    )
}
/// The last `max` UTF-16 units, as the reference `log.slice(-max)`; a split pair is dropped.
pub fn log_tail(log: &str, max: usize) -> String {
    let mut units = 0;
    let start = log
        .char_indices()
        .rev()
        .take_while(|(_, character)| {
            units += character.len_utf16();
            units <= max
        })
        .last()
        .map_or(log.len(), |(index, _)| index);
    log[start..].to_owned()
}
fn select(value: &Value, keys: &[&str]) -> Value {
    let mut out = json!({});
    for key in keys {
        if let Some(v) = value.get(*key).filter(|v| !v.is_null()) {
            out[*key] = v.clone();
        }
    }
    out
}
pub fn add_summaries(out: &mut Value, meta: &Value) {
    if meta["status"] == "completed"
        && (meta["cleanupWarning"].is_string()
            || (meta["recoveryRequired"] == true
                && meta["browser"]["runtime"]["targetDispositionPending"] == true))
    {
        out["browserCleanup"] = json!({"state":"pending","recoveryRequired":true});
    }
    if let Some(runs) = meta["models"].as_array().filter(|a| !a.is_empty()) {
        out["models"] = json!(
            runs.iter()
                .map(|run| {
                    let mut summary = select(
                        run,
                        &[
                            "model",
                            "effectiveModel",
                            "status",
                            "startedAt",
                            "completedAt",
                            "usage",
                        ],
                    );
                    if run["response"].is_object() {
                        summary["response"] =
                            select(&run["response"], &["id", "requestId", "status"]);
                    }
                    if run["error"].is_object() {
                        summary["error"] = select(&run["error"], &["category", "message"]);
                    }
                    if let Some(path) = run["log"]["path"].as_str() {
                        summary["logPath"] = json!(path);
                    }
                    summary
                })
                .collect::<Vec<_>>()
        );
    }
    if let Some(artifacts) = meta["artifacts"].as_array().filter(|a| !a.is_empty()) {
        out["artifacts"] = json!(
            artifacts
                .iter()
                .map(|a| select(a, &["kind", "path", "label", "mimeType", "sizeBytes"]))
                .collect::<Vec<_>>()
        );
        let images = artifacts
            .iter()
            .filter(|a| a["kind"] == "image")
            .map(|a| {
                select(
                    a,
                    &[
                        "kind",
                        "path",
                        "label",
                        "mimeType",
                        "sizeBytes",
                        "alt",
                        "width",
                        "height",
                        "fileId",
                    ],
                )
            })
            .collect::<Vec<_>>();
        if !images.is_empty() {
            out["images"] = json!(images);
        }
    }
    // Arbitrary warning strings can contain credentials and private paths. Only the
    // maintained bounded cleanup warning is allowed across this public boundary.
    let warnings=meta["browser"]["warnings"].as_array().into_iter().flatten().filter_map(|w|{
        if w["code"]!="artifact-staging-cleanup-failed"||w["severity"]!="warning"||!matches!(w["details"]["artifactKind"].as_str(),Some("file"|"image"))||!matches!(w["details"]["origin"].as_str(),Some("local"|"host"|"receiver")){return None;}
        let filename=crate::browser_output::filename(w["details"]["filename"].as_str().filter(|file|!file.is_empty())?);
        Some(json!({"code":"artifact-staging-cleanup-failed","severity":"warning","message":"Artifact publication succeeded and was verified, but staging cleanup failed; residual staging data may remain.","details":{"artifactKind":w["details"]["artifactKind"],"origin":w["details"]["origin"],"filename":filename}}))
    }).collect::<Vec<_>>();
    if !warnings.is_empty() {
        out["warnings"] = json!(warnings);
    }
}
/// Checks MCP tool input against its schema; keys the schema does not declare are refused.
pub fn validate_input(value: &Value, schema: &Value, path: &str) -> Result<()> {
    let kind = schema["type"].as_str().unwrap_or("");
    let valid = match kind {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "number" => value.as_f64().is_some_and(f64::is_finite),
        "integer" => value
            .as_f64()
            .is_some_and(|n| n.fract() == 0.0 && n.abs() <= 9007199254740991.0),
        "boolean" => value.is_boolean(),
        _ => true,
    };
    anyhow::ensure!(valid, "{path}: expected {kind}");
    if let Some(allowed) = schema["enum"].as_array() {
        anyhow::ensure!(allowed.contains(value), "{path}: invalid enum value");
    }
    if let Some(min) = schema["minLength"].as_u64() {
        anyhow::ensure!(
            value
                .as_str()
                .is_some_and(|s| s.encode_utf16().count() as u64 >= min),
            "{}",
            match path {
                "input.prompt" => "Prompt is required.".to_owned(),
                "input.id" => "Session id is required.".to_owned(),
                _ => format!("{path}: must contain at least {min} character(s)"),
            }
        );
    }
    if let Some(min) = schema["minimum"].as_f64() {
        anyhow::ensure!(
            value.as_f64().is_some_and(|n| n >= min),
            "{path}: must be at least {min}"
        );
    }
    if kind == "object" {
        let object = value.as_object().context("Object required")?;
        let properties = schema["properties"]
            .as_object()
            .context("Missing schema properties")?;
        for key in schema["required"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            anyhow::ensure!(
                object.contains_key(key),
                "{path}.{key}: required field is missing"
            );
        }
        for (key, value) in object {
            if let Some(child) = properties.get(key) {
                validate_input(value, child, &format!("{path}.{key}"))?;
            } else {
                bail!("Unrecognized key(s) in object: '{key}'");
            }
        }
    }
    if kind == "array" {
        for (index, item) in value
            .as_array()
            .context("Array required")?
            .iter()
            .enumerate()
        {
            validate_input(item, &schema["items"], &format!("{path}[{index}]"))?;
        }
    }
    Ok(())
}
fn decode_component(raw: &str) -> Result<String> {
    let mut bytes = vec![];
    let mut i = 0;
    while i < raw.len() {
        if raw.as_bytes()[i] == b'%' {
            let code = raw.get(i + 1..i + 3).context("Invalid URI escape")?;
            bytes.push(u8::from_str_radix(code, 16)?);
            i += 3;
        } else {
            bytes.push(raw.as_bytes()[i]);
            i += 1;
        }
    }
    Ok(String::from_utf8(bytes)?)
}
#[cfg(test)]
mod initialize_tests {
    #[test]
    fn server_info_reports_the_package_version() {
        let info = super::initialize_info(
            r#"{"protocolVersion":"2025-11-25","capabilities":{},"serverInfo":{"name":"alcuin-mcp","version":"0.0.0-PUBLIC"}}"#,
        );
        assert_eq!(info["serverInfo"]["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(info["serverInfo"]["name"], "alcuin-mcp");
    }
}
