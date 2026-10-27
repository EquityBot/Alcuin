//! Verified remote downloads retain one native source and a bounded transfer buffer.
use super::{MAX_ARTIFACT_BYTES, require_artifact_identity};
use crate::{
    artifacts,
    native_fs::{NativeEntry, ReadStamp, read_stamp},
    remote_result::RemoteRecord,
    sessions::SessionStore,
};
use anyhow::{Context, Result, ensure};
use bytes::Bytes;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    io::{Read, Seek, SeekFrom},
    path::PathBuf,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub(super) struct VerifiedArtifact {
    entry: NativeEntry,
    parent: NativeEntry,
    root: NativeEntry,
    original: PathBuf,
    canonical: PathBuf,
    root_path: PathBuf,
    canonical_root: PathBuf,
    stamp: ReadStamp,
    expected: u64,
    hash: String,
}
impl VerifiedArtifact {
    pub(super) fn open(
        store: &SessionStore,
        record: &RemoteRecord,
        artifact: &Value,
        cancel: &CancellationToken,
    ) -> Result<Self> {
        ensure!(
            !cancel.is_cancelled(),
            "Remote artifact verification cancelled"
        );
        let descriptor = &artifact["descriptor"];
        require_artifact_identity(descriptor, &record.native()["runId"])?;
        let expected = descriptor["byteSize"]
            .as_u64()
            .context("Remote artifact size is missing")?;
        ensure!(
            expected > 0 && expected <= MAX_ARTIFACT_BYTES,
            "Remote artifact size is invalid"
        );
        let hash = descriptor["sha256"]
            .as_str()
            .context("Remote artifact digest is missing")?
            .to_owned();
        let original = PathBuf::from(
            artifact["filePath"]
                .as_str()
                .context("Remote artifact path is missing")?,
        );
        let root_path = store.root();
        let root = NativeEntry::open(&root_path, true)?;
        let canonical_root = root_path.canonicalize()?;
        let canonical = original.canonicalize()?;
        let relative = canonical
            .strip_prefix(&canonical_root)
            .context("Remote artifact is outside Industrious session storage")?;
        let components = relative.components().collect::<Vec<_>>();
        ensure!(
            components.len() >= 3 && components[1].as_os_str() == "artifacts",
            "Remote artifact is outside its session artifact boundary"
        );
        let parent = NativeEntry::open(
            canonical
                .parent()
                .context("Remote artifact parent missing")?,
            true,
        )?;
        let mut entry = NativeEntry::open(&canonical, false)?;
        let stamp = read_stamp(&entry.handle)?;
        ensure!(
            entry.handle.metadata()?.len() == expected,
            "Remote artifact size changed"
        );
        artifacts::verify_streamed_file_until(
            &mut entry.handle,
            expected,
            &hash,
            Some(cancel),
            None,
        )?;
        let mut verified = Self {
            entry,
            parent,
            root,
            original,
            canonical,
            root_path,
            canonical_root,
            stamp,
            expected,
            hash,
        };
        verified.require(cancel)?;
        verified.entry.handle.seek(SeekFrom::Start(0))?;
        Ok(verified)
    }
    fn require(&self, cancel: &CancellationToken) -> Result<()> {
        ensure!(!cancel.is_cancelled(), "Remote artifact transfer cancelled");
        ensure!(
            read_stamp(&self.entry.handle)? == self.stamp,
            "Remote artifact changed"
        );
        self.entry.require_matches(&self.canonical)?;
        self.parent.require_matches(
            self.canonical
                .parent()
                .context("Remote artifact parent missing")?,
        )?;
        self.root.require_matches(&self.root_path)?;
        ensure!(
            self.original.canonicalize()? == self.canonical
                && self.root_path.canonicalize()? == self.canonical_root,
            "Remote artifact storage boundary changed"
        );
        Ok(())
    }
    pub(super) fn stream(
        mut self,
        sender: mpsc::Sender<Bytes>,
        cancel: &CancellationToken,
    ) -> Result<()> {
        self.require(cancel)?;
        let mut total = 0u64;
        let mut hash = Sha256::new();
        let mut buffer = [0u8; 64 * 1024];
        loop {
            ensure!(!cancel.is_cancelled(), "Remote artifact transfer cancelled");
            let bound = (self.expected - total).min(buffer.len() as u64) as usize;
            let count = self.entry.handle.read(&mut buffer[..bound])?;
            ensure!(count > 0, "Remote artifact ended before its receipt");
            total += count as u64;
            hash.update(&buffer[..count]);
            if total == self.expected {
                // Content-Length must remain incomplete until the actual outgoing bytes and
                // original native/storage identities have all been checked a second time.
                let mut extra = [0u8; 1];
                ensure!(
                    self.entry.handle.read(&mut extra)? == 0,
                    "Remote artifact grew"
                );
                let actual = hash
                    .clone()
                    .finalize()
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>();
                ensure!(
                    actual == self.hash,
                    "Remote artifact bytes changed during transfer"
                );
                self.require(cancel)?;
            }
            sender
                .blocking_send(Bytes::copy_from_slice(&buffer[..count]))
                .context("Remote artifact receiver disconnected")?;
            if total == self.expected {
                return Ok(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote_result::{self, Phase};
    use serde_json::json;
    use std::{fs, io::Write, sync::mpsc as completion};
    const CHUNK: usize = 64 * 1024;

    fn fixture(size: usize) -> (tempfile::TempDir, SessionStore, RemoteRecord, PathBuf) {
        let home = tempfile::tempdir().unwrap();
        let store = SessionStore::new(home.path().join(".oracle"));
        let run = uuid::Uuid::new_v4().to_string();
        let id = remote_result::session_id(&run).unwrap();
        store
            .create(
                json!({"prompt":"PUBLIC artifact stream"}),
                home.path(),
                Some(&id),
            )
            .unwrap();
        let directory = store.directory(&id).unwrap().join("artifacts");
        fs::create_dir(&directory).unwrap();
        let path = directory.join("PUBLIC-large.bin");
        let mut file = fs::File::create(&path).unwrap();
        let mut hash = Sha256::new();
        let chunk = [0x52u8; CHUNK];
        let mut remaining = size;
        while remaining > 0 {
            let count = remaining.min(CHUNK);
            file.write_all(&chunk[..count]).unwrap();
            hash.update(&chunk[..count]);
            remaining -= count;
        }
        file.sync_all().unwrap();
        drop(file);
        let hash = hash
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let captured = json!({"answerText":"PUBLIC streamed artifact","artifacts":[{"kind":"file","path":path,"sizeBytes":size,"sha256":hash}]});
        let produced = crate::remote_protocol::produced(&store, &run, captured, false).unwrap();
        remote_result::save(
            &store,
            &run,
            &produced.captured,
            &produced.result,
            &produced.artifacts,
            Phase::Result,
            &CancellationToken::new(),
        )
        .unwrap();
        let record = remote_result::read_phase(&store, &run, Phase::Result).unwrap();
        (home, store, record, path)
    }

    #[test]
    fn stream_is_chunked_and_withholds_completion_after_source_or_identity_changes() {
        let size = 8 * CHUNK + 17;
        for mode in [
            "valid",
            "truncate",
            "grow",
            "same-size",
            "replace",
            "cancel",
        ] {
            let (_home, store, record, path) = fixture(size);
            let cancel = CancellationToken::new();
            let source =
                VerifiedArtifact::open(&store, &record, &record.native()["artifacts"][0], &cancel)
                    .unwrap();
            let (sender, mut receiver) = mpsc::channel(1);
            let worker_cancel = cancel.clone();
            let worker = std::thread::spawn(move || source.stream(sender, &worker_cancel));
            let first = receiver.blocking_recv().unwrap();
            assert_eq!(first.len(), CHUNK);
            match mode {
                "valid" => {}
                "truncate" => fs::OpenOptions::new()
                    .write(true)
                    .open(&path)
                    .unwrap()
                    .set_len((size / 2) as u64)
                    .unwrap(),
                "grow" => {
                    let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
                    file.write_all(b"PUBLIC growth").unwrap();
                    file.sync_all().unwrap();
                }
                "same-size" => {
                    let mut file = fs::OpenOptions::new().write(true).open(&path).unwrap();
                    file.write_all(b"PUBLIC changed").unwrap();
                    file.sync_all().unwrap();
                }
                "replace" => {
                    fs::rename(&path, path.with_extension("PUBLIC-original")).unwrap();
                    let mut file = fs::File::create(&path).unwrap();
                    file.set_len(size as u64).unwrap();
                    file.write_all(b"PUBLIC replacement").unwrap();
                    file.sync_all().unwrap();
                }
                "cancel" => cancel.cancel(),
                _ => unreachable!(),
            }
            let mut total = first.len();
            let mut hash = Sha256::new();
            hash.update(&first);
            while let Some(bytes) = receiver.blocking_recv() {
                assert!(bytes.len() <= CHUNK);
                total += bytes.len();
                hash.update(&bytes);
            }
            let outcome = worker.join().unwrap();
            if mode == "valid" {
                outcome.unwrap();
                assert_eq!(total, size);
                assert_eq!(
                    hash.finalize()
                        .iter()
                        .map(|byte| format!("{byte:02x}"))
                        .collect::<String>(),
                    record.native()["artifacts"][0]["descriptor"]["sha256"]
                        .as_str()
                        .unwrap()
                );
            } else {
                assert!(outcome.is_err(), "{mode}");
                assert!(total < size, "{mode} released the final chunk");
            }
        }
    }

    #[tokio::test]
    async fn incomplete_stream_fails_http_transfer_without_publishing_the_destination() {
        use crate::remote_host::RemoteBody;
        use hyper::{Request, Response, body::Incoming, server::conn::http1, service::service_fn};
        use hyper_util::rt::TokioIo;
        use std::{
            convert::Infallible,
            sync::{Arc, Mutex},
        };
        let size = 8 * CHUNK + 17;
        for changed in [false, true] {
            let (home, store, record, path) = fixture(size);
            let cancel = CancellationToken::new();
            let descriptor = record.native()["artifacts"][0]["descriptor"].clone();
            let source =
                VerifiedArtifact::open(&store, &record, &record.native()["artifacts"][0], &cancel)
                    .unwrap();
            let (sender, mut chunks) = mpsc::channel(1);
            let producer_cancel = cancel.clone();
            let producer =
                tokio::task::spawn_blocking(move || source.stream(sender, &producer_cancel));
            let first = chunks.recv().await.unwrap();
            if changed {
                let mut file = fs::OpenOptions::new().write(true).open(&path).unwrap();
                file.write_all(b"PUBLIC changed in flight").unwrap();
                file.sync_all().unwrap();
            }
            let (outgoing, receiver) = mpsc::channel(1);
            let bridge = tokio::spawn(async move {
                if outgoing.send(first).await.is_err() {
                    return;
                }
                while let Some(bytes) = chunks.recv().await {
                    if outgoing.send(bytes).await.is_err() {
                        break;
                    }
                }
            });
            let body = Arc::new(Mutex::new(Some(receiver)));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (socket, _) = listener.accept().await.unwrap();
                let service = service_fn(move |_: Request<Incoming>| {
                    let receiver = body.lock().unwrap().take().unwrap();
                    async move {
                        Ok::<_, Infallible>(
                            Response::builder()
                                .status(200)
                                .header("Content-Length", size)
                                .body(RemoteBody {
                                    receiver,
                                    disconnect: None,
                                })
                                .unwrap(),
                        )
                    }
                });
                http1::Builder::new()
                    .timer(hyper_util::rt::TokioTimer::new())
                    .serve_connection(TokioIo::new(socket), service)
                    .await
            });
            let client = reqwest::Client::builder()
                .no_proxy()
                .timeout(std::time::Duration::from_secs(5))
                .build()
                .unwrap();
            let destination = home.path().join("PUBLIC-downloaded.bin");
            let endpoint = reqwest::Url::parse(&format!("http://{address}/")).unwrap();
            let outcome = crate::remote_transfer::transfer(
                &client,
                &endpoint,
                None,
                &descriptor,
                &destination,
                false,
                &cancel,
            )
            .await;
            // Close the otherwise reusable HTTP connection after the one request.
            drop(client);
            if changed {
                assert!(outcome.is_err());
                assert!(!destination.exists());
            } else {
                let transferred = outcome.unwrap();
                assert_eq!(transferred.artifact["sizeBytes"], size);
                crate::browser_output::verify_saved_file(&transferred.artifact, &cancel).unwrap();
            }
            let produced = producer.await.unwrap();
            assert_eq!(produced.is_ok(), !changed);
            bridge.await.unwrap();
            let connection = tokio::time::timeout(std::time::Duration::from_secs(5), server)
                .await
                .unwrap()
                .unwrap();
            if changed {
                assert!(connection.is_err(), "truncated Content-Length was accepted");
            }
            assert!(
                !store
                    .directory(
                        record.native()["runId"]
                            .as_str()
                            .map(|run| remote_result::session_id(run).unwrap())
                            .as_deref()
                            .unwrap()
                    )
                    .unwrap()
                    .join("remote-ack.json")
                    .exists()
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "measures process peak RSS; run alone with --ignored --exact --test-threads=1"]
    fn remote_artifact_stream_has_bounded_peak_rss() {
        fn peak_kib() -> u64 {
            fs::read_to_string("/proc/self/status")
                .unwrap()
                .lines()
                .find_map(|line| line.strip_prefix("VmHWM:"))
                .unwrap()
                .split_whitespace()
                .next()
                .unwrap()
                .parse()
                .unwrap()
        }
        let size = 128 * 1024 * 1024;
        let (_home, store, record, path) = fixture(size);
        let cancel = CancellationToken::new();
        let before = peak_kib();
        let source =
            VerifiedArtifact::open(&store, &record, &record.native()["artifacts"][0], &cancel)
                .unwrap();
        let (sender, mut receiver) = mpsc::channel(1);
        let worker = std::thread::spawn(move || source.stream(sender, &cancel));
        let mut received = 0usize;
        while let Some(bytes) = receiver.blocking_recv() {
            assert!(bytes.len() <= CHUNK);
            received += bytes.len();
            std::hint::black_box(bytes);
        }
        worker.join().unwrap().unwrap();
        let streamed = peak_kib();
        assert_eq!(received, size);
        let streamed_delta = streamed.saturating_sub(before);
        assert!(
            streamed_delta < 8 * 1024,
            "remote stream grew peak RSS by {streamed_delta} KiB"
        );
        let control = fs::read(path).unwrap();
        std::hint::black_box(&control);
        let buffered = peak_kib();
        let additional = buffered.saturating_sub(streamed);
        println!(
            "PUBLIC_REMOTE_ARTIFACT_RSS {}",
            json!({"fileBytes":size,
            "beforeKiB":before,"streamedPeakKiB":streamed,"streamedDeltaKiB":streamed_delta,
            "wholeFilePeakKiB":buffered,"wholeFileAdditionalDeltaKiB":additional})
        );
        assert_eq!(control.len(), size);
        assert!(
            additional >= 64 * 1024,
            "whole-file RSS control did not calibrate: {additional} KiB"
        );
    }

    #[test]
    fn disconnect_releases_a_producer_blocked_on_the_bounded_channel() {
        let (_home, store, record, _path) = fixture(8 * CHUNK);
        let cancel = CancellationToken::new();
        let source =
            VerifiedArtifact::open(&store, &record, &record.native()["artifacts"][0], &cancel)
                .unwrap();
        let (sender, mut receiver) = mpsc::channel(1);
        let (done, completion) = completion::channel();
        let worker = std::thread::spawn(move || {
            done.send(source.stream(sender, &cancel)).unwrap();
        });
        assert_eq!(receiver.blocking_recv().unwrap().len(), CHUNK);
        drop(receiver);
        assert!(
            completion
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap()
                .is_err()
        );
        worker.join().unwrap();
    }

    #[test]
    fn changed_source_refuses_before_headers_and_cancelled_open_never_reads_it() {
        let (_home, store, record, path) = fixture(CHUNK);
        let cancel = CancellationToken::new();
        let row = &record.native()["artifacts"][0];
        cancel.cancel();
        assert!(VerifiedArtifact::open(&store, &record, row, &cancel).is_err());
        fs::write(&path, b"PUBLIC corrupt source").unwrap();
        assert!(VerifiedArtifact::open(&store, &record, row, &CancellationToken::new()).is_err());
    }
}
