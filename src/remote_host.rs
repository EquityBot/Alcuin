//! Native HTTP host for durable remote delivery and bounded run admission.
mod artifact;
use crate::{
    policy, remote_json,
    remote_result::{self, Phase, RemoteRecord},
    remote_slots::{Reservation, RunSlots},
    sessions::SessionStore,
};
use anyhow::{Context, Result, ensure};
use artifact::VerifiedArtifact;
use bytes::Bytes;
use futures_util::future::BoxFuture;
use http_body_util::BodyExt;
use hyper::{
    Method, Request, Response,
    body::{Body, Frame, Incoming},
    server::conn::http1,
    service::service_fn,
};
use hyper_util::rt::TokioIo;
use serde_json::{Value, json, value::RawValue};
use std::{
    convert::Infallible,
    net::SocketAddr,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context as TaskContext, Poll},
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::TcpListener,
    sync::{Mutex, mpsc},
    task::{JoinHandle, JoinSet},
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

pub const MAX_ARTIFACT_BYTES: u64 = 512 * 1024 * 1024;
/// An artifact route refusal with the reference status and error code.
#[derive(Debug, Clone, Copy)]
struct ArtifactRefusal(u16, &'static str);
impl std::fmt::Display for ArtifactRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.1)
    }
}
impl std::error::Error for ArtifactRefusal {}
const CLIENT_FIELDS: &[&str] = &[
    "chatgptUrl",
    "url",
    "desiredModel",
    "modelIsImplicitDefault",
    "automaticModelPriority",
    "modelStrategy",
    "thinkingTime",
    "requireVerifiedSelection",
    "researchMode",
    "archiveConversations",
    "resumeConversationUrl",
    "timeoutMs",
    "inputTimeoutMs",
    "attachmentTimeoutMs",
    "assistantRecheckDelayMs",
    "assistantRecheckTimeoutMs",
    "autoReattachDelayMs",
    "autoReattachIntervalMs",
    "autoReattachTimeoutMs",
    "keepBrowser",
    "debug",
];
pub fn client_browser_config(requested: &Value) -> Value {
    let mut accepted = json!({});
    for field in CLIENT_FIELDS {
        if let Some(value) = requested.get(*field)
            && (*field != "automaticModelPriority" || value.is_boolean())
        {
            accepted[*field] = value.clone();
        }
    }
    accepted
}
pub struct HostOptions {
    pub address: SocketAddr,
    pub token: String,
    pub max_concurrent_runs: Option<usize>,
    pub max_queued_runs: Option<usize>,
    pub max_browser_tabs: usize,
}
#[derive(Clone)]
pub struct ProducedRun {
    pub captured: Value,
    pub result: Value,
    pub artifacts: Value,
}
pub type RunDriver =
    Arc<dyn Fn(RunContext) -> BoxFuture<'static, Result<ProducedRun>> + Send + Sync>;
