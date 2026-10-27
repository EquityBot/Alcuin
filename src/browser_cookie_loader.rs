//! Fresh, bounded ordinary Chrome extraction. Batches stay in the native worker.
use crate::{
    browser_cookie_crypto::CookieKey, browser_cookie_sources::CookieSource, config::Environment,
};
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    time::Instant,
};
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

fn check(deadline: Instant, cancel: &CancellationToken) -> Result<()> {
    ensure!(!cancel.is_cancelled(), "Chrome cookie extraction cancelled");
    ensure!(
        Instant::now() < deadline,
        "Chrome cookie extraction deadline reached"
    );
    Ok(())
}
async fn blocking<T: Send + 'static>(
    deadline: Instant,
    cancel: &CancellationToken,
    work: impl FnOnce(&CancellationToken) -> Result<T> + Send + 'static,
) -> Result<T> {
    check(deadline, cancel)?;
    let task_cancel = cancel.child_token();
    let _cancel_on_drop = task_cancel.clone().drop_guard();
    let worker = tokio::task::spawn_blocking(move || {
        check(deadline, &task_cancel)?;
        let result = work(&task_cancel)?;
        check(deadline, &task_cancel)?;
        Ok::<_, anyhow::Error>(result)
    });
    tokio::select! {
        biased;
        _ = cancel.cancelled() => anyhow::bail!("Chrome cookie extraction cancelled"),
        result = tokio::time::timeout_at(tokio::time::Instant::from_std(deadline),worker) =>
            result.context("Chrome cookie extraction deadline reached")?
                .context("Chrome cookie extraction worker failed")?,
    }
}
struct Keys {
    primary: CookieKey,
    current: Option<CookieKey>,
    empty: Option<CookieKey>,
}
impl Keys {
    fn decrypt(&self, bytes: &[u8], host: &str, version: u32) -> Option<Zeroizing<String>> {
        if !bytes.starts_with(b"v10") && !bytes.starts_with(b"v11") {
            return None;
        }
        let key = if cfg!(target_os = "linux") && bytes.starts_with(b"v11") {
            self.current.as_ref().or(self.empty.as_ref())?
        } else {
            &self.primary
        };
        key.decrypt(bytes, host, version)
            .ok()
            .or_else(|| self.empty.as_ref()?.decrypt(bytes, host, version).ok())
    }
}
fn initial_keys(source: &mut CookieSource) -> Result<Keys> {
    #[cfg(windows)]
    {
        let state = source.read_local_state()?;
        Ok(Keys {
            primary: CookieKey::windows(crate::browser_cookie_keys::windows_master_key(&state)?),
            current: None,
            empty: None,
        })
    }
    #[cfg(target_os = "linux")]
    {
        source.verify()?;
        Ok(Keys {
            primary: CookieKey::linux(b"peanuts"),
            current: None,
            empty: Some(CookieKey::linux(b"")),
        })
    }
    #[cfg(target_os = "macos")]
    {
        source.verify()?;
        let password = crate::browser_cookie_keys::macos_password()?;
        Ok(Keys {
            primary: CookieKey::macos(&password),
            current: None,
            empty: None,
        })
    }
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    anyhow::bail!("Chrome cookie extraction is unavailable on this platform")
}
/// Copied when `cookieNames` is unset: the session and Cloudflare cookies ChatGPT needs.
pub(crate) const DEFAULT_COOKIE_NAMES: [&str; 9] = [
    "__Secure-next-auth.session-token",
    "__Secure-next-auth.session-token.0",
    "__Secure-next-auth.session-token.1",
    "_account",
    "cf_clearance",
    "__cf_bm",
    "_cfuvid",
    "CF_Authorization",
    "__cflb",
];
fn names(config: &Value) -> Result<Option<BTreeSet<String>>> {
    if config["cookieNames"].is_null() {
        return Ok(Some(
            DEFAULT_COOKIE_NAMES
                .iter()
                .map(|name| name.to_string())
                .collect(),
        ));
    }
    let names = config["cookieNames"]
        .as_array()
        .context("Chrome cookie names must be an array")?;
    ensure!(
        names.len() <= 4096,
        "Chrome cookie name filter exceeds its limit"
    );
    names
        .iter()
        .map(|name| {
            let name = name
                .as_str()
                .filter(|name| {
                    !name.is_empty() && name.len() <= 4096 && !name.chars().any(char::is_control)
                })
                .context("Chrome cookie name filter contains an invalid name")?;
            Ok(name.to_owned())
        })
        .collect::<Result<BTreeSet<_>>>()
        .map(Some)
}
pub(super) async fn read_native(
    config: &Value,
    environment: &Environment,
    cwd: &Path,
    deadline: Instant,
    cancel: &CancellationToken,
) -> Result<Vec<Value>> {
    check(deadline, cancel)?;
    ensure!(
        config["cookieSync"] == true || config["manualLoginCookieSync"] == true,
        "Ordinary Chrome cookie extraction requires an explicit sync choice"
    );
    ensure!(
        config["copyProfileSource"].is_null()
            && config["inlineCookies"].is_null()
            && config["cookieBootstrapRequired"].is_null()
            && config["attachRunning"] != true
            && !config["remoteChrome"].is_object(),
        "Ordinary Chrome cookie extraction cannot replace another selected source or seed a borrowed browser"
    );
    let selected_names = names(config)?;
    let requested = config.clone();
    let env = environment.clone();
    let directory = cwd.to_owned();
    let (source, keys) = blocking(deadline, cancel, move |_| {
        let mut source = crate::browser_cookie_sources::select(&requested, &env, &directory)?;
        let keys = initial_keys(&mut source)?;
        source.verify()?;
        Ok((source, keys))
    })
    .await?;
    #[cfg(target_os = "linux")]
    let keys = {
        let mut keys = keys;
        match environment
            .get("SWEET_COOKIE_LINUX_KEYRING")
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
        {
            Some("basic") => keys.current = Some(CookieKey::linux(b"")),
            Some("kwallet") => {
                anyhow::bail!("KWallet Chrome cookie extraction remains unavailable")
            }
            None | Some("gnome") => {
                let password = crate::browser_cookie_keys::linux_password(deadline, cancel).await;
                check(deadline, cancel)?;
                if let Ok(password) = password {
                    keys.current = Some(CookieKey::linux(&password));
                }
            }
            Some(_) => anyhow::bail!("Unknown Chrome cookie keyring backend"),
        }
        keys
    };
    let base = environment
        .get(if cfg!(windows) { "TEMP" } else { "TMPDIR" })
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let base = cwd.join(base);
    let rows = blocking(deadline, cancel, move |token| {
        source.verify()?;
        crate::browser_cookie_extract::read_database(
            &source,
            &base,
            selected_names.as_ref(),
            deadline,
            token,
            |bytes, host, version| keys.decrypt(bytes, host, version),
        )
    })
    .await?;
    ensure!(!rows.is_empty(), crate::browser_auth::NO_PROFILE_COOKIES);
    Ok(rows)
}

