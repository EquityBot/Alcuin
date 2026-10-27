//! Native producer wiring for remote sessions and durable browser capture.
use crate::{
    artifacts,
    browser_inputs::StagedUploads,
    browser_prompt::PendingBundle,
    browser_runner::{self, BrowserFallback, BrowserRequest},
    config::Environment,
    policy,
    process::ProcessIdentity,
    remote_host::{self, ProducedRun, RunContext, RunDriver},
    remote_protocol, remote_result,
    sessions::merge_object,
    storage::atomic_json,
};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{path::PathBuf, sync::Arc};
use uuid::Uuid;

/// Remote Deep Research stays unsupported in this build.
pub(crate) const REMOTE_DEEP_RESEARCH: &str = "Remote Deep Research: this build does not support it. Run Deep Research with local Chrome or --browser-attach-running.";
#[derive(Clone)]
pub struct BrowserDriver {
    pub environment: Environment,
    pub cwd: PathBuf,
    /// Trusted configuration supplied locally by the service, never by the client.
    pub browser_config: Value,
}
impl BrowserDriver {
    pub fn driver(self) -> RunDriver {
        Arc::new(move |context| {
            let options = self.clone();
            Box::pin(async move { options.run(context).await })
        })
    }
    pub(crate) fn prepare(&self, context: &RunContext) -> Result<String> {
        ensure!(
            !context.cancel.is_cancelled(),
            "Browser run cancelled before preparation"
        );
        let id = remote_result::session_id(&context.run_id)?;
        let prompt = context.payload["prompt"]
            .as_str()
            .context("Missing remote prompt")?;
        let requested = &context.payload["browserConfig"];
        let mut browser = merge_object(
            &remote_host::client_browser_config(requested),
            &self.browser_config,
        );
        ensure!(
            browser.is_object(),
            "Host browser configuration must be an object"
        );
        browser["inlineCookies"] = Value::Null;
        browser["inlineCookiesSource"] = Value::Null;
        let model = context.payload["options"]["model"]
            .as_str()
            .unwrap_or("gpt-6-pro");
        policy::assert_model_allowed(Some(model))?;
        policy::assert_model_allowed(browser["desiredModel"].as_str())?;
        if browser["desiredModel"].is_null() {
            browser["desiredModel"] = json!("GPT-6");
        }
        if browser["thinkingTime"].is_null() {
            browser["thinkingTime"] = json!("pro");
        }
        if browser["modelStrategy"].is_null() {
            browser["modelStrategy"] = json!("select");
        }
        let url = normalise_url(browser.get("url").or(browser.get("chatgptUrl")))?;
        browser["url"] = json!(url);
        browser["chatgptUrl"] = json!(url);
        ensure!(browser["researchMode"] != "deep", "{REMOTE_DEEP_RESEARCH}");
        ensure!(
            browser["researchMode"]
                .as_str()
                .is_none_or(|mode| mode == "off"),
            "Web Search is a local browser pilot; --remote-host does not negotiate this capability yet. Use local Chrome or --browser-attach-running."
        );
        let followups = crate::browser_turns::prompts(
            &json!({"browserFollowUps":context.payload["options"]["followUpPrompts"]}),
        )?;
        ensure!(
            prompt.encode_utf16().count() >= 10,
            "Prompt is too short (<10 chars). This was likely accidental; please provide more detail."
        );
        let mut options = json!({"prompt":prompt,"model":model,"mode":"browser","browserConfig":browser,"force":true});
        if !followups.is_empty() {
            options["browserFollowUps"] = json!(followups);
        }
        ensure!(
            context.payload["options"]["imageOutputRequested"] != true,
            "{}",
            crate::browser_runner::IMAGE_GENERATION_UNAVAILABLE
        );
        for (source, target) in [
            ("heartbeatIntervalMs", "heartbeatIntervalMs"),
            ("verbose", "verbose"),
        ] {
            if let Some(value) = context.payload["options"].get(source) {
                options[target] = value.clone();
            }
        }
        let created = context.store.create(options, &self.cwd, Some(&id))?;
        ensure!(
            created["id"] == id,
            "Remote run identity collided with an existing session"
        );
        let staged = if let Some(attachments) = context.payload["attachments"]
            .as_array()
            .filter(|attachments| !attachments.is_empty())
        {
            Some(StagedUploads::capture_remote(
                &context.store,
                &id,
                attachments,
            )?)
        } else {
            None
        };
        let fallback = if let Some(fallback) = context
            .payload
            .get("fallbackSubmission")
            .filter(|value| !value.is_null())
        {
            let pending = fallback
                .get("bundle")
                .filter(|value| !value.is_null())
                .cloned()
                .map(serde_json::from_value::<PendingBundle>)
                .transpose()?;
            let attachments = fallback["attachments"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            Some(BrowserFallback {
                composer_text: fallback["prompt"]
                    .as_str()
                    .context("Fallback has no composer prompt")?
                    .into(),
                pending_bundle: pending,
                text_sources: vec![],
                raw_source: None,
                remote_source: Some(StagedUploads::capture_remote(
                    &context.store,
                    &id,
                    &attachments,
                )?),
            })
        } else {
            None
        };
        let committed = BrowserRequest {
            version: 1,
            session_id: id.clone(),
            reservation_id: Uuid::new_v4().to_string(),
            prompt: prompt.into(),
            prompt_sha256: artifacts::sha256(prompt.as_bytes()),
            options_sha256: artifacts::sha256(&serde_json::to_vec(&created["options"])?),
            upload_paths: staged
                .as_ref()
                .map(StagedUploads::paths)
                .transpose()?
                .unwrap_or_default(),
            staged_uploads: staged,
            fallback,
            fallback_used: false,
            extra: serde_json::Map::from_iter([
                ("attachmentMode".into(), json!("remote")),
                ("inlineFileCount".into(), json!(0)),
            ]),
        };
        atomic_json(
            &context.store.directory(&id)?.join("browser-request.json"),
            &committed,
        )?;
        context.store.update(&id,|current|Ok(Some(json!({"browser":merge_object(&current["browser"],&json!({"remoteDelivery":{"runId":context.run_id,"acknowledged":false,"producerFinished":false}}))}))))?;
        Ok(id)
    }
    async fn run(&self, context: RunContext) -> Result<ProducedRun> {
        let id = self.prepare(&context)?;
        let identity = ProcessIdentity::current()?;
        browser_runner::start_owner(&context.store, &id, &identity)?;
        let images_required = context.payload["options"]["imageOutputRequested"] == true;
        let capture: browser_runner::CaptureHook = {
            let context = context.clone();
            Arc::new(move |captured| {
                let produced = remote_protocol::produced(
                    &context.store,
                    &context.run_id,
                    captured,
                    images_required,
                )?;
                context.capture(&produced)
            })
        };
        browser_runner::run_native_with_capture(
            &context.store,
            &id,
            &self.environment,
            &identity,
            &context.cancel,
            Some(capture),
        )
        .await?;
        ensure!(
            !context.cancel.is_cancelled(),
            "Browser run cancelled after capture"
        );
        let record = remote_result::read(&context.store, &context.run_id)?;
        let captured = remote_protocol::captured(&context.store, &id)?;
        let warnings = record.native()["result"]["warnings"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|warning| {
                matches!(
                    warning["code"].as_str(),
                    Some(
                        "remote-image-registration-failed" | "remote-artifact-registration-failed"
                    )
                )
            })
            .cloned()
            .collect::<Vec<_>>();
        let result = remote_protocol::sanitise(&captured, &warnings);
        context.store.update(&id, |current| {
            ensure!(
                current["status"] == "running"
                    && current["browser"]["remoteDelivery"]["runId"] == context.run_id,
                "Remote producer changed before final delivery"
            );
            Ok(None)
        })?;
        Ok(ProducedRun {
            captured: record.native()["capturedResult"].clone(),
            result,
            artifacts: record.native()["artifacts"].clone(),
        })
    }
}
pub(crate) fn normalise_url(value: Option<&Value>) -> Result<String> {
    let raw = value.and_then(Value::as_str).unwrap_or_default();
    let value = value
        .and_then(Value::as_str)
        .map(policy::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("https://chatgpt.com/");
    let has_scheme = value.find("://").is_some_and(|at| {
        at > 0
            && value.as_bytes()[0].is_ascii_alphabetic()
            && value.as_bytes()[1..at]
                .iter()
                .all(|byte| byte.is_ascii_alphanumeric() || b"+.-".contains(byte))
    });
    let value = if has_scheme {
        value.to_owned()
    } else {
        format!("https://{value}")
    };
    let url = reqwest::Url::parse(&value).map_err(|_| {
        anyhow::anyhow!("Invalid ChatGPT URL: \"{raw}\". Provide an absolute http(s) URL.")
    })?;
    ensure!(
        matches!(url.scheme(), "http" | "https"),
        "Invalid ChatGPT URL protocol: \"{}:\". Use http or https.",
        url.scheme()
    );
    // Native navigation independently enforces the permitted provider origin.
    crate::browser_dom::navigation(url.as_str())?;
    Ok(url.to_string())
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{remote_host::RunContext, sessions::SessionStore};
    const RUN: &str = "12345678-1234-1234-1234-123456789abc";
    fn fixture(payload: Value) -> (tempfile::TempDir, BrowserDriver, RunContext) {
        let home = tempfile::tempdir().unwrap();
        let driver = BrowserDriver {
            environment: Environment::new(),
            cwd: home.path().into(),
            browser_config: json!({"manualLogin":true,"chromePath":"PUBLIC-host-chrome","manualLoginProfileDir":"PUBLIC-host-profile"}),
        };
        let context =
            RunContext::fixture(RUN, payload, SessionStore::new(home.path().join(".oracle")));
        (home, driver, context)
    }
    #[test]
    fn producer_preparation_keeps_host_authority_and_stages_case_collisions_and_base64() {
        let (_home, driver, context) = fixture(
            json!({"prompt":"PUBLIC remote original prompt","options":{"model":"gpt-6-pro"},
            "browserConfig":{"chromePath":"PUBLIC-client-executable","manualLoginProfileDir":"PUBLIC-client-profile","inlineCookies":"PUBLIC fake cookies","debugPort":1,"url":"chatgpt.com/?PUBLIC=1"},
            "attachments":[{"fileName":"file.txt","contentBase64":"UFVCTElD","displayPath":"PUBLIC first"},
                {"fileName":"FILE.txt","contentBase64":"U F ! V C T E l D=ignored","displayPath":"PUBLIC duplicate"},
                {"fileName":"file-2.txt","contentBase64":"UFVCTElD","displayPath":"PUBLIC future"},
                {"fileName":"💡.txt","contentBase64":"Zh","displayPath":"PUBLIC UTF16 name"}]}),
        );
        let id = driver.prepare(&context).unwrap();
        let metadata = context.store.require(&id).unwrap();
        let config = &metadata["browser"]["config"];
        assert_eq!(config["chromePath"], "PUBLIC-host-chrome");
        assert_eq!(config["manualLoginProfileDir"], "PUBLIC-host-profile");
        assert!(config["inlineCookies"].is_null());
        assert!(config["debugPort"].is_null());
        assert_eq!(config["url"], "https://chatgpt.com/?PUBLIC=1");
        let request: BrowserRequest = serde_json::from_slice(
            &std::fs::read(
                context
                    .store
                    .directory(&id)
                    .unwrap()
                    .join("browser-request.json"),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(request.prompt, "PUBLIC remote original prompt");
        let staged = request.staged_uploads.unwrap();
        assert_eq!(
            staged
                .files
                .iter()
                .map(|file| file.manifest["name"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["file.txt", "FILE-3.txt", "file-2.txt", "__.txt"]
        );
        for index in 0..3 {
            assert_eq!(staged.open(index).unwrap().contents().unwrap(), b"PUBLIC");
        }
        assert_eq!(staged.open(3).unwrap().contents().unwrap(), b"f");
        staged.verify(&context.store, &id).unwrap();
        assert_eq!(metadata["status"], "pending");
        assert_eq!(metadata["browser"]["remoteDelivery"]["runId"], RUN);
    }
    #[test]
    fn deferred_remote_fallback_retains_exact_composer_and_all_owned_sources() {
        let (_home, driver, context) = fixture(
            json!({"prompt":"PUBLIC inline original prompt","attachments":[],"options":{},"fallbackSubmission":{"prompt":"PUBLIC fallback original composer",
            "attachments":[{"fileName":"source.txt","contentBase64":"UFVCTElD","displayPath":"PUBLIC original/source.txt"}],"bundle":{"format":"zip","scope":"all"}}}),
        );
        let id = driver.prepare(&context).unwrap();
        let request: BrowserRequest = serde_json::from_slice(
            &std::fs::read(
                context
                    .store
                    .directory(&id)
                    .unwrap()
                    .join("browser-request.json"),
            )
            .unwrap(),
        )
        .unwrap();
        let fallback = request.fallback.unwrap();
        assert_eq!(fallback.composer_text, "PUBLIC fallback original composer");
        assert_eq!(
            fallback.pending_bundle.unwrap().format,
            crate::browser_prompt::BundleFormat::Zip
        );
        let source = fallback.remote_source.unwrap();
        source.verify(&context.store, &id).unwrap();
        assert_eq!(source.open(0).unwrap().contents().unwrap(), b"PUBLIC");
        assert_eq!(source.files[0].display_path, "PUBLIC original/source.txt");
    }
    #[test]
    fn producer_rejects_unsupported_navigation_and_research_before_creating_session() {
        for config in [
            json!({"url":"https://example.invalid/"}),
            json!({"researchMode":"deep"}),
        ] {
            let (_home, driver, context) = fixture(
                json!({"prompt":"PUBLIC original prompt","browserConfig":config,"options":{}}),
            );
            let error = driver.prepare(&context).unwrap_err();
            if config["researchMode"] == "deep" {
                assert_eq!(error.to_string(), REMOTE_DEEP_RESEARCH);
            }
            assert!(!context.store.root().exists());
        }
    }
    #[test]
    fn chatgpt_url_errors_use_the_reference_wording() {
        assert_eq!(
            normalise_url(Some(&json!("ftp://x")))
                .unwrap_err()
                .to_string(),
            r#"Invalid ChatGPT URL protocol: "ftp:". Use http or https."#
        );
        assert_eq!(
            normalise_url(Some(&json!("https://[bad")))
                .unwrap_err()
                .to_string(),
            r#"Invalid ChatGPT URL: "https://[bad". Provide an absolute http(s) URL."#
        );
    }
}
