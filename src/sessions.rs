//! Compatible on-disk records. JSON objects retain historical and producer-owned fields.
use crate::{
    native_fs::{NativeEntry, no_follow, reject_link},
    process::{Liveness, ProcessIdentity},
    storage::{
        DirectoryLock, atomic_json, atomic_json_until, private_dir, private_file, same_directory,
    },
};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;

pub fn valid_id(id: &str) -> bool {
    let mut chars = id.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && !id.ends_with('.')
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}
pub fn assert_id(id: &str) -> Result<()> {
    if !valid_id(id) {
        bail!("Session ID must be a directory leaf.");
    }
    Ok(())
}
pub fn create_id(prompt: &str, custom: Option<&str>) -> Result<String> {
    let input = custom
        .filter(|s| !s.is_empty())
        .unwrap_or(prompt)
        .to_lowercase();
    let words: Vec<String> = input
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|s| !s.is_empty())
        .take(5)
        .map(|s| s.chars().take(10).collect())
        .collect();
    if custom.is_some_and(|s| !s.is_empty()) && words.len() < 3 {
        bail!("Custom slug must include between 3 and 5 words.");
    }
    Ok(if words.is_empty() {
        "session".into()
    } else {
        words.join("-")
    })
}
pub fn safe_model_slug(model: &str) -> String {
    model
        .chars()
        .map(|c| match c {
            '/' | '\\' => "__".to_owned(),
            ':' | '*' | '?' | '"' | '<' | '>' | '|' => "_".to_owned(),
            _ => c.to_string(),
        })
        .collect()
}
pub fn terminal(status: &str) -> bool {
    matches!(status, "completed" | "partial" | "error" | "cancelled")
}
pub fn timestamp() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

