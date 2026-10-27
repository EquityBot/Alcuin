//! Completion notifications (reference src/cli/notifier.ts). Delivery is best effort: it never
//! changes a run's outcome, and test environments never deliver.
use crate::config::Environment;
use serde_json::{Value, json};
use std::{process::Stdio, time::Duration};

const TITLE: &str = "Industrious\u{1f9ff} finished";
const DELIVERY_TIMEOUT: Duration = Duration::from_secs(10);

/// Reference resolveNotificationSettings: option, then ORACLE_NOTIFY[_SOUND], then config
/// `notify`, then on unless CI, SSH or a configured mute applies. Sound defaults off.
pub fn resolve(input: &Value, env: &Environment, config: &Value) -> Value {
    let set = |key: &str| env.get(key).is_some_and(|value| !value.is_empty());
    let mute = config["muteIn"].as_array().is_some_and(|mute| {
        (mute.iter().any(|v| v == "CI") && set("CI"))
            || (mute.iter().any(|v| v == "SSH") && set("SSH_CONNECTION"))
    });
    let default = !(set("CI") || set("SSH_CONNECTION") || mute);
    let toggle = |key: &str| {
        let value = env.get(key)?.trim().to_lowercase();
        match value.as_str() {
            "1" | "true" | "yes" | "on" => Some(true),
            "0" | "false" | "no" | "off" => Some(false),
            _ => None,
        }
    };
    json!({
        "enabled": input["notify"].as_bool()
            .or_else(|| toggle("ORACLE_NOTIFY"))
            .or(config["enabled"].as_bool())
            .unwrap_or(default),
        "sound": input["notifySound"].as_bool()
            .or_else(|| toggle("ORACLE_NOTIFY_SOUND"))
            .or(config["sound"].as_bool())
            .unwrap_or(false),
    })
}

/// Reference test-environment guard, read from this process.
fn disabled_here() -> bool {
    let var = |key: &str| std::env::var(key).ok();
    var("ORACLE_DISABLE_NOTIFICATIONS").as_deref() == Some("1")
        || var("NODE_ENV").as_deref() == Some("test")
        || ["VITEST", "VITEST_WORKER_ID", "JEST_WORKER_ID"]
            .iter()
            .any(|key| var(key).is_some_and(|value| !value.is_empty()))
}

/// Reference buildMessage: session name, API cost, character count and a cleaned preview.
pub fn message(metadata: &Value, answer: &str) -> String {
    let mut parts = vec![
        metadata["options"]["slug"]
            .as_str()
            .or(metadata["id"].as_str())
            .unwrap_or("session")
            .to_owned(),
    ];
    // Reference: API runs show the stored cost, else one priced from the token usage, at cents.
    if metadata["mode"] == "api"
        && let Some(cost) = metadata["usage"]["cost"]
            .as_f64()
            .or_else(|| crate::session_display::priced_cost(metadata))
    {
        let cents: f64 = crate::cli_execution::to_fixed(cost, 2)
            .parse()
            .unwrap_or_default();
        parts.push(format!("${}", crate::cli_execution::to_fixed(cents, 4)));
    }
    parts.push(format!(
        "{} chars",
        crate::cli::grouped(answer.encode_utf16().count())
    ));
    let preview: String =
        String::from_utf16_lossy(&answer.encode_utf16().take(140).collect::<Vec<_>>());
    let preview = sanitize_preview(&preview);
    if !preview.is_empty() {
        parts.push(preview);
    }
    parts.join(" \u{b7} ")
}

