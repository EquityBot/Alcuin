//! Remote result reception, verified publication and durable acknowledgement.
use crate::{policy, remote_json, remote_result, remote_transfer, sessions::SessionStore};
use anyhow::{Context, Result, bail, ensure};
use futures_util::{StreamExt, future::BoxFuture};
use serde_json::{Value, json, value::RawValue};
use std::{path::PathBuf, sync::Arc, time::Duration};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub type Persist = Arc<dyn Fn(Value) -> BoxFuture<'static, Result<()>> + Send + Sync>;
pub type Log = Arc<dyn Fn(&str) -> Result<()> + Send + Sync>;
#[derive(Clone, Default)]
pub struct Hooks {
    pub runtime_hint: Option<Persist>,
    pub artifact_receipt: Option<Persist>,
    pub result_captured: Option<Persist>,
    pub log: Option<Log>,
}
pub struct Request {
    pub payload: Value,
    pub recover_run_id: Option<String>,
    pub session_id: Option<String>,
    pub image_destination: Option<PathBuf>,
    pub cancel: CancellationToken,
    pub require_cancellation: bool,
    pub verbose: bool,
}
#[derive(Clone)]
pub struct RemoteClient {
    http: reqwest::Client,
    endpoint: reqwest::Url,
    host: String,
    token: Option<String>,
    store: SessionStore,
}
impl RemoteClient {
    pub fn new(host: &str, token: Option<String>, store: SessionStore) -> Result<Self> {
        let host = policy::trim(host);
        let (hostname, port_text) = if let Some(rest) = host.strip_prefix('[') {
            let (hostname, port) = rest
                .split_once("]:")
                .context("IPv6 must use [host]:port notation")?;
            (format!("[{}]", policy::trim(hostname)), port)
        } else {
            let (hostname, port) = host.rsplit_once(':').context("Expected host:port")?;
            ensure!(
                !hostname.contains(':'),
                "IPv6 must use [host]:port notation"
            );
            (policy::trim(hostname).to_owned(), port)
        };
        ensure!(
            !hostname.is_empty() && !hostname.contains(['/', '\\', '?', '#', '@']),
            "Invalid remote host"
        );
        // The maintained parser uses parseInt for an unbracketed port.
        let port_text = policy::trim(port_text);
        let digits = port_text
            .strip_prefix('+')
            .unwrap_or(port_text)
            .chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>();
        let port = digits
            .parse::<u16>()
            .context("Expected a port between 1 and 65535")?;
        ensure!(port > 0, "Expected a port between 1 and 65535");
        if host.starts_with('[') {
            ensure!(digits == port_text, "Invalid bracketed IPv6 port");
        }
        let endpoint = reqwest::Url::parse(&format!("http://{hostname}:{port}/"))?;
        let http = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .build()?;
        Ok(Self {
            http,
            endpoint,
            host: host.into(),
            token,
            store,
        })
    }
    fn auth(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if let Some(token) = &self.token {
            request.bearer_auth(token)
        } else {
            request
        }
    }
    async fn send(
        &self,
        request: reqwest::RequestBuilder,
        cancel: &CancellationToken,
    ) -> Result<reqwest::Response> {
        tokio::select! { biased; _=cancel.cancelled()=>bail!("Browser run cancelled before the request completed"),response=self.auth(request).send()=>Ok(response?) }
    }
    pub async fn health(&self, cancel: &CancellationToken) -> Result<Value> {
        let operation=async {
            let response=tokio::time::timeout(Duration::from_secs(5),self.send(self.http.get(self.endpoint.join("health")?),cancel)).await.map_err(|_|anyhow::anyhow!("timeout after 5000ms"))??;
            let status=response.status();
            let bytes=limited_body_idle(response,cancel,64*1024,Some(Duration::from_secs(5))).await?;
            let value=serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null);
            if status!=reqwest::StatusCode::OK || !value.is_object() {
                // Reference extractErrorMessage: trimmed error or body, else `HTTP <status>`.
                let body=String::from_utf8_lossy(&bytes);
                return Ok(json!({"ok":false,"statusCode":status.as_u16(),"error":if status==reqwest::StatusCode::NOT_FOUND {
                    "remote host does not expose /health (upgrade oracle on the host and retry)".into()
                } else {value["error"].as_str().map(policy::trim).filter(|error|!error.is_empty())
                    .or(Some(policy::trim(&body)).filter(|body|!body.is_empty()))
                    .map_or_else(||format!("HTTP {}",status.as_u16()),str::to_owned)}}));
            }
            let mut result=json!({"ok":value["ok"]==true,"statusCode":status.as_u16()});
            if value["version"].is_string() {result["version"]=value["version"].clone();}
            if value["uptimeSeconds"].is_number() {result["uptimeSeconds"]=value["uptimeSeconds"].clone();}
            let raw=&value["capabilities"];
            let positive_safe=|value:&Value|value.as_f64().is_some_and(|n|n.is_finite() && n.fract()==0.0 && n>0.0 && n<=9_007_199_254_740_991.0);
            if raw["artifactTransfer"]==true && positive_safe(&raw["artifactProtocolVersion"]) && positive_safe(&raw["maxArtifactBytes"]) {
                let mut caps=json!({"artifactTransfer":true,"artifactProtocolVersion":raw["artifactProtocolVersion"],
                    "maxArtifactBytes":raw["maxArtifactBytes"].as_f64().unwrap_or(0.0).min(crate::remote_host::MAX_ARTIFACT_BYTES as f64)});
                for field in ["deferredFallbackBundling","runCancellation","generatedImages","durableResultRecovery"] {
                    if raw[field]==true {caps[field]=json!(true);}
                }
                result["capabilities"]=caps;
            }
            Ok::<_,anyhow::Error>(result)
        }.await;
        Ok(operation.unwrap_or_else(|error| json!({"ok":false,"error":error.to_string()})))
    }
    pub async fn run(&self, request: Request, hooks: Hooks) -> Result<Value> {
        let cancel = request.cancel.child_token();
        ensure!(
            !cancel.is_cancelled(),
            "Browser run cancelled before the request was sent"
        );
        ensure!(
            request.payload["browserConfig"]["researchMode"] != "search",
            "Web Search is a local browser pilot; --remote-host does not negotiate this capability yet. Use local Chrome or --browser-attach-running."
        );
        ensure!(
            request.payload["browserConfig"]["researchMode"] != "deep",
            "{}",
            crate::remote_driver::REMOTE_DEEP_RESEARCH
        );
        if let Some(id) = &request.recover_run_id {
            remote_result::session_id(id)?;
        }
        if let Some(id) = &request.session_id {
            crate::sessions::assert_id(id)?;
        }
        // Reference order: model policy, then the health preflight, images before cancellation.
        if request.recover_run_id.is_none() {
            policy::assert_model_allowed(request.payload["options"]["model"].as_str())?;
            policy::assert_model_allowed(
                request.payload["browserConfig"]["desiredModel"].as_str(),
            )?;
        }
        if request.require_cancellation || request.image_destination.is_some() {
            let health = self.health(&cancel).await?;
            ensure!(
                !cancel.is_cancelled(),
                "Browser run cancelled before the request was sent"
            );
            if request.image_destination.is_some() {
                ensure!(
                    health["ok"] == true
                        && health["capabilities"]["generatedImages"] == true
                        && health["capabilities"]["artifactProtocolVersion"].as_f64() == Some(1.0),
                    "Remote host cannot capture and transfer generated images; upgrade Industrious on the host and retry. The image request was not sent."
                );
            }
            if request.require_cancellation {
                ensure!(
                    health["capabilities"]["runCancellation"] == true,
                    "Remote host does not support run cancellation; upgrade the host before using an AbortSignal."
                );
            }
        }
        let outgoing = if let Some(id) = &request.recover_run_id {
            self.http
                .get(self.endpoint.join(&format!("runs/{id}/result"))?)
        } else {
            self.http
                .post(self.endpoint.join("runs")?)
                .json(&request.payload)
        };
        let response = self.send(outgoing, &cancel).await?;
        if response.status() != reqwest::StatusCode::OK {
            bail!("{}", response_error(response, &cancel).await?);
        }
        let (sender, receiver) = mpsc::channel(16);
        let receive = async {
            let result = self
                .receive(response, sender, &request, &hooks, &cancel)
                .await;
            if result.is_err() {
                cancel.cancel();
            }
            result
        };
        let transfer = self.transfers(receiver, &request, &hooks, &cancel);
        // These futures share a scope. Failure waits for every issued persistence callback.
        let (received, transferred) = tokio::join!(receive, transfer);
        let (received, transferred) = match (received, transferred) {
            (Err(primary), Err(persistence)) => {
                return Err(primary.context(format!(
                    "Browser evidence persistence also failed: {persistence:#}"
                )));
            }
            (Err(error), _) | (_, Err(error)) => return Err(error),
            (Ok(received), Ok(transferred)) => (received, transferred),
        };
        ensure!(!cancel.is_cancelled(), "Browser run cancelled");
        let raw = serde_json::from_str::<&RawValue>(&received.result)?;
        if let Some(delivery) = &received.delivery {
            let id = received
                .run_id
                .as_deref()
                .context("Remote delivery has no run identity")?;
            let wire = format!(
                "{{\"runId\":{},\"result\":{},\"artifacts\":[{}]}}",
                serde_json::to_string(id)?,
                raw.get(),
                received.descriptors.join(",")
            );
            ensure!(
                delivery["version"] == 1
                    && delivery["runId"] == id
                    && delivery["sha256"].as_str()
                        == Some(crate::artifacts::sha256(&remote_json::stringify(&wire)?).as_str()),
                "Remote delivery identity or digest does not match its result and artifacts"
            );
        }
        let original = remote_json::native_projection(raw)?;
        ensure!(
            original.is_object(),
            "Remote browser result must be an object"
        );
        if request.image_destination.is_some() {
            let images = transferred
                .artifacts
                .iter()
                .filter(|artifact| artifact["kind"] == "image")
                .count();
            ensure!(
                images > 0
                    && images == received.image_count
                    && !registration_failed(&original, true),
                "Remote image output was not fully delivered; recover the saved host result without resubmitting it"
            );
        }
        let mut result = original.clone();
        merge(&mut result, "artifacts", &transferred.artifacts, None);
        merge(
            &mut result,
            "savedFiles",
            &transferred.artifacts,
            Some("file"),
        );
        merge(
            &mut result,
            "savedImages",
            &transferred.artifacts,
            Some("image"),
        );
        let mut warnings = original["warnings"].as_array().cloned().unwrap_or_default();
        warnings.extend(received.failures.iter().cloned());
        warnings.extend(transferred.warnings.iter().cloned());
        if !warnings.is_empty() {
            result["warnings"] = json!(warnings);
        }
        if request.recover_run_id.is_none() {
            let model =
                serde_json::from_value::<policy::ModelEvidence>(result["modelSelection"].clone())
                    .ok();
            let thinking = serde_json::from_value::<policy::ThinkingEvidence>(
                result["thinkingSelection"].clone(),
            )
            .ok();
            policy::assert_browser_completion(
                model.as_ref(),
                thinking.as_ref(),
                result["effectiveModel"].as_str(),
            )?;
        }
        if let Some(persist) = &hooks.result_captured {
            // Once issued, result persistence finishes even if the caller cancels.
            let persisted = persist(result.clone()).await;
            match (persisted, cancel.is_cancelled()) {
                (Err(error), true) => {
                    return Err(
                        error.context("Browser run cancelled; result persistence also failed")
                    );
                }
                (Err(error), false) => return Err(error),
                (Ok(()), true) => bail!("Browser run cancelled"),
                (Ok(()), false) => {}
            }
            if let Some(delivery) = received.delivery.filter(|_| {
                received.failures.is_empty()
                    && transferred.failures == 0
                    && !registration_failed(&original, false)
            }) {
                self.acknowledge(delivery, &cancel).await?;
            }
        }
        ensure!(!cancel.is_cancelled(), "Browser run cancelled");
        Ok(result)
    }
    async fn acknowledge(&self, delivery: Value, cancel: &CancellationToken) -> Result<()> {
        let id = delivery["runId"]
            .as_str()
            .context("Remote acknowledgement has no run")?;
        let url = self.endpoint.join(&format!("runs/{id}/ack"))?;
        // The host replies once the acknowledgement is durable; 15 s covers one 10 s
        // session-lock wait there.
        tokio::time::timeout(Duration::from_secs(15),async {
            let response=self.send(self.http.post(url).json(&delivery),cancel).await?;
            let status=response.status();
            // Drain the response before accepting the acknowledgement.
            limited_body(response,cancel,64*1024).await?;
            ensure!(status==reqwest::StatusCode::OK,"Remote handoff acknowledgement failed; recover the saved run without submitting it again");
            Ok::<_,anyhow::Error>(())
        }).await.context("Remote handoff acknowledgement timed out; its host result remains recoverable")?
    }
    async fn receive(
        &self,
        response: reqwest::Response,
        sender: mpsc::Sender<Transfer>,
        request: &Request,
        hooks: &Hooks,
        cancel: &CancellationToken,
    ) -> Result<Received> {
        let mut stream = response.bytes_stream();
        let mut buffer = vec![];
        let mut received = Received {
            run_id: request.recover_run_id.clone(),
            ..Default::default()
        };
        loop {
            let next = tokio::select! { biased; _=cancel.cancelled()=>bail!("Browser run cancelled"),next=stream.next()=>next };
            let Some(bytes) = next else {
                break;
            };
            buffer.extend_from_slice(&bytes?);
            let mut consumed = 0;
            while let Some(relative) = buffer[consumed..].iter().position(|byte| *byte == b'\n') {
                let end = consumed + relative;
                let line = String::from_utf8_lossy(&buffer[consumed..end]);
                let line = policy::trim(&line);
                consumed = end + 1;
                if line.is_empty() {
                    continue;
                }
                let raw = serde_json::from_str::<&RawValue>(line)
                    .context("Failed to parse remote event")?;
                let event = remote_json::native_projection(raw)?;
                match event["type"].as_str() {
                    Some("run-started") => {
                        let id = event["runId"]
                            .as_str()
                            .context("Invalid remote run identity")?;
                        remote_result::session_id(id)?;
                        received.run_id = Some(id.into());
                        if let Some(log) = &hooks.log {
                            log(&format!("[browser] Remote result recovery ID: {id}"))?;
                        }
                        if let Some(persist) = &hooks.runtime_hint {
                            persist(json!({"remoteRunId":id,"remoteHost":self.host})).await?;
                        }
                    }
                    Some("log") => {
                        if let Some(log) = &hooks.log {
                            log(event["message"].as_str().unwrap_or(""))?;
                        }
                    }
                    Some("error") => bail!(
                        "{}",
                        event["message"]
                            .as_str()
                            .unwrap_or("Remote browser run failed")
                    ),
                    Some("artifact-progress") => {
                        if request.verbose
                            && let Some(log) = &hooks.log
                        {
                            // Reference template: both counts present, null included.
                            let text = |value: &Value| {
                                value
                                    .as_str()
                                    .map_or_else(|| value.to_string(), str::to_owned)
                            };
                            let bytes = match (event.get("receivedBytes"), event.get("totalBytes"))
                            {
                                (Some(received), Some(total)) => {
                                    format!(" {}/{} bytes", text(received), text(total))
                                }
                                _ => String::new(),
                            };
                            log(&format!(
                                "[browser] Artifact {} {}{bytes}",
                                event["artifactId"].as_str().unwrap_or(""),
                                event["phase"].as_str().unwrap_or("")
                            ))?;
                        }
                    }
                    Some("artifact-ready") => {
                        let artifact = &event["artifact"];
                        if let Err(error) = remote_transfer::descriptor(artifact) {
                            received
                                .failures
                                .push(failure(artifact, None, &error, hooks));
                            continue;
                        }
                        let descriptor = remote_json::field(raw, "artifact")?
                            .context("Missing remote artifact")?;
                        received.descriptors.push(descriptor.get().into());
                        let image = artifact["kind"] == "image";
                        let image_index = received.image_count;
                        if image {
                            received.image_count += 1;
                        }
                        let transfer = Transfer {
                            descriptor: artifact.clone(),
                            image_index,
                        };
                        tokio::select! { biased; _=cancel.cancelled()=>bail!("Browser run cancelled"), result=sender.send(transfer)=>result.context("Remote artifact receiver stopped")? };
                    }
                    Some("result") => {
                        received.result = remote_json::field(raw, "result")?
                            .context("Missing remote browser result")?
                            .get()
                            .into();
                        received.delivery = event
                            .get("delivery")
                            .filter(|value| !value.is_null())
                            .cloned();
                    }
                    _ => {}
                }
            }
            buffer.drain(..consumed);
        }
        // The maintained wire reader ignores an unterminated final line.
        ensure!(
            !received.result.is_empty(),
            "Remote browser run completed without a result"
        );
        Ok(received)
    }
    async fn transfers(
        &self,
        mut receiver: mpsc::Receiver<Transfer>,
        request: &Request,
        hooks: &Hooks,
        cancel: &CancellationToken,
    ) -> Result<Transfers> {
        let mut output = Transfers::default();
        loop {
            let next =
                tokio::select! { biased; _=cancel.cancelled()=>break,next=receiver.recv()=>next };
            let Some(transfer) = next else {
                break;
            };
            let result = async {
                let id = request
                    .session_id
                    .as_deref()
                    .or(transfer.descriptor["runId"].as_str())
                    .context("Missing receiver session")?;
                let directory = self.store.directory(id)?.join("artifacts");
                let preferred = if transfer.descriptor["kind"] == "image" {
                    request.image_destination.as_deref()
                } else {
                    None
                };
                let destination = remote_transfer::destination(
                    &directory,
                    &transfer.descriptor,
                    preferred,
                    transfer.image_index,
                )?;
                if let Some(log) = &hooks.log {
                    let _ = log(&format!(
                        "[browser] Transferring artifact {} from bridge host...",
                        crate::browser_output::filename(
                            transfer.descriptor["filename"].as_str().unwrap_or("")
                        )
                    ));
                }
                remote_transfer::transfer(
                    &self.http,
                    &self.endpoint,
                    self.token.as_deref(),
                    &transfer.descriptor,
                    &destination,
                    preferred.is_none(),
                    cancel,
                )
                .await
            }
            .await;
            match result {
                Ok(transferred) => {
                    let artifact = transferred.artifact;
                    output.artifacts.push(artifact.clone());
                    // Register before running log code or checking cancellation again.
                    if let Some(persist) = &hooks.artifact_receipt
                        && let Err(error) = persist(artifact.clone()).await
                    {
                        cancel.cancel();
                        return Err(error.context("Browser artifact receipt persistence failed; published output is retained"));
                    }
                    if let Some(warning) = transferred.warning {
                        output.warnings.push(warning);
                    }
                    if let Some(log) = &hooks.log {
                        let _ = log(&format!(
                            "[browser] Transferred artifact to {}",
                            artifact["path"].as_str().unwrap_or("")
                        ));
                    }
                }
                Err(error) => {
                    if cancel.is_cancelled() {
                        break;
                    }
                    output.failures += 1;
                    output.warnings.push(failure(
                        &transfer.descriptor,
                        transfer.descriptor["kind"].as_str(),
                        &error,
                        hooks,
                    ));
                }
            }
        }
        Ok(output)
    }
}
#[derive(Default)]
struct Received {
    run_id: Option<String>,
    result: String,
    delivery: Option<Value>,
    descriptors: Vec<String>,
    failures: Vec<Value>,
    image_count: usize,
}
struct Transfer {
    descriptor: Value,
    image_index: usize,
}
#[derive(Default)]
struct Transfers {
    artifacts: Vec<Value>,
    warnings: Vec<Value>,
    failures: usize,
}
/// The reference reportFailure: the fallback warning, also logged as it happens.
fn failure(descriptor: &Value, kind: Option<&str>, error: &anyhow::Error, hooks: &Hooks) -> Value {
    let filename = crate::browser_output::filename(descriptor["filename"].as_str().unwrap_or(""));
    let message = format!(
        "Industrious captured the browser text response, but bridge artifact transfer failed for {filename}. Open the ChatGPT browser on the bridge host, download the ZIP/file shown in the current response, and copy it to a cloud-readable path. Reason: {error:#}"
    );
    if let Some(log) = &hooks.log {
        let _ = log(&format!("[browser] {message}"));
    }
    let mut warning =
        json!({"code":"remote-artifact-transfer-failed","severity":"warning","message":message});
    if let Some(kind) = kind {
        warning["details"] = json!({"artifactKind":kind});
    }
    warning
}
fn registration_failed(result: &Value, image_only: bool) -> bool {
    result["warnings"].as_array().is_some_and(|warnings| {
        warnings.iter().any(|warning| {
            warning["code"] == "remote-image-registration-failed"
                || !image_only && warning["code"] == "remote-artifact-registration-failed"
        })
    })
}
fn merge(result: &mut Value, field: &str, additions: &[Value], kind: Option<&str>) {
    let mut values = result[field].as_array().cloned().unwrap_or_default();
    for value in additions
        .iter()
        .filter(|value| kind.is_none_or(|kind| value["kind"] == kind))
    {
        if let Some(index) = values
            .iter()
            .position(|existing| existing["path"] == value["path"])
        {
            values[index] = value.clone();
        } else {
            values.push(value.clone());
        }
    }
    if !values.is_empty() {
        result[field] = json!(values);
    }
}
async fn limited_body(
    response: reqwest::Response,
    cancel: &CancellationToken,
    limit: usize,
) -> Result<Vec<u8>> {
    limited_body_idle(response, cancel, limit, None).await
}
async fn limited_body_idle(
    response: reqwest::Response,
    cancel: &CancellationToken,
    limit: usize,
    idle: Option<Duration>,
) -> Result<Vec<u8>> {
    let mut stream = response.bytes_stream();
    let mut body = vec![];
    loop {
        let read = async {
            if let Some(idle) = idle {
                Ok(tokio::time::timeout(idle, stream.next()).await?)
            } else {
                Ok::<_, anyhow::Error>(stream.next().await)
            }
        };
        let next = tokio::select! { biased; _=cancel.cancelled()=>bail!("Browser run cancelled"),next=read=>next? };
        let Some(bytes) = next else {
            break;
        };
        let bytes = bytes?;
        ensure!(
            body.len()
                .checked_add(bytes.len())
                .is_some_and(|size| size <= limit),
            "Remote response exceeded its limit"
        );
        body.extend_from_slice(&bytes);
    }
    Ok(body)
}
async fn response_error(response: reqwest::Response, cancel: &CancellationToken) -> Result<String> {
    let status = response.status();
    let bytes = limited_body(response, cancel, 64 * 1024).await?;
    if let Ok(value) = serde_json::from_slice::<Value>(&bytes)
        && let Some(error) = value["error"].as_str()
    {
        return Ok(error.into());
    }
    if !bytes.is_empty() {
        return Ok(String::from_utf8_lossy(&bytes).into());
    }
    Ok(format!("Remote host responded with status {status}"))
}
#[cfg(test)]
#[path = "remote_client_tests.rs"]
mod tests;
