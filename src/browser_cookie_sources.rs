//! Choose one Chrome cookie source; never merge accounts or scan every profile.
use crate::{
    config::Environment,
    native_fs::{NativeEntry, read_stamp},
};
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::{
    fs,
    io::Read,
    path::{Component, Path, PathBuf},
};

pub(super) struct CookieSource {
    pub(super) database: PathBuf,
    pub(super) user_data: PathBuf,
    witnesses: Vec<Witness>,
}
struct Witness {
    path: PathBuf,
    entry: NativeEntry,
    stamp: Option<crate::native_fs::ReadStamp>,
}
fn capture(path: &Path, directory: bool) -> Result<Vec<Witness>> {
    ensure!(
        path.is_absolute(),
        "Chrome cookie source must resolve to an absolute path"
    );
    let mut current = PathBuf::new();
    let mut witnesses = vec![];
    for part in path.components() {
        current.push(part);
        if current.has_root() {
            witnesses.push(Witness {
                path: current.clone(),
                entry: NativeEntry::open(&current, current != path || directory)?,
                stamp: None,
            });
        }
    }
    Ok(witnesses)
}
impl CookieSource {
    fn seal(database: PathBuf, user_data: PathBuf, mut witnesses: Vec<Witness>) -> Result<Self> {
        witnesses.extend(capture(&database, false)?);
        let source = Self {
            database,
            user_data,
            witnesses,
        };
        source.verify()?;
        Ok(source)
    }
    pub(super) fn verify(&self) -> Result<()> {
        ensure!(
            self.database.starts_with(&self.user_data),
            "Chrome cookie database left its selected user-data root"
        );
        for witness in &self.witnesses {
            witness.entry.require_matches(&witness.path)?;
            if let Some(stamp) = &witness.stamp {
                ensure!(
                    &read_stamp(&witness.entry.handle)? == stamp,
                    "Chrome profile selection changed after resolution"
                );
            }
        }
        Ok(())
    }
    #[cfg(any(windows, test))]
    pub(super) fn read_local_state(&mut self) -> Result<zeroize::Zeroizing<Vec<u8>>> {
        self.verify()?;
        let root = &self
            .witnesses
            .iter()
            .find(|w| w.path == self.user_data)
            .context("Chrome key source has no retained user-data root")?
            .entry;
        let path = self.user_data.join("Local State");
        let mut entry =
            root.open_child(&self.user_data, std::ffi::OsStr::new("Local State"), false)?;
        let stamp = read_stamp(&entry.handle)?;
        let mut bytes = zeroize::Zeroizing::new(Vec::new());
        Read::by_ref(&mut entry.handle)
            .take(crate::browser_auth::MAX_COOKIE_INPUT as u64 + 1)
            .read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() <= crate::browser_auth::MAX_COOKIE_INPUT,
            "Chrome Local State exceeds its key input limit"
        );
        entry.require_matches(&path)?;
        ensure!(
            read_stamp(&entry.handle)? == stamp,
            "Chrome key source changed while reading"
        );
        self.witnesses.push(Witness {
            path,
            entry,
            stamp: Some(stamp),
        });
        self.verify()?;
        Ok(bytes)
    }
}
fn normalise(path: &Path) -> PathBuf {
    let mut output = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                if output.file_name().is_some() {
                    output.pop();
                }
            }
            _ => output.push(part),
        }
    }
    output
}
fn home(env: &Environment) -> Result<&str> {
    env.get(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .map(String::as_str)
        .filter(|home| !home.trim().is_empty())
        .context("Chrome cookie extraction has no user home")
}
fn roots(env: &Environment) -> Result<Vec<PathBuf>> {
    #[cfg(windows)]
    {
        Ok(vec![
            PathBuf::from(
                env.get("LOCALAPPDATA")
                    .context("Chrome cookie extraction has no local application directory")?,
            )
            .join("Google/Chrome/User Data"),
        ])
    }
    #[cfg(target_os = "linux")]
    {
        let home = PathBuf::from(home(env)?);
        let config = env
            .get("XDG_CONFIG_HOME")
            .filter(|s| !s.trim().is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"));
        Ok(vec![
            config.join("google-chrome"),
            home.join(".var/app/com.google.Chrome/config/google-chrome"),
        ])
    }
    #[cfg(target_os = "macos")]
    {
        Ok(vec![
            PathBuf::from(home(env)?).join("Library/Application Support/Google/Chrome"),
        ])
    }
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    anyhow::bail!("Chrome cookie extraction is unavailable on this platform")
}
fn entry_kind(path: &Path) -> Result<Option<bool>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            ensure!(
                !metadata.file_type().is_symlink(),
                "Chrome cookie source cannot be a link"
            );
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                ensure!(
                    metadata.file_attributes()
                        & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
                        == 0,
                    "Chrome cookie source cannot be a reparse point"
                );
            }
            ensure!(
                metadata.is_file() || metadata.is_dir(),
                "Chrome cookie source has an unsupported type"
            );
            Ok(Some(metadata.is_dir()))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => anyhow::bail!("Chrome cookie source is inaccessible"),
    }
}
fn database(profile: &Path) -> Result<Option<PathBuf>> {
    let order = if cfg!(windows) {
        ["Network/Cookies", "Cookies"]
    } else {
        ["Cookies", "Network/Cookies"]
    };
    for relative in order {
        let path = profile.join(relative);
        if let Some(directory) = entry_kind(&path)? {
            ensure!(!directory, "Chrome cookie database must be a regular file");
            return Ok(Some(path));
        }
    }
    Ok(None)
}
fn alias(root: &Path, requested: &str, witnesses: &mut Vec<Witness>) -> Result<Option<String>> {
    let path = root.join("Local State");
    if entry_kind(&path)?.is_none() {
        return Ok(None);
    }
    let mut entry = NativeEntry::open(&path, false)?;
    let stamp = read_stamp(&entry.handle)?;
    let mut bytes = zeroize::Zeroizing::new(Vec::new());
    Read::by_ref(&mut entry.handle)
        .take(crate::browser_auth::MAX_COOKIE_INPUT as u64 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= crate::browser_auth::MAX_COOKIE_INPUT,
        "Chrome Local State exceeds its selection limit"
    );
    let state: Value = serde_json::from_slice(&bytes)
        .map_err(|_| anyhow::anyhow!("Chrome Local State is not valid JSON"))?;
    entry.require_matches(&path)?;
    ensure!(
        read_stamp(&entry.handle)? == stamp,
        "Chrome profile selection changed while reading"
    );
    let matches = state["profile"]["info_cache"]
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(_, value)| value["name"] == requested)
        .map(|(name, _)| name.clone())
        .collect::<Vec<_>>();
    ensure!(
        matches.len() <= 1,
        "Chrome profile display name is ambiguous; choose its directory explicitly"
    );
    if let Some(name) = matches.first() {
        direct_name(name)?;
    }
    witnesses.push(Witness {
        path,
        entry,
        stamp: Some(stamp),
    });
    Ok(matches.into_iter().next())
}
fn direct_name(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && !name.contains(['/', '\\', ':', '\0'])
            && Path::new(name).components().count() == 1
            && matches!(
                Path::new(name).components().next(),
                Some(Component::Normal(_))
            ),
        "Chrome profile selection must identify one direct profile directory"
    );
    Ok(())
}
pub(super) fn select(config: &Value, env: &Environment, cwd: &Path) -> Result<CookieSource> {
    for key in ["chromeCookiePath", "chromeProfile"] {
        ensure!(
            config[key].is_null() || config[key].is_string(),
            "Chrome cookie source selection must be a string"
        );
    }
    let selected = config["chromeCookiePath"]
        .as_str()
        .or(config["chromeProfile"].as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if let Some(selected) =
        selected.filter(|s| s.contains(['/', '\\']) || Path::new(s).is_absolute())
    {
        let path = if let Some(suffix) = selected
            .strip_prefix("~/")
            .or_else(|| selected.strip_prefix("~\\"))
        {
            PathBuf::from(home(env)?).join(suffix)
        } else {
            cwd.join(selected)
        };
        let path = normalise(&path);
        ensure!(
            path.is_absolute(),
            "Chrome cookie source must resolve to an absolute path"
        );
        let database = match entry_kind(&path)? {
            Some(false) => path,
            Some(true) => {
                database(&path)?.context("Selected Chrome profile has no cookie database")?
            }
            None => anyhow::bail!("Selected Chrome cookie source is missing"),
        };
        let directory = database
            .parent()
            .context("Selected Chrome cookie database has no parent")?;
        let profile = if directory.file_name().is_some_and(|name| name == "Network") {
            directory
                .parent()
                .context("Selected Chrome profile is missing")?
        } else {
            directory
        };
        let user_data = profile
            .parent()
            .context("Selected Chrome user-data directory is missing")?
            .to_owned();
        return CookieSource::seal(database, user_data, vec![]);
    }
    let requested = selected.unwrap_or("Default");
    direct_name(requested)?;
    for root in roots(env)? {
        let root = normalise(&cwd.join(root));
        if entry_kind(&root)?.is_none() {
            continue;
        }
        let mut witnesses = capture(&root, true)?;
        let direct = root.join(requested);
        let profile = match entry_kind(&direct)? {
            Some(true) => direct,
            Some(false) => anyhow::bail!("Selected Chrome profile is not a directory"),
            None if requested == "Default" => continue,
            None => match alias(&root, requested, &mut witnesses)? {
                Some(name) => root.join(name),
                None => continue,
            },
        };
        let database =
            database(&profile)?.context("Selected Chrome profile has no cookie database")?;
        return CookieSource::seal(database, root, witnesses);
    }
    anyhow::bail!("Selected Chrome cookie profile was not found")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn fixture() -> (tempfile::TempDir, Environment, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let env = Environment::from_iter([
            ("HOME".into(), temp.path().to_string_lossy().into_owned()),
            (
                "USERPROFILE".into(),
                temp.path().to_string_lossy().into_owned(),
            ),
            (
                "LOCALAPPDATA".into(),
                temp.path().to_string_lossy().into_owned(),
            ),
        ]);
        let root = roots(&env).unwrap().remove(0);
        fs::create_dir_all(&root).unwrap();
        (temp, env, root)
    }
    fn profile(root: &Path, name: &str) -> PathBuf {
        let path = root.join(name).join(if cfg!(windows) {
            "Network/Cookies"
        } else {
            "Cookies"
        });
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"PUBLIC synthetic database placeholder").unwrap();
        path
    }
    #[test]
    fn default_is_default_and_an_explicit_directory_or_unique_alias_selects_one_account() {
        let (temp, env, root) = fixture();
        let first = profile(&root, "Default");
        let second = profile(&root, "Profile 2");
        fs::write(root.join("Local State"),r#"{"profile":{"last_used":"Profile 2","info_cache":{"Profile 2":{"name":"PUBLIC-selected"}}}}"#).unwrap();
        assert_eq!(
            select(&json!({}), &env, temp.path()).unwrap().database,
            first
        );
        for choice in ["Profile 2", "PUBLIC-selected"] {
            let source = select(&json!({"chromeProfile":choice}), &env, temp.path()).unwrap();
            assert_eq!(source.database, second);
            assert_eq!(source.user_data, root);
        }
    }
    #[test]
    fn explicit_paths_take_precedence_and_missing_or_ambiguous_choices_never_fall_back() {
        let (temp, env, root) = fixture();
        profile(&root, "Default");
        let second = profile(&root, "Profile 2");
        let source = select(
            &json!({"chromeCookiePath":second,"chromeProfile":"Default"}),
            &env,
            temp.path(),
        )
        .unwrap();
        assert_eq!(source.database, second);
        assert!(
            select(
                &json!({"chromeCookiePath":temp.path().join("PUBLIC-missing")}),
                &env,
                temp.path()
            )
            .is_err()
        );
        fs::write(root.join("Local State"),r#"{"profile":{"info_cache":{"Profile 2":{"name":"PUBLIC-ambiguous"},"Profile 3":{"name":"PUBLIC-ambiguous"}}}}"#).unwrap();
        assert!(
            select(
                &json!({"chromeProfile":"PUBLIC-ambiguous"}),
                &env,
                temp.path()
            )
            .is_err()
        );
        assert!(select(&json!({"chromeProfile":false}), &env, temp.path()).is_err());
    }
    #[test]
    fn default_never_resolves_through_another_profiles_display_name() {
        let (temp, env, root) = fixture();
        profile(&root, "Profile 2");
        fs::write(
            root.join("Local State"),
            r#"{"profile":{"info_cache":{"Profile 2":{"name":"Default"}}}}"#,
        )
        .unwrap();
        assert!(select(&json!({}), &env, temp.path()).is_err());
        assert!(select(&json!({"chromeProfile":"Default"}), &env, temp.path()).is_err());
    }
    #[test]
    fn alias_selection_refuses_later_state_changes_and_database_replacement() {
        let (temp, env, root) = fixture();
        let database = profile(&root, "Profile 2");
        let state = root.join("Local State");
        fs::write(
            &state,
            r#"{"profile":{"info_cache":{"Profile 2":{"name":"PUBLIC-selected"}}}}"#,
        )
        .unwrap();
        let source = select(
            &json!({"chromeProfile":"PUBLIC-selected"}),
            &env,
            temp.path(),
        )
        .unwrap();
        fs::write(
            &state,
            r#"{"profile":{"info_cache":{"Profile 3":{"name":"PUBLIC-selected-replaced"}}}}"#,
        )
        .unwrap();
        assert!(source.verify().is_err());
        #[cfg(not(windows))]
        {
            let source = select(&json!({"chromeProfile":"Profile 2"}), &env, temp.path()).unwrap();
            fs::rename(&database, database.with_file_name("PUBLIC-original")).unwrap();
            fs::write(&database, b"PUBLIC replacement account").unwrap();
            assert!(source.verify().is_err());
        }
        #[cfg(windows)]
        let _ = database;
    }
    #[test]
    fn key_source_read_retains_the_same_local_state_until_database_extraction_finishes() {
        let (temp, env, root) = fixture();
        profile(&root, "Default");
        let state = root.join("Local State");
        fs::write(&state, b"PUBLIC synthetic key source").unwrap();
        let mut source = select(&json!({}), &env, temp.path()).unwrap();
        assert_eq!(
            &*source.read_local_state().unwrap(),
            b"PUBLIC synthetic key source"
        );
        fs::write(&state, b"PUBLIC changed key source with another length").unwrap();
        assert!(source.verify().is_err());
    }
}
