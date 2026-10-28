//! Read only a stable private copy of the selected Chromium cookie database.
use crate::{
    browser_auth::MAX_COOKIE_INPUT,
    browser_profile_copy::TemporaryProfile,
    native_fs::{NativeEntry, ReadStamp, read_stamp},
};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OpenFlags, limits::Limit};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    ffi::OsString,
    fs,
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

const MAX_DATABASE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAX_ROWS: usize = 4096;
fn check(deadline: Instant, cancel: &CancellationToken) -> Result<()> {
    ensure!(!cancel.is_cancelled(), "Chrome cookie extraction cancelled");
    ensure!(
        Instant::now() < deadline,
        "Chrome cookie extraction deadline reached"
    );
    Ok(())
}
struct SnapshotFile {
    name: OsString,
    destination: String,
    entry: NativeEntry,
    stamp: ReadStamp,
}
struct Snapshot {
    profile: TemporaryProfile,
    path: PathBuf,
}
impl Snapshot {
    fn create(
        source: &Path,
        base: &Path,
        deadline: Instant,
        cancel: &CancellationToken,
    ) -> Result<Self> {
        Self::create_with(source, base, deadline, cancel, || {})
    }
    fn create_with(
        source: &Path,
        base: &Path,
        deadline: Instant,
        cancel: &CancellationToken,
        before_copy: impl FnOnce(),
    ) -> Result<Self> {
        check(deadline, cancel)?;
        ensure!(
            source.is_absolute(),
            "Chrome cookie database must resolve to an absolute path"
        );
        let parent = source
            .parent()
            .context("Chrome cookie database has no parent")?;
        let leaf = source
            .file_name()
            .context("Chrome cookie database has no filename")?;
        let mut current = PathBuf::new();
        let mut ancestors = vec![];
        for component in parent.components() {
            ensure!(
                !matches!(component, Component::ParentDir | Component::CurDir),
                "Chrome cookie database path must be normalised"
            );
            current.push(component);
            if current.has_root() {
                ancestors.push((current.clone(), NativeEntry::open(&current, true)?));
            }
        }
        let directory = &ancestors
            .last()
            .context("Chrome cookie database parent is missing")?
            .1;
        let mut files = vec![];
        let mut absent = vec![];
        let mut total = 0u64;
        for suffix in ["", "-wal", "-shm"] {
            check(deadline, cancel)?;
            let mut name = leaf.to_os_string();
            name.push(suffix);
            let path = parent.join(&name);
            let entry = match directory.open_child(parent, &name, false) {
                Ok(entry) => entry,
                Err(_)
                    if !suffix.is_empty()
                        && matches!(fs::symlink_metadata(&path), Err(error) if error.kind() == std::io::ErrorKind::NotFound) =>
                {
                    absent.push(name);
                    continue;
                }
                Err(_) => anyhow::bail!("Chrome cookie database entry is unavailable or unsafe"),
            };
            let stamp = read_stamp(&entry.handle)?;
            total = total
                .checked_add(entry.handle.metadata()?.len())
                .context("Chrome cookie database size overflow")?;
            ensure!(
                total <= MAX_DATABASE_BYTES,
                "Chrome cookie database exceeds its snapshot limit"
            );
            files.push(SnapshotFile {
                name,
                destination: format!("Cookies{suffix}"),
                entry,
                stamp,
            });
        }
        let profile = TemporaryProfile::create(base)?;
        before_copy();
        let mut copied = 0u64;
        for file in &mut files {
            let mut output = profile.create_file(std::ffi::OsStr::new(&file.destination))?;
            let mut buffer = Zeroizing::new([0u8; 64 * 1024]);
            loop {
                check(deadline, cancel)?;
                let count = file.entry.handle.read(&mut buffer[..])?;
                if count == 0 {
                    break;
                }
                copied += count as u64;
                ensure!(
                    copied <= MAX_DATABASE_BYTES,
                    "Chrome cookie database changed beyond its snapshot limit"
                );
                output.write_all(&buffer[..count])?;
            }
            output.sync_all()?;
        }
        // Capture all stamps before copying and verify all after copying. This
        // requires a shared stable interval across the database and both sidecars.
        for file in &files {
            file.entry.require_matches(&parent.join(&file.name))?;
            ensure!(
                read_stamp(&file.entry.handle)? == file.stamp,
                "Chrome cookie database changed during extraction"
            );
        }
        for name in absent {
            ensure!(
                matches!(fs::symlink_metadata(parent.join(name)), Err(error) if error.kind() == std::io::ErrorKind::NotFound),
                "Chrome cookie database sidecars changed during extraction"
            );
        }
        for (path, entry) in ancestors {
            entry.require_matches(&path)?;
        }
        check(deadline, cancel)?;
        Ok(Self {
            path: profile.path().join("Cookies"),
            profile,
        })
    }
}