/// Synthetic Chrome source: one encrypted `.chatgpt.com` cookie and one unrelated row.
#[cfg(all(test, any(windows, target_os = "linux")))]
pub(crate) fn synthetic_source(home: &Path) -> (PathBuf, Environment) {
    let root = home.join("PUBLIC-Chrome");
    let source = root.join("Default/Network/Cookies");
    std::fs::create_dir_all(source.parent().unwrap()).unwrap();
    #[cfg(windows)]
    std::fs::write(
        root.join("Local State"),
        crate::browser_cookie_keys::synthetic_local_state(),
    )
    .unwrap();
    let hex = if cfg!(windows) {
        "7631302424242424242424242424241792d43a8fc0d3add833278b0c41f5376f5ea3889d1dae96fa7c9c32a95aec82525e13c93407877fba29dbde3783d4f3d4cc623e1bd975878e24f7a1a1d2569143d0fc"
    } else {
        "763130b6bfa63c66774b16309c3b492b56bc030f5dbcf34e106dd42f933295edd1dec4e164c358c7f2b717359ba1a626ceb27fc4797ce337e7360a34da93a063f1b9ca"
    };
    let encrypted: Vec<u8> = hex
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect();
    let db = rusqlite::Connection::open(&source).unwrap();
    db.execute_batch("CREATE TABLE meta(key TEXT,value TEXT);INSERT INTO meta VALUES('version','24');CREATE TABLE cookies(host_key TEXT,name TEXT,value TEXT,encrypted_value BLOB,path TEXT,expires_utc INTEGER,is_secure INTEGER,is_httponly INTEGER,samesite INTEGER,top_frame_site_key TEXT);").unwrap();
    db.execute(
        "INSERT INTO cookies VALUES('.chatgpt.com','PUBLIC-session','',?1,'/',0,1,1,1,'')",
        [&encrypted],
    )
    .unwrap();
    db.execute("INSERT INTO cookies VALUES('.example.com','PUBLIC-unrelated','PUBLIC-other',?1,'/',0,1,1,1,'')",[Vec::<u8>::new()]).unwrap();
    drop(db);
    let temporary = home.join("PUBLIC-snapshots");
    let env = Environment::from([
        ("TEMP".into(), temporary.to_string_lossy().into_owned()),
        ("TMPDIR".into(), temporary.to_string_lossy().into_owned()),
        ("SWEET_COOKIE_LINUX_KEYRING".into(), "basic".into()),
    ]);
    (source, env)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[cfg(any(windows, target_os = "linux"))]
    #[tokio::test]
    async fn native_loader_extracts_only_the_selected_synthetic_account_and_removes_its_snapshot() {
        let home = tempfile::tempdir().unwrap();
        let (source, env) = synthetic_source(home.path());
        let original = std::fs::read(&source).unwrap();
        let temporary = home.path().join("PUBLIC-snapshots");
        let cookies = read_native(
            &json!({"cookieSync":true,"chromeCookiePath":source,"cookieNames":["PUBLIC-session"]}),
            &env,
            home.path(),
            Instant::now() + std::time::Duration::from_secs(5),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(cookies.len(), 1);
        assert_eq!(cookies[0]["name"], "PUBLIC-session");
        assert_eq!(cookies[0]["value"], "PUBLIC-cookie-value");
        assert_eq!(cookies[0]["domain"], ".chatgpt.com");
        assert_eq!(std::fs::read(&source).unwrap(), original);
        assert_eq!(std::fs::read_dir(&temporary).unwrap().count(), 0);
    }
    // src/browser/config.ts DEFAULT_CHATGPT_COOKIE_NAMES: unset names copy only the allowlist.
    #[cfg(any(windows, target_os = "linux"))]
    #[tokio::test]
    async fn unset_cookie_names_copy_only_the_default_chatgpt_allowlist() {
        let home = tempfile::tempdir().unwrap();
        let (source, env) = synthetic_source(home.path());
        let db = rusqlite::Connection::open(&source).unwrap();
        // Chrome binds the ciphertext to the host, so the session blob decrypts under any name.
        db.execute("INSERT INTO cookies SELECT host_key,'cf_clearance',value,encrypted_value,path,expires_utc,is_secure,is_httponly,samesite,top_frame_site_key FROM cookies WHERE name='PUBLIC-session'",[]).unwrap();
        drop(db);
        let cookies = read_native(
            &json!({"cookieSync":true,"chromeCookiePath":source}),
            &env,
            home.path(),
            Instant::now() + std::time::Duration::from_secs(5),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        let names: Vec<_> = cookies.iter().map(|cookie| &cookie["name"]).collect();
        assert_eq!(names, ["cf_clearance"]);
    }
    #[test]
    fn queued_extraction_keeps_the_original_deadline_and_never_enters_late_source_work() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(1)
            .build()
            .unwrap();
        let entered = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        runtime.block_on(async {
            let (started, running) = tokio::sync::oneshot::channel();
            let (release, held) = std::sync::mpsc::channel();
            let blocker = tokio::task::spawn_blocking(move || {
                started.send(()).unwrap();
                held.recv().unwrap();
            });
            running.await.unwrap();
            let observed = entered.clone();
            let cancel = CancellationToken::new();
            let extraction = blocking(
                Instant::now() + std::time::Duration::from_millis(50),
                &cancel,
                move |_| {
                    observed.store(true, std::sync::atomic::Ordering::SeqCst);
                    Ok(())
                },
            );
            let release_after_expiry = async {
                tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                release.send(()).unwrap();
            };
            let (result, ()) = tokio::join!(extraction, release_after_expiry);
            assert!(result.is_err());
            blocker.await.unwrap();
        });
        runtime.shutdown_timeout(std::time::Duration::from_secs(1));
        assert!(!entered.load(std::sync::atomic::Ordering::SeqCst));
    }
    #[tokio::test]
    async fn missing_opt_in_conflicts_and_expired_or_cancelled_work_stop_before_source_access() {
        let env = Environment::new();
        let cwd = Path::new("PUBLIC-path-that-does-not-exist");
        for config in [
            json!({}),
            json!({"cookieSync":true,"attachRunning":true}),
            json!({"cookieSync":true,"copyProfileSource":"PUBLIC-source"}),
            json!({"cookieSync":true,"cookieBootstrapRequired":{"version":1}}),
        ] {
            let error = read_native(
                &config,
                &env,
                cwd,
                Instant::now() + std::time::Duration::from_secs(5),
                &CancellationToken::new(),
            )
            .await
            .unwrap_err()
            .to_string();
            assert!(!error.contains("user home") && !error.contains("local application directory"));
        }
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(
            read_native(
                &json!({"cookieSync":true}),
                &env,
                cwd,
                Instant::now() + std::time::Duration::from_secs(5),
                &cancel
            )
            .await
            .unwrap_err()
            .to_string()
            .contains("cancelled")
        );
        assert!(
            read_native(
                &json!({"cookieSync":true}),
                &env,
                cwd,
                Instant::now(),
                &CancellationToken::new()
            )
            .await
            .unwrap_err()
            .to_string()
            .contains("deadline")
        );
    }
}
