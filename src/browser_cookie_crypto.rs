//! Chromium's existing cookie formats. This module never retrieves keys or logs data.
use aes::cipher::{BlockModeDecrypt, KeyIvInit, block_padding::Pkcs7};
use aes_gcm::{
    Aes256Gcm,
    aead::{Aead, KeyInit},
};
use sha2::{Digest, Sha256};
use std::fmt;
use zeroize::Zeroizing;

// Each platform constructs only its own Chromium key schemes; tests exercise all of them.
#[cfg_attr(not(test), allow(dead_code))]
pub(super) enum CookieKey {
    Cbc(Zeroizing<[u8; 16]>),
    Gcm(Zeroizing<[u8; 32]>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DecodeError {
    Unsupported,
    Invalid,
    HostMismatch,
}
impl fmt::Display for DecodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Unsupported => "Unsupported Chrome cookie encryption",
            Self::Invalid => "Chrome cookie decryption failed",
            Self::HostMismatch => "Chrome cookie host binding does not match",
        })
    }
}
impl std::error::Error for DecodeError {}

#[cfg_attr(not(test), allow(dead_code))]
impl CookieKey {
    pub(super) fn linux(password: &[u8]) -> Self {
        Self::cbc(password, 1)
    }
    pub(super) fn macos(password: &[u8]) -> Self {
        Self::cbc(password, 1003)
    }
    fn cbc(password: &[u8], rounds: u32) -> Self {
        Self::Cbc(Zeroizing::new(pbkdf2::pbkdf2_hmac_array::<sha1::Sha1, 16>(
            password,
            b"saltysalt",
            rounds,
        )))
    }
    pub(super) fn windows(master_key: Zeroizing<[u8; 32]>) -> Self {
        Self::Gcm(master_key)
    }
    pub(super) fn decrypt(
        &self,
        encrypted: &[u8],
        host: &str,
        database_version: u32,
    ) -> Result<Zeroizing<String>, DecodeError> {
        // App-Bound v20 is deliberately unavailable. Never try another scheme/key
        // or invoke browser injection, elevation or another account on failure.
        if !encrypted.starts_with(b"v10") && !encrypted.starts_with(b"v11") {
            return Err(DecodeError::Unsupported);
        }
        if encrypted.len() > crate::browser_auth::MAX_COOKIE_INPUT {
            return Err(DecodeError::Invalid);
        }
        let payload = &encrypted[3..];
        let plaintext = match self {
            Self::Cbc(key) => {
                let mut buffer = Zeroizing::new(payload.to_vec());
                let plaintext =
                    cbc::Decryptor::<aes::Aes128>::new((&**key).into(), (&[b' '; 16]).into())
                        .decrypt_padded::<Pkcs7>(&mut buffer)
                        .map_err(|_| DecodeError::Invalid)?;
                Zeroizing::new(plaintext.to_vec())
            }
            Self::Gcm(key) => {
                if payload.len() < 12 + 16 {
                    return Err(DecodeError::Invalid);
                }
                let cipher = Aes256Gcm::new((&**key).into());
                let nonce: [u8; 12] = payload[..12].try_into().map_err(|_| DecodeError::Invalid)?;
                Zeroizing::new(
                    cipher
                        .decrypt(&nonce.into(), &payload[12..])
                        .map_err(|_| DecodeError::Invalid)?,
                )
            }
        };
        let value = if database_version >= 24 {
            let digest = Sha256::digest(host.as_bytes());
            if plaintext.len() < digest.len() || plaintext[..digest.len()] != digest[..] {
                return Err(DecodeError::HostMismatch);
            }
            &plaintext[digest.len()..]
        } else {
            &plaintext[..]
        };
        std::str::from_utf8(value)
            .map(|value| Zeroizing::new(value.to_owned()))
            .map_err(|_| DecodeError::Invalid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn bytes(hex: &str) -> Vec<u8> {
        hex.as_bytes()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect()
    }
    // Independent node:crypto vectors, using only public keys and plaintext.
    const LINUX_V10: &str = "763130b6bfa63c66774b16309c3b492b56bc030f5dbcf34e106dd42f933295edd1dec4e164c358c7f2b717359ba1a626ceb27fc4797ce337e7360a34da93a063f1b9ca";
    const LINUX_V11: &str = "76313135d9d83973964c5883fbfc5969ec5393b5e3a276d1b4b4b721232e9cc96f01e3af4fc6923a7f5f8b62236dd1faf572dbd6502682d3c9d52290407faaac307eee";
    const MAC_V10: &str = "7631300693eda844bc6b6299812bffe260b8c106a890c3c788514de9208d9954ea5c21";
    const WINDOWS_V10: &str = "7631302424242424242424242424241792d43a8fc0d3add833278b0c41f5376f5ea3889d1dae96fa7c9c32a95aec82525e13c93407877fba29dbde3783d4f3d4cc623e1bd975878e24f7a1a1d2569143d0fc";

    #[test]
    fn independently_generated_legacy_cookie_vectors_decode_with_host_binding() {
        for (key, vector, version) in [
            (CookieKey::linux(b"peanuts"), LINUX_V10, 24),
            (CookieKey::linux(b"PUBLIC-password"), LINUX_V11, 24),
            (CookieKey::macos(b"PUBLIC-password"), MAC_V10, 23),
            (
                CookieKey::windows(Zeroizing::new([0x42; 32])),
                WINDOWS_V10,
                24,
            ),
        ] {
            let encrypted = bytes(vector);
            assert_eq!(
                &*key.decrypt(&encrypted, ".chatgpt.com", version).unwrap(),
                "PUBLIC-cookie-value"
            );
            if version >= 24 {
                assert_eq!(
                    key.decrypt(&encrypted, ".chat.openai.com", version),
                    Err(DecodeError::HostMismatch)
                );
            }
        }
    }
    #[test]
    fn malformed_unsupported_and_unauthenticated_records_fail_without_fallback() {
        for key in [
            CookieKey::linux(b"peanuts"),
            CookieKey::windows(Zeroizing::new([0x42; 32])),
        ] {
            for value in [b"v20PUBLIC".as_slice(), b"v99PUBLIC", b"PUBLIC", b""] {
                let error = key.decrypt(value, ".chatgpt.com", 24).unwrap_err();
                assert_eq!(error, DecodeError::Unsupported);
                assert!(!format!("{error:?}: {error}").contains("PUBLIC"));
            }
            assert_eq!(
                key.decrypt(b"v10", ".chatgpt.com", 24),
                Err(DecodeError::Invalid)
            );
        }
        let key = CookieKey::windows(Zeroizing::new([0x42; 32]));
        let mut encrypted = bytes(WINDOWS_V10);
        *encrypted.last_mut().unwrap() ^= 1;
        assert_eq!(
            key.decrypt(&encrypted, ".chatgpt.com", 24),
            Err(DecodeError::Invalid)
        );
        let mut encrypted = bytes(LINUX_V10);
        encrypted.pop();
        assert_eq!(
            CookieKey::linux(b"peanuts").decrypt(&encrypted, ".chatgpt.com", 24),
            Err(DecodeError::Invalid)
        );
    }
}