#[derive(Clone, Debug)]
pub struct SessionStore {
    home: PathBuf,
}
pub(crate) struct WorkerCancellation {
    path: PathBuf,
    entry: NativeEntry,
}
impl WorkerCancellation {
    pub(crate) fn clear(self) -> Result<()> {
        crate::native_fs::remove_owned_file(&self.path, &self.entry)
    }
}
struct WriteDirectory {
    root_path: PathBuf,
    root: NativeEntry,
    path: PathBuf,
    directory: NativeEntry,
}
impl WriteDirectory {
    fn check(&self) -> Result<()> {
        self.root.require_matches(&self.root_path)?;
        self.directory.require_matches(&self.path)
    }
    fn leaf(&self, path: &Path) -> Result<()> {
        self.check()?;
        match reject_link(path) {
            Ok(metadata) => anyhow::ensure!(
                metadata.is_file(),
                "Session write requires a native regular file"
            ),
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) => {}
            Err(error) => return Err(error),
        }
        Ok(())
    }
    fn models(&self) -> Result<NativeEntry> {
        self.check()?;
        let path = self.path.join("models");
        private_dir(&path)?;
        NativeEntry::open(&path, true)
    }
}
impl SessionStore {
    pub fn new(home: impl Into<PathBuf>) -> Self {
        Self { home: home.into() }
    }
    pub fn home(&self) -> &Path {
        &self.home
    }
    pub(crate) fn cancel_launched_worker(
        &self,
        id: &str,
        worker: &ProcessIdentity,
    ) -> Result<Option<WorkerCancellation>> {
        let lock = self.transaction(id)?;
        let guard = self.write_directory(id)?;
        let current = self.stored_for_update(id)?;
        if terminal(current["status"].as_str().unwrap_or("")) {
            lock.release()?;
            return Ok(None);
        }
        anyhow::ensure!(
            current["lifecycle"]["detached"] == true
                && current["lifecycle"]["workerPid"] == worker.pid
                && current["lifecycle"]["workerProcessStartedAtMs"].as_f64()
                    == Some(worker.started_at_ms)
                && serde_json::from_value::<ProcessIdentity>(
                    current["lifecycle"]["processIdentity"].clone()
                )
                .is_ok_and(|identity| identity == *worker)
                && worker.liveness() == Liveness::Alive,
            "Launched worker ownership changed; cancellation was not requested"
        );
        let path = self
            .directory(id)?
            .join(format!(".cancel-requested-{}", worker.pid));
        guard.leaf(&path)?;
        let mut file = private_file(&path, true)?;
        file.write_all(&serde_json::to_vec(
            &json!({"sessionId":id,"processIdentity":worker}),
        )?)?;
        file.sync_all()?;
        let entry = NativeEntry::from_handle(file, false)?;
        entry.require_matches(&path)?;
        guard.check()?;
        lock.release()?;
        Ok(Some(WorkerCancellation { path, entry }))
    }
    pub fn root(&self) -> PathBuf {
        self.home.join("sessions")
    }
    pub fn directory(&self, id: &str) -> Result<PathBuf> {
        assert_id(id)?;
        Ok(self.root().join(id))
    }
    pub fn ensure_storage(&self) -> Result<()> {
        private_dir(&self.root())?;
        let _root = NativeEntry::open(&self.root(), true)?;
        #[cfg(unix)]
        harden_storage(&self.root(), _root)?;
        Ok(())
    }
    fn write_directory(&self, id: &str) -> Result<WriteDirectory> {
        let root_path = self.root();
        let path = self.directory(id)?;
        let root = NativeEntry::open(&root_path, true)?;
        let directory = NativeEntry::open(&path, true)?;
        Ok(WriteDirectory {
            root_path,
            root,
            path,
            directory,
        })
    }
    fn transaction(&self, id: &str) -> Result<DirectoryLock> {
        self.transaction_until(id, None)
    }
    fn transaction_until(&self, id: &str, deadline: Option<Instant>) -> Result<DirectoryLock> {
        assert_id(id)?;
        self.ensure_storage()?;
        let root = fs::canonicalize(self.root())?;
        let locks = root
            .parent()
            .context("Missing session parent")?
            .join(format!(
                "{}-write-locks",
                root.file_name()
                    .context("Missing session directory")?
                    .to_string_lossy()
            ));
        let path = locks.join(format!("{id}.lock"));
        match deadline {
            Some(deadline) => DirectoryLock::acquire_until(&path, deadline),
            None => DirectoryLock::acquire(&path, Duration::from_secs(10)),
        }
    }
    pub fn create(
        &self,
        mut options: Value,
        cwd: &Path,
        base_override: Option<&str>,
    ) -> Result<Value> {
        let prompt = options["prompt"].as_str().unwrap_or("session");
        let base = match base_override {
            Some(base) => base.to_owned(),
            None => create_id(prompt, options["slug"].as_str())?,
        };
        assert_id(&base)?;
        if !options.is_object() {
            bail!("Session options must be an object");
        }
        // Reference createSessionMetadata: the models given, else the model, else none; a
        // model record is still written for the default.
        let model_names = options["models"]
            .as_array()
            .filter(|a| !a.is_empty())
            .cloned()
            .or_else(|| {
                options["model"]
                    .as_str()
                    .filter(|model| !model.is_empty())
                    .map(|model| vec![json!(model)])
            })
            .unwrap_or_default();
        let primary = model_names.first().cloned().or_else(|| {
            options
                .get("model")
                .filter(|model| !model.is_null())
                .cloned()
        });
        let recorded = if model_names.is_empty() {
            vec![json!(
                primary
                    .as_ref()
                    .and_then(Value::as_str)
                    .unwrap_or(crate::policy::DEFAULT_API_MODEL)
            )]
        } else {
            model_names.clone()
        };
        let mode = options["mode"].as_str().unwrap_or("api").to_owned();
        let mut suffix = 1;
        loop {
            let id = if suffix == 1 {
                base.clone()
            } else {
                format!("{base}-{suffix}")
            };
            let lock = self.transaction(&id)?;
            let dir = self.directory(&id)?;
            #[cfg_attr(windows, allow(unused_mut))]
            let mut builder = fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            match builder.create(&dir) {
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    lock.release()?;
                    suffix += 1;
                    continue;
                }
                Err(error) => return Err(error.into()),
                Ok(()) => {}
            }
            let guard = self.write_directory(&id)?;
            options["slug"] = json!(id);
            options["mode"] = json!(mode);
            options["models"] = json!(model_names);
            if options.get("file").is_none() {
                options["file"] = json!([]);
            }
            // Reference slice(0, 160) counts UTF-16 units; a split surrogate pair is dropped.
            let mut units = 0;
            let preview = options["prompt"]
                .as_str()
                .unwrap_or("")
                .chars()
                .take_while(|character| {
                    units += character.len_utf16();
                    units <= 160
                })
                .collect::<String>();
            let mut metadata = json!({"id":id,"createdAt":timestamp(),"status":"pending","promptPreview":preview,"model":primary,"models":model_names.iter().map(|model| json!({"model":model,"status":"pending"})).collect::<Vec<_>>(),"cwd":cwd,"mode":mode,"options":options});
            if primary.is_none() {
                metadata.as_object_mut().expect("object").remove("model");
            }
            // Reference metadata.notifications sits beside the options it was resolved from.
            if let Some(settings) = metadata["options"]
                .as_object_mut()
                .and_then(|options| options.remove("notifications"))
            {
                metadata["notifications"] = settings;
            }
            if mode == "browser" {
                metadata["browser"] = json!({"runtime":{"submittedPromptHash":null}});
                if let Some(config) = metadata["options"].get("browserConfig").cloned() {
                    metadata["browser"]["config"] = config;
                }
            } else if let Some(config) = metadata["options"].get("browserConfig").cloned() {
                metadata["browser"] = json!({"config":config});
            }
            private_dir(&dir.join("models"))?;
            for name in &recorded {
                let name = name.as_str().context("Model names must be strings")?;
                let slug = safe_model_slug(name);
                atomic_json(
                    &dir.join("models").join(format!("{slug}.json")),
                    &json!({"model":name,"status":"pending","log":{"path":format!("models/{slug}.log")}}),
                )?;
                private_file(&dir.join("models").join(format!("{slug}.log")), true)?;
            }
            private_file(&dir.join("output.log"), true)?;
            guard.leaf(&dir.join("meta.json"))?;
            atomic_json(&dir.join("meta.json"), &metadata)?;
            guard.check()?;
            lock.release()?;
            return Ok(metadata);
        }
    }
    fn read_raw(&self, id: &str, summary: bool) -> Result<Option<Value>> {
        let dir = self.directory(id)?;
        for filename in ["meta.json", "session.json"] {
            let Some(mut value) = read_optional_json(&dir.join(filename)).ok().flatten() else {
                continue;
            };
            let Some(stored_id) = value["id"].as_str() else {
                continue;
            };
            if !valid_id(stored_id) || value["status"].as_str().is_none_or(|s| s.is_empty()) {
                continue;
            }
            if id != stored_id && !same_directory(&dir, &self.directory(stored_id)?) {
                continue;
            }
            if !summary || value["status"] == "running" {
                let runs = self.model_runs(id)?;
                if !runs.is_empty() {
                    value["models"] = json!(runs);
                }
            }
            return Ok(Some(value));
        }
        Ok(None)
    }
    pub fn read(&self, id: &str) -> Result<Option<Value>> {
        self.read_raw(id, false)
    }
    pub fn require(&self, id: &str) -> Result<Value> {
        self.read(id)?
            .with_context(|| format!("Session \"{id}\" not found."))
    }
    pub(crate) fn stored_for_update(&self, id: &str) -> Result<Value> {
        let dir = self.directory(id)?;
        let stored = match read_optional_json(&dir.join("meta.json"))? {
            Some(value) => Some(value),
            None => read_optional_json(&dir.join("session.json"))?,
        };
        // Only the synthesised placeholder for a missing record may lack a status.
        let Some(value) = stored else {
            return Ok(json!({"id":id}));
        };
        if value["id"] != id || !value["status"].is_string() {
            bail!("Invalid stored metadata for session {id}.");
        }
        Ok(value)
    }
    pub fn update(
        &self,
        id: &str,
        decide: impl FnOnce(&Value) -> Result<Option<Value>>,
    ) -> Result<Value> {
        self.update_with_deadline(id, None, decide)
    }
    pub(crate) fn update_until(
        &self,
        id: &str,
        deadline: Instant,
        decide: impl FnOnce(&Value) -> Result<Option<Value>>,
    ) -> Result<Value> {
        self.update_with_deadline(id, Some(deadline), decide)
    }
    fn update_with_deadline(
        &self,
        id: &str,
        deadline: Option<Instant>,
        decide: impl FnOnce(&Value) -> Result<Option<Value>>,
    ) -> Result<Value> {
        let lock = self.transaction_until(id, deadline)?;
        let dir = self.directory(id)?;
        let guard = self.write_directory(id)?;
        let mut current = self.stored_for_update(id)?;
        if let Some(patch) = decide(&current)? {
            apply_patch(&mut current, &patch)?;
            if current["id"] != id {
                bail!("Invalid stored metadata for session {id}.");
            }
            guard.leaf(&dir.join("meta.json"))?;
            match deadline {
                Some(deadline) => atomic_json_until(&dir.join("meta.json"), &current, deadline)?,
                None => atomic_json(&dir.join("meta.json"), &current)?,
            }
            guard.check()?;
        }
        match deadline {
            Some(deadline) => lock.release_until(deadline)?,
            None => lock.release()?,
        }
        Ok(current)
    }
    pub fn update_model(&self, id: &str, model: &str, patch: &Value) -> Result<Value> {
        let lock = self.transaction(id)?;
        let dir = self.directory(id)?;
        let guard = self.write_directory(id)?;
        let mut current = self
            .model_for_update(id, model)?
            .unwrap_or_else(|| json!({"model":model,"status":"pending"}));
        apply_patch(&mut current, patch)?;
        current["model"] = json!(model);
        ensure_model_log(&mut current, model);
        let models = guard.models()?;
        let path = self.model_path(id, model)?;
        guard.leaf(&path)?;
        atomic_json(&path, &current)?;
        models.require_matches(&dir.join("models"))?;
        guard.check()?;
        lock.release()?;
        Ok(current)
    }
    pub fn update_session_and_model(
        &self,
        id: &str,
        model: &str,
        decide: impl FnOnce(&Value, Option<&Value>) -> Result<Option<(Value, Option<Value>)>>,
    ) -> Result<Value> {
        let lock = self.transaction(id)?;
        let dir = self.directory(id)?;
        let guard = self.write_directory(id)?;
        let mut current = self.stored_for_update(id)?;
        let mut run = self.model_for_update(id, model)?;
        if let Some((patch, model_patch)) = decide(&current, run.as_ref())? {
            if let Some(model_patch) = &model_patch {
                let next = run.get_or_insert_with(|| json!({"model":model,"status":"pending"}));
                apply_patch(next, model_patch)?;
                next["model"] = json!(model);
                ensure_model_log(next, model);
            }
            if terminal(patch["status"].as_str().unwrap_or(""))
                && patch.get("models").is_none()
                && current["models"].is_array()
            {
                let mut runs = vec![];
                for original in current["models"].as_array().context("Missing models")? {
                    let name = original["model"].as_str().context("Malformed model list")?;
                    let value = if name == model {
                        run.clone()
                    } else {
                        self.model_for_update(id, name)?
                    };
                    runs.push(value.unwrap_or_else(|| original.clone()));
                }
                current["models"] = json!(runs);
            }
            apply_patch(&mut current, &patch)?;
            if current["id"] != id {
                bail!("Invalid stored metadata for session {id}.");
            }
            if model_patch.is_some() {
                let models = guard.models()?;
                let path = self.model_path(id, model)?;
                guard.leaf(&path)?;
                atomic_json(&path, &run)?;
                models.require_matches(&dir.join("models"))?;
            }
            guard.leaf(&dir.join("meta.json"))?;
            let written = atomic_json(&dir.join("meta.json"), &current);
            if model_patch.is_some() {
                written.with_context(|| format!("Model metadata for {model} was written, but session {id} metadata was not committed."))?;
            } else {
                written?;
            }
            guard.check()?;
        }
        lock.release()?;
        Ok(current)
    }
    fn model_path(&self, id: &str, model: &str) -> Result<PathBuf> {
        Ok(self
            .directory(id)?
            .join("models")
            .join(format!("{}.json", safe_model_slug(model))))
    }
    fn model_for_update(&self, id: &str, model: &str) -> Result<Option<Value>> {
        let Some(mut run) = read_optional_json(&self.model_path(id, model)?)? else {
            return Ok(None);
        };
        if run["model"] != model || run["status"].as_str().is_none_or(|s| s.is_empty()) {
            bail!("Invalid stored model metadata for {model} in session {id}.");
        }
        ensure_model_log(&mut run, model);
        Ok(Some(run))
    }
    pub fn model_runs(&self, id: &str) -> Result<Vec<Value>> {
        let dir = self.directory(id)?.join("models");
        let mut runs = vec![];
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                if entry.path().extension().is_none_or(|ext| ext != "json") {
                    continue;
                }
                if let Ok(Some(mut run)) = read_optional_json(&entry.path())
                    && let Some(model) = run["model"].as_str().map(str::to_owned)
                {
                    ensure_model_log(&mut run, &model);
                    runs.push(run);
                }
            }
        }
        runs.sort_by(|a, b| a["model"].as_str().cmp(&b["model"].as_str()));
        Ok(runs)
    }
    pub fn request(&self, id: &str) -> Result<Option<Value>> {
        if let Some(meta) = self.read_raw(id, false)?
            && let Some(options) = meta.get("options")
        {
            return Ok(Some(options.clone()));
        }
        let value = read_optional_json(&self.directory(id)?.join("request.json"))
            .ok()
            .flatten();
        Ok(value.and_then(|v| {
            if v["id"].is_string() && v["status"].is_string() {
                v.get("options").cloned()
            } else {
                Some(v)
            }
        }))
    }
    pub fn log(&self, id: &str) -> Result<String> {
        let dir = self.directory(id)?;
        let mut runs = self.model_runs(id)?;
        runs.sort_by(
            |a, b| match (a["startedAt"].as_str(), b["startedAt"].as_str()) {
                (Some(a), Some(b)) => a.cmp(b),
                _ => a["model"].as_str().cmp(&b["model"].as_str()),
            },
        );
        let mut sections = vec![];
        let mut has_content = false;
        for run in &runs {
            let model = run["model"].as_str().unwrap_or("unknown");
            let path = run["log"]["path"]
                .as_str()
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(format!("models/{}.log", safe_model_slug(model))));
            // Historical log paths remain readable; no record path is used for publication.
            let body = fs::read_to_string(dir.join(path)).unwrap_or_default();
            has_content |= !body.is_empty();
            sections.push(format!("=== {model} ===\n{body}").trim_end().to_owned());
        }
        if !has_content && let Ok(log) = fs::read_to_string(dir.join("output.log")) {
            return Ok(log);
        }
        Ok(sections.join("\n\n"))
    }
    pub fn log_for_model(&self, id: &str, model: &str) -> Result<String> {
        let filter = crate::policy::trim(model).to_lowercase();
        if filter.is_empty() {
            return self.log(id);
        }
        let meta = self.require(id)?;
        let runs = meta["models"].as_array();
        let run = runs.and_then(|runs| {
            runs.iter().find(|run| {
                run["model"]
                    .as_str()
                    .is_some_and(|name| name.to_lowercase() == filter)
            })
        });
        let name = run
            .and_then(|run| run["model"].as_str())
            .or_else(|| {
                runs.is_none()
                    .then(|| meta["model"].as_str())
                    .flatten()
                    .filter(|name| name.to_lowercase() == filter)
            })
            .with_context(|| format!("Model \"{model}\" not found in session {id}."))?;
        let dir = self.directory(id)?;
        let path = run
            .and_then(|run| run["log"]["path"].as_str())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(format!("models/{}.log", safe_model_slug(name))));
        let body = fs::read_to_string(dir.join(path)).unwrap_or_default();
        if crate::policy::trim(&body).is_empty() {
            return Ok(if runs.is_some_and(|runs| runs.len() > 1) {
                String::new()
            } else {
                fs::read_to_string(dir.join("output.log")).unwrap_or_default()
            });
        }
        Ok(if run.is_some() {
            format!("=== {name} ===\n{body}").trim_end().to_owned()
        } else {
            body
        })
    }
    pub fn append_log(&self, id: &str, model: Option<&str>, text: &str) -> Result<()> {
        let lock = self.transaction(id)?;
        self.write_log_unlocked(id, model, text)?;
        lock.release()?;
        Ok(())
    }
    pub fn append_log_guarded(
        &self,
        id: &str,
        model: Option<&str>,
        text: &str,
        check: impl FnOnce(&Value, Option<&Value>) -> Result<()>,
    ) -> Result<()> {
        let lock = self.transaction(id)?;
        let current = self.stored_for_update(id)?;
        let run = model
            .map(|model| self.model_for_update(id, model))
            .transpose()?
            .flatten();
        check(&current, run.as_ref())?;
        self.write_log_unlocked(id, model, text)?;
        lock.release()?;
        Ok(())
    }
    // Both callers hold the session transaction through the native write and flush.
    fn write_log_unlocked(&self, id: &str, model: Option<&str>, text: &str) -> Result<()> {
        let dir = self.directory(id)?;
        let guard = self.write_directory(id)?;
        let models = if model.is_some() {
            Some(guard.models()?)
        } else {
            None
        };
        let path = if let Some(model) = model {
            dir.join(format!("models/{}.log", safe_model_slug(model)))
        } else {
            dir.join("output.log")
        };
        let mut options = OpenOptions::new();
        guard.leaf(&path)?;
        options.create(true).append(true);
        no_follow(&mut options, false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(path)?;
        file.write_all(text.as_bytes())?;
        file.sync_data()?;
        guard.check()?;
        if let Some(models) = models {
            models.require_matches(&dir.join("models"))?;
        }
        Ok(())
    }
    pub fn list(&self, summary: bool) -> Result<Vec<Value>> {
        let mut output = vec![];
        match fs::read_dir(self.root()) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(output),
            Err(error) => return Err(error.into()),
            Ok(entries) => {
                for entry in entries.flatten() {
                    let id = entry.file_name().to_string_lossy().into_owned();
                    if valid_id(&id)
                        && let Some(meta) = self.read_raw(&id, summary)?
                    {
                        output.push(meta);
                    }
                }
            }
        }
        output.sort_by(|a, b| b["createdAt"].as_str().cmp(&a["createdAt"].as_str()));
        Ok(output)
    }
    pub async fn wait(
        &self,
        id: &str,
        timeout: Option<Duration>,
        cancel: &CancellationToken,
    ) -> Result<(Value, bool)> {
        assert_id(id)?;
        let started = Instant::now();
        let mut meta = self.reported(id, self.require(id)?)?;
        loop {
            if cancel.is_cancelled() {
                bail!("Alcuin session wait was cancelled.");
            }
            if terminal(meta["status"].as_str().unwrap_or("")) {
                return Ok((meta, false));
            }
            if timeout.is_some_and(|limit| started.elapsed() >= limit) {
                return Ok((meta, true));
            }
            let delay = Duration::from_millis(1000).min(
                timeout
                    .map(|limit| limit.saturating_sub(started.elapsed()))
                    .unwrap_or(Duration::from_secs(1)),
            );
            tokio::select! { _ = cancel.cancelled() => bail!("Alcuin session wait was cancelled."), _ = tokio::time::sleep(delay) => {} }
            if let Some(updated) = self.read(id)? {
                meta = self.reported(id, updated)?;
            }
        }
    }
    /// Report-only read reconciliation (reference reconcileSessionForRead without persistence):
    /// a running session whose verified worker is dead reads as failed with the reference
    /// detached-worker wording, and an unverified stale worker as a zombie. Nothing is written,
    /// so dispatch and target evidence stay with recovery.
    pub fn reported(&self, id: &str, mut meta: Value) -> Result<Value> {
        if meta["status"] != "running" {
            return Ok(meta);
        }
        let patch = match self.dead_worker_patch(id, &meta)? {
            Some(patch) => Some(patch),
            None => self.zombie_patch(id, &meta)?,
        };
        if let Some(patch) = patch {
            apply_patch(&mut meta, &patch)?;
        }
        Ok(meta)
    }
    /// Listing status: the unverified zombie rule is persisted, as `alcuin status` always did,
    /// then a verified dead worker is reported without a write. Listing never fails on
    /// reconciliation; the saved status is shown instead.
    pub fn listed_status(&self, entry: &Value) -> Value {
        let Some(id) = entry["id"]
            .as_str()
            .filter(|_| entry["status"] == "running")
        else {
            return entry["status"].clone();
        };
        self.reconcile_unverified_zombie(id)
            .and_then(|current| self.reported(id, current))
            .map_or_else(|_| entry["status"].clone(), |meta| meta["status"].clone())
    }
    /// The detached-worker settlement patch for a worker whose exact saved identity is dead and
    /// whose browser controller, when recorded, is positively gone.
    fn dead_worker_patch(&self, id: &str, meta: &Value) -> Result<Option<Value>> {
        let lifecycle = &meta["lifecycle"];
        let Ok(identity) =
            serde_json::from_value::<ProcessIdentity>(lifecycle["processIdentity"].clone())
        else {
            return Ok(None);
        };
        if identity.liveness() != Liveness::Dead
            || lifecycle["workerPid"].as_u64() != Some(u64::from(identity.pid))
            || lifecycle["workerProcessStartedAtMs"].as_f64() != Some(identity.started_at_ms)
            || controller_may_live(&meta["browser"]["runtime"])
            || lone_terminal_run(meta, &self.model_runs(id)?)
        {
            return Ok(None);
        }
        let message = "Detached worker exited before the session reached a terminal state.";
        let incomplete = json!({"status":"incomplete","incompleteReason":"incomplete-capture"});
        Ok(Some(json!({
            "status": "error",
            "completedAt": timestamp(),
            "errorMessage": message,
            "error": {"category":"internal","message":message},
            "response": merge_object(&meta["response"], &incomplete),
            "recoveryRequired": meta["recoveryRequired"] == true
                || meta.get("dispatch").is_some()
                || meta["browser"]["runtime"].get("ownedRecoveryTarget").is_some(),
        })))
    }
    /// Reference zombie rule, applied only where Rust cannot verify the worker: a running
    /// session whose saved process identity is unknown (or absent) and whose bare worker PID is
    /// not running is marked failed once it is staler than its zombie timeout. A verified live
    /// or dead worker is left to the identity-based paths, and browser sessions holding
    /// recovery evidence are left to browser recovery. Dispatch evidence is kept and still
    /// requires recovery.
    pub fn reconcile_unverified_zombie(&self, id: &str) -> Result<Value> {
        // Lock-free precheck: listing never contends with a verifiable worker's transactions.
        let saved = self.require(id)?;
        if saved["status"] != "running" || !unverified_worker(&saved) {
            return Ok(saved);
        }
        self.update(id, |current| self.zombie_patch(id, current))
    }
    fn zombie_patch(&self, id: &str, current: &Value) -> Result<Option<Value>> {
        if current["status"] != "running" || !unverified_worker(current) {
            return Ok(None);
        }
        // Model runs live beside meta.json; a lone terminal run may precede the session commit.
        let runs = self.model_runs(id)?;
        if lone_terminal_run(current, &runs) {
            return Ok(None);
        }
        let runtime = &current["browser"]["runtime"];
        if current["mode"] == "browser"
            && (current.get("dispatch").is_some()
                || [
                    "chromePid",
                    "chromePort",
                    "chromeTargetId",
                    "ownedRecoveryTarget",
                    "conversationId",
                    "tabUrl",
                    "chromeBrowserWSEndpoint",
                    "chromeProfileRoot",
                ]
                .iter()
                .any(|key| !runtime[*key].is_null()))
        {
            return Ok(None);
        }
        let Some(started) = current["startedAt"]
            .as_str()
            .or(current["createdAt"].as_str())
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        else {
            return Ok(None);
        };
        let mut anchor = started.with_timezone(&chrono::Utc);
        if current["options"]["zombieUseLastActivity"] == true
            && let Some(activity) = self.last_activity(id, &current["model"], &runs)?
        {
            anchor = activity;
        }
        let max_age = zombie_max_age(current);
        let stale = chrono::Utc::now().signed_duration_since(anchor);
        if stale.to_std().is_ok_and(|stale| stale > max_age) {
            Ok(Some(json!({
                "status": "error",
                "errorMessage": format!(
                    "Session marked as zombie (> {} stale)",
                    crate::execution::format_elapsed(max_age)
                ),
                "completedAt": timestamp(),
                "recoveryRequired": current["recoveryRequired"] == true || current.get("dispatch").is_some(),
            })))
        } else {
            Ok(None)
        }
    }
    /// Latest modification of the session log or a model log (reference getLastActivityMs).
    fn last_activity(
        &self,
        id: &str,
        model: &Value,
        runs: &[Value],
    ) -> Result<Option<chrono::DateTime<chrono::Utc>>> {
        let dir = self.directory(id)?;
        let mut paths = vec![dir.join("output.log")];
        for model in std::iter::once(model).chain(runs.iter().map(|run| &run["model"])) {
            if let Some(model) = model.as_str().filter(|s| !s.is_empty()) {
                paths.push(dir.join(format!("models/{}.log", safe_model_slug(model))));
            }
        }
        Ok(paths
            .iter()
            .filter_map(|path| fs::symlink_metadata(path).ok()?.modified().ok())
            .max()
            .map(chrono::DateTime::<chrono::Utc>::from))
    }
    /// Capacity release records worker death without deleting dispatch or target evidence.
    pub fn release_dead_worker_capacity(&self, id: &str) -> Result<Value> {
        self.update(id,|current| {
            let Some(identity) = current["lifecycle"].get("processIdentity") else { return Ok(None) };
            let identity: ProcessIdentity = serde_json::from_value(identity.clone())?;
            if identity.liveness() != Liveness::Dead { return Ok(None); }
            Ok(Some(json!({"lifecycle":merge_object(&current["lifecycle"], &json!({"capacityReleased":true,"workerDeathObservedAt":timestamp()})),"recoveryRequired":current["recoveryRequired"]==true||(current["status"]!="completed"&&(current.get("dispatch").is_some()||current["browser"]["runtime"].get("ownedRecoveryTarget").is_some()))})))
        })
    }
}