/// Reference sanitizePreview: plain text without code, links, emphasis or block markers.
pub fn sanitize_preview(text: &str) -> String {
    use regex::Regex;
    use std::sync::LazyLock;
    static RULES: LazyLock<Vec<(Regex, &str)>> = LazyLock::new(|| {
        [
            (r"```[\s\S]*?```", " "),
            (r"`([^`]+)`", "$1"),
            (r"!\[([^\]]*)\]\([^)]+\)", "$1"),
            (r"\[([^\]]+)\]\([^)]+\)", "$1"),
            (r"(\*\*|__|\*|_)", ""),
            (r"(?m)^\s*#+\s*", ""),
            (r"(?m)^\s*[-*+]\s+", ""),
            (r"(?m)^\s*>\s+", ""),
            (r"\s+", " "),
        ]
        .into_iter()
        .map(|(pattern, replacement)| (Regex::new(pattern).expect("static pattern"), replacement))
        .collect()
    });
    let mut text = text.to_owned();
    for (pattern, replacement) in RULES.iter() {
        text = pattern.replace_all(&text, *replacement).into_owned();
    }
    let text = text.trim();
    let units: Vec<u16> = text.encode_utf16().collect();
    if units.len() > 200 {
        format!("{}\u{2026}", String::from_utf16_lossy(&units[..199]))
    } else {
        text.to_owned()
    }
}

/// Sends the completion notification saved with the session, if enabled. Failures are
/// reported on stderr and never returned.
pub async fn completed(metadata: &Value, answer: &str) {
    let settings = &metadata["notifications"];
    if settings["enabled"] != true || disabled_here() {
        return;
    }
    let message = message(metadata, answer);
    if let Err(error) = deliver(TITLE, &message, settings["sound"] == true).await {
        eprintln!("(notify skipped: {error:#})");
    }
}

async fn deliver(title: &str, message: &str, sound: bool) -> anyhow::Result<()> {
    let mut command = platform_command(title, message, sound)?;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let status = tokio::time::timeout(DELIVERY_TIMEOUT, command.status())
        .await
        .map_err(|_| anyhow::anyhow!("notifier timed out"))??;
    anyhow::ensure!(status.success(), "notifier exited with {status}");
    Ok(())
}