pub(super) fn read_database(
    source: &crate::browser_cookie_sources::CookieSource,
    temporary: &Path,
    names: Option<&BTreeSet<String>>,
    deadline: Instant,
    cancel: &CancellationToken,
    mut decrypt: impl FnMut(&[u8], &str, u32) -> Option<Zeroizing<String>>,
) -> Result<Vec<Value>> {
    source.verify()?;
    let mut snapshot = Snapshot::create(&source.database, temporary, deadline, cancel)?;
    let result = (|| {
        let mut connection = Connection::open_with_flags(
            &snapshot.path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        connection.busy_timeout(Duration::ZERO)?;
        connection.set_limit(Limit::SQLITE_LIMIT_LENGTH, MAX_COOKIE_INPUT as i32)?;
        connection.set_limit(Limit::SQLITE_LIMIT_SQL_LENGTH, 64 * 1024)?;
        connection.set_limit(Limit::SQLITE_LIMIT_COLUMN, 200)?;
        let interrupted = cancel.clone();
        connection.progress_handler(
            1000,
            Some(move || interrupted.is_cancelled() || Instant::now() >= deadline),
        )?;
        connection.pragma_update(None, "trusted_schema", false)?;
        connection.pragma_update(None, "query_only", true)?;
        check(deadline, cancel)?;
        let version: String =
            connection.query_row("SELECT value FROM meta WHERE key='version'", [], |row| {
                row.get(0)
            })?;
        let version = version
            .parse::<u32>()
            .context("Unsupported Chrome cookie database version")?;
        let partitioned =
            connection.column_exists(None::<&str>, "cookies", "top_frame_site_key")?;
        let partition_column = if partitioned {
            "COALESCE(top_frame_site_key,'')"
        } else {
            "''"
        };
        let query = format!(
            "SELECT host_key,name,value,encrypted_value,path,expires_utc,is_secure,is_httponly,samesite,{partition_column} FROM cookies WHERE host_key IN ('chatgpt.com','.chatgpt.com','chat.openai.com','.chat.openai.com','atlas.openai.com','.atlas.openai.com','.openai.com')"
        );
        let mut statement = connection.prepare(&query)?;
        let mut rows = statement.query([])?;
        let mut cookies = vec![];
        let mut seen = BTreeSet::new();
        let mut examined = 0usize;
        let mut bytes = 2usize;
        let now = chrono::Utc::now().timestamp();
        while let Some(row) = rows.next()? {
            check(deadline, cancel)?;
            examined += 1;
            ensure!(
                examined <= MAX_ROWS,
                "Chrome cookie database exceeds its selected row limit"
            );
            let host: String = row.get(0)?;
            let name: String = row.get(1)?;
            let partition: String = row.get(9)?;
            if name.is_empty()
                || !partition.is_empty()
                || names.is_some_and(|names| !names.is_empty() && !names.contains(&name))
            {
                continue;
            }
            let expires: i64 = row.get(5)?;
            let expires = (expires > 0).then(|| expires / 1_000_000 - 11_644_473_600);
            if expires.is_some_and(|expires| expires < now) {
                continue;
            }
            let mut value = Zeroizing::new(row.get::<_, String>(2)?);
            if value.is_empty() {
                let encrypted = Zeroizing::new(row.get::<_, Vec<u8>>(3)?);
                let Some(decoded) = decrypt(&encrypted, &host, version) else {
                    continue;
                };
                value = decoded;
            }
            if !seen.insert((host.clone(), name.clone())) {
                continue;
            }
            let same_site: i64 = row.get(8)?;
            let mut cookie = json!({"name":name,"value":&*value,"domain":host,"path":row.get::<_,String>(4)?,"secure":row.get::<_,i64>(6)? != 0,"httpOnly":row.get::<_,i64>(7)? != 0});
            if let Some(expires) = expires {
                cookie["expires"] = json!(expires);
            }
            if let Some(same_site) = match same_site {
                0 => Some("None"),
                1 => Some("Lax"),
                2 => Some("Strict"),
                _ => None,
            } {
                cookie["sameSite"] = json!(same_site);
            }
            bytes = bytes
                .checked_add(serde_json::to_vec(&cookie)?.len() + usize::from(!cookies.is_empty()))
                .context("Chrome cookie batch size overflow")?;
            ensure!(
                bytes <= MAX_COOKIE_INPUT,
                "Chrome cookie batch exceeds its memory limit"
            );
            cookies.push(cookie);
        }
        check(deadline, cancel)?;
        source.verify()?;
        Ok(cookies)
    })();
    let cleaned = snapshot.profile.cleanup();
    // SQLite diagnostics can contain row values. Expose only fixed extraction errors.
    let result = result.map_err(|_: anyhow::Error| {
        anyhow::anyhow!("Chrome cookie database extraction failed or exceeded its limits")
    });
    cleaned?;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::browser_cookie_crypto::CookieKey;
    fn database(path: &Path, wal: bool) -> Connection {
        let db = Connection::open(path).unwrap();
        if wal {
            db.pragma_update(None, "journal_mode", "WAL").unwrap();
            db.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
        }
        db.execute_batch("CREATE TABLE meta(key TEXT,value TEXT); INSERT INTO meta VALUES('version','24'); CREATE TABLE cookies(host_key TEXT,name TEXT,value TEXT,encrypted_value BLOB,path TEXT,expires_utc INTEGER,is_secure INTEGER,is_httponly INTEGER,samesite INTEGER,top_frame_site_key TEXT);").unwrap();
        db
    }
    fn row(
        db: &Connection,
        host: &str,
        name: &str,
        value: &str,
        encrypted: &[u8],
        expires: i64,
        partition: &str,
    ) {
        db.execute(
            "INSERT INTO cookies VALUES(?1,?2,?3,?4,'/',?5,1,1,1,?6)",
            rusqlite::params![host, name, value, encrypted, expires, partition],
        )
        .unwrap();
    }
    #[test]
    fn stable_wal_snapshot_selects_only_usable_requested_provider_cookies_without_source_writes() {
        let home = tempfile::tempdir().unwrap();
        let source = home.path().join("PUBLIC-Cookies");
        let temporary = home.path().join("PUBLIC-copies");
        let db = database(&source, true);
        row(
            &db,
            ".chatgpt.com",
            "PUBLIC-first",
            "PUBLIC-value",
            &[],
            0,
            "",
        );
        row(
            &db,
            ".chatgpt.com",
            "PUBLIC-first",
            "PUBLIC-duplicate",
            &[],
            0,
            "",
        );
        row(
            &db,
            ".example.com",
            "PUBLIC-private",
            "PUBLIC-unrelated",
            &[],
            0,
            "",
        );
        row(
            &db,
            ".openai.com",
            "PUBLIC-parent",
            "PUBLIC-shared-provider",
            &[],
            0,
            "",
        );
        row(
            &db,
            ".chatgpt.com",
            "PUBLIC-expired",
            "PUBLIC-old",
            &[],
            11_644_473_601_000_000,
            "",
        );
        row(
            &db,
            ".chatgpt.com",
            "PUBLIC-partitioned",
            "PUBLIC-partition",
            &[],
            0,
            "https://example.com",
        );
        row(
            &db,
            ".chatgpt.com",
            "PUBLIC-unreadable",
            "",
            b"v20PUBLIC",
            0,
            "",
        );
        row(
            &db,
            ".chatgpt.com",
            "PUBLIC-not-requested",
            "PUBLIC-other",
            &[],
            0,
            "",
        );
        let encrypted = "763130b6bfa63c66774b16309c3b492b56bc030f5dbcf34e106dd42f933295edd1dec4e164c358c7f2b717359ba1a626ceb27fc4797ce337e7360a34da93a063f1b9ca";
        let encrypted: Vec<u8> = encrypted
            .as_bytes()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|p| u8::from_str_radix(std::str::from_utf8(p).unwrap(), 16).unwrap())
            .collect();
        row(
            &db,
            ".chatgpt.com",
            "PUBLIC-encrypted",
            "",
            &encrypted,
            0,
            "",
        );
        let before = [
            source.clone(),
            source.with_file_name("PUBLIC-Cookies-wal"),
            source.with_file_name("PUBLIC-Cookies-shm"),
        ]
        .map(|p| fs::read(p).unwrap());
        let names = [
            "PUBLIC-first",
            "PUBLIC-encrypted",
            "PUBLIC-expired",
            "PUBLIC-partitioned",
            "PUBLIC-unreadable",
            "PUBLIC-parent",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        let key = CookieKey::linux(b"peanuts");
        let selected = crate::browser_cookie_sources::select(
            &json!({"chromeCookiePath":source}),
            &crate::config::Environment::new(),
            home.path(),
        )
        .unwrap();
        let cookies = read_database(
            &selected,
            &temporary,
            Some(&names),
            Instant::now() + Duration::from_secs(5),
            &CancellationToken::new(),
            |encrypted, host, version| key.decrypt(encrypted, host, version).ok(),
        )
        .unwrap();
        assert_eq!(cookies.len(), 3);
        assert_eq!(cookies[0]["value"], "PUBLIC-value");
        assert_eq!(cookies[1]["domain"], ".openai.com");
        assert_eq!(cookies[1]["value"], "PUBLIC-shared-provider");
        assert_eq!(cookies[2]["value"], "PUBLIC-cookie-value");
        assert_eq!(cookies[2]["sameSite"], "Lax");
        for (p, b) in [
            source.clone(),
            source.with_file_name("PUBLIC-Cookies-wal"),
            source.with_file_name("PUBLIC-Cookies-shm"),
        ]
        .into_iter()
        .zip(before)
        {
            assert_eq!(fs::read(p).unwrap(), b);
        }
        assert_eq!(fs::read_dir(&temporary).unwrap().count(), 0);
        drop(db);
    }
    #[test]
    fn snapshot_refuses_mutation_and_cancellation_and_removes_partial_private_files() {
        let home = tempfile::tempdir().unwrap();
        let source = home.path().join("Cookies");
        let temporary = home.path().join("PUBLIC-copies");
        fs::write(&source, b"PUBLIC original").unwrap();
        let result = Snapshot::create_with(
            &source,
            &temporary,
            Instant::now() + Duration::from_secs(5),
            &CancellationToken::new(),
            || fs::write(&source, b"PUBLIC replacement with changed bytes").unwrap(),
        );
        assert!(result.is_err());
        assert_eq!(fs::read_dir(&temporary).unwrap().count(), 0);
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(
            Snapshot::create(
                &source,
                &temporary,
                Instant::now() + Duration::from_secs(5),
                &cancel
            )
            .is_err()
        );
        assert!(
            Snapshot::create(
                &source,
                &temporary,
                Instant::now(),
                &CancellationToken::new()
            )
            .is_err()
        );
        assert_eq!(
            fs::read(&source).unwrap(),
            b"PUBLIC replacement with changed bytes"
        );
    }
}
