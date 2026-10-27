// tests/browser/defaultProfileGuard.test.ts: ORACLE_REFUSE_DEFAULT_BROWSER_PROFILE=1 keeps a
// missing profile option from ever reaching the signed-in ~/.oracle/browser-profile.
use alcuin::config::{Environment, REFUSE_DEFAULT_PROFILE_ENV, manual_login_profile_dir};
use std::path::PathBuf;

#[test]
fn proof_isolation_refuses_only_the_default_profile() {
    let refuse = Environment::from([(REFUSE_DEFAULT_PROFILE_ENV.into(), "1".into())]);
    let error = manual_login_profile_dir([None, Some("  ")], &refuse, "PUBLIC-home").unwrap_err();
    assert_eq!(
        error.to_string(),
        "ORACLE_REFUSE_DEFAULT_BROWSER_PROFILE=1 refuses the default signed-in browser profile; pass an explicit profile directory."
    );
    assert_eq!(
        manual_login_profile_dir([None, Some(" PUBLIC-profile ")], &refuse, "PUBLIC-home").unwrap(),
        PathBuf::from("PUBLIC-profile")
    );
    assert_eq!(
        manual_login_profile_dir([None], &Environment::new(), "PUBLIC-home").unwrap(),
        PathBuf::from("PUBLIC-home").join(".oracle/browser-profile")
    );
}

#[test]
fn compiled_browser_health_refuses_the_default_profile_before_chrome() {
    let home = tempfile::tempdir().unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_alcuin"))
        .arg("browser-health")
        .env_clear()
        .env("ORACLE_DISABLE_NOTIFICATIONS", "1")
        .env("HOME", home.path())
        .env("USERPROFILE", home.path())
        .env("ORACLE_HOME_DIR", home.path().join(".oracle"))
        .env(REFUSE_DEFAULT_PROFILE_ENV, "1")
        .current_dir(home.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("ORACLE_REFUSE_DEFAULT_BROWSER_PROFILE=1 refuses"),
        "{stderr}"
    );
    assert!(!home.path().join(".oracle/browser-profile").exists());
}