#[cfg(windows)]
fn platform_command(
    title: &str,
    message: &str,
    sound: bool,
) -> anyhow::Result<tokio::process::Command> {
    // Windows PowerShell 5.1 loads the WinRT toast API; text arrives through the environment
    // so no notification content is ever parsed as script.
    const SCRIPT: &str = r#"
[void][Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType = WindowsRuntime]
[void][Windows.Data.Xml.Dom.XmlDocument, Windows.Data.Xml.Dom.XmlDocument, ContentType = WindowsRuntime]
$xml = New-Object Windows.Data.Xml.Dom.XmlDocument
$audio = if ($env:INDUSTRIOUS_NOTIFY_SOUND -eq '1') { '' } else { '<audio silent="true"/>' }
$xml.LoadXml("<toast><visual><binding template=`"ToastGeneric`"><text/><text/></binding></visual>$audio</toast>")
$texts = $xml.GetElementsByTagName('text')
[void]$texts.Item(0).AppendChild($xml.CreateTextNode($env:INDUSTRIOUS_NOTIFY_TITLE))
[void]$texts.Item(1).AppendChild($xml.CreateTextNode($env:INDUSTRIOUS_NOTIFY_MESSAGE))
$app = '{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}\WindowsPowerShell\v1.0\powershell.exe'
[Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier($app).Show([Windows.UI.Notifications.ToastNotification]::new($xml))
"#;
    let root = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
    let mut command = tokio::process::Command::new(
        std::path::Path::new(&root).join(r"System32\WindowsPowerShell\v1.0\powershell.exe"),
    );
    command
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            SCRIPT,
        ])
        .env("INDUSTRIOUS_NOTIFY_TITLE", title)
        .env("INDUSTRIOUS_NOTIFY_MESSAGE", message)
        .env("INDUSTRIOUS_NOTIFY_SOUND", if sound { "1" } else { "0" })
        .creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
    Ok(command)
}

#[cfg(target_os = "macos")]
fn platform_command(
    title: &str,
    message: &str,
    sound: bool,
) -> anyhow::Result<tokio::process::Command> {
    // Reference AppleScript fallback with its escaping.
    let escape = |value: &str| value.replace('\\', "\\\\").replace('"', "\\\"");
    let mut script = format!(
        "display notification \"{}\" with title \"{}\"",
        escape(message),
        escape(title)
    );
    if sound {
        script.push_str(" sound name \"Glass\"");
    }
    let mut command = tokio::process::Command::new("/usr/bin/osascript");
    command.args(["-e", &script]);
    Ok(command)
}

#[cfg(all(unix, not(target_os = "macos")))]
fn platform_command(
    title: &str,
    message: &str,
    _sound: bool,
) -> anyhow::Result<tokio::process::Command> {
    let mut command = tokio::process::Command::new("notify-send");
    command.args(["--app-name=Industrious", "--", title, message]);
    Ok(command)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn settings_follow_option_environment_config_then_ci_and_ssh_defaults() {
        let env = |pairs: &[(&str, &str)]| -> Environment {
            pairs
                .iter()
                .map(|(k, v)| ((*k).into(), (*v).into()))
                .collect()
        };
        let none = json!({});
        assert_eq!(
            resolve(&none, &env(&[]), &none),
            json!({"enabled":true,"sound":false})
        );
        assert_eq!(
            resolve(&none, &env(&[("CI", "1")]), &none)["enabled"],
            false
        );
        assert_eq!(
            resolve(&none, &env(&[("SSH_CONNECTION", "x")]), &none)["enabled"],
            false
        );
        assert_eq!(
            resolve(&none, &env(&[("CI", "1")]), &json!({"enabled":true}))["enabled"],
            true
        );
        assert_eq!(
            resolve(
                &none,
                &env(&[("ORACLE_NOTIFY", "off")]),
                &json!({"enabled":true})
            )["enabled"],
            false
        );
        assert_eq!(
            resolve(
                &json!({"notify":true,"notifySound":true}),
                &env(&[("ORACLE_NOTIFY", "0"), ("ORACLE_NOTIFY_SOUND", "no")]),
                &none
            ),
            json!({"enabled":true,"sound":true})
        );
        assert_eq!(
            resolve(
                &none,
                &env(&[("ORACLE_NOTIFY_SOUND", "maybe")]),
                &json!({"sound":true})
            )["sound"],
            true
        );
        assert_eq!(
            resolve(&none, &env(&[]), &json!({"muteIn":["CI"]}))["enabled"],
            true
        );
    }
    /// Manual check on a desktop: `cargo test --lib notify -- --ignored`. Shows one toast.
    #[tokio::test]
    #[ignore]
    async fn delivers_a_real_notification() {
        deliver(
            TITLE,
            "PUBLIC notification check \u{b7} 12 chars \u{b7} delivery works",
            false,
        )
        .await
        .unwrap();
    }
    #[test]
    fn cargo_test_processes_never_deliver() {
        // rust/.cargo/config.toml sets this for every cargo-run process; tests that clear the
        // environment of a launched binary set it themselves.
        assert!(disabled_here());
    }
    #[test]
    fn message_matches_the_reference_layout_and_preview_cleaning() {
        let api = json!({"id":"PUBLIC-id","mode":"api","options":{"slug":"public-run"},"usage":{"cost":0.12345}});
        let answer = "# Heading\n\nSee [the docs](https://example.invalid) and `code`.\n```\nfenced\n```\n- **bold** item\n> quoted";
        assert_eq!(
            message(&api, answer),
            format!(
                "public-run \u{b7} $0.1200 \u{b7} {} chars \u{b7} Heading See the docs and code. bold item quoted",
                answer.encode_utf16().count()
            )
        );
        let priced = json!({"id":"PUBLIC-id","mode":"api","model":"gpt-6-astra","usage":{"inputTokens":1000,"outputTokens":200}});
        assert_eq!(
            message(&priced, ""),
            "PUBLIC-id \u{b7} $0.0200 \u{b7} 0 chars"
        );
        let tie = json!({"id":"PUBLIC-id","mode":"api","usage":{"cost":0.125}});
        assert_eq!(message(&tie, ""), "PUBLIC-id \u{b7} $0.1300 \u{b7} 0 chars");
        let browser = json!({"id":"PUBLIC-id","mode":"browser","usage":{"cost":1.0}});
        assert_eq!(message(&browser, ""), "PUBLIC-id \u{b7} 0 chars");
        assert_eq!(crate::cli::grouped(1_234_567), "1,234,567");
        let long = "x".repeat(250);
        let cleaned = sanitize_preview(&long);
        assert_eq!(cleaned.chars().count(), 200);
        assert!(cleaned.ends_with('\u{2026}'));
    }
}