/// Reference ensureSessionStorage: an existing tree becomes owner-only (0700 directories,
/// 0600 files) once per process and root identity. Every change goes through a descriptor
/// opened beneath its parent without following links, so a link and its target are never
/// changed; entries that vanish or turn into links meanwhile are skipped.
#[cfg(unix)]
fn harden_storage(path: &Path, root: NativeEntry) -> Result<()> {
    use std::sync::{Mutex, OnceLock};
    static HARDENED: OnceLock<Mutex<Vec<(PathBuf, crate::native_fs::NativeIdentity)>>> =
        OnceLock::new();
    let mut hardened = HARDENED
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let key = (path.to_path_buf(), root.identity.clone());
    if !hardened.contains(&key) {
        harden_entry(&root, path, true)?;
        hardened.push(key);
    }
    Ok(())
}
#[cfg(unix)]
fn harden_entry(entry: &NativeEntry, path: &Path, directory: bool) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    entry
        .handle
        .set_permissions(fs::Permissions::from_mode(if directory {
            0o700
        } else {
            0o600
        }))?;
    if !directory {
        return Ok(());
    }
    let children = match fs::read_dir(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        children => children?,
    };
    for child in children {
        let child = child?;
        let kind = child.file_type()?;
        if !kind.is_dir() && !kind.is_file() {
            continue;
        }
        match entry.open_child(path, &child.file_name(), kind.is_dir()) {
            Ok(opened) => harden_entry(&opened, &child.path(), kind.is_dir())?,
            Err(error)
                if error.downcast_ref::<std::io::Error>().is_some_and(|error| {
                    error.kind() == std::io::ErrorKind::NotFound
                        || matches!(error.raw_os_error(), Some(libc::ELOOP | libc::ENOTDIR))
                }) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}
fn lone_terminal_run(meta: &Value, runs: &[Value]) -> bool {
    runs.len() == 1
        && runs[0]["model"] == meta["model"]
        && terminal(runs[0]["status"].as_str().unwrap_or(""))
}
/// A recorded browser controller counts as gone only without a process at its PID or with
/// positive evidence that the process there is a different incarnation.
fn controller_may_live(runtime: &Value) -> bool {
    let Some(pid) = runtime.get("controllerPid") else {
        return false;
    };
    let Some(pid) = pid.as_u64().and_then(|pid| u32::try_from(pid).ok()) else {
        return true;
    };
    match ProcessIdentity::read(pid) {
        Ok(None) => false,
        Ok(Some(actual)) => !runtime["controllerProcessStartedAtMs"]
            .as_f64()
            .is_some_and(|start| start.is_finite() && start > 0.0 && start != actual.started_at_ms),
        Err(_) => true,
    }
}
/// True when neither the saved native identity nor the bare worker PID shows the worker state.
fn unverified_worker(meta: &Value) -> bool {
    let lifecycle = &meta["lifecycle"];
    if serde_json::from_value::<ProcessIdentity>(lifecycle["processIdentity"].clone())
        .is_ok_and(|identity| identity.liveness() != Liveness::Unknown)
    {
        return false;
    }
    // The reference's own signal: a running process with the saved worker PID.
    !lifecycle["workerPid"]
        .as_u64()
        .and_then(|pid| u32::try_from(pid).ok())
        .is_some_and(|pid| matches!(ProcessIdentity::read(pid), Ok(Some(_))))
}
/// Reference resolveZombieMaxAgeMs: the explicit zombie timeout, else 60 minutes or the
/// overall timeout when that is longer.
fn zombie_max_age(meta: &Value) -> std::time::Duration {
    let ms = |value: &Value| value.as_f64().filter(|n| n.is_finite() && *n > 0.0);
    let explicit = ms(&meta["options"]["zombieTimeoutMs"]);
    let timeout = ms(&meta["options"]["timeoutSeconds"]).map(|seconds| seconds * 1000.0);
    let max_age = explicit.unwrap_or_else(|| timeout.unwrap_or(0.0).max(3_600_000.0));
    std::time::Duration::from_millis(max_age as u64)
}
pub fn filter_range(
    mut entries: Vec<Value>,
    hours: f64,
    include_all: bool,
    limit: usize,
) -> (Vec<Value>, usize, bool) {
    if !include_all {
        let cutoff = chrono::Utc::now().timestamp_millis() as f64 - hours * 3600000.0;
        entries.retain(|meta| {
            meta["createdAt"]
                .as_str()
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .is_some_and(|time| time.timestamp_millis() as f64 >= cutoff)
        });
    }
    let total = entries.len();
    entries.truncate(limit.min(1000));
    let truncated = entries.len() < total;
    (entries, total, truncated)
}
fn read_optional_json(path: &Path) -> Result<Option<Value>> {
    match fs::read(path) {
        Ok(raw) => Ok(Some(serde_json::from_slice(&raw)?)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}
fn ensure_model_log(value: &mut Value, model: &str) {
    if !value["log"].is_object() {
        value["log"] = json!({});
    }
    if value["log"].get("path").is_none() {
        value["log"]["path"] = json!(format!("models/{}.log", safe_model_slug(model)));
    }
}
pub fn apply_patch(value: &mut Value, patch: &Value) -> Result<()> {
    let target = value
        .as_object_mut()
        .context("Metadata must be an object")?;
    for (key, value) in patch
        .as_object()
        .context("Metadata patch must be an object")?
    {
        target.insert(key.clone(), value.clone());
    }
    Ok(())
}
pub fn merge_object(current: &Value, patch: &Value) -> Value {
    let mut output = if current.is_object() {
        current.clone()
    } else {
        json!({})
    };
    if let Some(patch) = patch.as_object() {
        for (key, value) in patch {
            output[key] = value.clone();
        }
    }
    output
}

fn changed_fields(next: &Value, basis: &Value) -> Value {
    let mut output = json!({});
    if let Some(next) = next.as_object() {
        for (key, value) in next {
            if basis.get(key) != Some(value) {
                output[key] = value.clone();
            }
        }
    }
    output
}
pub fn merge_browser(current: &Value, next: &Value, basis: &Value) -> Value {
    if !next.is_object() {
        return current.clone();
    }
    let mut patch = changed_fields(next, basis);
    for key in ["config", "runtime", "warnings"] {
        patch.as_object_mut().expect("object").remove(key);
    }
    let mut merged = merge_object(current, &patch);
    for key in ["config", "runtime"] {
        if let Some(next) = next.get(key).filter(|next| next.is_object()) {
            merged[key] = merge_object(&current[key], &changed_fields(next, &basis[key]));
        }
    }
    if let Some(warnings) = next["warnings"].as_array() {
        // Reference Map by code: the first position keeps the latest warning.
        let mut retained = keyed(&current["warnings"], |v| v["code"].clone());
        for warning in warnings {
            let previous = basis["warnings"]
                .as_array()
                .and_then(|a| a.iter().find(|v| v["code"] == warning["code"]));
            if previous == Some(warning) {
                continue;
            }
            if let Some(index) = retained.iter().position(|v| v["code"] == warning["code"]) {
                retained[index] = warning.clone();
            } else {
                retained.push(warning.clone());
            }
        }
        if retained.is_empty() {
            merged.as_object_mut().expect("object").remove("warnings");
        } else {
            merged["warnings"] = json!(retained);
        }
    }
    merged
}
/// Deduplicates `values` by `key`, keeping the first position and the last value.
fn keyed(values: &Value, key: impl Fn(&Value) -> Value) -> Vec<Value> {
    let mut output: Vec<Value> = vec![];
    for value in values.as_array().into_iter().flatten() {
        match output
            .iter()
            .position(|existing| key(existing) == key(value))
        {
            Some(index) => output[index] = value.clone(),
            None => output.push(value.clone()),
        }
    }
    output
}
pub fn merge_artifacts(current: &Value, additions: &Value, basis: &Value) -> Value {
    let mut retained = keyed(current, |v| json!([v["kind"], v["path"]]));
    for artifact in additions.as_array().into_iter().flatten() {
        let same = |v: &&Value| v["kind"] == artifact["kind"] && v["path"] == artifact["path"];
        let old = basis
            .as_array()
            .and_then(|a| a.iter().find(same))
            .cloned()
            .unwrap_or(Value::Null);
        if let Some(index) = retained.iter().position(|v| same(&v)) {
            retained[index] = merge_object(&retained[index], &changed_fields(artifact, &old));
        } else {
            retained.push(artifact.clone());
        }
    }
    if retained.is_empty() {
        Value::Null
    } else {
        json!(retained)
    }
}

#[cfg(test)]
mod deadline_tests {
    use super::*;
    #[test]
    fn a_contended_cleanup_update_respects_its_remaining_deadline_without_mutation() {
        let home = tempfile::tempdir().unwrap();
        let store = SessionStore::new(home.path());
        let meta = store
            .create(json!({"prompt":"PUBLIC deadline"}), home.path(), None)
            .unwrap();
        let id = meta["id"].as_str().unwrap();
        let before = fs::read(store.directory(id).unwrap().join("meta.json")).unwrap();
        let lock = store.transaction(id).unwrap();
        let start = Instant::now();
        let called = std::cell::Cell::new(false);
        let result = store.update_until(id, start + Duration::from_millis(40), |_| {
            called.set(true);
            Ok(Some(json!({"marker":"PUBLIC should not publish"})))
        });
        assert!(result.is_err());
        assert!(!called.get());
        assert!(start.elapsed() < Duration::from_secs(1));
        assert_eq!(
            fs::read(store.directory(id).unwrap().join("meta.json")).unwrap(),
            before
        );
        lock.release().unwrap();
        let saved = store
            .update_until(id, Instant::now() + Duration::from_secs(2), |_| {
                Ok(Some(json!({"marker":"PUBLIC settled"})))
            })
            .unwrap();
        assert_eq!(saved["marker"], "PUBLIC settled");
    }
}
