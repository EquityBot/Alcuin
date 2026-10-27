use super::*;
use std::sync::{
    Mutex,
    atomic::{AtomicUsize, Ordering},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::Notify,
    task::JoinSet,
};

const RUN: &str = "12345678-1234-1234-1234-123456789abc";
const ARTIFACT: &str = "12345678-1234-1234-1234-123456789abd";
#[derive(Clone)]
struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    parts: Vec<(Vec<u8>, Duration)>,
    last_part_gate: Option<Arc<Notify>>,
}
impl Reply {
    fn body(body: impl Into<Vec<u8>>) -> Self {
        Self {
            status: 200,
            headers: vec![],
            parts: vec![(body.into(), Duration::ZERO)],
            last_part_gate: None,
        }
    }
    fn status(status: u16) -> Self {
        Self {
            status,
            ..Self::body(b"{}".to_vec())
        }
    }
}
#[derive(Clone, Debug)]
struct Seen {
    method: String,
    path: String,
    body: Vec<u8>,
}
type Routes = Arc<dyn Fn(&Seen) -> Reply + Send + Sync>;
struct Host {
    address: String,
    seen: Arc<Mutex<Vec<Seen>>>,
    cancel: CancellationToken,
    worker: tokio::task::JoinHandle<()>,
}
impl Host {
    async fn start(routes: Routes) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let cancel = CancellationToken::new();
        let stop = cancel.clone();
        let seen = Arc::new(Mutex::new(vec![]));
        let requests = seen.clone();
        let worker = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            loop {
                let accepted = tokio::select! { biased; _=stop.cancelled()=>break,accepted=listener.accept()=>accepted.unwrap() };
                let routes = routes.clone();
                let requests = requests.clone();
                let stop = stop.clone();
                connections.spawn(async move {
                    let (mut socket,_)=accepted;
                    let operation=async {
                        let mut bytes=vec![];let mut buffer=[0;4096];
                        let header_end=loop {
                            let count=socket.read(&mut buffer).await.unwrap();
                            if count==0 {return;}
                            bytes.extend_from_slice(&buffer[..count]);
                            assert!(bytes.len()<64*1024,"Fixture request exceeds its limit");
                            if let Some(end)=bytes.windows(4).position(|w|w==b"\r\n\r\n") {break end+4;}
                        };
                        let headers=String::from_utf8(bytes[..header_end].to_vec()).unwrap();
                        let mut first=headers.lines().next().unwrap().split_whitespace();
                        let method=first.next().unwrap().into();let path=first.next().unwrap().into();
                        let size=headers.lines().find_map(|line|line.to_ascii_lowercase().strip_prefix("content-length:").map(|n|n.trim().parse::<usize>().unwrap())).unwrap_or(0);
                        while bytes.len()<header_end+size {
                            let count=socket.read(&mut buffer).await.unwrap();if count==0{return;}
                            bytes.extend_from_slice(&buffer[..count]);
                        }
                        let request=Seen {method,path,body:bytes[header_end..header_end+size].to_vec()};
                        requests.lock().unwrap().push(request.clone());
                        let reply=routes(&request);
                        let size:usize=reply.parts.iter().map(|(bytes,_)|bytes.len()).sum();
                        let extra=reply.headers.iter().map(|(k,v)|format!("{k}: {v}\r\n")).collect::<String>();
                        let headers=format!("HTTP/1.1 {} Fixture\r\nContent-Length: {size}\r\nConnection: close\r\n{extra}\r\n",reply.status);
                        if socket.write_all(headers.as_bytes()).await.is_err() {return;}
                        let last=reply.parts.len().saturating_sub(1);
                        for (index,(bytes,delay)) in reply.parts.into_iter().enumerate() {
                            if index==last && let Some(gate)=&reply.last_part_gate {gate.notified().await;}
                            tokio::time::sleep(delay).await;
                            if socket.write_all(&bytes).await.is_err() {return;}
                        }
                        let _=socket.shutdown().await;
                    };
                    tokio::select! { biased; _=stop.cancelled()=>{},_ =tokio::time::timeout(Duration::from_secs(5),operation)=>{} }
                });
            }
            while let Some(result) = connections.join_next().await {
                result.unwrap();
            }
        });
        Self {
            address,
            seen,
            cancel,
            worker,
        }
    }
    fn client(&self, home: &tempfile::TempDir) -> RemoteClient {
        RemoteClient::new(
            &self.address,
            Some("PUBLIC test token".into()),
            SessionStore::new(home.path()),
        )
        .unwrap()
    }
    fn count(&self, method: &str, path: &str) -> usize {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request.method == method && request.path == path)
            .count()
    }
    async fn shutdown(self) {
        self.cancel.cancel();
        tokio::time::timeout(Duration::from_secs(2), self.worker)
            .await
            .unwrap()
            .unwrap();
    }
}
fn request(recovery: bool) -> Request {
    Request {
        payload: json!({"prompt":"PUBLIC fixture","options":{"model":"gpt-6-pro"}}),
        recover_run_id: recovery.then(|| RUN.into()),
        session_id: Some("public-receiver".into()),
        image_destination: None,
        cancel: CancellationToken::new(),
        require_cancellation: false,
        verbose: false,
    }
}
fn accepted_result() -> Value {
    json!({"answerText":"PUBLIC answer","answerMarkdown":"PUBLIC answer",
    "effectiveModel":"gpt-6-pro","modelSelection":{"verified":true,"source":"chatgpt-model-picker","requestedModel":"Latest","resolvedLabel":"GPT-6"},
    "thinkingSelection":{"verified":true,"strictFailClosed":true,"source":"chatgpt-thinking-picker","requestedLevel":"pro","resolvedLabel":"Pro"}})
}

