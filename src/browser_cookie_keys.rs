//! Native current-user Safe Storage access. No browser injection or automatic unlock.
#[cfg(windows)]
use anyhow::Context;
use anyhow::{Result, ensure};
use zeroize::Zeroizing;

#[cfg(windows)]
struct ProtectedOutput(windows_sys::Win32::Security::Cryptography::CRYPT_INTEGER_BLOB);
#[cfg(windows)]
impl Drop for ProtectedOutput {
    fn drop(&mut self) {
        if !self.0.pbData.is_null() {
            use zeroize::Zeroize;
            // SAFETY: Crypt(Un)ProtectData allocated exactly cbData bytes through
            // LocalAlloc. This owner wipes and releases that allocation once.
            unsafe {
                std::slice::from_raw_parts_mut(self.0.pbData, self.0.cbData as usize).zeroize();
                windows_sys::Win32::Foundation::LocalFree(self.0.pbData.cast());
            }
        }
    }
}
#[cfg(windows)]
pub(super) fn windows_master_key(local_state: &[u8]) -> Result<Zeroizing<[u8; 32]>> {
    use base64::{Engine, engine::general_purpose::STANDARD};
    use windows_sys::Win32::Security::Cryptography::{
        CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptUnprotectData,
    };
    ensure!(
        local_state.len() <= crate::browser_auth::MAX_COOKIE_INPUT,
        "Chrome Local State exceeds its key input limit"
    );
    let state: serde_json::Value = serde_json::from_slice(local_state)
        .map_err(|_| anyhow::anyhow!("Chrome Local State is not valid JSON"))?;
    let encrypted = state["os_crypt"]["encrypted_key"]
        .as_str()
        .context("Chrome Local State has no current-user encrypted key")?;
    let encrypted = Zeroizing::new(
        STANDARD
            .decode(encrypted)
            .map_err(|_| anyhow::anyhow!("Chrome current-user encrypted key is invalid"))?,
    );
    let encrypted = encrypted
        .strip_prefix(b"DPAPI")
        .context("Chrome key does not use current-user DPAPI")?;
    let input = CRYPT_INTEGER_BLOB {
        cbData: u32::try_from(encrypted.len())?,
        pbData: encrypted.as_ptr().cast_mut(),
    };
    let mut output = ProtectedOutput(CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    });
    // SAFETY: input bytes and both blob records remain live for this synchronous
    // current-user operation. No prompt, optional entropy or machine scope is used.
    let ok = unsafe {
        CryptUnprotectData(
            &input,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output.0,
        )
    };
    ensure!(
        ok != 0 && output.0.cbData == 32 && !output.0.pbData.is_null(),
        "Chrome current-user key is unavailable"
    );
    let mut key = Zeroizing::new([0u8; 32]);
    // SAFETY: successful unprotection returned exactly32 bytes in the owned blob.
    key.copy_from_slice(unsafe { std::slice::from_raw_parts(output.0.pbData, 32) });
    Ok(key)
}

#[cfg(target_os = "macos")]
pub(super) fn macos_password() -> Result<Zeroizing<Vec<u8>>> {
    let password = Zeroizing::new(
        security_framework::passwords::get_generic_password("Chrome Safe Storage", "Chrome")
            .map_err(|_| anyhow::anyhow!("Chrome Safe Storage password is unavailable"))?,
    );
    ensure!(
        !password.is_empty() && password.len() <= 16 * 1024,
        "Chrome Safe Storage password has an unsupported size"
    );
    Ok(password)
}

