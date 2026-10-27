//! Verified streamed bridge downloads and no-replace publication.
use crate::{
    artifacts::ArtifactStage,
    browser_output,
    remote_host::{MAX_ARTIFACT_BYTES, artifact_mime},
};
use anyhow::{Context, Result, ensure};
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};
use tokio_util::sync::CancellationToken;

pub(crate) fn descriptor(value: &Value) -> Result<()> {
    fn identity(value: &Value) -> bool {
        value.as_str().is_some_and(|value| {
            !value.is_empty()
                && value.len() <= 128
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        })
    }
    ensure!(
        value.is_object()
            && matches!(value["kind"].as_str(), Some("file" | "image"))
            && identity(&value["runId"])
            && identity(&value["artifactId"])
            && value["filename"].is_string()
            && value["byteSize"]
                .as_f64()
                .is_some_and(|size| size.is_finite()
                    && size.fract() == 0.0
                    && size > 0.0
                    && size <= MAX_ARTIFACT_BYTES as f64)
            && value["sha256"].as_str().is_some_and(|hash| hash.len() == 64
                && hash
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))),
        "invalid bridge artifact descriptor"
    );
    Ok(())
}
pub(crate) fn validation(file: &mut File, filename: &str, mime: Option<&str>) -> Result<Value> {
    let size = file.metadata()?.len();
    validation_of(file, size, filename, mime)
}
/// Reference validateArtifactBuffer for a local download: a failed check is recorded, not fatal.
pub(crate) fn local_validation(bytes: &[u8], filename: &str, mime: Option<&str>) -> Value {
    let reader = &mut std::io::Cursor::new(bytes);
    validation_of(reader, bytes.len() as u64, filename, mime).unwrap_or_else(|error| {
        let message = error.to_string();
        let (kind, code) = message
            .split_once(" validation failed: ")
            .unwrap_or(("generic", message.as_str()));
        json!({"type":kind,"ok":false,"error":code})
    })
}
fn validation_of(
    file: &mut (impl Read + Seek),
    size: u64,
    filename: &str,
    mime: Option<&str>,
) -> Result<Value> {
    ensure!(size > 0, "generic validation failed: empty-file");
    let zip = filename.to_ascii_lowercase().ends_with(".zip")
        || mime.is_some_and(|mime| {
            matches!(mime, "application/zip" | "application/x-zip-compressed")
                || mime.ends_with("+zip")
        });
    if !zip {
        return Ok(json!({"type":"generic","ok":true}));
    }
    ensure!(size >= 22, "zip validation failed: zip-too-small");
    let mut magic = [0; 4];
    file.read_exact(&mut magic)?;
    ensure!(
        matches!(u32::from_le_bytes(magic), 0x04034b50 | 0x06054b50),
        "zip validation failed: zip-magic-mismatch"
    );
    let window = size.min(22 + 65535) as usize;
    file.seek(SeekFrom::Start(size - window as u64))?;
    let mut tail = vec![0; window];
    file.read_exact(&mut tail)?;
    let offset = (0..=window - 22)
        .rev()
        .find(|offset| tail[*offset..*offset + 4] == [0x50, 0x4b, 0x05, 0x06])
        .context("zip validation failed: zip-central-directory-missing")?;
    let short = |at| {
        u16::from_le_bytes(
            tail[at..at + 2]
                .try_into()
                .expect("ZIP field is in the tail"),
        )
    };
    let long = |at| {
        u32::from_le_bytes(
            tail[at..at + 4]
                .try_into()
                .expect("ZIP field is in the tail"),
        )
    };
    ensure!(
        offset + 22 + short(offset + 20) as usize == window,
        "zip validation failed: zip-eocd-size-mismatch"
    );
    ensure!(
        u64::from(long(offset + 12)) + u64::from(long(offset + 16))
            <= size - window as u64 + offset as u64,
        "zip validation failed: zip-central-directory-out-of-range"
    );
    Ok(json!({"type":"zip","ok":true}))
}
pub(crate) struct Transferred {
    pub artifact: Value,
    pub warning: Option<Value>,
}
pub(crate) async fn transfer(
    client: &reqwest::Client,
    endpoint: &reqwest::Url,
    token: Option<&str>,
    descriptor_value: &Value,
    destination: &Path,
    unique: bool,
    cancel: &CancellationToken,
) -> Result<Transferred> {
    descriptor(descriptor_value)?;
    let expected_size = descriptor_value["byteSize"]
        .as_f64()
        .context("Missing artifact size")? as u64;
    let expected_hash = descriptor_value["sha256"]
        .as_str()
        .context("Missing artifact digest")?;
    let run = descriptor_value["runId"]
        .as_str()
        .context("Missing artifact run")?;
    let id = descriptor_value["artifactId"]
        .as_str()
        .context("Missing artifact identity")?;
    let url = endpoint.join(&format!("runs/{run}/artifacts/{id}"))?;
    let mut request = client.get(url);
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    let response = tokio::select! { biased; _=cancel.cancelled()=>anyhow::bail!("Browser run cancelled"),response=request.send()=>response? };
    ensure!(
        response.status() == reqwest::StatusCode::OK,
        "Remote artifact responded with status {}",
        response.status()
    );
    if let Some(header) = response.headers().get("x-oracle-artifact-sha256") {
        ensure!(
            header.as_bytes() == expected_hash.as_bytes(),
            "artifact sha256 header mismatch"
        );
    }
    if let Some(header) = response.headers().get("content-length") {
        let size = header.to_str()?.parse::<u64>()?;
        ensure!(
            size == expected_size && size > 0 && size <= MAX_ARTIFACT_BYTES,
            "artifact content-length mismatch"
        );
    }
    let mut stage = ArtifactStage::create(destination)?;
    let outcome = async {
        let mut stream = response.bytes_stream();
        let mut received = 0u64;
        loop {
            let next = tokio::select! { biased; _=cancel.cancelled()=>anyhow::bail!("Browser run cancelled"),next=stream.next()=>next };
            let Some(bytes) = next else { break; };
            let bytes = bytes?;
            received = received.checked_add(bytes.len() as u64).context("Artifact size overflow")?;
            ensure!(received<=expected_size && received<=MAX_ARTIFACT_BYTES,"artifact exceeded declared size");
            stage.write(&bytes)?;
        }
        ensure!(received==expected_size,"artifact size mismatch");
        let mime = artifact_mime(descriptor_value["mimeType"].as_str());
        let name = browser_output::filename(descriptor_value["filename"].as_str().unwrap_or(""));
        let verified = stage.inspect(|file|validation(file,&name,mime.as_deref()))?;
        Ok::<_,anyhow::Error>((mime,verified))
    }.await;
    let (mime, validation) = match outcome {
        Ok(value) => value,
        Err(error) => return Err(stage.abort(error)),
    };
    let published = stage.publish(destination, unique, expected_size, expected_hash, cancel)?;
    let filename = published
        .path
        .file_name()
        .context("Published file has no filename")?
        .to_string_lossy();
    let mut artifact = json!({"kind":descriptor_value["kind"],"path":published.path,"label":filename,
        "sizeBytes":published.size_bytes,"sourceUrl":"bridge-artifact","sha256":published.sha256,
        "nativeIdentity":published.native_identity,"validation":validation,"transfer":{"status":"completed","bytes":published.size_bytes},
        "origin":{"mode":"bridge"},"url":"bridge-artifact","finalUrl":"bridge-artifact"});
    if let Some(mime) = mime {
        artifact["mimeType"] = json!(mime);
    }
    if descriptor_value["kind"] == "image" {
        for (field, value) in crate::remote_protocol::image_metadata(&descriptor_value["image"])
            .as_object()
            .context("Image metadata must be an object")?
        {
            artifact[field] = value.clone();
        }
    } else {
        artifact["filename"] = json!(filename);
    }
    let warning = published.cleanup_error.map(|_| {
        crate::remote_protocol::cleanup_warning(
            descriptor_value["kind"].as_str().unwrap_or("file"),
            "receiver",
            &published.path,
        )
    });
    Ok(Transferred { artifact, warning })
}
pub(crate) fn destination(
    directory: &Path,
    descriptor: &Value,
    preferred: Option<&Path>,
    index: usize,
) -> Result<PathBuf> {
    let name = browser_output::filename(descriptor["filename"].as_str().unwrap_or(""));
    let cwd = std::env::current_dir()?;
    if let Some(preferred) = preferred {
        let preferred = std::path::absolute(preferred)?;
        let preferred = crate::prompts::absolute(
            &cwd,
            preferred
                .to_str()
                .context("Image destination must be a path string")?,
        );
        let extension = Path::new(&name)
            .extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or("png");
        crate::browser_images::sibling_path(&preferred, index, extension)
    } else {
        let directory = std::path::absolute(directory)?;
        Ok(crate::prompts::absolute(
            &cwd,
            directory
                .to_str()
                .context("Artifact directory must be a path string")?,
        )
        .join(name))
    }
}
