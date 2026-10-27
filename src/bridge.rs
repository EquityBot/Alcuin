//! Connection artifacts and printed MCP configuration for the local bridge commands.
use crate::{config::Environment, policy};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

pub fn parse_host_port(raw: &str) -> Result<(String, u16)> {
    let raw = policy::trim(raw);
    ensure!(
        !raw.is_empty(),
        "Expected host:port but received an empty value."
    );
    let bracketed = raw
        .strip_prefix('[')
        .and_then(|tail| tail.rsplit_once("]:"))
        .filter(|(host, port)| {
            !host.is_empty() && !port.is_empty() && port.bytes().all(|byte| byte.is_ascii_digit())
        });
    let (host, port) = if let Some(pair) = bracketed {
        pair
    } else {
        let pair = raw.rsplit_once(':').with_context(|| {
            format!(
                "Invalid host:port format: {raw}. Expected host:port (IPv6 must use [host]:port notation)."
            )
        })?;
        ensure!(
            !policy::trim(pair.0).contains(':'),
            "Invalid host:port format: {raw}. Wrap IPv6 addresses in brackets, e.g. \"[2001:db8::1]:9473\"."
        );
        pair
    };
    let host = policy::trim(host);
    ensure!(
        !host.is_empty(),
        "Invalid host:port format: {raw}. Host portion is missing."
    );
    let segment = policy::trim(port);
    let invalid = || format!("Invalid port: \"{segment}\". Expected a number between 1 and 65535.");
    let digits = segment
        .strip_prefix('+')
        .unwrap_or(segment)
        .bytes()
        .take_while(u8::is_ascii_digit)
        .map(char::from)
        .collect::<String>();
    let port = digits
        .parse::<u16>()
        .ok()
        .filter(|port| *port > 0)
        .with_context(invalid)?;
    Ok((host.into(), port))
}
pub fn normalise_host_port(host: &str, port: u16) -> String {
    let host = policy::trim(host);
    let host = host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host);
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}
pub fn parse_connection(raw: &str) -> Result<Value> {
    let raw = policy::trim(raw);
    ensure!(!raw.is_empty(), "Missing connection string.");
    let url = reqwest::Url::parse(&if raw.contains("://") {
        raw.into()
    } else {
        format!("oracle+tcp://{raw}")
    })
    .context("Invalid connection string")?;
    let host = url
        .host_str()
        .context("Connection string host is missing")?;
    let port = url
        .port()
        .filter(|port| *port > 0)
        .context("Invalid connection string host. Expected host:port.")?;
    let token = url
        .query_pairs()
        .find(|(name, _)| name == "token")
        .map(|(_, value)| policy::trim(&value).to_owned())
        .filter(|value| !value.is_empty())
        .context("Connection string is missing token. Expected ?token=...")?;
    Ok(json!({"remoteHost":normalise_host_port(host,port),"remoteToken":token}))
}
pub fn format_connection(connection: &Value, include_token: bool) -> Result<String> {
    let (host, port) = parse_host_port(
        connection["remoteHost"]
            .as_str()
            .context("Connection host is missing")?,
    )?;
    let base = format!("oracle+tcp://{}", normalise_host_port(&host, port));
    if !include_token {
        return Ok(base);
    }
    let token = connection["remoteToken"]
        .as_str()
        .context("Connection token is missing")?;
    let mut url = reqwest::Url::parse("oracle+tcp://localhost")?;
    url.query_pairs_mut().append_pair("token", token);
    Ok(format!("{base}?{}", url.query().unwrap_or("")))
}
pub fn read_connection(path: &Path) -> Result<Value> {
    let shown = std::path::absolute(path)?;
    let shown = shown.display();
    let value: Value = serde_json::from_slice(&std::fs::read(path)?).map_err(|error| {
        anyhow::anyhow!("Failed to parse connection artifact JSON at {shown}: {error}")
    })?;
    // Reference typeof check: arrays count as objects and fail on their fields.
    ensure!(
        value.is_object() || value.is_array(),
        "Invalid connection artifact at {shown}: expected an object."
    );
    for field in ["remoteHost", "remoteToken"] {
        ensure!(
            value[field]
                .as_str()
                .is_some_and(|value| !policy::trim(value).is_empty()),
            "Invalid connection artifact at {shown}: {field} is missing."
        );
    }
    parse_host_port(value["remoteHost"].as_str().unwrap())?;
    Ok(value)
}
fn clean(value: Option<&str>) -> Option<String> {
    value
        .map(policy::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}
pub fn resolve(config: &Value, env: &Environment) -> Value {
    let mut result = json!({"sources":{}});
    for (field, variable, key) in [
        ("remoteHost", "ORACLE_REMOTE_HOST", "host"),
        ("remoteToken", "ORACLE_REMOTE_TOKEN", "token"),
    ] {
        let configured = clean(config["browser"][field].as_str());
        let environment = clean(env.get(variable).map(String::as_str));
        let source = if configured.is_some() {
            "config.browser"
        } else if environment.is_some() {
            "env"
        } else {
            "unset"
        };
        if let Some(value) = configured.or(environment) {
            result[key] = json!(value);
        }
        result["sources"][key] = json!(source);
    }
    result
}
pub fn mcp_snippet(
    kind: &str,
    options: &Value,
    config: &Value,
    env: &Environment,
) -> Result<String> {
    ensure!(
        matches!(kind, "codex-config" | "claude-config"),
        "Unknown MCP configuration kind"
    );
    let local = options["local-browser"] == true;
    let codex = kind == "codex-config";
    let home = env.get(if cfg!(windows) { "USERPROFILE" } else { "HOME" });
    let needs_default = (local || !codex)
        && ((options["oracle-home-dir"].is_null() && !env.contains_key("ORACLE_HOME_DIR"))
            || (options["browser-profile-dir"].is_null()
                && !env.contains_key("ORACLE_BROWSER_PROFILE_DIR")));
    ensure!(
        !needs_default || home.is_some(),
        "MCP configuration has no user home"
    );
    let default = PathBuf::from(home.map(String::as_str).unwrap_or("")).join(if local || codex {
        ".oracle"
    } else {
        ".oracle-local"
    });
    let state = options["oracle-home-dir"]
        .as_str()
        .map(PathBuf::from)
        .or_else(|| env.get("ORACLE_HOME_DIR").map(PathBuf::from))
        .unwrap_or(default.clone());
    let profile = options["browser-profile-dir"]
        .as_str()
        .map(PathBuf::from)
        .or_else(|| env.get("ORACLE_BROWSER_PROFILE_DIR").map(PathBuf::from))
        .unwrap_or_else(|| default.join("browser-profile"));
    if local && codex {
        ensure!(
            !state.as_os_str().is_empty() && !profile.as_os_str().is_empty(),
            "Local browser configuration requires session and profile paths."
        );
    }
    let resolved = resolve(config, env);
    let mut values = json!({"ORACLE_ENGINE":"browser"});
    if local || !codex {
        values["ORACLE_HOME_DIR"] = json!(state);
        values["ORACLE_BROWSER_PROFILE_DIR"] = json!(profile);
    }
    if !local && (codex || resolved["host"].is_string()) {
        values["ORACLE_REMOTE_HOST"] = json!(resolved["host"].as_str().unwrap_or("127.0.0.1:9473"));
        values["ORACLE_REMOTE_TOKEN"] = json!(if options["print-token"] == true {
            resolved["token"].as_str().unwrap_or("<YOUR_TOKEN>")
        } else {
            "<YOUR_TOKEN>"
        });
    }
    if codex {
        let entries = values
            .as_object()
            .unwrap()
            .iter()
            .map(|(name, value)| {
                Ok(format!(
                    "{name} = {}",
                    serde_json::to_string(value)?.replace('\u{7f}', "\\u007f")
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(format!(
            "# ~/.codex/config.toml\n\n[mcp_servers.alcuin]\ncommand = \"alcuin-mcp\"\nargs = []\nenv = {{ {} }}",
            entries.join(", ")
        ))
    } else {
        Ok(serde_json::to_string_pretty(
            &json!({"mcpServers":{"alcuin":{"type":"stdio","command":"alcuin-mcp","args":[],"env":values}}}),
        )?)
    }
}
fn resolve_connection(raw: &str, cwd: &Path) -> Result<Value> {
    if raw.contains("://") {
        return parse_connection(raw);
    }
    if raw.contains(['/', '\\']) || raw.ends_with(".json") {
        let path = std::path::absolute(cwd.join(raw))?;
        match std::fs::metadata(&path) {
            Ok(metadata) if metadata.is_file() => {}
            Ok(_) => bail!(
                "--connect points to {}, but it is not a file.",
                path.display()
            ),
            Err(_) => bail!("Connection artifact not found at {}", path.display()),
        }
        read_connection(&path)
    } else {
        parse_connection(raw)
    }
}
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}
pub async fn doctor(
    loaded: &crate::config::LoadedConfig,
    store: &crate::sessions::SessionStore,
    env: &Environment,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<(String, i32)> {
    let config_path = store.home().join("config.json");
    let configured = loaded.paths.contains(&config_path);
    let mut lines = vec![
        "Bridge doctor".into(),
        format!("OS: {} ({})", std::env::consts::OS, std::env::consts::ARCH),
        "Runtime: Rust".into(),
        format!("Alcuin: {}", env!("CARGO_PKG_VERSION")),
        format!(
            "Config: {}{}",
            config_path.display(),
            if configured { "" } else { " (missing)" }
        ),
    ];
    let projects = loaded
        .paths
        .iter()
        .filter(|path| **path != config_path)
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>();
    if !projects.is_empty() {
        lines.push(format!(
            "Project config{}: {}",
            if projects.len() == 1 { "" } else { "s" },
            projects.join(", ")
        ));
    }
    for (key, label) in [("engine", "Default engine"), ("model", "Default model")] {
        if let Some(value) = loaded.config[key].as_str() {
            lines.push(format!("{label}: {value}"));
        }
    }
    lines.extend(["".into(), "Browser mode".into()]);
    let remote = resolve(&loaded.config, env);
    let mut problems = vec![];
    let mut warnings = loaded.warnings.clone();
    if let Some(host) = remote["host"].as_str() {
        let token = remote["token"].as_str();
        lines.extend([
            "Remote service: configured".into(),
            format!(
                "remoteHost: {host} ({})",
                remote["sources"]["host"].as_str().unwrap()
            ),
            format!(
                "remoteToken: {} ({})",
                if token.is_some() { "set" } else { "missing" },
                remote["sources"]["token"].as_str().unwrap()
            ),
        ]);
        let tcp = async {
            let (host, port) = parse_host_port(host)?;
            let stream = tokio::net::TcpStream::connect((host.as_str(), port)).await?;
            drop(stream);
            Ok::<_, anyhow::Error>(())
        };
        let tcp = tokio::select! {biased; _=cancel.cancelled()=>anyhow::bail!("Bridge doctor cancelled"),result=tokio::time::timeout(std::time::Duration::from_secs(2),tcp)=>result.map_err(anyhow::Error::from).and_then(|result|result)};
        if let Err(error) = &tcp {
            lines.push(format!("TCP connect: failed ({error})"));
            problems.push(format!("Cannot reach {host} ({error})."));
        } else {
            lines.push("TCP connect: ok".into());
        }
        if token.is_none() {
            problems.push("Remote token is missing. Run `alcuin bridge client --connect <...>` or set ORACLE_REMOTE_TOKEN.".into());
        } else if tcp.is_ok() {
            let health = tokio::time::timeout(std::time::Duration::from_secs(5), async {
                crate::remote_client::RemoteClient::new(
                    host,
                    token.map(str::to_owned),
                    store.clone(),
                )?
                .health(cancel)
                .await
            })
            .await;
            let health = match health {
                Ok(Ok(value)) => value,
                Ok(Err(error)) => json!({"ok":false,"error":error.to_string()}),
                Err(_) => json!({"ok":false,"error":"Health check timed out"}),
            };
            if health["ok"] == true {
                lines.push(format!(
                    "Auth (/health): {}",
                    health["version"]
                        .as_str()
                        .map(|version| format!("oracle {version}"))
                        .unwrap_or("ok".into())
                ));
                if health["capabilities"]["artifactTransfer"] == true {
                    let mut size = health["capabilities"]["maxArtifactBytes"]
                        .as_f64()
                        .unwrap_or(0.0);
                    let mut unit = 0;
                    while size >= 1024.0 && unit < 3 {
                        size /= 1024.0;
                        unit += 1;
                    }
                    let digits = usize::from(size < 10.0 && unit > 0);
                    lines.push(format!(
                        "Artifact transfer: bridge v{} ({size:.digits$} {} max)",
                        health["capabilities"]["artifactProtocolVersion"],
                        ["B", "KB", "MB", "GB"][unit]
                    ));
                } else {
                    lines.push("Artifact transfer: manual fallback".into());
                    warnings.push("Remote host does not advertise bridge artifact transfer; ChatGPT-generated files may need manual copy from the browser host.".into());
                }
            } else {
                let detail = health["error"].as_str().unwrap_or("unknown error");
                let status = health["statusCode"]
                    .as_u64()
                    .map(|code| format!("HTTP {code}"))
                    .unwrap_or("network".into());
                lines.push(format!("Auth (/health): {status} ({detail})"));
                problems.push(format!("Remote auth failed: {detail}"));
            }
        }
    } else {
        lines.push("Remote service: not configured".into());
        if let Some(path) = crate::browser_detect::chrome(env) {
            lines.push(format!("Chrome: {}", path.display()));
        } else {
            lines.push("Chrome: not found".into());
            problems.push("No Chrome installation detected. Install Chrome/Chromium or set --browser-chrome-path.".into());
        }
        if cfg!(windows) {
            lines.push("Cookies: (cookie sync disabled on Windows)".into());
            warnings.push("Cookie sync is disabled on Windows; use --browser-manual-login or run browser automation on another host.".into());
        } else if let Some(path) = crate::browser_detect::cookie_db(env) {
            lines.push(format!("Cookies DB: {}", path.display()));
        } else {
            lines.push("Cookies DB: not found".into());
            warnings.push("Chrome cookies DB not detected. You may need --browser-cookie-path or --browser-manual-login.".into());
        }
    }
    lines.extend([
        "".into(),
        "Codex MCP".into(),
        mcp_snippet("codex-config", &json!({}), &loaded.config, env)?,
    ]);
    for (label, messages) in [("Warnings", &warnings), ("Problems", &problems)] {
        if !messages.is_empty() {
            lines.extend(["".into(), format!("{label}:")]);
            lines.extend(messages.iter().map(|message| format!("- {message}")));
        }
    }
    let mut text = lines.join("\n");
    if let Some(token) = remote["token"].as_str() {
        text = text.replace(token, "[redacted]");
    }
    Ok((text, i32::from(!problems.is_empty())))
}
pub async fn configure_client(
    options: &Value,
    store: &crate::sessions::SessionStore,
    cwd: &Path,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<String> {
    ensure!(
        !cancel.is_cancelled(),
        "Bridge configuration cancelled before preparation"
    );
    let raw = options["connect"]
        .as_str()
        .map(policy::trim)
        .filter(|value| !value.is_empty())
        .context(
            "Missing --connect. Provide a connection string or a bridge-connection.json path.",
        )?;
    let connection = resolve_connection(raw, cwd)?;
    let host = connection["remoteHost"]
        .as_str()
        .context("Missing remote host")?;
    let token = connection["remoteToken"]
        .as_str()
        .context("Missing remote token")?;
    let mut lines = vec![];
    if options["no-test"] != true {
        let client =
            crate::remote_client::RemoteClient::new(host, Some(token.into()), store.clone())?;
        let health = client.health(cancel).await?;
        ensure!(
            health["ok"] == true,
            "Remote service health check failed: {}{}",
            health["error"].as_str().unwrap_or("unknown error"),
            health["statusCode"]
                .as_u64()
                .map(|code| format!(" (HTTP {code})"))
                .unwrap_or_default()
        );
        let transfer = if health["capabilities"]["artifactTransfer"] == true {
            format!(
                " — artifacts bridge v{}",
                health["capabilities"]["artifactProtocolVersion"]
            )
        } else {
            " — artifact transfer unavailable; file downloads require manual copy".into()
        };
        let version = health["version"]
            .as_str()
            .map(|value| format!(" — oracle {value}"))
            .unwrap_or_default();
        lines.push(format!("Remote service OK ({host}){version}{transfer}"));
    }
    if options["no-write-config"] != true {
        let displayed = clean(options["config"].as_str())
            .map(PathBuf::from)
            .unwrap_or_else(|| store.home().join("config.json"));
        let path = cwd.join(&displayed);
        // Reference readUserConfigFile: one message for read and parse failures.
        let failed = |cause: &dyn std::fmt::Display| {
            anyhow::anyhow!("Failed to read {}: {cause}", displayed.display())
        };
        let mut config = match std::fs::read_to_string(&path) {
            Ok(raw) => json5::from_str::<Value>(&raw).map_err(|error| failed(&error))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => json!({}),
            Err(error) => return Err(failed(&error)),
        };
        if config.is_null() {
            config = json!({});
        }
        ensure!(
            config.is_object(),
            "Bridge configuration must be an object; existing file was preserved"
        );
        if config["browser"].is_null() {
            config["browser"] = json!({});
        }
        ensure!(
            config["browser"].is_object(),
            "Browser configuration must be an object; existing file was preserved"
        );
        config["browser"]["remoteHost"] = json!(host);
        config["browser"]["remoteToken"] = json!(token);
        if connection["tunnel"].is_object() {
            let mut tunnel = json!({});
            for key in ["ssh", "remotePort", "localPort", "identity", "extraArgs"] {
                if let Some(value) = connection["tunnel"].get(key) {
                    tunnel[key] = value.clone();
                }
            }
            config["browser"]["remoteViaSshReverseTunnel"] = tunnel;
        }
        ensure!(
            !cancel.is_cancelled(),
            "Bridge configuration cancelled before publication"
        );
        let mut bytes = serde_json::to_vec_pretty(&config)?;
        bytes.push(b'\n');
        crate::storage::private_dir(path.parent().context("Configuration path has no parent")?)?;
        crate::storage::atomic_bytes(&path, &bytes)?;
        lines.push(format!("Wrote remote config to {}", displayed.display()));
    }
    lines.extend([
        "".into(),
        "Next:".into(),
        "- alcuin --engine browser -p \"hello\" --file README.md".into(),
    ]);
    if options["print-env"] == true {
        lines.extend([
            "".into(),
            "# Optional env overrides (paste into your shell):".into(),
            "export ORACLE_ENGINE=browser".into(),
            format!("export ORACLE_REMOTE_HOST={}", shell_quote(host)),
            format!("export ORACLE_REMOTE_TOKEN={}", shell_quote(token)),
        ]);
    }
    Ok(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn connection_strings_round_trip_ipv6_and_escaped_tokens_without_default_disclosure() {
        for host in ["127.0.0.1:9473", "example.com:1234", "[2001:db8::1]:9473"] {
            let connection = json!({"remoteHost":host,"remoteToken":"PUBLIC a+b /'~ token"});
            let encoded = format_connection(&connection, true).unwrap();
            assert_eq!(parse_connection(&encoded).unwrap(), connection);
            assert!(
                !format_connection(&connection, false)
                    .unwrap()
                    .contains("token=")
            );
        }
        assert_eq!(
            parse_connection("127.0.0.1:9473?token=PUBLIC").unwrap()["remoteToken"],
            "PUBLIC"
        );
        for raw in ["2001:db8::1:9473", "host:0", "host:65536", ""] {
            assert!(parse_host_port(raw).is_err());
        }
        assert!(parse_connection("host:9473").is_err());
    }
    #[test]
    fn snippets_preserve_independent_paths_source_precedence_and_token_redaction() {
        let env = Environment::from_iter([
            ("HOME".into(), "PUBLIC-home".into()),
            ("USERPROFILE".into(), "PUBLIC-home".into()),
            (
                "ORACLE_REMOTE_TOKEN".into(),
                " PUBLIC environment token ".into(),
            ),
        ]);
        let config = json!({"browser":{"remoteHost":" 127.0.0.1:9473 ","remoteToken":" PUBLIC configured token "}});
        let codex = mcp_snippet("codex-config", &json!({}), &config, &env).unwrap();
        assert!(codex.contains("<YOUR_TOKEN>"));
        assert!(!codex.contains("PUBLIC configured token"));
        assert!(
            mcp_snippet("codex-config", &json!({"print-token":true}), &config, &env)
                .unwrap()
                .contains("PUBLIC configured token")
        );
        let claude: Value =
            serde_json::from_str(&mcp_snippet("claude-config", &json!({}), &config, &env).unwrap())
                .unwrap();
        assert!(
            claude["mcpServers"]["alcuin"]["env"]["ORACLE_HOME_DIR"]
                .as_str()
                .unwrap()
                .ends_with(".oracle-local")
        );
        for kind in ["codex-config", "claude-config"] {
            let text=mcp_snippet(kind,&json!({"local-browser":true,"print-token":true,"oracle-home-dir":"PUBLIC-state","browser-profile-dir":"PUBLIC-profile"}),&config,&env).unwrap();
            assert!(text.contains("PUBLIC-state") && text.contains("PUBLIC-profile"));
            assert!(
                !text.contains("ORACLE_REMOTE_HOST") && !text.contains("PUBLIC configured token")
            );
        }
    }
    #[tokio::test]
    async fn client_negated_flags_preserve_config_fields_and_health_failure_preserves_bytes() {
        use crate::{
            remote_host::{HostOptions, RemoteHost, RunDriver},
            sessions::SessionStore,
        };
        use std::sync::Arc;
        use tokio_util::sync::CancellationToken;
        let home = tempfile::tempdir().unwrap();
        let store = SessionStore::new(home.path().join("oracle"));
        crate::storage::private_dir(store.home()).unwrap();
        let path = store.home().join("config.json");
        std::fs::write(&path,b"{ // PUBLIC comment\n model:'PUBLIC-model', browser:{keepBrowser:true}, extra:{answer:42},\n}").unwrap();
        let connection = json!({"remoteHost":"127.0.0.1:1","remoteToken":"PUBLIC a'b"});
        let options =
            json!({"connect":format_connection(&connection,true).unwrap(),"no-test":true});
        let cancel = CancellationToken::new();
        let output = configure_client(&options, &store, home.path(), &cancel)
            .await
            .unwrap();
        assert!(!output.contains("PUBLIC a'b"));
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(bytes.last(), Some(&b'\n'));
        let saved: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(saved["extra"]["answer"], 42);
        assert_eq!(saved["browser"]["keepBrowser"], true);
        assert_eq!(saved["browser"]["remoteToken"], "PUBLIC a'b");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let no_write = json!({"connect":format_connection(&connection,true).unwrap(),"no-test":true,"no-write-config":true,"print-env":true});
        let output = configure_client(&no_write, &store, home.path(), &cancel)
            .await
            .unwrap();
        assert!(output.contains("'PUBLIC a'\"'\"'b'"));
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        let driver: RunDriver =
            Arc::new(|_| Box::pin(async { anyhow::bail!("PUBLIC health must not dispatch") }));
        let host = RemoteHost::start(
            SessionStore::new(home.path().join("producer")),
            HostOptions {
                address: "127.0.0.1:0".parse().unwrap(),
                token: "PUBLIC correct token".into(),
                max_concurrent_runs: None,
                max_queued_runs: None,
                max_browser_tabs: 1,
            },
            driver,
        )
        .await
        .unwrap();
        let invalid = json!({"connect":format_connection(&json!({"remoteHost":host.address.to_string(),"remoteToken":"PUBLIC wrong token"}),true).unwrap()});
        assert!(
            configure_client(&invalid, &store, home.path(), &cancel)
                .await
                .is_err()
        );
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        host.shutdown().await.unwrap();
    }
}