#[cfg(target_os = "linux")]
pub(super) async fn linux_password(
    deadline: std::time::Instant,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<Zeroizing<Vec<u8>>> {
    use secret_service::{EncryptionType, SecretService};
    use std::collections::HashMap;
    ensure!(
        !cancel.is_cancelled(),
        "Chrome Safe Storage lookup cancelled"
    );
    ensure!(
        std::time::Instant::now() < deadline,
        "Chrome Safe Storage lookup deadline reached"
    );
    let work = async {
        let service = SecretService::connect(EncryptionType::Dh).await?;
        let mut matches = service
            .search_items(HashMap::from([
                ("service", "Chrome Safe Storage"),
                ("account", "Chrome"),
            ]))
            .await?;
        if matches.unlocked.is_empty() && matches.locked.is_empty() {
            matches = service
                .search_items(HashMap::from([("application", "chrome")]))
                .await?;
        }
        // Never unlock or choose among ambiguous entries from another account/store.
        ensure!(
            matches.locked.is_empty() && matches.unlocked.len() == 1,
            "Chrome Safe Storage is locked, missing or ambiguous"
        );
        let password = Zeroizing::new(matches.unlocked.remove(0).get_secret().await?);
        ensure!(
            !password.is_empty() && password.len() <= 16 * 1024,
            "Chrome Safe Storage password has an unsupported size"
        );
        Ok::<_, anyhow::Error>(password)
    };
    tokio::select! {
        biased;
        _ = cancel.cancelled() => anyhow::bail!("Chrome Safe Storage lookup cancelled"),
        result = tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), work) => {
            result.map_err(|_|anyhow::anyhow!("Chrome Safe Storage lookup deadline reached"))?
                .map_err(|_|anyhow::anyhow!("Chrome Safe Storage password is unavailable"))
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod linux_tests {
    use super::*;
    #[tokio::test]
    async fn cancelled_or_expired_lookup_never_opens_the_user_keyring() {
        let cancel = tokio_util::sync::CancellationToken::new();
        cancel.cancel();
        assert!(
            linux_password(
                std::time::Instant::now() + std::time::Duration::from_secs(5),
                &cancel
            )
            .await
            .unwrap_err()
            .to_string()
            .contains("cancelled")
        );
        assert!(
            linux_password(
                std::time::Instant::now(),
                &tokio_util::sync::CancellationToken::new()
            )
            .await
            .unwrap_err()
            .to_string()
            .contains("deadline")
        );
    }
}

#[cfg(all(test, windows))]
pub(crate) fn synthetic_local_state() -> Vec<u8> {
    use base64::{Engine, engine::general_purpose::STANDARD};
    use windows_sys::Win32::Security::Cryptography::{
        CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData,
    };
    let key = [0x42; 32];
    let input = CRYPT_INTEGER_BLOB {
        cbData: key.len() as u32,
        pbData: key.as_ptr().cast_mut(),
    };
    let mut output = ProtectedOutput(CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    });
    // SAFETY: all pointers reference live records and a public synthetic key.
    assert_ne!(
        unsafe {
            CryptProtectData(
                &input,
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output.0,
            )
        },
        0
    );
    let mut encrypted = b"DPAPI".to_vec();
    // SAFETY: successful protection returned an allocation owned by output.
    encrypted.extend_from_slice(unsafe {
        std::slice::from_raw_parts(output.0.pbData, output.0.cbData as usize)
    });
    serde_json::to_vec(
        &serde_json::json!({"os_crypt":{"encrypted_key":STANDARD.encode(encrypted)}}),
    )
    .unwrap()
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    #[test]
    fn synthetic_current_user_dpapi_key_round_trip_and_invalid_inputs_are_sanitised() {
        let state = synthetic_local_state();
        let key = [0x42; 32];
        assert_eq!(&*windows_master_key(&state).unwrap(), &key);
        for invalid in [
            b"PUBLIC invalid JSON".as_slice(),
            br#"{"os_crypt":{"encrypted_key":"PUBLIC-invalid"}}"#,
            br#"{"os_crypt":{"encrypted_key":"djIwUFVCTElD"}}"#,
        ] {
            let error = windows_master_key(invalid).unwrap_err().to_string();
            assert!(!error.contains("PUBLIC"));
        }
    }
}