#[tokio::test]
async fn receiver_sessions_complete_before_ack_preserve_ack_failures_and_recover_without_replay() {
    let home = tempfile::tempdir().unwrap();
    let store = SessionStore::new(home.path().join("receiver"));
    let bytes = b"PUBLIC durable received file";
    let descriptor = artifact(bytes, "file", "public.txt");
    let mut answer = accepted_result();
    answer["elapsedMs"] = json!(2345);
    answer["usage"] = json!({"outputTokens":7});
    let events = wire(
        &serde_json::to_string(&answer).unwrap(),
        &[descriptor],
        true,
    );
    let saved = store.clone();
    let ack_ok = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let allow_ack = ack_ok.clone();
    let host = Host::start(Arc::new(move |request| {
        if request.path == "/health" {
            Reply::body(
                serde_json::to_vec(&json!({"ok":true,"capabilities":{"artifactTransfer":true,"artifactProtocolVersion":1,"maxArtifactBytes":512*1024*1024,"runCancellation":true}}))
                    .unwrap(),
            )
        } else if request.path.ends_with("/ack") {
            for meta in saved.list(false).unwrap() {
                assert_eq!(meta["status"], "completed");
                assert_eq!(meta["response"]["status"], "completed");
                assert_eq!(meta["usage"]["outputTokens"], 7);
                assert_eq!(
                    saved.model_runs(meta["id"].as_str().unwrap()).unwrap()[0]["status"],
                    "completed"
                );
                assert!(meta["browser"]["remoteResult"].is_object());
                let receipt = &meta["artifacts"][0];
                assert_eq!(
                    std::fs::read(receipt["path"].as_str().unwrap()).unwrap(),
                    bytes
                );
                let capture = serde_json::from_value(meta["capturedAnswer"].clone()).unwrap();
                assert_eq!(
                    crate::artifacts::verify_capture(&saved, &capture).unwrap(),
                    b"PUBLIC answer"
                );
            }
            Reply::status(if allow_ack.load(Ordering::SeqCst) {
                200
            } else {
                500
            })
        } else if request.path.contains("/artifacts/") {
            Reply::body(bytes.to_vec())
        } else {
            Reply::body(events.clone().into_bytes())
        }
    }))
    .await;
    let connection = crate::remote_session::Connection {
        host: host.address.clone(),
        token: Some("PUBLIC ephemeral token".into()),
    };
    let request=crate::execution::resolve(&json!({"prompt":"PUBLIC durable remote receiver test","engine":"browser","model":"gpt-6-pro","writeOutputPath":home.path().join("answer.md")}),&json!({}),&crate::config::Environment::new(),home.path()).unwrap();
    let meta = crate::remote_session::prepare(&store, &request, home.path(), &connection).unwrap();
    let id = meta["id"].as_str().unwrap();
    let identity = crate::process::ProcessIdentity::current().unwrap();
    crate::execution::start_owner(&store, id, &identity, false).unwrap();
    let cancel = CancellationToken::new();
    let error = crate::remote_session::execute(&store, id, &connection, &identity, &cancel)
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("acknowledgement failed"),
        "{error:#}"
    );
    let completed = store.require(id).unwrap();
    assert_eq!(completed["status"], "completed");
    assert_eq!(completed["recoveryRequired"], true);
    assert!(
        std::fs::read_to_string(home.path().join("answer.md"))
            .unwrap()
            .contains("PUBLIC answer")
    );
    assert!(
        crate::remote_session::execute(&store, id, &connection, &identity, &cancel)
            .await
            .is_err()
    );
    ack_ok.store(true, Ordering::SeqCst);
    let (recovered, result) =
        crate::remote_session::recover(&store, home.path(), &connection, RUN, &cancel)
            .await
            .unwrap();
    assert_ne!(recovered, id);
    assert_eq!(store.require(&recovered).unwrap()["status"], "completed");
    assert_eq!(result["answerText"], "PUBLIC answer");
    assert_eq!(host.count("POST", "/runs"), 1);
    assert_eq!(host.count("GET", &format!("/runs/{RUN}/result")), 1);
    assert_eq!(host.count("POST", &format!("/runs/{RUN}/ack")), 2);
    assert_eq!(store.list(false).unwrap().len(), 2);
    host.shutdown().await;
}
fn artifact(bytes: &[u8], kind: &str, name: &str) -> Value {
    json!({"runId":RUN,"artifactId":ARTIFACT,"kind":kind,"filename":name,
    "byteSize":bytes.len(),"sha256":crate::artifacts::sha256(bytes),"mimeType":"application/octet-stream"})
}
fn wire(result: &str, descriptors: &[Value], valid: bool) -> String {
    let descriptors_raw = serde_json::to_string(descriptors).unwrap();
    let envelope =
        format!("{{\"runId\":\"{RUN}\",\"result\":{result},\"artifacts\":{descriptors_raw}}}");
    let hash = if valid {
        crate::artifacts::sha256(&remote_json::stringify(&envelope).unwrap())
    } else {
        "0".repeat(64)
    };
    let mut events = format!("{{\"type\":\"run-started\",\"runId\":\"{RUN}\"}}\r\n\n");
    for descriptor in descriptors {
        events += &format!(
            "{}\n",
            json!({"type":"artifact-ready","artifact":descriptor})
        );
    }
    events += &format!(
        "{{\"type\":\"result\",\"result\":{result},\"delivery\":{{\"version\":1,\"runId\":\"{RUN}\",\"sha256\":\"{hash}\"}}}}\n"
    );
    events
}
fn persist_counter(counter: Arc<AtomicUsize>) -> Persist {
    Arc::new(move |_| {
        let counter = counter.clone();
        Box::pin(async move {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    })
}

#[tokio::test]
async fn fragmented_lossless_digest_and_ack_follow_durable_result_without_post_replay() {
    let home = tempfile::tempdir().unwrap();
    let saved = Arc::new(AtomicUsize::new(0));
    let result = r#"{"answerText":"PUBLIC \ud800 text","n":9007199254740993,"ordered":{"2":"two","1":"one"}}"#;
    let events = wire(result, &[], true).into_bytes();
    let saved_check = saved.clone();
    let host = Host::start(Arc::new(move |request| {
        if request.path.ends_with("/ack") {
            assert_eq!(saved_check.load(Ordering::SeqCst), 1);
            Reply::body(b"{}".to_vec())
        } else {
            assert_eq!(request.method, "GET");
            assert!(request.body.is_empty());
            Reply {
                parts: events
                    .chunks(7)
                    .map(|bytes| (bytes.to_vec(), Duration::from_millis(1)))
                    .collect(),
                ..Reply::body(vec![])
            }
        }
    }))
    .await;
    let result = host
        .client(&home)
        .run(
            request(true),
            Hooks {
                result_captured: Some(persist_counter(saved)),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(result["answerText"], "PUBLIC \u{fffd} text");
    assert_eq!(result["n"].as_f64(), Some(9_007_199_254_740_992.0));
    assert_eq!(host.count("POST", "/runs"), 0);
    assert_eq!(host.count("POST", &format!("/runs/{RUN}/ack")), 1);
    host.shutdown().await;
}

#[tokio::test]
async fn malformed_digest_or_unterminated_result_never_persists_or_acknowledges() {
    for unterminated in [false, true] {
        let home = tempfile::tempdir().unwrap();
        let saved = Arc::new(AtomicUsize::new(0));
        let mut events = wire("{}", &[], false);
        if unterminated {
            events.pop();
        }
        let host = Host::start(Arc::new(move |_| Reply::body(events.clone().into_bytes()))).await;
        let error = host
            .client(&home)
            .run(
                request(true),
                Hooks {
                    result_captured: Some(persist_counter(saved.clone())),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains(if unterminated {
            "without a result"
        } else {
            "digest"
        }));
        assert_eq!(saved.load(Ordering::SeqCst), 0);
        assert_eq!(host.count("POST", &format!("/runs/{RUN}/ack")), 0);
        host.shutdown().await;
    }
}

#[tokio::test]
async fn publication_receipt_finishes_after_cancellation_and_keeps_the_file() {
    let home = tempfile::tempdir().unwrap();
    let bytes = b"PUBLIC artifact";
    let descriptor = artifact(bytes, "file", "answer.txt");
    let events = wire(
        &serde_json::to_string(&accepted_result()).unwrap(),
        &[descriptor],
        true,
    );
    let host = Host::start(Arc::new(move |request| {
        if request.path.contains("/artifacts/") {
            Reply::body(bytes.to_vec())
        } else {
            Reply::body(events.clone().into_bytes())
        }
    }))
    .await;
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let persisted = Arc::new(AtomicUsize::new(0));
    let hooks = Hooks {
        artifact_receipt: Some({
            let started = started.clone();
            let release = release.clone();
            let persisted = persisted.clone();
            Arc::new(move |_| {
                let started = started.clone();
                let release = release.clone();
                let persisted = persisted.clone();
                Box::pin(async move {
                    started.notify_one();
                    release.notified().await;
                    persisted.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                })
            })
        }),
        ..Default::default()
    };
    let req = request(false);
    let cancel = req.cancel.clone();
    let client = host.client(&home);
    let worker = tokio::spawn(async move { client.run(req, hooks).await });
    tokio::time::timeout(Duration::from_secs(2), started.notified())
        .await
        .unwrap();
    cancel.cancel();
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(!worker.is_finished());
    let file = home
        .path()
        .join("sessions/public-receiver/artifacts/answer.txt");
    assert_eq!(std::fs::read(&file).unwrap(), bytes);
    release.notify_one();
    assert!(
        worker
            .await
            .unwrap()
            .unwrap_err()
            .to_string()
            .contains("cancelled")
    );
    assert_eq!(persisted.load(Ordering::SeqCst), 1);
    assert_eq!(std::fs::read(file).unwrap(), bytes);
    assert_eq!(host.count("POST", "/runs"), 1);
    assert_eq!(host.count("POST", &format!("/runs/{RUN}/ack")), 0);
    host.shutdown().await;
}

#[tokio::test]
async fn stream_failure_waits_for_receipt_and_preserves_both_errors() {
    let home = tempfile::tempdir().unwrap();
    let bytes = b"PUBLIC partial";
    let descriptor = artifact(bytes, "file", "partial.txt");
    let artifact_event = format!(
        "{}\n",
        json!({"type":"artifact-ready","artifact":descriptor})
    );
    let receipt_started = Arc::new(Notify::new());
    let producer_gate = receipt_started.clone();
    let host = Host::start(Arc::new(move |request| {
        if request.path.contains("/artifacts/") {
            Reply::body(bytes.to_vec())
        } else {
            Reply {
                parts: vec![
                    (artifact_event.clone().into_bytes(), Duration::ZERO),
                    (
                        b"{\"type\":\"error\",\"message\":\"PUBLIC producer failed\"}\n".to_vec(),
                        Duration::ZERO,
                    ),
                ],
                last_part_gate: Some(producer_gate.clone()),
                ..Reply::body(vec![])
            }
        }
    }))
    .await;
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let hooks = Hooks {
        artifact_receipt: Some({
            let started = started.clone();
            let release = release.clone();
            let receipt_started = receipt_started.clone();
            Arc::new(move |_| {
                let started = started.clone();
                let release = release.clone();
                let receipt_started = receipt_started.clone();
                Box::pin(async move {
                    receipt_started.notify_one();
                    started.notify_one();
                    release.notified().await;
                    bail!("PUBLIC receipt failed")
                })
            })
        }),
        ..Default::default()
    };
    let client = host.client(&home);
    let worker = tokio::spawn(async move { client.run(request(false), hooks).await });
    tokio::time::timeout(Duration::from_secs(2), started.notified())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(!worker.is_finished());
    release.notify_one();
    let error = format!("{:#}", worker.await.unwrap().unwrap_err());
    assert!(
        error.contains("PUBLIC producer failed") && error.contains("PUBLIC receipt failed"),
        "{error}"
    );
    assert_eq!(
        std::fs::read(
            home.path()
                .join("sessions/public-receiver/artifacts/partial.txt")
        )
        .unwrap(),
        bytes
    );
    host.shutdown().await;
}

#[tokio::test]
async fn transfer_collision_and_failure_keep_text_but_prevent_acknowledgement() {
    for bad_bytes in [false, true] {
        let home = tempfile::tempdir().unwrap();
        let bytes = b"PUBLIC bytes";
        let descriptor = artifact(bytes, "file", "same.txt");
        let directory = home.path().join("sessions/public-receiver/artifacts");
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("same.txt"), b"PUBLIC existing").unwrap();
        let events = wire("{\"answerText\":\"PUBLIC text\"}", &[descriptor], true);
        let host = Host::start(Arc::new(move |request| {
            if request.path.contains("/artifacts/") {
                Reply::body(if bad_bytes {
                    b"PUBLIC other".to_vec()
                } else {
                    bytes.to_vec()
                })
            } else {
                Reply::body(events.clone().into_bytes())
            }
        }))
        .await;
        let saved = Arc::new(AtomicUsize::new(0));
        let result = host
            .client(&home)
            .run(
                request(true),
                Hooks {
                    result_captured: Some(persist_counter(saved.clone())),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(result["answerText"], "PUBLIC text");
        assert_eq!(saved.load(Ordering::SeqCst), 1);
        assert_eq!(
            std::fs::read(directory.join("same.txt")).unwrap(),
            b"PUBLIC existing"
        );
        if bad_bytes {
            assert_eq!(
                result["warnings"][0]["code"],
                "remote-artifact-transfer-failed"
            );
            assert!(!directory.join("same-2.txt").exists());
            assert_eq!(host.count("POST", &format!("/runs/{RUN}/ack")), 0);
        } else {
            assert_eq!(std::fs::read(directory.join("same-2.txt")).unwrap(), bytes);
            assert_eq!(result["savedFiles"][0]["transfer"]["status"], "completed");
            assert_eq!(host.count("POST", &format!("/runs/{RUN}/ack")), 1);
        }
        host.shutdown().await;
    }
}

#[tokio::test]
async fn capability_preflight_and_cancelled_requests_send_no_prompt() {
    let home = tempfile::tempdir().unwrap();
    let host = Host::start(Arc::new(|_| {
        Reply::body(b"{\"ok\":true,\"capabilities\":{}}".to_vec())
    }))
    .await;
    let mut req = request(false);
    req.cancel.cancel();
    assert!(host.client(&home).run(req, Hooks::default()).await.is_err());
    assert!(host.seen.lock().unwrap().is_empty());
    req = request(false);
    req.require_cancellation = true;
    assert!(
        host.client(&home)
            .run(req, Hooks::default())
            .await
            .unwrap_err()
            .to_string()
            .contains("cancellation")
    );
    req = request(false);
    req.image_destination = Some(home.path().join("image.png"));
    assert!(
        host.client(&home)
            .run(req, Hooks::default())
            .await
            .unwrap_err()
            .to_string()
            .contains("image request was not sent")
    );
    assert_eq!(host.count("GET", "/health"), 2);
    assert_eq!(host.count("POST", "/runs"), 0);
    host.shutdown().await;
}

#[tokio::test]
async fn result_persistence_failure_blocks_ack_and_failed_ack_keeps_durable_result() {
    for persistence_fails in [true, false] {
        let home = tempfile::tempdir().unwrap();
        let saved = Arc::new(AtomicUsize::new(0));
        let events = wire("{\"answerText\":\"PUBLIC original\"}", &[], true);
        let host = Host::start(Arc::new(move |request| {
            if request.path.ends_with("/ack") {
                Reply::status(409)
            } else {
                Reply::body(events.clone().into_bytes())
            }
        }))
        .await;
        let hooks = Hooks {
            result_captured: Some({
                let saved = saved.clone();
                Arc::new(move |_| {
                    let saved = saved.clone();
                    Box::pin(async move {
                        if persistence_fails {
                            bail!("PUBLIC persistence failed");
                        }
                        saved.fetch_add(1, Ordering::SeqCst);
                        Ok(())
                    })
                })
            }),
            ..Default::default()
        };
        let error = host
            .client(&home)
            .run(request(true), hooks)
            .await
            .unwrap_err();
        assert!(error.to_string().contains(if persistence_fails {
            "persistence"
        } else {
            "acknowledgement"
        }));
        assert_eq!(
            saved.load(Ordering::SeqCst),
            usize::from(!persistence_fails)
        );
        assert_eq!(
            host.count("POST", &format!("/runs/{RUN}/ack")),
            usize::from(!persistence_fails)
        );
        assert_eq!(host.count("POST", "/runs"), 0);
        host.shutdown().await;
    }
}

#[tokio::test]
async fn image_destinations_preserve_collision_and_generated_images_have_image_receipts() {
    let cases = if cfg!(windows) {
        vec![(false, 0), (false, 1), (true, 0), (false, 2)]
    } else {
        vec![(false, 0), (false, 1), (true, 0)]
    };
    for (collision, mode) in cases {
        let cwd = std::env::current_dir().unwrap();
        let home = if mode > 0 {
            tempfile::TempDir::new_in(&cwd).unwrap()
        } else {
            tempfile::tempdir().unwrap()
        };
        let bytes = b"PUBLIC image transfer";
        let descriptor = artifact(bytes, "image", "generated.png");
        let events = wire("{\"answerText\":\"PUBLIC image\"}", &[descriptor], true);
        let host=Host::start(Arc::new(move |request|if request.path=="/health" {Reply::body(b"{\"ok\":true,\"capabilities\":{\"artifactTransfer\":true,\"maxArtifactBytes\":536870912,\"generatedImages\":true,\"artifactProtocolVersion\":1}}".to_vec())}else if request.path.contains("/artifacts/") {Reply::body(bytes.to_vec())}else{Reply::body(events.clone().into_bytes())})).await;
        let destination = home.path().join("preferred.png");
        if collision {
            std::fs::write(&destination, b"PUBLIC existing").unwrap();
        }
        let mut req = request(true);
        req.image_destination = Some(match mode {
            1 => destination.strip_prefix(&cwd).unwrap().to_owned(),
            2 => PathBuf::from(format!(
                "{}{}",
                &cwd.to_str().unwrap()[..2],
                destination.strip_prefix(&cwd).unwrap().display()
            )),
            _ => destination.clone(),
        });
        let saved = Arc::new(AtomicUsize::new(0));
        let receipts = Arc::new(AtomicUsize::new(0));
        let result = host
            .client(&home)
            .run(
                req,
                Hooks {
                    result_captured: Some(persist_counter(saved.clone())),
                    artifact_receipt: Some(persist_counter(receipts.clone())),
                    ..Default::default()
                },
            )
            .await;
        if collision {
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("not fully delivered")
            );
            assert_eq!(std::fs::read(destination).unwrap(), b"PUBLIC existing");
            assert_eq!(saved.load(Ordering::SeqCst), 0);
            assert_eq!(receipts.load(Ordering::SeqCst), 0);
        } else {
            let result = result.unwrap();
            assert!(
                std::path::Path::new(result["savedImages"][0]["path"].as_str().unwrap())
                    .is_absolute()
            );
            assert_eq!(result["savedImages"][0]["kind"], "image");
            assert_eq!(receipts.load(Ordering::SeqCst), 1);
            assert_eq!(std::fs::read(destination).unwrap(), bytes);
        }
        host.shutdown().await;
    }
}

#[tokio::test]
async fn remote_web_search_refuses_before_any_request() {
    let home = tempfile::tempdir().unwrap();
    let host = Host::start(Arc::new(|_| Reply::body(b"{}".to_vec()))).await;
    for (mode, message) in [
        (
            "search",
            "Web Search is a local browser pilot; --remote-host does not negotiate this capability yet. Use local Chrome or --browser-attach-running.",
        ),
        ("deep", crate::remote_driver::REMOTE_DEEP_RESEARCH),
    ] {
        let mut search = request(false);
        search.payload["browserConfig"] = json!({"researchMode":mode});
        let error = host
            .client(&home)
            .run(
                search,
                Hooks {
                    runtime_hint: None,
                    artifact_receipt: None,
                    result_captured: None,
                    log: None,
                },
            )
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), message);
    }
    assert!(host.seen.lock().unwrap().is_empty());
    host.shutdown().await;
}
#[test]
fn remote_address_parser_accepts_bracketed_ipv6_and_refuses_paths_and_bad_ports() {
    let store = SessionStore::new("PUBLIC-unused-home");
    assert_eq!(
        RemoteClient::new("[::1]:9473", None, store.clone())
            .unwrap()
            .endpoint
            .as_str(),
        "http://[::1]:9473/"
    );
    for address in [
        "::1:9473",
        "127.0.0.1:0",
        "127.0.0.1:65536",
        "http://localhost:80",
        "user@localhost:80",
        "[::1]:80junk",
    ] {
        assert!(
            RemoteClient::new(address, None, store.clone()).is_err(),
            "{address}"
        );
    }
}

fn recorder() -> (Log, Arc<Mutex<Vec<String>>>) {
    let lines = Arc::new(Mutex::new(vec![]));
    let sink = lines.clone();
    let log: Log = Arc::new(move |line: &str| {
        sink.lock().unwrap().push(line.to_owned());
        Ok(())
    });
    (log, lines)
}

#[tokio::test]
async fn progress_transfer_and_failure_lines_follow_the_reference() {
    let home = tempfile::tempdir().unwrap();
    let descriptor = artifact(b"PUBLIC bytes", "file", "Report.ZIP");
    let progress = [
        json!({"type":"artifact-progress","artifactId":ARTIFACT,"phase":"staging","receivedBytes":5,"totalBytes":12}),
        json!({"type":"artifact-progress","artifactId":ARTIFACT,"phase":"staged"}),
    ];
    let mut events = wire("{\"answerText\":\"PUBLIC text\"}", &[descriptor], true);
    for event in progress.iter().rev() {
        events.insert_str(events.find('\n').unwrap() + 1, &format!("{event}\n"));
    }
    let host = Host::start(Arc::new(move |request| {
        if request.path.contains("/artifacts/") {
            Reply::body(b"PUBLIC other".to_vec())
        } else {
            Reply::body(events.clone().into_bytes())
        }
    }))
    .await;
    let (log, lines) = recorder();
    let mut req = request(true);
    req.verbose = true;
    let result = host
        .client(&home)
        .run(
            req,
            Hooks {
                log: Some(log),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let message = result["warnings"][0]["message"].as_str().unwrap();
    assert!(message.contains("download the ZIP/file shown"), "{message}");
    let lines = lines.lock().unwrap()[1..].to_vec();
    assert_eq!(
        lines[..3],
        [
            format!("[browser] Artifact {ARTIFACT} staging 5/12 bytes"),
            format!("[browser] Artifact {ARTIFACT} staged"),
            "[browser] Transferring artifact report.zip from bridge host...".into(),
        ]
    );
    assert_eq!(lines[3], format!("[browser] {message}"));
    host.shutdown().await;
}

#[tokio::test]
async fn capability_refusals_use_the_reference_texts_and_order() {
    let home = tempfile::tempdir().unwrap();
    let host = Host::start(Arc::new(|_| {
        Reply::body(b"{\"ok\":true,\"capabilities\":{}}".to_vec())
    }))
    .await;
    let refusal = |image: bool, cancellation: bool, model: &str| {
        let mut req = request(false);
        req.payload["options"]["model"] = json!(model);
        req.require_cancellation = cancellation;
        req.image_destination = image.then(|| home.path().join("image.png"));
        let client = host.client(&home);
        async move {
            client
                .run(req, Hooks::default())
                .await
                .unwrap_err()
                .to_string()
        }
    };
    assert_eq!(
        refusal(true, true, "gpt-6-pro").await,
        "Remote host cannot capture and transfer generated images; upgrade Alcuin on the host and retry. The image request was not sent."
    );
    assert_eq!(
        refusal(false, true, "gpt-6-pro").await,
        "Remote host does not support run cancellation; upgrade the host before using an AbortSignal."
    );
    assert_eq!(host.count("GET", "/health"), 2);
    // Model policy refuses before the preflight is sent.
    assert!(
        refusal(true, true, "gpt-4o")
            .await
            .starts_with("Alcuin only permits")
    );
    assert_eq!(host.count("GET", "/health"), 2);
    host.shutdown().await;
}

#[tokio::test]
async fn health_errors_trim_fall_back_to_the_status_and_name_the_timeout() {
    let home = tempfile::tempdir().unwrap();
    for (body, expected) in [
        ("", "HTTP 500"),
        ("  PUBLIC down \n", "PUBLIC down"),
        ("{\"error\":\"  \"}", "{\"error\":\"  \"}"),
        ("{\"error\":\" PUBLIC busy \"}", "PUBLIC busy"),
    ] {
        let host = Host::start(Arc::new(move |_| Reply {
            status: 500,
            ..Reply::body(body.as_bytes().to_vec())
        }))
        .await;
        let health = host
            .client(&home)
            .health(&CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(health["error"], expected, "{body:?}");
        host.shutdown().await;
    }
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let silent = tokio::spawn(async move {
        let _socket = listener.accept().await;
        tokio::time::sleep(Duration::from_secs(10)).await;
    });
    let client = RemoteClient::new(&address, None, SessionStore::new(home.path())).unwrap();
    let health = client.health(&CancellationToken::new()).await.unwrap();
    assert_eq!(health["error"], "timeout after 5000ms");
    silent.abort();
}
