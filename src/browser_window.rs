//! Window placement uses only the current consultation target and its profile.
use crate::{
    browser_profile::ProfileRegistry, cdp::Cdp, native_fs::NativeEntry, storage::atomic_json,
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{io::Read, time::Duration};
use tokio_util::sync::CancellationToken;

const SAVED_STATE: &str = "oracle-window-state.json";

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct Bounds {
    #[serde(skip_serializing_if = "Option::is_none")]
    left: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    width: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    height: Option<i32>,
    #[serde(default = "normal")]
    window_state: String,
}
fn normal() -> String {
    "normal".into()
}
impl Bounds {
    fn parse(value: &Value) -> Result<Self> {
        let bounds: Self = serde_json::from_value(value.clone())?;
        ensure!(
            matches!(
                bounds.window_state.as_str(),
                "normal" | "minimized" | "maximized" | "fullscreen"
            ),
            "Unknown Chrome window state"
        );
        ensure!(
            bounds.width.is_none_or(|value| value > 0)
                && bounds.height.is_none_or(|value| value > 0),
            "Chrome window size must be positive"
        );
        Ok(bounds)
    }
    fn saved(value: &Value) -> Option<Self> {
        let bounds = Self::parse(value).ok()?;
        if bounds.window_state != "normal" {
            return Some(Self {
                window_state: bounds.window_state,
                ..Self::default()
            });
        }
        (bounds.window_state != "normal"
            || [bounds.left, bounds.top, bounds.width, bounds.height]
                .iter()
                .all(Option::is_some))
        .then_some(bounds)
    }
    fn rect(&self) -> Value {
        let mut rect = json!(self);
        rect.as_object_mut().unwrap().remove("windowState");
        rect
    }
    fn satisfied(&self, current: &Self) -> bool {
        if self.window_state != current.window_state {
            return false;
        }
        if matches!(self.window_state.as_str(), "maximized" | "fullscreen") {
            return covers(current.left, current.width, self.left, self.width)
                && covers(current.top, current.height, self.top, self.height);
        }
        [
            (self.left, current.left),
            (self.top, current.top),
            (self.width, current.width),
            (self.height, current.height),
        ]
        .iter()
        .all(|(wanted, actual)| wanted.is_none() || wanted == actual)
    }
}
fn covers(
    start: Option<i32>,
    size: Option<i32>,
    wanted: Option<i32>,
    wanted_size: Option<i32>,
) -> bool {
    let Some(wanted) = wanted else {
        return true;
    };
    let centre = f64::from(wanted) + f64::from(wanted_size.unwrap_or(0)) / 2.0;
    start.zip(size).is_some_and(|(start, size)| {
        centre >= f64::from(start) && centre < f64::from(start) + f64::from(size)
    })
}
fn read_json(profile: &ProfileRegistry, name: &str, limit: u64) -> Result<Value> {
    profile.check()?;
    let mut entry = NativeEntry::open(&profile.path().join(name), false)?;
    ensure!(
        entry.handle.metadata()?.len() <= limit,
        "Chrome window metadata exceeds its limit"
    );
    let mut bytes = Vec::new();
    entry
        .handle
        .by_ref()
        .take(limit + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= limit,
        "Chrome window metadata grew beyond its limit"
    );
    profile.check()?;
    Ok(serde_json::from_slice(&bytes)?)
}
fn saved(profile: &ProfileRegistry) -> Option<Bounds> {
    let value = read_json(profile, SAVED_STATE, 64 * 1024).ok()?;
    (value["version"] == 1).then(|| Bounds::saved(&value["bounds"]))?
}
fn save(profile: &ProfileRegistry, bounds: &Bounds) -> Result<()> {
    profile.check()?;
    atomic_json(
        &profile.path().join(SAVED_STATE),
        &json!({"version":1,"bounds":bounds}),
    )?;
    profile.check()
}
pub(crate) fn prepare_hidden_launch(profile: &ProfileRegistry) -> Result<()> {
    if saved(profile).is_some() {
        return Ok(());
    }
    let value = read_json(profile, "Default/Preferences", 16 * 1024 * 1024).unwrap_or(Value::Null);
    let bounds = persisted(&value["browser"]["window_placement"]).unwrap_or_else(|| Bounds {
        left: Some(80),
        top: Some(80),
        width: Some(1280),
        height: Some(720),
        window_state: normal(),
    });
    save(profile, &bounds)
}
fn persisted(value: &Value) -> Option<Bounds> {
    if value["maximized"] == true {
        return Some(Bounds {
            window_state: "maximized".into(),
            ..Bounds::default()
        });
    }
    let number = |key| {
        value[key]
            .as_i64()
            .and_then(|value| i32::try_from(value).ok())
    };
    let left = number("left")?;
    let top = number("top")?;
    let width = number("right")?.checked_sub(left)?;
    let height = number("bottom")?.checked_sub(top)?;
    (width > 0 && height > 0).then_some(Bounds {
        left: Some(left),
        top: Some(top),
        width: Some(width),
        height: Some(height),
        window_state: normal(),
    })
}
pub(crate) async fn configure(
    page: &Cdp,
    profile: &ProfileRegistry,
    config: &Value,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<()> {
    if config["headless"] == true {
        return Ok(());
    }
    if cfg!(target_os = "macos") {
        let hidden = config["hideWindow"] == true;
        let restore = saved(profile);
        if hidden || restore.is_some() {
            let found = page
                .call("Browser.getWindowForTarget", json!({}), timeout, cancel)
                .await?;
            let window = found["windowId"]
                .as_i64()
                .context("Chrome has no target window")?;
            let bounds = if hidden {
                if restore.is_none() {
                    let observed = page
                        .call(
                            "Browser.getWindowBounds",
                            json!({"windowId":window}),
                            timeout,
                            cancel,
                        )
                        .await?;
                    save(
                        profile,
                        &Bounds::saved(&observed["bounds"])
                            .context("Chrome returned invalid pre-hide bounds")?,
                    )?;
                }
                json!({"left":-32000,"top":-32000,"windowState":"normal"})
            } else {
                json!(restore.unwrap())
            };
            page.call(
                "Browser.setWindowBounds",
                json!({"windowId":window,"bounds":bounds}),
                timeout,
                cancel,
            )
            .await?;
            if !hidden {
                profile.check()?;
                std::fs::remove_file(profile.path().join(SAVED_STATE))?;
            }
        }
    } else if config["hideWindow"] == true {
        eprintln!("Window hiding is only supported on macOS");
    }
    if config["hideWindow"] != true && !config["windowBounds"].is_null() {
        place(
            page,
            &Bounds::parse(&config["windowBounds"])?,
            timeout,
            cancel,
        )
        .await?;
    }
    Ok(())
}
async fn place(
    page: &Cdp,
    target: &Bounds,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<()> {
    let found = page
        .call("Browser.getWindowForTarget", json!({}), timeout, cancel)
        .await?;
    let window = found["windowId"]
        .as_i64()
        .context("Chrome has no target window")?;
    let mut bounds = Bounds::parse(&found["bounds"])?;
    if target.satisfied(&bounds) {
        return Ok(());
    }
    for _ in 0..3 {
        if bounds.window_state == "normal" {
            break;
        }
        page.call(
            "Browser.setWindowBounds",
            json!({"windowId":window,"bounds":{"windowState":"normal"}}),
            timeout,
            cancel,
        )
        .await?;
        let found = page
            .call(
                "Browser.getWindowBounds",
                json!({"windowId":window}),
                timeout,
                cancel,
            )
            .await?;
        bounds = Bounds::parse(&found["bounds"])?;
    }
    let mut rect = target.rect();
    if matches!(target.window_state.as_str(), "maximized" | "fullscreen")
        && (target.left.is_some() || target.top.is_some())
    {
        if target.width.is_none() {
            rect["width"] = json!(800);
        }
        if target.height.is_none() {
            rect["height"] = json!(600);
        }
    }
    if !rect.as_object().unwrap().is_empty() {
        rect["windowState"] = json!("normal");
        page.call(
            "Browser.setWindowBounds",
            json!({"windowId":window,"bounds":rect}),
            timeout,
            cancel,
        )
        .await?;
    }
    if target.window_state != "normal" {
        page.call(
            "Browser.setWindowBounds",
            json!({"windowId":window,"bounds":{"windowState":target.window_state}}),
            timeout,
            cancel,
        )
        .await?;
    }
    let placed = page
        .call(
            "Browser.getWindowBounds",
            json!({"windowId":window}),
            timeout,
            cancel,
        )
        .await?;
    if !target.satisfied(&Bounds::parse(&placed["bounds"])?) {
        eprintln!("Chrome window did not reach its configured bounds");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::{
        WebSocketStream,
        tungstenite::{Message, protocol::Role},
    };

    #[tokio::test]
    async fn placement_normalises_then_stages_the_requested_display_before_maximising() {
        let (client, server) = tokio::io::duplex(64 * 1024);
        let client = WebSocketStream::from_raw_socket(client, Role::Client, None).await;
        let mut server = WebSocketStream::from_raw_socket(server, Role::Server, None).await;
        let worker = tokio::spawn(async move {
            for (method, params, result) in [
                (
                    "Browser.getWindowForTarget",
                    json!({}),
                    json!({"windowId":7,"bounds":{"windowState":"fullscreen"}}),
                ),
                (
                    "Browser.setWindowBounds",
                    json!({"windowId":7,"bounds":{"windowState":"normal"}}),
                    json!({}),
                ),
                (
                    "Browser.getWindowBounds",
                    json!({"windowId":7}),
                    json!({"bounds":{"windowState":"maximized"}}),
                ),
                (
                    "Browser.setWindowBounds",
                    json!({"windowId":7,"bounds":{"windowState":"normal"}}),
                    json!({}),
                ),
                (
                    "Browser.getWindowBounds",
                    json!({"windowId":7}),
                    json!({"bounds":{"left":80,"top":80,"width":1280,"height":720,"windowState":"normal"}}),
                ),
                (
                    "Browser.setWindowBounds",
                    json!({"windowId":7,"bounds":{"left":1920,"top":0,"width":800,"height":600,"windowState":"normal"}}),
                    json!({}),
                ),
                (
                    "Browser.setWindowBounds",
                    json!({"windowId":7,"bounds":{"windowState":"maximized"}}),
                    json!({}),
                ),
                (
                    "Browser.getWindowBounds",
                    json!({"windowId":7}),
                    json!({"bounds":{"left":1912,"top":-8,"width":1936,"height":1096,"windowState":"maximized"}}),
                ),
            ] {
                let frame = tokio::time::timeout(Duration::from_secs(1), server.next())
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap();
                let Message::Text(frame) = frame else {
                    panic!("Expected CDP request")
                };
                let request: Value = serde_json::from_str(&frame).unwrap();
                assert_eq!(request["method"], method);
                assert_eq!(request["params"], params);
                server
                    .send(Message::Text(
                        json!({"id":request["id"],"result":result})
                            .to_string()
                            .into(),
                    ))
                    .await
                    .unwrap();
            }
        });
        let page = Cdp::from_socket(client, Duration::from_secs(1));
        let home = tempfile::tempdir().unwrap();
        let profile = ProfileRegistry::open(home.path()).unwrap();
        configure(
            &page,
            &profile,
            &json!({"windowBounds":{"left":1920,"top":0,"windowState":"maximized"}}),
            Duration::from_secs(1),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        tokio::time::timeout(Duration::from_secs(1), worker)
            .await
            .unwrap()
            .unwrap();
        page.disconnect_and_wait(Duration::from_secs(1))
            .await
            .unwrap();
    }
    #[test]
    fn placement_checks_display_centres_and_rejects_invalid_saved_bounds() {
        let target = Bounds::parse(&json!({"left":1920,"windowState":"maximized"})).unwrap();
        for (left, width, expected) in [(1912, 1936, true), (0, 1920, false), (1920, 1920, true)] {
            let actual =
                Bounds::parse(&json!({"left":left,"width":width,"windowState":"maximized"}))
                    .unwrap();
            assert_eq!(target.satisfied(&actual), expected);
        }
        for value in [
            json!({"left":0}),
            json!({"left":0,"top":0,"width":0,"height":10}),
            json!({"windowState":"unknown"}),
        ] {
            assert!(Bounds::saved(&value).is_none());
        }
        assert!(Bounds::saved(&json!({"windowState":"fullscreen"})).is_some());
        let maximised = Bounds::saved(
            &json!({"left":0,"top":0,"width":1920,"height":1080,"windowState":"maximized"}),
        )
        .unwrap();
        assert_eq!(json!(maximised), json!({"windowState":"maximized"}));
        assert!(persisted(&json!({"left":10,"right":0,"top":0,"bottom":10})).is_none());
    }
    #[test]
    fn hidden_launch_saves_original_placement_once_and_ignores_invalid_cache() {
        let home = tempfile::tempdir().unwrap();
        let profile = ProfileRegistry::open(home.path()).unwrap();
        std::fs::create_dir(home.path().join("Default")).unwrap();
        atomic_json(&home.path().join("Default/Preferences"),&json!({"browser":{"window_placement":{"left":120,"top":200,"right":1400,"bottom":920}}})).unwrap();
        prepare_hidden_launch(&profile).unwrap();
        let original = std::fs::read(home.path().join(SAVED_STATE)).unwrap();
        assert_eq!(saved(&profile).unwrap().left, Some(120));
        atomic_json(
            &home.path().join("Default/Preferences"),
            &json!({"browser":{"window_placement":{"maximized":true}}}),
        )
        .unwrap();
        prepare_hidden_launch(&profile).unwrap();
        assert_eq!(
            std::fs::read(home.path().join(SAVED_STATE)).unwrap(),
            original
        );
        atomic_json(
            &home.path().join(SAVED_STATE),
            &json!({"version":1,"bounds":{"width":-1}}),
        )
        .unwrap();
        prepare_hidden_launch(&profile).unwrap();
        assert_eq!(saved(&profile).unwrap().window_state, "maximized");
    }
}