#[derive(Clone)]
pub struct RunContext {
    pub run_id: String,
    pub payload: Value,
    pub store: SessionStore,
    pub cancel: CancellationToken,
    events: mpsc::Sender<Bytes>,
    captured: Arc<AtomicBool>,
}
impl RunContext {
    #[cfg(test)]
    pub(crate) fn fixture(run_id: &str, payload: Value, store: SessionStore) -> Self {
        let (events, _) = mpsc::channel(16);
        Self {
            run_id: run_id.into(),
            payload,
            store,
            cancel: CancellationToken::new(),
            events,
            captured: Arc::new(AtomicBool::new(false)),
        }
    }
    pub async fn log(&self, message: &str) {
        let _ = self
            .events
            .send(Bytes::from(format!(
                "{}\n",
                json!({"type":"log","message":message})
            )))
            .await;
    }
    /// The browser driver calls this before disposing its target.
    pub fn capture(&self, produced: &ProducedRun) -> Result<()> {
        ensure!(
            !self.captured.load(Ordering::Acquire),
            "Remote capture was already saved"
        );
        remote_result::save(
            &self.store,
            &self.run_id,
            &produced.captured,
            &produced.result,
            &produced.artifacts,
            Phase::Capture,
            &self.cancel,
        )?;
        self.captured.store(true, Ordering::Release);
        Ok(())
    }
}
struct State {
    store: SessionStore,
    token: String,
    slots: RunSlots,
    queued: bool,
    busy: Arc<AtomicBool>,
    driver: RunDriver,
    started: Instant,
    shutdown: CancellationToken,
    workers: Mutex<Vec<JoinHandle<()>>>,
    receive_timeout: Duration,
}
pub struct RemoteHost {
    pub address: SocketAddr,
    stop: CancellationToken,
    worker: Option<JoinHandle<Result<()>>>,
    #[cfg(test)]
    state: Arc<State>,
}
impl RemoteHost {
    pub async fn start(
        store: SessionStore,
        options: HostOptions,
        driver: RunDriver,
    ) -> Result<Self> {
        Self::start_with_timeouts(
            store,
            options,
            driver,
            Duration::from_secs(60),
            Duration::from_secs(300),
        )
        .await
    }
    async fn start_with_timeouts(
        store: SessionStore,
        options: HostOptions,
        driver: RunDriver,
        header_timeout: Duration,
        receive_timeout: Duration,
    ) -> Result<Self> {
        ensure!(
            options.max_concurrent_runs.is_some() || options.max_queued_runs.is_none(),
            "--max-queued-runs requires --max-concurrent-runs."
        );
        ensure!(
            !options.token.contains(['\r', '\n']),
            "Remote token cannot contain a header line break"
        );
        let queued = options.max_concurrent_runs.is_some();
        let requested = options.max_concurrent_runs.unwrap_or(1);
        ensure!(
            requested > 0 && options.max_browser_tabs > 0,
            "--max-concurrent-runs must be a positive integer."
        );
        let slots = RunSlots::new(
            requested.min(options.max_browser_tabs),
            if queued {
                options.max_queued_runs.unwrap_or(8)
            } else {
                0
            },
        )?;
        let listener = TcpListener::bind(options.address).await?;
        let address = listener.local_addr()?;
        let stop = CancellationToken::new();
        let state = Arc::new(State {
            store,
            token: options.token,
            slots,
            queued,
            busy: Arc::new(AtomicBool::new(false)),
            driver,
            started: Instant::now(),
            shutdown: stop.clone(),
            workers: Mutex::new(vec![]),
            receive_timeout,
        });
        #[cfg(test)]
        let test_state = state.clone();
        let worker = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            let outcome = loop {
                tokio::select! {biased;
                    _=state.shutdown.cancelled()=>break Ok(()),
                    joined=connections.join_next(),if !connections.is_empty()=>{let _=joined;},
                    accepted=listener.accept()=>{
                        let (stream,_)=match accepted {Ok(accepted)=>accepted,Err(error)=>break Err(anyhow::Error::from(error))};
                        connections.spawn(serve_stream(stream,state.clone(),header_timeout));
                    }
                }
            };
            state.shutdown.cancel();
            connections.abort_all();
            while connections.join_next().await.is_some() {}
            let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            for mut worker in state.workers.lock().await.drain(..) {
                if tokio::time::timeout_at(deadline, &mut worker)
                    .await
                    .is_err()
                {
                    worker.abort();
                    let _ = worker.await;
                }
            }
            outcome
        });
        Ok(Self {
            address,
            stop,
            worker: Some(worker),
            #[cfg(test)]
            state: test_state,
        })
    }
    pub async fn shutdown(mut self) -> Result<()> {
        self.stop.cancel();
        if let Some(worker) = self.worker.take() {
            worker.await??;
        }
        Ok(())
    }
}
async fn serve_stream<T: AsyncRead + AsyncWrite + Unpin>(
    stream: T,
    state: Arc<State>,
    header_timeout: Duration,
) {
    let gone = state.shutdown.child_token();
    let request_gone = gone.clone();
    let service_state = state.clone();
    let service = service_fn(move |request| {
        route(request, service_state.clone(), request_gone.child_token())
    });
    let connection = http1::Builder::new()
        .timer(hyper_util::rt::TokioTimer::new())
        .header_read_timeout(header_timeout)
        .serve_connection(TokioIo::new(stream), service);
    tokio::pin!(connection);
    tokio::select! { _ = state.shutdown.cancelled() => {}, _ = &mut connection => {} }
    gone.cancel();
}
impl Drop for RemoteHost {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}
struct RemoteBody {
    receiver: mpsc::Receiver<Bytes>,
    disconnect: Option<CancellationToken>,
}
impl Body for RemoteBody {
    type Data = Bytes;
    type Error = Infallible;
    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
    ) -> Poll<Option<std::result::Result<Frame<Bytes>, Infallible>>> {
        self.receiver
            .poll_recv(cx)
            .map(|value| value.map(|bytes| Ok(Frame::data(bytes))))
    }
}
impl Drop for RemoteBody {
    fn drop(&mut self) {
        if let Some(cancel) = &self.disconnect {
            cancel.cancel();
        }
    }
}
fn response(status: u16, content: &str, bytes: impl Into<Bytes>) -> Response<RemoteBody> {
    let (sender, receiver) = mpsc::channel(1);
    sender
        .try_send(bytes.into())
        .expect("Fresh response channel has capacity");
    drop(sender);
    Response::builder()
        .status(status)
        .header("Content-Type", content)
        .body(RemoteBody {
            receiver,
            disconnect: None,
        })
        .expect("Static response headers")
}
fn json_response(status: u16, value: Value) -> Response<RemoteBody> {
    response(status, "application/json", value.to_string())
}
fn receive_timeout_response() -> Response<RemoteBody> {
    let mut result = response(408, "application/json", Bytes::new());
    result.headers_mut().insert(
        "Connection",
        hyper::header::HeaderValue::from_static("close"),
    );
    result
}
struct RequestGuard(Option<CancellationToken>);
impl Drop for RequestGuard {
    fn drop(&mut self) {
        if let Some(cancel) = &self.0 {
            cancel.cancel();
        }
    }
}
struct BusyGuard(Option<Arc<AtomicBool>>);
impl Drop for BusyGuard {
    fn drop(&mut self) {
        if let Some(busy) = &self.0 {
            busy.store(false, Ordering::Release);
        }
    }
}
async fn body(
    request: &mut Request<Incoming>,
    cancel: &CancellationToken,
    limit: Option<usize>,
    deadline: tokio::time::Instant,
) -> Result<Vec<u8>> {
    tokio::time::timeout_at(deadline,async {
    let mut bytes = vec![];
    loop {
        let frame = tokio::select! {biased;_=cancel.cancelled()=>anyhow::bail!("Remote request was cancelled"),frame=request.body_mut().frame()=>frame};
        let Some(frame) = frame else { break };
        let frame = frame?;
        if let Ok(data) = frame.into_data() {
            ensure!(
                limit.is_none_or(|limit| bytes
                    .len()
                    .checked_add(data.len())
                    .is_some_and(|size| size <= limit)),
                "Remote request body exceeds its limit."
            );
            bytes.extend_from_slice(&data);
        }
    }
    Ok::<_,anyhow::Error>(bytes)
    }).await?
}
fn parse_json(bytes: &[u8]) -> Result<Value> {
    remote_json::native_projection(serde_json::from_slice::<&RawValue>(bytes)?)
}
pub(crate) fn require_artifact_identity(descriptor: &Value, run: &Value) -> Result<()> {
    ensure!(
        descriptor["runId"] == *run
            && descriptor["artifactId"]
                .as_str()
                .is_some_and(|id| id.len() == 36
                    && id.bytes().all(|byte| byte.is_ascii_digit()
                        || (b'a'..=b'f').contains(&byte)
                        || byte == b'-')),
        "Remote artifact run identity differs"
    );
    Ok(())
}
pub(crate) fn artifact_mime(value: Option<&str>) -> Option<String> {
    let mime = policy::trim(value?.split(';').next()?).to_ascii_lowercase();
    let mut parts = mime.split('/');
    let (Some(kind), Some(subtype)) = (parts.next(), parts.next()) else {
        return None;
    };
    let valid = |part: &str| {
        part.as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
            && part
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"!#$&^_.+-".contains(&byte))
    };
    (mime.len() <= 127 && parts.next().is_none() && valid(kind) && valid(subtype)).then_some(mime)
}
fn events(
    store: &SessionStore,
    record: &RemoteRecord,
    cancel: &CancellationToken,
) -> Result<Vec<Bytes>> {
    let raw: &RawValue = serde_json::from_str(record.wire())?;
    let artifacts =
        remote_json::field(raw, "artifacts")?.context("Remote artifacts are missing")?;
    for artifact in record.native()["artifacts"]
        .as_array()
        .context("Remote artifacts are invalid")?
    {
        VerifiedArtifact::open(store, record, artifact, cancel)?;
    }
    let rows: Vec<&RawValue> = serde_json::from_str(artifacts.get())?;
    let mut output = vec![];
    if record.native()["phase"] == "capture" {
        output.push(Bytes::from(format!("{}\n",json!({"type":"log","message":"[browser] Recovering the durable capture; host finalisation is unconfirmed."}))));
    }
    for artifact in rows {
        let descriptor = remote_json::field(artifact, "descriptor")?
            .context("Remote artifact descriptor missing")?;
        output.push(Bytes::from(format!(
            "{{\"type\":\"artifact-ready\",\"runId\":{},\"artifact\":{}}}\n",
            record.native()["runId"],
            descriptor.get()
        )));
    }
    output.push(Bytes::from(format!(
        "{{\"type\":\"result\",\"result\":{},\"delivery\":{}}}\n",
        remote_json::field(raw, "result")?
            .context("Remote result missing")?
            .get(),
        record.native()["delivery"]
    )));
    Ok(output)
}
async fn spawn(state: &State, future: impl std::future::Future<Output = ()> + Send + 'static) {
    let mut workers = state.workers.lock().await;
    workers.retain(|worker| !worker.is_finished());
    workers.push(tokio::spawn(future));
}
async fn route(
    mut request: Request<Incoming>,
    state: Arc<State>,
    gone: CancellationToken,
) -> std::result::Result<Response<RemoteBody>, Infallible> {
    let receive_deadline = tokio::time::Instant::from_std(
        request
            .extensions()
            .get::<hyper::ext::RequestStart>()
            .expect("HTTP server records each request start")
            .0,
    ) + state.receive_timeout;
    let path = request
        .uri()
        .path_and_query()
        .map(|value| value.as_str())
        .unwrap_or("")
        .to_owned();
    let method = request.method().clone();
    if method == Method::GET && path == "/status" {
        return Ok(json_response(200, json!({"ok":true})));
    }
    let parts: Vec<_> = path.split('/').collect();
    let artifact_parts: Vec<_> = request.uri().path().split('/').map(str::to_owned).collect();
    let result_route =
        parts.len() == 4 && parts[1] == "runs" && matches!(parts[3], "result" | "ack");
    let artifact_route = method == Method::GET
        && artifact_parts.len() == 5
        && artifact_parts[1] == "runs"
        && artifact_parts[3] == "artifacts";
    if !(method == Method::GET && path == "/health"
        || result_route
        || artifact_route
        || method == Method::POST && path == "/runs")
    {
        return Ok(response(404, "application/json", Bytes::new()));
    }
    if request
        .headers()
        .get("authorization")
        .is_none_or(|value| value.as_bytes() != format!("Bearer {}", state.token).as_bytes())
    {
        return Ok(json_response(401, json!({"error":"unauthorized"})));
    }
    if method == Method::GET && path == "/health" {
        let snapshot = state.slots.snapshot();
        return Ok(json_response(
            200,
            json!({"ok":true,"version":env!("CARGO_PKG_VERSION"),"uptimeSeconds":state.started.elapsed().as_secs_f64().round(),
            "capabilities":{"runCancellation":true,"deferredFallbackBundling":true,"artifactTransfer":true,"generatedImages":true,"durableResultRecovery":true,"artifactProtocolVersion":1,"maxArtifactBytes":MAX_ARTIFACT_BYTES},
            "activeRuns":snapshot.active,"queuedRuns":snapshot.queued,"maxConcurrentRuns":snapshot.capacity,"maxQueuedRuns":snapshot.queue_capacity,"admissionMode":if state.queued{"queue"}else{"legacy"}}),
        ));
    }
    if result_route {
        let operation = async {
            let record = remote_result::read(&state.store, parts[2])?;
            if parts[3] == "result" && method == Method::GET {
                let mut result = response(
                    200,
                    "application/x-ndjson",
                    events(&state.store, &record, &gone)?.concat(),
                );
                result.headers_mut().insert(
                    "Cache-Control",
                    hyper::header::HeaderValue::from_static("no-store"),
                );
                Ok(result)
            } else if parts[3] == "ack" && method == Method::POST {
                let delivery =
                    parse_json(&body(&mut request, &gone, Some(1024), receive_deadline).await?)?;
                ensure!(
                    delivery["version"] == 1
                        && delivery["runId"] == record.native()["runId"]
                        && delivery["sha256"] == record.native()["delivery"]["sha256"],
                    "Remote result acknowledgement does not match the captured delivery."
                );
                let store = state.store.clone();
                let (sender, receiver) = tokio::sync::oneshot::channel();
                spawn(&state, async move {
                    // Reply once the acknowledgement is durable. Target retirement and
                    // browser cleanup can outlast the client's acknowledgement timeout, so
                    // they run afterwards in this tracked worker.
                    let committed = match remote_result::commit_acknowledgement(&store, &record) {
                        Ok(committed) => committed,
                        Err(error) => {
                            let _ = sender.send(Err(error));
                            return;
                        }
                    };
                    let _ = sender.send(Ok(()));
                    let run = record.native()["runId"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned();
                    match committed.finish(&store, &record).await {
                        Ok(None) => {}
                        Ok(Some(warning)) => eprintln!("Remote run {run}: {warning}"),
                        Err(error) => eprintln!(
                            "Remote run {run}: delivery is saved; cleanup failed: {error:#}"
                        ),
                    }
                })
                .await;
                receiver.await??;
                Ok(json_response(200, json!({"acknowledged":true})))
            } else {
                Ok(response(404, "application/json", Bytes::new()))
            }
        }
        .await;
        return Ok(operation.unwrap_or_else(|error: anyhow::Error| {
            if error
                .downcast_ref::<tokio::time::error::Elapsed>()
                .is_some()
            {
                return receive_timeout_response();
            }
            json_response(
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound)
                {
                    404
                } else {
                    409
                },
                json!({"error":"remote_result_unavailable"}),
            )
        }));
    }
    if artifact_route {
        let operation = || -> Result<_> {
            let missing = || ArtifactRefusal(404, "artifact_not_found");
            let record =
                remote_result::read(&state.store, &artifact_parts[2]).map_err(|_| missing())?;
            let artifact = record.native()["artifacts"]
                .as_array()
                .and_then(|artifacts| {
                    artifacts
                        .iter()
                        .find(|artifact| artifact["descriptor"]["artifactId"] == artifact_parts[4])
                })
                .ok_or_else(missing)?;
            // Reference stat checks: a missing or empty file, then an oversized one.
            match artifact["filePath"].as_str().map(std::fs::metadata) {
                Some(Ok(metadata)) if metadata.is_file() && metadata.len() > 0 => {
                    if metadata.len() > MAX_ARTIFACT_BYTES {
                        return Err(ArtifactRefusal(413, "artifact_too_large").into());
                    }
                }
                _ => return Err(ArtifactRefusal(410, "artifact_unavailable").into()),
            }
            let verified = VerifiedArtifact::open(&state.store, &record, artifact, &gone)?;
            let descriptor = &artifact["descriptor"];
            let filename =
                crate::browser_output::filename(descriptor["filename"].as_str().unwrap_or(""));
            let mime = artifact_mime(descriptor["mimeType"].as_str())
                .unwrap_or_else(|| "application/octet-stream".into());
            let (sender, receiver) = mpsc::channel(1);
            let mut result = Response::builder()
                .status(200)
                .header("Content-Type", &mime)
                .body(RemoteBody {
                    receiver,
                    disconnect: Some(gone.clone()),
                })?;
            result.headers_mut().insert(
                "Content-Length",
                hyper::header::HeaderValue::from_str(&descriptor["byteSize"].to_string())?,
            );
            result.headers_mut().insert(
                "Content-Disposition",
                hyper::header::HeaderValue::from_str(&format!(
                    "attachment; filename=\"{filename}\""
                ))?,
            );
            result.headers_mut().insert(
                "Cache-Control",
                hyper::header::HeaderValue::from_static("no-store"),
            );
            result.headers_mut().insert(
                "X-Content-Type-Options",
                hyper::header::HeaderValue::from_static("nosniff"),
            );
            result.headers_mut().insert(
                "X-Oracle-Artifact-Id",
                hyper::header::HeaderValue::from_str(
                    descriptor["artifactId"]
                        .as_str()
                        .context("Missing artifact ID")?,
                )?,
            );
            result.headers_mut().insert(
                "X-Oracle-Artifact-Sha256",
                hyper::header::HeaderValue::from_str(
                    descriptor["sha256"]
                        .as_str()
                        .context("Missing artifact digest")?,
                )?,
            );
            Ok((result, verified, sender))
        };
        return Ok(match operation() {
            Ok((result, verified, sender)) => {
                let cancel = gone.clone();
                spawn(&state, async move {
                    // A dropped response releases a blocked sender; shutdown also cancels
                    // the request. No browser worker or target authority is involved.
                    let _ =
                        tokio::task::spawn_blocking(move || verified.stream(sender, &cancel)).await;
                })
                .await;
                result
            }
            // A file that fails verification is unavailable, as the reference has no such file.
            Err(error) => {
                let ArtifactRefusal(status, code) = error
                    .downcast_ref::<ArtifactRefusal>()
                    .copied()
                    .unwrap_or(ArtifactRefusal(410, "artifact_unavailable"));
                json_response(status, json!({"error":code}))
            }
        });
    }
    if method != Method::POST || path != "/runs" {
        return Ok(response(404, "application/json", Bytes::new()));
    }
    let busy = if state.queued {
        None
    } else if state
        .busy
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
    {
        Some(state.busy.clone())
    } else {
        return Ok(json_response(409, json!({"error":"busy"})));
    };
    let busy = BusyGuard(busy);
    let mut guard = RequestGuard(Some(gone.clone()));
    let mut reservation: Option<Reservation> = None;
    // Reference positionFor, taken as the request joins the queue.
    let position = state.slots.snapshot().position;
    if state.queued {
        match state.slots.reserve(&gone) {
            Ok(slot) => reservation = Some(slot),
            Err(_) => {
                let mut result = json_response(503, json!({"error":"queue_full"}));
                result
                    .headers_mut()
                    .insert("Retry-After", hyper::header::HeaderValue::from_static("60"));
                return Ok(result);
            }
        }
    }
    let payload = async {
        let mut payload = parse_json(&body(&mut request, &gone, None, receive_deadline).await?)?;
        ensure!(payload["prompt"].is_string(), "Missing prompt");
        payload["browserConfig"] = client_browser_config(&payload["browserConfig"]);
        Ok::<_, anyhow::Error>(payload)
    }
    .await;
    let payload = match payload {
        Ok(payload) => payload,
        Err(error)
            if error
                .downcast_ref::<tokio::time::error::Elapsed>()
                .is_some() =>
        {
            return Ok(receive_timeout_response());
        }
        Err(_) => return Ok(json_response(400, json!({"error":"invalid_request"}))),
    };
    // Reference: a model policy refusal keeps its reason for the client.
    if let Err(error) =
        policy::assert_model_allowed(payload["options"]["model"].as_str()).and_then(|()| {
            policy::assert_model_allowed(payload["browserConfig"]["desiredModel"].as_str())
        })
    {
        return Ok(json_response(
            400,
            json!({"error":"unsupported_browser_provider","message":error.to_string()}),
        ));
    }
    let cancel_on_disconnect = state.queued || payload["options"]["cancelOnDisconnect"] == true;
    let run_cancel = if cancel_on_disconnect {
        gone.clone()
    } else {
        state.shutdown.child_token()
    };
    let reservation = match reservation {
        Some(reservation) => reservation,
        None => match state.slots.reserve(&run_cancel) {
            Ok(reservation) => reservation,
            Err(_) => return Ok(json_response(409, json!({"error":"busy"}))),
        },
    };
    let (sender, receiver) = mpsc::channel(32);
    if position > 0 && reservation.waiting() {
        let _ = sender.try_send(Bytes::from(format!(
            "{}\n",
            json!({"type":"log","message":format!(
                "[serve] Waiting for a browser slot (position {position}; {} active).",
                state.slots.snapshot().active
            )})
        )));
    }
    let run_id = Uuid::new_v4().to_string();
    let context = RunContext {
        run_id: run_id.clone(),
        payload,
        store: state.store.clone(),
        cancel: run_cancel,
        events: sender.clone(),
        captured: Arc::new(AtomicBool::new(false)),
    };
    let driver = state.driver.clone();
    spawn(&state, async move {
        let _busy = busy;
        let outcome = async {
            let _slot = reservation.wait().await?;
            let _ = sender
                .send(Bytes::from(format!(
                    "{}\n",
                    json!({"type":"run-started","runId":run_id})
                )))
                .await;
            let result = driver(context.clone()).await?;
            if !context.captured.load(Ordering::Acquire) {
                context.capture(&result)?;
            }
            remote_result::save(
                &context.store,
                &run_id,
                &result.captured,
                &result.result,
                &result.artifacts,
                Phase::Result,
                &context.cancel,
            )?;
            let record = remote_result::read(&context.store, &run_id)?;
            let count = record.native()["artifacts"].as_array().map_or(0, Vec::len);
            if count > 0 {
                let _ = sender
                    .send(Bytes::from(format!(
                        "{}\n",
                        json!({"type":"log","message":format!("[browser] {count} artifact(s) ready for bridge transfer. If no cloud-local artifact path appears, upgrade both Industrious bridge endpoints or copy the file manually from the Windows browser host.")})
                    )))
                    .await;
            }
            for event in events(&context.store, &record, &context.cancel)? {
                let _ = sender.send(event).await;
            }
            Ok::<_, anyhow::Error>(())
        }
        .await;
        if let Err(error) = outcome {
            let _ = sender
                .send(Bytes::from(format!(
                    "{}\n",
                    json!({"type":"error","message":error.to_string()})
                )))
                .await;
        }
    })
    .await;
    // The response body owns disconnect cancellation after the request has been parsed.
    guard.0.take();
    let mut result = Response::new(RemoteBody {
        receiver,
        disconnect: Some(gone),
    });
    result.headers_mut().insert(
        "Content-Type",
        hyper::header::HeaderValue::from_static("application/x-ndjson"),
    );
    Ok(result)
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifacts;
    use std::sync::atomic::AtomicUsize;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    const TOKEN: &str = "PUBLIC fixture token";
    fn client() -> reqwest::Client {
        reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap()
    }
    async fn fixture(
        partial: bool,
    ) -> (
        tempfile::TempDir,
        SessionStore,
        RemoteHost,
        Arc<AtomicUsize>,
    ) {
        fixture_timeout(partial, Duration::from_secs(300)).await
    }
    async fn fixture_timeout(
        partial: bool,
        receive_timeout: Duration,
    ) -> (
        tempfile::TempDir,
        SessionStore,
        RemoteHost,
        Arc<AtomicUsize>,
    ) {
        fixture_timeouts(partial, Duration::from_secs(60), receive_timeout).await
    }
    async fn fixture_timeouts(
        partial: bool,
        header_timeout: Duration,
        receive_timeout: Duration,
    ) -> (
        tempfile::TempDir,
        SessionStore,
        RemoteHost,
        Arc<AtomicUsize>,
    ) {
        let home = tempfile::tempdir().unwrap();
        let store = SessionStore::new(home.path().join(".oracle"));
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        let driver: RunDriver = Arc::new(move |context| {
            let calls = observed.clone();
            Box::pin(async move {
                calls.fetch_add(1, Ordering::SeqCst);
                let id = remote_result::session_id(&context.run_id)?;
                context.store.create(
                    json!({"prompt":"PUBLIC remote fixture"}),
                    context.store.home(),
                    Some(&id),
                )?;
                context.store.update(&id, |_| Ok(Some(json!({"status":"running","browser":{"remoteDelivery":{"runId":context.run_id}}}))))?;
                context.log("PUBLIC driver log").await;
                let result = json!({"answerText":"PUBLIC café reply","answerMarkdown":"**PUBLIC café reply**"});
                let produced = ProducedRun {
                    captured: result.clone(),
                    result,
                    artifacts: json!([]),
                };
                if partial {
                    context.capture(&produced)?;
                    anyhow::bail!("PUBLIC finalisation interrupted");
                }
                Ok(produced)
            })
        });
        let host = RemoteHost::start_with_timeouts(
            store.clone(),
            HostOptions {
                address: "127.0.0.1:0".parse().unwrap(),
                token: TOKEN.into(),
                max_concurrent_runs: Some(1),
                max_queued_runs: Some(1),
                max_browser_tabs: 1,
            },
            driver,
            header_timeout,
            receive_timeout,
        )
        .await
        .unwrap();
        (home, store, host, calls)
    }
    async fn health(client: &reqwest::Client, url: &str) -> Value {
        client
            .get(format!("{url}/health"))
            .bearer_auth(TOKEN)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    }
    #[tokio::test]
    async fn stalled_headers_return_timeout_without_changing_malformed_request_errors() {
        let (_home, _store, host, calls) = fixture_timeouts(
            false,
            Duration::from_millis(150),
            Duration::from_millis(400),
        )
        .await;
        for (request, expected) in [
            (b"".as_slice(), "HTTP/1.1 408"),
            (
                b"POST /runs HTTP/1.1\r\nHost: fixture\r\n".as_slice(),
                "HTTP/1.1 408",
            ),
            (
                b"POST /runs HTTP/1.1\r\nInvalid Header\r\n\r\n".as_slice(),
                "HTTP/1.1 400",
            ),
        ] {
            let mut socket = tokio::net::TcpStream::connect(host.address).await.unwrap();
            socket.write_all(request).await.unwrap();
            let mut response = vec![];
            tokio::time::timeout(Duration::from_secs(3), socket.read_to_end(&mut response))
                .await
                .unwrap()
                .unwrap();
            let response = String::from_utf8_lossy(&response);
            assert!(response.starts_with(expected), "{response:?}");
            assert_eq!(response.matches("HTTP/1.1").count(), 1);
        }
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        host.shutdown().await.unwrap();
    }
    #[tokio::test]
    async fn header_timeout_does_not_append_a_response_while_previous_output_is_blocked() {
        let (server, mut client) = tokio::io::duplex(128);
        let (sender, mut receiver) = mpsc::channel(1);
        let service = service_fn(move |request: Request<Incoming>| {
            sender.try_send(request.into_body()).unwrap();
            async {
                Ok::<_, Infallible>(Response::new(http_body_util::Full::new(Bytes::from(
                    vec![b'z'; 4096],
                ))))
            }
        });
        let connection = http1::Builder::new()
            .writev(true)
            .timer(hyper_util::rt::TokioTimer::new())
            .header_read_timeout(Duration::from_millis(100))
            .serve_connection(TokioIo::new(server), service);
        let worker = tokio::spawn(connection);
        client
            .write_all(b"GET /first HTTP/1.1\r\nHost: fixture\r\nContent-Length: 1\r\n\r\n")
            .await
            .unwrap();
        let mut body = tokio::time::timeout(Duration::from_secs(2), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        let first = tokio::time::timeout(Duration::from_secs(2), client.read_u8())
            .await
            .unwrap()
            .unwrap();
        client
            .write_all(b"xGET /second HTTP/1.1\r\nHost: fixture\r\n")
            .await
            .unwrap();
        let received = tokio::time::timeout(Duration::from_secs(2), body.frame())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(received.into_data().unwrap(), "x");
        tokio::time::sleep(Duration::from_millis(200)).await;
        let mut bytes = vec![first];
        tokio::time::timeout(Duration::from_secs(2), client.read_to_end(&mut bytes))
            .await
            .unwrap()
            .unwrap();
        let boundary = bytes
            .windows(4)
            .position(|bytes| bytes == b"\r\n\r\n")
            .unwrap()
            + 4;
        assert!(bytes.starts_with(b"HTTP/1.1 200"));
        assert!(bytes.len() - boundary < 4096);
        assert!(bytes[boundary..].iter().all(|byte| *byte == b'z'));
        assert!(!bytes.windows(12).any(|part| part == b"HTTP/1.1 408"));
        let result = tokio::time::timeout(Duration::from_secs(2), worker)
            .await
            .unwrap()
            .unwrap();
        assert!(result.unwrap_err().is_timeout());
    }
    #[tokio::test(start_paused = true)]
    async fn reused_idle_connection_closes_without_a_request_timeout_response() {
        let (server, mut client) = tokio::io::duplex(512);
        let service = service_fn(|_: Request<Incoming>| async {
            Ok::<_, Infallible>(Response::new(http_body_util::Full::new(Bytes::new())))
        });
        let connection = http1::Builder::new()
            .timer(hyper_util::rt::TokioTimer::new())
            .header_read_timeout(Duration::from_secs(60))
            .serve_connection(TokioIo::new(server), service);
        let worker = tokio::spawn(connection);
        client
            .write_all(b"GET / HTTP/1.1\r\nHost: fixture\r\n\r\n")
            .await
            .unwrap();
        let mut response = vec![];
        while !response.ends_with(b"\r\n\r\n") {
            response.push(client.read_u8().await.unwrap());
            assert!(response.len() < 512);
        }
        assert!(response.starts_with(b"HTTP/1.1 200"));
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(5)).await;
        let mut byte = [0];
        assert!(
            tokio::time::timeout(Duration::from_millis(1), client.read(&mut byte))
                .await
                .is_err()
        );
        tokio::time::advance(Duration::from_secs(1)).await;
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), client.read(&mut byte))
                .await
                .unwrap()
                .unwrap(),
            0
        );
        tokio::time::timeout(Duration::from_secs(1), worker)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
    #[tokio::test(start_paused = true)]
    async fn stalled_receive_budget_includes_headers_and_excludes_previous_request_idle() {
        let (_home, _store, host, calls) = fixture_timeouts(
            false,
            Duration::from_millis(350),
            Duration::from_millis(600),
        )
        .await;
        for kind in ["fresh", "idle-fresh", "idle-keep-alive", "pipelined"] {
            // The same HTTP connection driver runs on an in-memory stream so its
            // clock can advance without racing the Windows TCP stack or scheduler.
            let (server, mut socket) = tokio::io::duplex(8192);
            let worker = tokio::spawn(serve_stream(
                server,
                host.state.clone(),
                Duration::from_millis(350),
            ));
            if kind == "idle-keep-alive" {
                socket
                    .write_all(b"GET /status HTTP/1.1\r\nHost: fixture\r\n\r\n")
                    .await
                    .unwrap();
                read_status_response(&mut socket).await;
                tokio::time::advance(Duration::from_millis(450)).await;
            } else if kind == "idle-fresh" {
                tokio::time::advance(Duration::from_millis(200)).await;
            }
            let started = tokio::time::Instant::now();
            if kind == "pipelined" {
                socket
                    .write_all(
                        b"GET /status HTTP/1.1\r\nHost: fixture\r\n\r\nPOST /runs HTTP/1.1\r\n",
                    )
                    .await
                    .unwrap();
                read_status_response(&mut socket).await;
            } else {
                socket.write_all(b"POST /runs HTTP/1.1\r\n").await.unwrap();
            }
            tokio::task::yield_now().await;
            tokio::time::advance(Duration::from_millis(250)).await;
            socket.write_all(format!("Host: fixture\r\nAuthorization: Bearer {TOKEN}\r\nContent-Length: 100\r\n\r\n{{").as_bytes()).await.unwrap();
            tokio::task::yield_now().await;
            let response = tokio::spawn(async move {
                let mut response = vec![];
                socket.read_to_end(&mut response).await.unwrap();
                response
            });
            tokio::time::advance(Duration::from_millis(349)).await;
            tokio::task::yield_now().await;
            assert!(
                !response.is_finished(),
                "{kind}: receive deadline fired early"
            );
            tokio::time::advance(Duration::from_millis(1)).await;
            let response = response.await.unwrap();
            let elapsed = started.elapsed();
            assert!(
                String::from_utf8_lossy(&response).starts_with("HTTP/1.1 408"),
                "{kind}: {response:?}"
            );
            assert!(elapsed == Duration::from_millis(600), "{kind}: {elapsed:?}");
            worker.await.unwrap();
        }
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(host.state.slots.snapshot().active, 0);
        assert_eq!(host.state.slots.snapshot().queued, 0);
        host.shutdown().await.unwrap();
    }
    async fn read_status_response(socket: &mut (impl AsyncRead + Unpin)) {
        tokio::time::timeout(Duration::from_secs(2), async {
            let mut bytes = vec![];
            while !bytes.ends_with(b"\r\n0\r\n\r\n") {
                bytes.push(socket.read_u8().await.unwrap());
                assert!(bytes.len() < 4096);
            }
            assert!(bytes.starts_with(b"HTTP/1.1 200"));
        })
        .await
        .unwrap();
    }
    #[tokio::test]
    async fn stalled_body_expires_and_returns_capacity_without_client_disconnect_or_dispatch() {
        let (_home, _store, host, calls) = fixture_timeout(false, Duration::from_millis(300)).await;
        let mut socket = tokio::net::TcpStream::connect(host.address).await.unwrap();
        socket.write_all(format!("POST /runs HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {TOKEN}\r\nContent-Length: 100\r\n\r\n{{",host.address).as_bytes()).await.unwrap();
        let mut response = vec![];
        tokio::time::timeout(Duration::from_secs(3), socket.read_to_end(&mut response))
            .await
            .unwrap()
            .unwrap();
        assert!(String::from_utf8_lossy(&response).starts_with("HTTP/1.1 408"));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        let status = health(&client(), &format!("http://{}", host.address)).await;
        assert_eq!(status["activeRuns"], 0);
        assert_eq!(status["queuedRuns"], 0);
        host.shutdown().await.unwrap();
    }
    #[tokio::test]
    async fn recovered_artifacts_keep_the_session_boundary_headers_and_verified_bytes() {
        for outside in [false, true] {
            let (home, store, host, calls) = fixture(false).await;
            let run = Uuid::new_v4().to_string();
            let id = remote_result::session_id(&run).unwrap();
            store
                .create(
                    json!({"prompt":"PUBLIC artifact fixture"}),
                    home.path(),
                    Some(&id),
                )
                .unwrap();
            let directory = store.directory(&id).unwrap().join("artifacts");
            crate::storage::private_dir(&directory).unwrap();
            let path = if outside {
                home.path().join("PUBLIC-outside.txt")
            } else {
                directory.join("PUBLIC-owned.txt")
            };
            let bytes = b"PUBLIC verified artifact";
            std::fs::write(&path, bytes).unwrap();
            let artifact_id = Uuid::new_v4().to_string();
            let result = json!({"answerText":"PUBLIC artifact answer","answerMarkdown":"PUBLIC artifact answer"});
            let descriptor = json!({"runId":run,"artifactId":artifact_id,"kind":"file","filename":"PUBLIC Dir/Café.txt.crdownload","mimeType":"TEXT/PLAIN; charset=UTF-8","byteSize":bytes.len(),"sha256":artifacts::sha256(bytes),"sourceUrlKind":"browser-download","transferStatus":"ready"});
            remote_result::save(
                &store,
                &run,
                &result,
                &result,
                &json!([{"descriptor":descriptor,"filePath":path}]),
                Phase::Result,
                &CancellationToken::new(),
            )
            .unwrap();
            let client = client();
            let url = format!("http://{}", host.address);
            let artifact = client
                .get(format!(
                    "{url}/runs/{run}/artifacts/{artifact_id}?PUBLIC=query"
                ))
                .bearer_auth(TOKEN)
                .send()
                .await
                .unwrap();
            let restored = client
                .get(format!("{url}/runs/{run}/result"))
                .bearer_auth(TOKEN)
                .send()
                .await
                .unwrap();
            if outside {
                assert_eq!(artifact.status(), 410);
                assert_eq!(restored.status(), 409);
            } else {
                assert_eq!(artifact.status(), 200);
                assert_eq!(artifact.headers()["content-type"], "text/plain");
                assert_eq!(
                    artifact.headers()["content-disposition"],
                    "attachment; filename=\"caf-.txt\""
                );
                assert_eq!(artifact.headers()["cache-control"], "no-store");
                assert_eq!(artifact.bytes().await.unwrap(), bytes.as_slice());
                assert_eq!(restored.status(), 200);
                std::fs::write(&path, b"PUBLIC changed artifact").unwrap();
                assert_eq!(
                    client
                        .get(format!("{url}/runs/{run}/result"))
                        .bearer_auth(TOKEN)
                        .send()
                        .await
                        .unwrap()
                        .status(),
                    409
                );
            }
            assert_eq!(calls.load(Ordering::SeqCst), 0);
            host.shutdown().await.unwrap();
        }
    }
    #[tokio::test]
    async fn loopback_run_retrieval_and_exact_ack_never_replay_the_producer() {
        let (_home, store, host, calls) = fixture(false).await;
        let client = client();
        let url = format!("http://{}", host.address);
        assert_eq!(
            client
                .get(format!("{url}/status"))
                .send()
                .await
                .unwrap()
                .status(),
            200
        );
        assert_eq!(
            client
                .get(format!("{url}/health"))
                .send()
                .await
                .unwrap()
                .status(),
            401
        );
        assert_eq!(
            client
                .get(format!("{url}/PUBLIC-unknown"))
                .send()
                .await
                .unwrap()
                .status(),
            404
        );
        let response=client.post(format!("{url}/runs")).bearer_auth(TOKEN).json(&json!({"prompt":"PUBLIC consultation","browserConfig":{"desiredModel":"Latest","chromePath":"PUBLIC caller override","automaticModelPriority":"invalid"}})).send().await.unwrap();
        assert_eq!(response.status(), 200);
        let wire = response.text().await.unwrap();
        let rows: Vec<Value> = wire
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let started = rows
            .iter()
            .find(|row| row["type"] == "run-started")
            .unwrap();
        let run = started["runId"].as_str().unwrap();
        let result = rows.iter().find(|row| row["type"] == "result").unwrap();
        let delivery = result["delivery"].clone();
        assert_eq!(result["result"]["answerText"], "PUBLIC café reply");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let recovered = client
            .get(format!("{url}/runs/{run}/result"))
            .bearer_auth(TOKEN)
            .send()
            .await
            .unwrap();
        assert_eq!(recovered.status(), 200);
        assert!(
            recovered
                .text()
                .await
                .unwrap()
                .contains("PUBLIC café reply")
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let mut wrong = delivery.clone();
        wrong["sha256"] = json!("PUBLIC wrong digest");
        assert_eq!(
            client
                .post(format!("{url}/runs/{run}/ack"))
                .bearer_auth(TOKEN)
                .json(&wrong)
                .send()
                .await
                .unwrap()
                .status(),
            409
        );
        assert_eq!(
            store
                .require(&remote_result::session_id(run).unwrap())
                .unwrap()["status"],
            "running"
        );
        for _ in 0..2 {
            assert_eq!(
                client
                    .post(format!("{url}/runs/{run}/ack"))
                    .bearer_auth(TOKEN)
                    .json(&delivery)
                    .send()
                    .await
                    .unwrap()
                    .status(),
                200
            );
        }
        assert_eq!(
            store
                .require(&remote_result::session_id(run).unwrap())
                .unwrap()["status"],
            "completed"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        host.shutdown().await.unwrap();
    }
    /// Target retirement and browser cleanup after an acknowledgement can outlast the
    /// client's acknowledgement timeout. The host replies once the acknowledgement is
    /// durable, whilst that work is still held, and shutdown still waits for it.
    #[tokio::test]
    async fn ack_replies_before_browser_cleanup_finishes() {
        let (_home, store, host, _calls) = fixture(false).await;
        let client = client();
        let url = format!("http://{}", host.address);
        let response = client
            .post(format!("{url}/runs"))
            .bearer_auth(TOKEN)
            .json(&json!({"prompt":"PUBLIC consultation"}))
            .send()
            .await
            .unwrap();
        let rows: Vec<Value> = response
            .text()
            .await
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let run = rows
            .iter()
            .find(|row| row["type"] == "run-started")
            .unwrap()["runId"]
            .as_str()
            .unwrap()
            .to_owned();
        let delivery = rows.iter().find(|row| row["type"] == "result").unwrap()["delivery"].clone();
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        let finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
        *remote_result::TEST_FINISH_GATE.lock().unwrap() = Some(remote_result::FinishGate {
            run: run.clone(),
            entered: entered.clone(),
            release: release.clone(),
            finished: finished.clone(),
        });
        let acknowledged = tokio::time::timeout(
            Duration::from_secs(3),
            client
                .post(format!("{url}/runs/{run}/ack"))
                .bearer_auth(TOKEN)
                .json(&delivery)
                .send(),
        )
        .await;
        // The cleanup was reached and is still held when the reply arrives.
        let reached = tokio::time::timeout(Duration::from_secs(1), entered.notified())
            .await
            .is_ok();
        let held = !finished.load(Ordering::SeqCst);
        let status = acknowledged.map(|response| response.unwrap().status());
        // Shutdown waits for the held cleanup and completes once it is released.
        let shutdown = tokio::spawn(host.shutdown());
        tokio::time::sleep(Duration::from_millis(200)).await;
        let waited = !shutdown.is_finished() && !finished.load(Ordering::SeqCst);
        release.add_permits(1);
        shutdown.await.unwrap().unwrap();
        *remote_result::TEST_FINISH_GATE.lock().unwrap() = None;
        assert_eq!(
            status.ok(),
            Some(reqwest::StatusCode::OK),
            "ACK waited for cleanup"
        );
        assert!(reached && held && waited);
        assert!(
            finished.load(Ordering::SeqCst),
            "shutdown did not drain the cleanup"
        );
        assert_eq!(
            store
                .require(&remote_result::session_id(&run).unwrap())
                .unwrap()["status"],
            "completed"
        );
    }
    #[tokio::test]
    async fn queued_admission_reserves_before_request_bodies_and_disconnect_returns_capacity() {
        let (_home, _store, host, calls) = fixture(false).await;
        let client = client();
        let url = format!("http://{}", host.address);
        let mut sockets = vec![];
        for expected in [(1, 0), (1, 1)] {
            let mut socket = tokio::net::TcpStream::connect(host.address).await.unwrap();
            socket.write_all(format!("POST /runs HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {TOKEN}\r\nContent-Length: 100\r\n\r\n{{",host.address).as_bytes()).await.unwrap();
            sockets.push(socket);
            let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
            loop {
                let status = health(&client, &url).await;
                if status["activeRuns"] == expected.0 && status["queuedRuns"] == expected.1 {
                    break;
                }
                assert!(tokio::time::Instant::now() < deadline, "{status}");
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
        let refused = client
            .post(format!("{url}/runs"))
            .bearer_auth(TOKEN)
            .json(&json!({"prompt":"PUBLIC third request"}))
            .send()
            .await
            .unwrap();
        assert_eq!(refused.status(), 503);
        assert_eq!(refused.headers()["retry-after"], "60");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        drop(sockets);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            let status = health(&client, &url).await;
            if status["activeRuns"] == 0 && status["queuedRuns"] == 0 {
                break;
            }
            assert!(tokio::time::Instant::now() < deadline, "{status}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        host.shutdown().await.unwrap();
    }
    #[tokio::test]
    async fn producer_failure_after_capture_recovers_the_capture_without_dispatch() {
        let (_home, store, host, calls) = fixture(true).await;
        let client = client();
        let url = format!("http://{}", host.address);
        let wire = client
            .post(format!("{url}/runs"))
            .bearer_auth(TOKEN)
            .json(&json!({"prompt":"PUBLIC consultation"}))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        let rows: Vec<Value> = wire
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let run = rows
            .iter()
            .find(|row| row["type"] == "run-started")
            .unwrap()["runId"]
            .as_str()
            .unwrap();
        assert!(rows.iter().any(|row| row["type"] == "error"));
        let recovered = client
            .get(format!("{url}/runs/{run}/result"))
            .bearer_auth(TOKEN)
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        let rows: Vec<Value> = recovered
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(rows[0]["type"], "log");
        let delivery = rows.iter().find(|row| row["type"] == "result").unwrap()["delivery"].clone();
        assert_eq!(
            client
                .post(format!("{url}/runs/{run}/ack"))
                .bearer_auth(TOKEN)
                .json(&delivery)
                .send()
                .await
                .unwrap()
                .status(),
            200
        );
        assert_eq!(
            store
                .require(&remote_result::session_id(run).unwrap())
                .unwrap()["status"],
            "partial"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        host.shutdown().await.unwrap();
    }
    #[test]
    fn callers_cannot_choose_host_profile_executable_debugger_or_cookie_fields() {
        let config = client_browser_config(
            &json!({"desiredModel":"Latest","thinkingTime":"pro","keepBrowser":true,"automaticModelPriority":false,
            "chromePath":"PUBLIC executable","userDataDir":"PUBLIC profile","remoteChrome":{"host":"PUBLIC","port":9222},"maxConcurrentTabs":99,"cookieSync":true,"automaticModelPriorityIgnored":"PUBLIC"}),
        );
        assert_eq!(
            config,
            json!({"desiredModel":"Latest","thinkingTime":"pro","keepBrowser":true,"automaticModelPriority":false})
        );
        assert_eq!(
            client_browser_config(&json!({"automaticModelPriority":"true"})),
            json!({})
        );
    }
    async fn start(driver: RunDriver) -> (tempfile::TempDir, SessionStore, RemoteHost) {
        let home = tempfile::tempdir().unwrap();
        let store = SessionStore::new(home.path().join(".oracle"));
        let host = RemoteHost::start(
            store.clone(),
            HostOptions {
                address: "127.0.0.1:0".parse().unwrap(),
                token: TOKEN.into(),
                max_concurrent_runs: Some(1),
                max_queued_runs: Some(1),
                max_browser_tabs: 1,
            },
            driver,
        )
        .await
        .unwrap();
        (home, store, host)
    }
    /// A run with one optional owned artifact; the first call waits for `gate`.
    fn gated(gate: Arc<tokio::sync::Notify>, artifact: bool) -> RunDriver {
        let calls = Arc::new(AtomicUsize::new(0));
        Arc::new(move |context| {
            let gate = gate.clone();
            let first = calls.fetch_add(1, Ordering::SeqCst) == 0;
            Box::pin(async move {
                if first {
                    gate.notified().await;
                }
                let id = remote_result::session_id(&context.run_id)?;
                context.store.create(
                    json!({"prompt":"PUBLIC remote fixture"}),
                    context.store.home(),
                    Some(&id),
                )?;
                let mut artifacts = json!([]);
                if artifact {
                    let directory = context.store.directory(&id)?.join("artifacts");
                    crate::storage::private_dir(&directory)?;
                    let path = directory.join("PUBLIC-owned.txt");
                    let bytes = b"PUBLIC verified artifact";
                    std::fs::write(&path, bytes)?;
                    let descriptor = json!({"runId":context.run_id,"artifactId":Uuid::new_v4().to_string(),"kind":"file","filename":"PUBLIC-owned.txt","mimeType":"text/plain","byteSize":bytes.len(),"sha256":artifacts::sha256(bytes),"sourceUrlKind":"browser-download","transferStatus":"ready"});
                    artifacts = json!([{"descriptor":descriptor,"filePath":path}]);
                }
                let result = json!({"answerText":"PUBLIC reply","answerMarkdown":"PUBLIC reply"});
                Ok(ProducedRun {
                    captured: result.clone(),
                    result,
                    artifacts,
                })
            })
        })
    }
    async fn post(client: &reqwest::Client, url: &str, body: Value) -> reqwest::Response {
        client
            .post(format!("{url}/runs"))
            .bearer_auth(TOKEN)
            .json(&body)
            .send()
            .await
            .unwrap()
    }
    fn lines(body: &str) -> Vec<Value> {
        body.lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
    #[tokio::test]
    async fn model_refusals_keep_the_policy_reason_and_malformed_bodies_stay_invalid() {
        let gate = Arc::new(tokio::sync::Notify::new());
        let (_home, _store, host) = start(gated(gate, false)).await;
        let client = client();
        let url = format!("http://{}", host.address);
        for (body, expected) in [
            (
                json!({"prompt":"PUBLIC","options":{"model":"gpt-4o"}}),
                json!({"error":"unsupported_browser_provider","message":"Industrious only permits Latest (GPT-6) for new dispatch. Pro is first; verified Extra High fallback remains on GPT-6."}),
            ),
            (
                json!({"prompt":"PUBLIC","browserConfig":{"desiredModel":"gpt-4o"}}),
                json!({"error":"unsupported_browser_provider","message":"Industrious only permits Latest (GPT-6) for new dispatch. Pro is first; verified Extra High fallback remains on GPT-6."}),
            ),
            (json!({"options":{}}), json!({"error":"invalid_request"})),
        ] {
            let response = post(&client, &url, body).await;
            assert_eq!(response.status(), 400);
            assert_eq!(response.json::<Value>().await.unwrap(), expected);
        }
        host.shutdown().await.unwrap();
    }
    #[tokio::test]
    async fn queued_runs_hear_their_position_and_ready_artifacts_are_announced() {
        let gate = Arc::new(tokio::sync::Notify::new());
        let (_home, _store, host) = start(gated(gate.clone(), true)).await;
        let client = client();
        let url = format!("http://{}", host.address);
        let first = post(&client, &url, json!({"prompt":"PUBLIC first"})).await;
        let second = post(&client, &url, json!({"prompt":"PUBLIC second"})).await;
        gate.notify_one();
        let first = lines(&first.text().await.unwrap());
        let second = lines(&second.text().await.unwrap());
        assert_eq!(first[0]["type"], "run-started", "{first:?}");
        assert_eq!(
            second[0],
            json!({"type":"log","message":"[serve] Waiting for a browser slot (position 1; 1 active)."})
        );
        assert_eq!(second[1]["type"], "run-started");
        for events in [&first, &second] {
            let ready = events
                .iter()
                .position(|event| event["type"] == "artifact-ready")
                .unwrap();
            assert_eq!(
                events[ready - 1],
                json!({"type":"log","message":"[browser] 1 artifact(s) ready for bridge transfer. If no cloud-local artifact path appears, upgrade both Industrious bridge endpoints or copy the file manually from the Windows browser host."})
            );
        }
        host.shutdown().await.unwrap();
    }
    #[tokio::test]
    async fn artifact_route_distinguishes_missing_unavailable_and_unverified_files() {
        let gate = Arc::new(tokio::sync::Notify::new());
        gate.notify_one();
        let (_home, store, host) = start(gated(gate, true)).await;
        let client = client();
        let url = format!("http://{}", host.address);
        let events = lines(
            &post(&client, &url, json!({"prompt":"PUBLIC first"}))
                .await
                .text()
                .await
                .unwrap(),
        );
        let descriptor = &events
            .iter()
            .find(|event| event["type"] == "artifact-ready")
            .unwrap()["artifact"];
        let (run, artifact) = (
            descriptor["runId"].as_str().unwrap(),
            descriptor["artifactId"].as_str().unwrap(),
        );
        let path = store
            .directory(&remote_result::session_id(run).unwrap())
            .unwrap()
            .join("artifacts/PUBLIC-owned.txt");
        let fetch = |artifact: String| {
            let request = client
                .get(format!("{url}/runs/{run}/artifacts/{artifact}"))
                .bearer_auth(TOKEN);
            async move {
                let response = request.send().await.unwrap();
                (
                    response.status().as_u16(),
                    response.json::<Value>().await.unwrap_or(Value::Null)["error"].clone(),
                )
            }
        };
        assert_eq!(fetch(artifact.into()).await.0, 200);
        assert_eq!(
            fetch(Uuid::new_v4().to_string()).await,
            (404, json!("artifact_not_found"))
        );
        std::fs::write(&path, b"PUBLIC changed artifact").unwrap();
        assert_eq!(
            fetch(artifact.into()).await,
            (410, json!("artifact_unavailable"))
        );
        std::fs::write(&path, b"").unwrap();
        assert_eq!(
            fetch(artifact.into()).await,
            (410, json!("artifact_unavailable"))
        );
        std::fs::remove_file(&path).unwrap();
        assert_eq!(
            fetch(artifact.into()).await,
            (410, json!("artifact_unavailable"))
        );
        // A sparse file needs no disk space on Unix; NTFS would allocate it.
        #[cfg(unix)]
        {
            std::fs::File::create(&path)
                .unwrap()
                .set_len(MAX_ARTIFACT_BYTES + 1)
                .unwrap();
            assert_eq!(
                fetch(artifact.into()).await,
                (413, json!("artifact_too_large"))
            );
        }
        host.shutdown().await.unwrap();
    }
}
