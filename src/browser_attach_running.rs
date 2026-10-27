//! Borrow an existing debugging endpoint without adopting its browser process or profile.
//! Reference: src/browser/attachRunning.ts and detect.ts.
use crate::{
    browser_chrome::{AttachmentEndpoint, endpoint_base},
    config::Environment,
    native_fs::{NativeEntry, ReadStamp, read_stamp},
};
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};
use tokio_util::sync::CancellationToken;

pub(crate) struct Attachment {
    pub endpoint: AttachmentEndpoint,
    pub profile_root: Option<PathBuf>,
    /// Reference progress lines for the session log.
    pub notes: Vec<String>,
}
type Ancestry = Vec<(PathBuf, Arc<NativeEntry>)>;
struct Candidate {
    path: PathBuf,
    endpoint: AttachmentEndpoint,
    profile_root: PathBuf,
    modified: SystemTime,
    ancestry: Ancestry,
    file: NativeEntry,
    stamp: ReadStamp,
}
impl Candidate {
    fn validate(&self) -> Result<()> {
        for (path, entry) in &self.ancestry {
            entry.require_matches(path)?;
        }
        self.file.require_matches(&self.path)?;
        ensure!(
            read_stamp(&self.file.handle)? == self.stamp,
            "Browser debugging metadata changed during discovery"
        );
        Ok(())
    }
}
fn roots(environment: &Environment) -> Result<Vec<PathBuf>> {
    let home = Path::new(
        environment
            .get(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
            .context("Browser attachment has no user home")?,
    );
    Ok(if cfg!(windows) {
        vec![
            environment
                .get("LOCALAPPDATA")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join("AppData/Local")),
        ]
    } else if cfg!(target_os = "macos") {
        vec![home.join("Library/Application Support")]
    } else if cfg!(target_os = "linux") {
        vec![home.join(".config"), home.join("snap")]
    } else {
        vec![]
    })
}
fn check(deadline: Instant, cancel: &CancellationToken) -> Result<Duration> {
    ensure!(
        !cancel.is_cancelled(),
        "Running browser attachment cancelled"
    );
    let remaining = deadline.saturating_duration_since(Instant::now());
    ensure!(
        !remaining.is_zero(),
        "Running browser attachment deadline reached"
    );
    Ok(remaining)
}
fn read_candidate(path: PathBuf, ancestry: &Ancestry, host: &str) -> Result<Candidate> {
    let (parent, directory) = ancestry
        .last()
        .context("Browser metadata parent is missing")?;
    let mut file = directory.open_child(
        parent,
        path.file_name()
            .context("Browser metadata filename is missing")?,
        false,
    )?;
    let before = read_stamp(&file.handle)?;
    let mut raw = String::new();
    file.handle.by_ref().take(4097).read_to_string(&mut raw)?;
    ensure!(
        raw.len() <= 4096 && read_stamp(&file.handle)? == before,
        "Browser debugging metadata is oversized or changed while reading"
    );
    let mut lines = raw.lines();
    let port = lines
        .next()
        .context("Browser metadata port is missing")?
        .trim()
        .parse::<u16>()?;
    let browser_path = lines
        .next()
        .context("Browser metadata socket is missing")?
        .trim();
    let base = endpoint_base(host, port)?;
    let endpoint = AttachmentEndpoint {
        host: host.into(),
        port,
        browser_ws_endpoint: format!("ws://{}{browser_path}", base.trim_start_matches("http://")),
    };
    endpoint.validate()?;
    let parent = path
        .parent()
        .context("Browser metadata profile is missing")?;
    let profile_root = if parent
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case("Default"))
    {
        parent
            .parent()
            .context("Browser metadata profile root is missing")?
    } else {
        parent
    }
    .to_owned();
    let candidate = Candidate {
        modified: file.handle.metadata()?.modified()?,
        path,
        endpoint,
        profile_root,
        ancestry: ancestry.clone(),
        file,
        stamp: before,
    };
    candidate.validate()?;
    Ok(candidate)
}
async fn discover(
    roots: &[PathBuf],
    host: &str,
    port: u16,
    deadline: Instant,
    cancel: &CancellationToken,
) -> Result<Option<Candidate>> {
    let mut best: Option<Candidate> = None;
    let mut stack: Vec<_> = roots
        .iter()
        .rev()
        .map(|root| (root.clone(), Ancestry::new(), 0usize))
        .collect();
    let mut visited = 0usize;
    while let Some((path, mut ancestry, depth)) = stack.pop() {
        check(deadline, cancel)?;
        let directory = if let Some((parent, entry)) = ancestry.last() {
            entry.open_child(
                parent,
                path.file_name()
                    .context("Discovery directory has no name")?,
                true,
            )
        } else {
            NativeEntry::open(&path, true)
        };
        let Ok(directory) = directory else { continue };
        ancestry.push((path.clone(), Arc::new(directory)));
        if ancestry.iter().any(|(path, entry)| !entry.matches(path)) {
            continue;
        }
        #[cfg(target_os = "linux")]
        let listing = {
            use std::os::fd::AsRawFd;
            let directory = &ancestry.last().unwrap().1;
            PathBuf::from(format!("/proc/self/fd/{}", directory.handle.as_raw_fd()))
        };
        #[cfg(not(target_os = "linux"))]
        let listing = path.clone();
        let Ok(entries) = fs::read_dir(listing) else {
            continue;
        };
        for entry in entries.flatten() {
            visited += 1;
            ensure!(
                visited <= 100_000,
                "Browser metadata discovery exceeded its entry limit"
            );
            if visited.is_multiple_of(128) {
                tokio::task::yield_now().await;
            }
            check(deadline, cancel)?;
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let name = entry.file_name();
            if kind.is_dir() && depth < 6 {
                stack.push((path.join(&name), ancestry.clone(), depth + 1));
            }
            if kind.is_file() && name == "DevToolsActivePort" {
                let Ok(candidate) = read_candidate(path.join(name), &ancestry, host) else {
                    continue;
                };
                if candidate.endpoint.port == port
                    && best.as_ref().is_none_or(|previous| {
                        candidate.modified > previous.modified
                            || (candidate.modified == previous.modified
                                && candidate.path < previous.path)
                    })
                {
                    best = Some(candidate);
                }
            }
        }
    }
    if let Some(candidate) = &best {
        candidate.validate()?;
    }
    Ok(best)
}
pub(crate) async fn resolve(
    config: &Value,
    environment: &Environment,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<Attachment> {
    let deadline = Instant::now()
        .checked_add(timeout)
        .context("Running browser attachment timeout exceeds the monotonic clock")?;
    check(deadline, cancel)?;
    let (host, port) = if config["remoteChrome"].is_null() {
        ("127.0.0.1", 9222)
    } else {
        (
            config["remoteChrome"]["host"]
                .as_str()
                .context("Running browser host is missing")?,
            config["remoteChrome"]["port"]
                .as_u64()
                .and_then(|port| u16::try_from(port).ok())
                .context("Running browser port is invalid")?,
        )
    };
    let base = endpoint_base(host, port)?;
    let mut notes = Vec::new();
    if config["chromePath"]
        .as_str()
        .is_some_and(|path| !path.is_empty())
    {
        notes.push(
            "Note: --browser-chrome-path is ignored when --browser-attach-running is enabled."
                .to_owned(),
        );
    }
    notes.push(format!(
        "Using {} attach-running target {host}:{port}.",
        if config["remoteChrome"].is_null() {
            "default"
        } else {
            "explicit"
        }
    ));
    let roots = roots(environment)?;
    if let Some(candidate) = discover(&roots, host, port, deadline, cancel).await? {
        notes.push(format!(
            "Selected attach-running browser metadata from {}",
            candidate.path.display()
        ));
        return Ok(Attachment {
            endpoint: candidate.endpoint,
            profile_root: Some(candidate.profile_root),
            notes,
        });
    }
    let mut failure = None;
    for attempt in 0..2 {
        match AttachmentEndpoint::inspect(
            host,
            port,
            check(deadline, cancel)?.min(Duration::from_secs(1)),
            cancel,
        )
        .await
        {
            Ok(endpoint) => {
                return Ok(Attachment {
                    endpoint,
                    profile_root: None,
                    notes,
                });
            }
            Err(error) => failure = Some(error),
        }
        if attempt == 0 {
            let delay = check(deadline, cancel)?.min(Duration::from_millis(500));
            tokio::select! {biased;_=cancel.cancelled()=>{check(deadline,cancel)?;},_=tokio::time::sleep(delay)=>{}}
        }
    }
    Err(failure.context("Running browser endpoint probe failed")?).with_context(||format!("No running browser matched {host}:{port}; metadata searched {}; endpoint {base}/json/version failed",roots.iter().map(|path|path.display().to_string()).collect::<Vec<_>>().join(", ")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn environment(home: &Path) -> Environment {
        Environment::from([
            ("HOME".into(), home.to_string_lossy().into_owned()),
            ("USERPROFILE".into(), home.to_string_lossy().into_owned()),
            (
                "LOCALAPPDATA".into(),
                home.join("AppData/Local").to_string_lossy().into_owned(),
            ),
        ])
    }
    fn metadata(directory: &Path, port: u16, name: &str, modified: u64) -> PathBuf {
        fs::create_dir_all(directory).unwrap();
        let path = directory.join("DevToolsActivePort");
        fs::write(&path, format!("{port}\n/devtools/browser/{name}\n")).unwrap();
        fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(
                fs::FileTimes::new()
                    .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(modified)),
            )
            .unwrap();
        path
    }
    #[tokio::test]
    async fn metadata_selects_exact_port_newest_record_then_path_and_preserves_profile_root() {
        let home = tempfile::tempdir().unwrap();
        let env = environment(home.path());
        let root = roots(&env).unwrap().remove(0);
        metadata(&root.join("PUBLIC-old/Default"), 9222, "PUBLIC-old", 1000);
        metadata(&root.join("PUBLIC-z/Default"), 9222, "PUBLIC-z", 2000);
        metadata(&root.join("PUBLIC-a/Default"), 9222, "PUBLIC-a", 2000);
        metadata(&root.join("PUBLIC-other"), 9223, "PUBLIC-other", 3000);
        fs::write(
            root.join("PUBLIC-a/Default/PUBLIC-unrelated"),
            "PUBLIC not metadata",
        )
        .unwrap();
        let before = fs::read_dir(root.join("PUBLIC-a")).unwrap().count();
        let attached = resolve(
            &json!({"chromePath":"PUBLIC-ignored"}),
            &env,
            Duration::from_secs(3),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(attached.endpoint.browser_ws_endpoint.ends_with("/PUBLIC-a"));
        assert_eq!(
            attached.notes,
            [
                "Note: --browser-chrome-path is ignored when --browser-attach-running is enabled."
                    .to_owned(),
                "Using default attach-running target 127.0.0.1:9222.".to_owned(),
                format!(
                    "Selected attach-running browser metadata from {}",
                    root.join("PUBLIC-a")
                        .join("Default")
                        .join("DevToolsActivePort")
                        .display()
                ),
            ]
        );
        assert_eq!(attached.profile_root, Some(root.join("PUBLIC-a")));
        assert_eq!(fs::read_dir(root.join("PUBLIC-a")).unwrap().count(), before);
        let explicit = resolve(
            &json!({"remoteChrome":{"host":"::1","port":9223}}),
            &env,
            Duration::from_secs(3),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(
            explicit.endpoint.browser_ws_endpoint,
            "ws://[::1]:9223/devtools/browser/PUBLIC-other"
        );
        assert_eq!(
            explicit.notes[0],
            "Using explicit attach-running target ::1:9223."
        );
    }
    #[tokio::test]
    async fn metadata_reads_are_bounded_and_reject_changed_parent_identity() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join("PUBLIC-root");
        let original = root.join("PUBLIC-browser");
        metadata(&original, 9222, "PUBLIC-exact", 1000);
        let candidate = discover(
            std::slice::from_ref(&root),
            "127.0.0.1",
            9222,
            Instant::now() + Duration::from_secs(2),
            &CancellationToken::new(),
        )
        .await
        .unwrap()
        .unwrap();
        match fs::rename(&original, root.join("PUBLIC-old-browser")) {
            Ok(()) => {
                metadata(&original, 9222, "PUBLIC-foreign", 2000);
                assert!(candidate.validate().is_err());
            }
            Err(error) if cfg!(windows) && error.kind() == std::io::ErrorKind::PermissionDenied => {
                candidate.validate().unwrap();
            }
            Err(error) => panic!("Fixture directory replacement failed: {error}"),
        }
        drop(candidate);
        fs::write(
            original.join("DevToolsActivePort"),
            format!(
                "9224\n/devtools/browser/PUBLIC-oversized\n{}",
                "x".repeat(4097)
            ),
        )
        .unwrap();
        let deep = root.join("a/b/c/d/e/f/g");
        metadata(&deep, 9224, "PUBLIC-too-deep", 3000);
        assert!(
            discover(
                std::slice::from_ref(&root),
                "127.0.0.1",
                9224,
                Instant::now() + Duration::from_secs(2),
                &CancellationToken::new()
            )
            .await
            .unwrap()
            .is_none()
        );
        let token = CancellationToken::new();
        token.cancel();
        assert!(
            discover(
                &[root],
                "127.0.0.1",
                9222,
                Instant::now() + Duration::from_secs(2),
                &token
            )
            .await
            .is_err()
        );
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn discovery_ignores_directory_links_and_fifo_metadata() {
        use std::os::unix::{ffi::OsStrExt, fs::symlink};
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join("PUBLIC-root");
        fs::create_dir(&root).unwrap();
        let outside = home.path().join("PUBLIC-outside");
        metadata(&outside, 9222, "PUBLIC-linked", 1000);
        symlink(&outside, root.join("PUBLIC-link")).unwrap();
        let fifo = root.join("DevToolsActivePort");
        let name = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: the NUL-terminated path names only this fixture's fresh temporary directory.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        assert!(
            discover(
                &[root],
                "127.0.0.1",
                9222,
                Instant::now() + Duration::from_secs(2),
                &CancellationToken::new()
            )
            .await
            .unwrap()
            .is_none()
        );
    }
    #[tokio::test]
    async fn absent_metadata_probes_the_explicit_endpoint_with_one_bounded_retry() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let home = tempfile::tempdir().unwrap();
        let env = environment(home.path());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            for attempt in 0..2 {
                let (mut socket, _) =
                    tokio::time::timeout(Duration::from_secs(3), listener.accept())
                        .await
                        .unwrap()
                        .unwrap();
                let mut bytes = [0u8; 2048];
                let length = socket.read(&mut bytes).await.unwrap();
                assert!(
                    std::str::from_utf8(&bytes[..length])
                        .unwrap()
                        .starts_with("GET /json/version HTTP/1.1")
                );
                let body = if attempt == 0 {
                    "{}".to_owned()
                } else {
                    json!({"webSocketDebuggerUrl":format!("ws://127.0.0.1:{port}/devtools/browser/PUBLIC-http")}).to_string()
                };
                socket
                    .write_all(
                        format!(
                            "HTTP/1.1 {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            if attempt == 0 {
                                "502 Bad Gateway"
                            } else {
                                "200 OK"
                            },
                            body.len()
                        )
                        .as_bytes(),
                    )
                    .await
                    .unwrap();
            }
        });
        let result = resolve(
            &json!({"remoteChrome":{"host":"127.0.0.1","port":port}}),
            &env,
            Duration::from_secs(4),
            &CancellationToken::new(),
        )
        .await;
        server.await.unwrap();
        let result = result.unwrap();
        assert!(result.profile_root.is_none());
        assert!(
            result
                .endpoint
                .browser_ws_endpoint
                .ends_with("/PUBLIC-http")
        );
    }
}
