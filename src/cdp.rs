//! Raw CDP transport. Closing a connection never closes a browser or target.
use anyhow::{Context, Result, bail};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    sync::{broadcast, mpsc, oneshot},
    time::Instant,
};
use tokio_tungstenite::{
    WebSocketStream, connect_async_with_config,
    tungstenite::{Message, protocol::WebSocketConfig},
};
use tokio_util::sync::CancellationToken;

/// A page read that gave no usable result: the renderer did not reply in time, or the
/// expression threw something other than the privacy guard's refusal.
#[derive(Debug)]
pub(crate) struct PageReadStalled(pub(crate) String);
impl std::fmt::Display for PageReadStalled {
    fn fmt(&self, output: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        output.write_str(&self.0)
    }
}
impl std::error::Error for PageReadStalled {}

struct Command {
    id: u64,
    message: Value,
    reply: oneshot::Sender<std::result::Result<Value, String>>,
    cancel: CancellationToken,
    deadline: Instant,
}
struct Connection {
    commands: mpsc::Sender<Command>,
    events: broadcast::Sender<Value>,
    next: AtomicU64,
    shutdown: CancellationToken,
    finished: CancellationToken,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Connection {
    fn drop(&mut self) {
        self.shutdown.cancel();
        self.task.abort();
    }
}
#[derive(Clone)]
pub struct Cdp {
    connection: Arc<Connection>,
    session: Option<String>,
}
fn websocket_limits() -> WebSocketConfig {
    WebSocketConfig::default()
        .max_message_size(Some(2 * 1024 * 1024))
        .max_frame_size(Some(2 * 1024 * 1024))
}
impl Cdp {
    pub async fn connect(
        endpoint: &str,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<Self> {
        let url = reqwest::Url::parse(endpoint).context("Invalid CDP endpoint")?;
        anyhow::ensure!(
            matches!(url.scheme(), "ws" | "wss")
                && url.username().is_empty()
                && url.password().is_none(),
            "CDP endpoint must use WebSocket without embedded credentials."
        );
        let socket = tokio::select! {biased;_=cancel.cancelled()=>bail!("CDP connection was cancelled."),result=tokio::time::timeout(timeout,connect_async_with_config(endpoint,Some(websocket_limits()),false))=>result.map_err(|_|anyhow::anyhow!("CDP connection timed out."))?.map_err(|_|anyhow::anyhow!("CDP endpoint is unavailable."))?.0};
        Ok(Self::from_socket(
            socket,
            timeout.min(Duration::from_secs(30)),
        ))
    }
    pub(crate) fn from_socket<S: AsyncRead + AsyncWrite + Unpin + Send + 'static>(
        socket: WebSocketStream<S>,
        io_timeout: Duration,
    ) -> Self {
        let (mut writer, mut reader) = socket.split();
        let (commands, mut requests) = mpsc::channel::<Command>(64);
        let (events, _) = broadcast::channel(256);
        let shutdown = CancellationToken::new();
        let stop = shutdown.clone();
        let notifications = events.clone();
        let finished = CancellationToken::new();
        let done = finished.clone();
        let task = tokio::spawn(async move {
            let mut pending = std::collections::BTreeMap::new();
            loop {
                pending.retain(
                    |_, reply: &mut oneshot::Sender<std::result::Result<Value, String>>| {
                        !reply.is_closed()
                    },
                );
                tokio::select! {biased;
                    // Dropping the transport is bounded even if the peer cannot read a Close frame.
                    _=stop.cancelled()=>break,
                    command=requests.recv()=>{
                        let Some(command)=command else{break};
                        if command.reply.is_closed()||command.cancel.is_cancelled()||Instant::now()>=command.deadline {
                            let _=command.reply.send(Err("CDP command expired or was cancelled before dispatch.".into()));continue;
                        }
                        if pending.len()>=64{let _=command.reply.send(Err("CDP pending command capacity is full; command was not sent.".into()));continue;}
                        let text=match serde_json::to_string(&command.message){Ok(text) if text.len()<=2*1024*1024=>text,_=>{let _=command.reply.send(Err("Invalid or oversized CDP request; command was not sent.".into()));continue;}};
                        pending.insert(command.id,command.reply);
                        let deadline=command.deadline.min(Instant::now()+io_timeout);
                        let sent=tokio::select!{biased;_=stop.cancelled()=>false,_=command.cancel.cancelled()=>false,result=tokio::time::timeout_at(deadline,writer.send(Message::Text(text.into())))=>matches!(result,Ok(Ok(())))};
                        // A cancelled partial write invalidates the transport. Never replay it.
                        if !sent{break;}
                    },
                    response=reader.next()=>{
                        match response{
                            Some(Ok(Message::Text(text)))=>{
                                let value=match serde_json::from_str::<Value>(&text){Ok(value) if value.is_object()=>value,_=>break};
                                if let Some(id)=value["id"].as_u64(){if let Some(reply)=pending.remove(&id){let _=reply.send(Ok(value));}}
                                else{let _=notifications.send(value);}
                            },
                            Some(Ok(Message::Ping(payload)))=>{
                                let sent=tokio::select!{biased;_=stop.cancelled()=>false,result=tokio::time::timeout(io_timeout,writer.send(Message::Pong(payload)))=>matches!(result,Ok(Ok(())))};
                                if !sent{break;}
                            },
                            Some(Ok(Message::Pong(_)))=>{},
                            _=>break,
                        }
                    }
                }
            }
            requests.close();
            stop.cancel();
            for (_, reply) in pending {
                let _=reply.send(Err("CDP endpoint disconnected; command acknowledgement is uncertain. Do not replay.".into()));
            }
            while let Ok(command) = requests.try_recv() {
                let _ = command
                    .reply
                    .send(Err("CDP connection is closed; command was not sent.".into()));
            }
            drop(writer);
            drop(reader);
            done.cancel();
        });
        Self {
            connection: Arc::new(Connection {
                commands,
                events,
                next: AtomicU64::new(1),
                shutdown,
                finished,
                task,
            }),
            session: None,
        }
    }
    pub fn events(&self) -> broadcast::Receiver<Value> {
        self.connection.events.subscribe()
    }
    pub(crate) fn session_id(&self) -> Option<&str> {
        self.session.as_deref()
    }
    pub(crate) fn browser_scope(&self) -> Self {
        Self {
            connection: self.connection.clone(),
            session: None,
        }
    }
    pub(crate) fn is_open(&self) -> bool {
        !self.connection.shutdown.is_cancelled()
    }
    pub fn with_session(&self, session_id: String) -> Self {
        Self {
            connection: self.connection.clone(),
            session: Some(session_id),
        }
    }
    pub async fn call(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<Value> {
        // Native retirement remains gated. A transport attachment supplies no ownership.
        anyhow::ensure!(
            !matches!(method, "Browser.close" | "Target.closeTarget"),
            "CDP attachment does not authorise browser or target retirement; Rust ownership qualification is unfinished."
        );
        self.call_inner(method, params, timeout, cancel).await
    }
    // Only the native target owner can reach this boundary. General attachments stay read-only
    // with respect to retirement; Browser.close is never admitted here.
    pub(crate) async fn close_owned_target(
        &self,
        target: &str,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<Value> {
        // Callers must confirm exact absence after this command, even if its reply is lost.
        // Reserve at least half the retirement budget for that observation and persistence.
        let acknowledgement_timeout = (timeout / 2).min(Duration::from_secs(1));
        self.call_inner(
            "Target.closeTarget",
            json!({"targetId":target}),
            acknowledgement_timeout,
            cancel,
        )
        .await
    }
    pub fn same_attachment(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.connection, &other.connection) && self.session == other.session
    }
    async fn call_inner(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<Value> {
        if self.connection.shutdown.is_cancelled() {
            bail!("CDP connection is closed; command was not sent.");
        }
        if cancel.is_cancelled() {
            bail!("CDP command was cancelled before dispatch.");
        }
        let deadline = Instant::now() + timeout;
        let id = self.connection.next.fetch_add(1, Ordering::Relaxed);
        let (reply, answer) = oneshot::channel();
        let mut message = json!({"id":id,"method":method,"params":params});
        if let Some(session) = &self.session {
            message["sessionId"] = json!(session);
        }
        tokio::select! {biased;_=cancel.cancelled()=>bail!("CDP command was cancelled before dispatch."),_=self.connection.shutdown.cancelled()=>bail!("CDP connection is closed; command was not sent."),result=tokio::time::timeout_at(deadline,self.connection.commands.send(Command{id,message,reply,cancel:cancel.clone(),deadline}))=>result.map_err(|_|anyhow::anyhow!("CDP command queue timed out; command was not sent."))?.map_err(|_|anyhow::anyhow!("CDP connection is closed; command was not sent."))?};
        let value = tokio::select! {biased;_=cancel.cancelled()=>bail!("CDP command was cancelled after reservation; acknowledgement is uncertain. Do not replay."),result=tokio::time::timeout_at(deadline,answer)=>result.map_err(|_|PageReadStalled(format!("CDP command timed out; acknowledgement is uncertain. Do not replay. Method: {method}.")))?.map_err(|_|anyhow::anyhow!("CDP command acknowledgement is unavailable."))?.map_err(anyhow::Error::msg)?};
        if value.get("error").is_some() {
            bail!("CDP command {method} was refused by the endpoint.");
        }
        value
            .get("result")
            .cloned()
            .context("CDP command did not return a result")
    }
    pub async fn evaluate(
        &self,
        expression: &str,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<Value> {
        let result = self
            .call(
                "Runtime.evaluate",
                json!({"expression":expression,"returnByValue":true,"awaitPromise":true}),
                timeout,
                cancel,
            )
            .await?;
        if let Some(details) = result.get("exceptionDetails") {
            let failed = "Browser expression failed; result was not accepted.";
            // The page's privacy guard throws its refusal, which is final. So is an
            // exception that does not say what was thrown.
            return Err(match details["exception"]["description"].as_str() {
                Some(thrown) if !thrown.contains(crate::browser_privacy::REFUSAL) => {
                    PageReadStalled(failed.into()).into()
                }
                _ => anyhow::anyhow!(failed),
            });
        }
        result["result"]
            .get("value")
            .cloned()
            .context("Browser expression did not return a value")
    }
    pub async fn evaluate_with_arguments(
        &self,
        function: &str,
        args: &Value,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<Value> {
        anyhow::ensure!(
            args.is_array(),
            "Browser expression arguments must be an array"
        );
        let expression = format!("({function})(...{})", serde_json::to_string(args)?);
        self.evaluate(&expression, timeout, cancel).await
    }
    pub fn disconnect(&self) {
        self.connection.shutdown.cancel();
    }
    pub(crate) fn is_closed(&self) -> bool {
        self.connection.shutdown.is_cancelled() || self.connection.commands.is_closed()
    }
    pub async fn disconnect_and_wait(&self, timeout: Duration) -> Result<()> {
        self.disconnect();
        if tokio::time::timeout(timeout, self.connection.finished.cancelled())
            .await
            .is_err()
        {
            self.connection.task.abort();
            bail!("CDP transport cleanup exceeded its deadline and was aborted");
        }
        Ok(())
    }
    pub async fn attach_target(
        &self,
        target: &str,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<Self> {
        anyhow::ensure!(
            !target.is_empty()
                && target.len() <= 256
                && target
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-'),
            "Invalid CDP target identity."
        );
        let result = self
            .call(
                "Target.attachToTarget",
                json!({"targetId":target,"flatten":true}),
                timeout,
                cancel,
            )
            .await?;
        Ok(self.with_session(
            result["sessionId"]
                .as_str()
                .context("CDP target attachment has no session ID")?
                .into(),
        ))
    }
}
pub async fn endpoint_json(
    base: &str,
    path: &str,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<Value> {
    anyhow::ensure!(
        matches!(
            path,
            "/json" | "/json/list" | "/json/version" | "/json/protocol"
        ),
        "CDP endpoint inspection does not authorise target creation or retirement"
    );
    let endpoint = reqwest::Url::parse(base).context("Invalid CDP inspection endpoint")?;
    anyhow::ensure!(
        matches!(endpoint.scheme(), "http" | "https")
            && endpoint.username().is_empty()
            && endpoint.password().is_none()
            && endpoint.query().is_none()
            && endpoint.fragment().is_none(),
        "CDP inspection endpoint must use HTTP without embedded credentials or URL parameters"
    );
    let client = reqwest::Client::builder()
        .retry(reqwest::retry::never())
        .timeout(timeout)
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let url = format!("{}{path}", base.trim_end_matches('/'));
    let response = tokio::select! {_=cancel.cancelled()=>bail!("CDP endpoint inspection was cancelled."),result=client.get(url).send()=>result.map_err(|_|anyhow::anyhow!("CDP endpoint is unavailable."))?};
    anyhow::ensure!(
        response.status().is_success(),
        "CDP endpoint inspection returned HTTP {}",
        response.status().as_u16()
    );
    let mut chunks = response.bytes_stream();
    let mut bytes = vec![];
    while let Some(chunk) = tokio::select! {_=cancel.cancelled()=>bail!("CDP endpoint inspection was cancelled."),chunk=chunks.next()=>chunk}
    {
        let chunk = chunk.map_err(|_| anyhow::anyhow!("CDP endpoint transfer failed."))?;
        anyhow::ensure!(
            bytes.len() + chunk.len() <= 2 * 1024 * 1024,
            "CDP endpoint reply exceeds 2 MiB."
        );
        bytes.extend_from_slice(&chunk);
    }
    Ok(serde_json::from_slice(&bytes)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_tungstenite::tungstenite::protocol::Role;
    async fn pair(
        capacity: usize,
        timeout: Duration,
    ) -> (Cdp, WebSocketStream<tokio::io::DuplexStream>) {
        let (client, server) = tokio::io::duplex(capacity);
        let client =
            WebSocketStream::from_raw_socket(client, Role::Client, Some(websocket_limits())).await;
        let server =
            WebSocketStream::from_raw_socket(server, Role::Server, Some(websocket_limits())).await;
        (Cdp::from_socket(client, timeout), server)
    }
    #[tokio::test]
    async fn websocket_protocol_pairs_commands_and_never_replays_a_lost_acknowledgement() {
        let (client, mut server) = pair(4096, Duration::from_secs(1)).await;
        let peer = tokio::spawn(async move {
            let first = server.next().await.unwrap().unwrap();
            let request: Value = serde_json::from_str(first.to_text().unwrap()).unwrap();
            assert_eq!(request["method"], "Runtime.evaluate");
            server
                .send(Message::Text(
                    json!({"id":request["id"],"result":{"result":{"value":"PUBLIC reply"}}})
                        .to_string()
                        .into(),
                ))
                .await
                .unwrap();
            let second = server.next().await.unwrap().unwrap();
            let request: Value = serde_json::from_str(second.to_text().unwrap()).unwrap();
            assert_eq!(request["method"], "Input.dispatchKeyEvent");
            // The connection drops after one send, with no acknowledgement.
        });
        let cancel = CancellationToken::new();
        assert_eq!(
            client
                .evaluate("'PUBLIC'", Duration::from_secs(1), &cancel)
                .await
                .unwrap(),
            "PUBLIC reply"
        );
        let error = client
            .call(
                "Input.dispatchKeyEvent",
                json!({"type":"keyDown"}),
                Duration::from_secs(1),
                &cancel,
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("uncertain"));
        peer.await.unwrap();
        assert!(
            client
                .call(
                    "Input.dispatchKeyEvent",
                    json!({}),
                    Duration::from_secs(1),
                    &cancel
                )
                .await
                .unwrap_err()
                .to_string()
                .contains("not sent")
        );
        client
            .disconnect_and_wait(Duration::from_secs(1))
            .await
            .unwrap();
    }
    #[tokio::test]
    async fn blocked_websocket_write_obeys_disconnect_and_io_deadlines() {
        for disconnect in [false, true] {
            let (client, server) = pair(64, Duration::from_millis(100)).await;
            let copy = client.clone();
            let call = tokio::spawn(async move {
                copy.call(
                    "Runtime.evaluate",
                    json!({"expression":"x".repeat(128*1024)}),
                    Duration::from_secs(5),
                    &CancellationToken::new(),
                )
                .await
            });
            tokio::time::sleep(Duration::from_millis(20)).await;
            if disconnect {
                client.disconnect();
            }
            tokio::time::timeout(
                Duration::from_millis(500),
                client.connection.finished.cancelled(),
            )
            .await
            .expect("transport must terminate while peer stops reading");
            assert!(
                tokio::time::timeout(Duration::from_millis(500), call)
                    .await
                    .unwrap()
                    .unwrap()
                    .is_err()
            );
            drop(server);
        }
    }
    #[tokio::test]
    async fn blocked_websocket_write_obeys_command_cancellation() {
        let (client, server) = pair(64, Duration::from_secs(5)).await;
        let cancel = CancellationToken::new();
        let token = cancel.clone();
        let copy = client.clone();
        let call = tokio::spawn(async move {
            copy.call(
                "Runtime.evaluate",
                json!({"expression":"x".repeat(128*1024)}),
                Duration::from_secs(5),
                &token,
            )
            .await
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        cancel.cancel();
        assert!(
            tokio::time::timeout(Duration::from_millis(500), call)
                .await
                .unwrap()
                .unwrap()
                .is_err()
        );
        tokio::time::timeout(
            Duration::from_millis(500),
            client.connection.finished.cancelled(),
        )
        .await
        .unwrap();
        drop(server);
    }
    #[tokio::test]
    async fn connection_and_attachment_never_grant_retirement_authority() {
        let (client, mut server) = pair(4096, Duration::from_secs(1)).await;
        for method in ["Browser.close", "Target.closeTarget"] {
            assert!(
                client
                    .call(
                        method,
                        json!({"targetId":"FOREIGN"}),
                        Duration::from_secs(1),
                        &CancellationToken::new()
                    )
                    .await
                    .is_err()
            );
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(25), server.next())
                .await
                .is_err()
        );
        client
            .disconnect_and_wait(Duration::from_secs(1))
            .await
            .unwrap();
    }
    #[tokio::test]
    async fn dropping_last_connection_aborts_owned_task_without_waiting_for_peer() {
        let (client, mut server) = pair(64, Duration::from_secs(30)).await;
        drop(client);
        let response = tokio::time::timeout(Duration::from_millis(500), server.next())
            .await
            .expect("owned transport must close on last owner drop");
        assert!(!matches!(response, Some(Ok(Message::Text(_)))));
    }
}

#[cfg(test)]
mod pong_deadline_tests {
    use super::*;
    use tokio_tungstenite::tungstenite::protocol::Role;
    #[tokio::test]
    async fn blocked_pong_write_terminates_within_the_io_deadline() {
        let (client, server) = tokio::io::duplex(64);
        let client = Cdp::from_socket(
            WebSocketStream::from_raw_socket(client, Role::Client, None).await,
            Duration::from_millis(100),
        );
        let mut server = WebSocketStream::from_raw_socket(server, Role::Server, None).await;
        tokio::time::timeout(
            Duration::from_secs(1),
            server.send(Message::Ping(vec![7; 125].into())),
        )
        .await
        .unwrap()
        .unwrap();
        // The peer sends Ping and then stops consuming the response.
        tokio::time::timeout(
            Duration::from_millis(500),
            client.connection.finished.cancelled(),
        )
        .await
        .expect("Pong must never leave the owned CDP task blocked");
        drop(server);
    }
}

#[cfg(test)]
mod http_authority_tests {
    use super::*;
    #[tokio::test]
    async fn inspection_refuses_management_routes_before_making_any_network_request() {
        for path in [
            "/json/close/FOREIGN",
            "/json/new?https://chatgpt.com",
            "/json/activate/FOREIGN",
            "/json/list/../close/FOREIGN",
        ] {
            let error = endpoint_json(
                "invalid endpoint",
                path,
                Duration::from_secs(1),
                &CancellationToken::new(),
            )
            .await
            .unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("does not authorise target creation or retirement")
            );
        }
    }
}
