//! Image destinations and delivery receipts from the maintained ChatGPT image contract.
use crate::{artifacts, browser_download, policy};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};
use tokio_util::sync::CancellationToken;

pub struct ImageDelivery {
    pub generated: Vec<Value>,
    pub requested: bool,
    base: PathBuf,
    saved: Vec<Value>,
    button_downloads: bool,
    output_offset: usize,
}
pub fn sibling_path(base: &Path, index: usize, detected_extension: &str) -> Result<PathBuf> {
    let name = base
        .file_name()
        .context("Image destination has no filename")?;
    let existing = base.extension();
    if index == 0 && existing.is_some() {
        return Ok(base.into());
    }
    let stem = if existing.is_some() {
        base.file_stem().context("Image destination has no stem")?
    } else {
        name
    };
    let extension = existing
        .map(|e| e.to_string_lossy().into_owned())
        .unwrap_or_else(|| detected_extension.into());
    let filename = if index == 0 {
        format!("{}.{extension}", stem.to_string_lossy())
    } else {
        format!(
            "{}.{}.{extension}",
            stem.to_string_lossy(),
            index.checked_add(1).context("Image index overflow")?
        )
    };
    Ok(base.with_file_name(filename))
}
fn generated_stem(value: &str) -> String {
    let mut name = String::new();
    for character in value.chars() {
        if character.is_ascii_alphanumeric() || character == '_' {
            name.push(character);
        } else if !name.ends_with('-') {
            name.push('-');
        }
    }
    let name = name.trim_matches('-');
    if name.is_empty() {
        "generated".into()
    } else {
        name.chars().take(48).collect()
    }
}
pub fn requested_destination(options: &Value) -> Result<Option<&str>> {
    let value = options
        .get("generateImage")
        .filter(|value| !value.is_null())
        .or_else(|| options.get("outputPath"));
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok((!value.is_empty()).then_some(value.as_str())),
        _ => anyhow::bail!("Browser image destination must be a path string"),
    }
}
pub fn normalise_inventory(images: &[Value]) -> Result<Vec<Value>> {
    let mut generated: Vec<Value> = vec![];
    let mut indices = BTreeMap::new();
    let area = |image: &Value| {
        image["width"].as_f64().unwrap_or(0.0) * image["height"].as_f64().unwrap_or(0.0)
    };
    for image in images {
        let mut image = image.clone();
        let url = browser_download::download_url(
            image["url"]
                .as_str()
                .context("Generated image has no URL")?,
        )?;
        let file_id = url
            .query_pairs()
            .find(|(key, _)| key == "id")
            .map(|(_, id)| id.into_owned())
            .context("Generated image URL has no file identity")?;
        ensure!(
            url.path() == "/backend-api/estuary/content" && file_id.starts_with("file_"),
            "Generated image has no permitted provider identity"
        );
        image["url"] = json!(url.as_str());
        image["fileId"] = json!(file_id);
        if let Some(&index) = indices.get(&file_id) {
            if area(&image) >= area(&generated[index]) {
                generated[index] = image;
            }
        } else {
            indices.insert(file_id, generated.len());
            generated.push(image);
        }
    }
    Ok(generated)
}
impl ImageDelivery {
    pub fn new(options: &Value, cwd: &Path, directory: &Path, images: &[Value]) -> Result<Self> {
        let target = requested_destination(options)?;
        let generated = normalise_inventory(images)?;
        let base = if let Some(target) = target {
            let target = PathBuf::from(target);
            if target.is_absolute() {
                target
            } else {
                cwd.join(target)
            }
        } else {
            let primary = generated.first();
            let source = primary
                .and_then(|image| {
                    image["fileId"]
                        .as_str()
                        .filter(|s| !s.is_empty())
                        .or_else(|| image["alt"].as_str().filter(|s| !s.is_empty()))
                        .or_else(|| image["url"].as_str().filter(|s| !s.is_empty()))
                })
                .unwrap_or("generated");
            directory.join(format!("{}.png", generated_stem(source)))
        };
        Ok(Self {
            generated,
            requested: target.is_some(),
            base,
            saved: vec![],
            button_downloads: false,
            output_offset: 0,
        })
    }
    pub(crate) fn after_previous(
        options: &Value,
        cwd: &Path,
        directory: &Path,
        images: &[Value],
        previous_count: usize,
    ) -> Result<Self> {
        let mut delivery = Self::new(options, cwd, directory, images)?;
        if delivery.requested {
            delivery.output_offset = previous_count;
        }
        Ok(delivery)
    }
    pub fn publish(
        &mut self,
        bytes: &[u8],
        candidate: &Value,
        cancel: &CancellationToken,
    ) -> Result<Value> {
        ensure!(
            candidate.is_object(),
            "Generated image receipt is not an object"
        );
        let (extension, mime) = browser_download::image_type(bytes)
            .context("Required generated image has no verified image signature")?;
        let destination = sibling_path(
            &self.base,
            self.output_offset
                .checked_add(self.saved.len())
                .context("Image index overflow")?,
            extension,
        )?;
        let publication = artifacts::publish(bytes, &destination, !self.requested, cancel)?;
        if self.saved.is_empty() && !self.requested {
            self.base = publication.path.clone();
        }
        let mut manifest = self
            .generated
            .iter()
            .find(|image| image["url"] == candidate["url"])
            .cloned()
            .unwrap_or_else(|| candidate.clone());
        for (key, value) in candidate.as_object().unwrap() {
            manifest[key] = value.clone();
        }
        manifest["kind"] = json!("image");
        manifest["path"] = json!(publication.path);
        manifest["sizeBytes"] = json!(publication.size_bytes);
        manifest["sha256"] = json!(publication.sha256);
        manifest["mimeType"] = json!(mime);
        manifest["sourceUrl"] = candidate["url"].clone();
        manifest["finalUrl"] = candidate["url"].clone();
        manifest["validation"] = json!({"type":"image-signature","ok":true});
        manifest["publicationReceipt"] = serde_json::to_value(&publication)?;
        manifest["transfer"] = json!({"status":"not-needed"});
        if let Some(warning) = publication.cleanup_error {
            manifest["cleanupWarning"] = json!(warning);
        }
        self.saved.push(manifest.clone());
        Ok(manifest)
    }
    pub fn require_complete(&self, warnings: &[Value]) -> Result<()> {
        if !self.requested {
            return Ok(());
        }
        let mut distinct = BTreeSet::new();
        for image in &self.saved {
            if let Some(path) = image["path"]
                .as_str()
                .filter(|s| !policy::trim(s).is_empty())
            {
                distinct.insert(if cfg!(windows) {
                    path.to_lowercase()
                } else {
                    path.into()
                });
            }
        }
        let failed = warnings.iter().any(|warning| {
            warning["code"] == "remote-image-registration-failed"
                || (warning["code"] == "remote-artifact-transfer-failed"
                    && warning["details"]["artifactKind"] != "file")
        });
        ensure!(
            !distinct.is_empty() && distinct.len() >= self.generated.len() && !failed,
            "Required browser image delivery is incomplete; preserve the conversation and captured images for recovery."
        );
        Ok(())
    }
    pub fn record_button_image(&mut self, candidate: &Value) {
        self.button_downloads = true;
        self.generated
            .push(json!({"url":candidate["url"],"alt":candidate["label"]}));
    }
    pub fn has_saved_images(&self) -> bool {
        !self.saved.is_empty()
    }
    pub fn saved_images(&self) -> &[Value] {
        &self.saved
    }
    pub fn restore_saved(
        &mut self,
        receipts: &[Value],
        expected_origin: &Value,
        buttons: bool,
    ) -> Result<()> {
        for receipt in receipts.iter().filter(|receipt| receipt["kind"] == "image") {
            let origin = &receipt["origin"];
            ensure!(
                origin["mode"] == "local"
                    && origin["targetId"] == expected_origin["targetId"]
                    && origin["assistantMessageId"] == expected_origin["assistantMessageId"]
                    && origin["assistantTurnId"] == expected_origin["assistantTurnId"],
                "Saved image belongs to a different owned response"
            );
            if buttons {
                ensure!(
                    receipt["imageButton"] == true,
                    "Saved image is not an owned button delivery"
                );
            } else {
                ensure!(
                    self.generated
                        .get(self.saved.len())
                        .is_some_and(|image| image["fileId"] == receipt["fileId"]),
                    "Saved image inventory changed before recovery"
                );
            }
            let publication: artifacts::PublishedArtifact =
                serde_json::from_value(receipt["publicationReceipt"].clone())
                    .context("Saved image has no native publication receipt")?;
            ensure!(
                receipt["path"] == json!(publication.path)
                    && receipt["sha256"] == publication.sha256
                    && receipt["sizeBytes"] == publication.size_bytes,
                "Saved image manifest differs from its publication receipt"
            );
            let parent = publication
                .path
                .parent()
                .context("Saved image has no parent directory")?;
            let directory = crate::native_fs::NativeEntry::open(parent, true)?;
            let mut file = crate::native_fs::NativeEntry::open(&publication.path, false)?;
            ensure!(
                file.identity == publication.native_identity
                    && file.handle.metadata()?.len() == publication.size_bytes
                    && publication.size_bytes <= 128 * 1024 * 1024,
                "Saved image native identity or size changed"
            );
            let stamp = crate::native_fs::read_stamp(&file.handle)?;
            let mut header = Vec::with_capacity(1024);
            file.handle.by_ref().take(1024).read_to_end(&mut header)?;
            let (extension, mime) =
                browser_download::image_type(&header).context("Saved image signature changed")?;
            file.handle.seek(SeekFrom::Start(0))?;
            artifacts::verify_streamed_file(
                &mut file.handle,
                publication.size_bytes,
                &publication.sha256,
                None,
            )?;
            ensure!(
                crate::native_fs::read_stamp(&file.handle)? == stamp && receipt["mimeType"] == mime,
                "Saved image bytes changed"
            );
            if self.saved.is_empty() && !self.requested {
                ensure!(
                    publication.path.parent() == self.base.parent(),
                    "Automatic image destination left its owned artifact directory"
                );
                self.base = publication.path.clone();
            }
            ensure!(
                if self.requested {
                    publication.path
                        == sibling_path(
                            &self.base,
                            self.output_offset
                                .checked_add(self.saved.len())
                                .context("Image index overflow")?,
                            extension,
                        )?
                } else {
                    publication.path.parent() == self.base.parent()
                },
                "Saved image destination differs from the original output order"
            );
            file.require_matches(&publication.path)?;
            directory.require_matches(parent)?;
            self.saved.push(receipt.clone());
            if buttons {
                self.record_button_image(receipt);
            }
        }
        Ok(())
    }
    pub fn summary(&self, answer_text: &str) -> Value {
        Self::summary_values(
            &self.generated,
            &self.saved,
            self.button_downloads,
            answer_text,
        )
    }
    pub(crate) fn combined_summary(deliveries: &[Self], answer_text: &str) -> Value {
        let generated = deliveries
            .iter()
            .flat_map(|d| d.generated.iter().cloned())
            .collect::<Vec<_>>();
        let saved = deliveries
            .iter()
            .flat_map(|d| d.saved.iter().cloned())
            .collect::<Vec<_>>();
        Self::summary_values(
            &generated,
            &saved,
            deliveries.iter().any(|d| d.button_downloads),
            answer_text,
        )
    }
    fn summary_values(
        generated: &[Value],
        saved: &[Value],
        button_downloads: bool,
        answer_text: &str,
    ) -> Value {
        let suffix = if let Some(first) = saved.first() {
            let path = first["path"].as_str().unwrap();
            if saved.len() == 1 {
                format!(
                    "\n\n*Generated {} image(s). Saved to: {path}*",
                    generated.len()
                )
            } else {
                format!(
                    "\n\n*Generated {} image(s). Saved {} file(s) starting at: {path}*",
                    generated.len(),
                    saved.len()
                )
            }
        } else {
            String::new()
        };
        json!({"generatedImages":generated,"savedImages":saved,"imageCount":generated.len(),"markdownSuffix":suffix,"buttonDownloads":button_downloads,"answerText":if button_downloads {answer_text.into()} else {crate::browser::normalise_image_answer(answer_text,generated.len())}})
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "Isolated peak RSS qualification; run exactly, without concurrent tests"]
    fn saved_image_recovery_has_bounded_peak_rss() {
        use sha2::{Digest, Sha256};
        fn peak_kib() -> u64 {
            std::fs::read_to_string("/proc/self/status")
                .unwrap()
                .lines()
                .find_map(|line| {
                    line.strip_prefix("VmHWM:")
                        .map(|rest| rest.split_whitespace().next().unwrap().parse().unwrap())
                })
                .unwrap()
        }
        let root = tempfile::tempdir().unwrap();
        let destination = root.path().join("PUBLIC.png");
        let mut stage = artifacts::ArtifactStage::create(&destination).unwrap();
        let mut block = [0; 64 * 1024];
        block[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
        let mut digest = Sha256::new();
        for _ in 0..1024 {
            stage.write(&block).unwrap();
            digest.update(block);
        }
        let size = 64 * 1024 * 1024;
        let hash: String = digest
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let publication = stage
            .publish(&destination, false, size, &hash, &CancellationToken::new())
            .unwrap();
        let images =
            [json!({"url":"https://chatgpt.com/backend-api/estuary/content?id=file_public"})];
        let options = json!({"generateImage":"PUBLIC.png"});
        let origin = json!({"mode":"local","targetId":"TARGET-PUBLIC","assistantMessageId":"assistant-public","assistantTurnId":"turn-public"});
        let receipt = json!({"kind":"image","fileId":"file_public","path":publication.path,"sha256":hash,
            "sizeBytes":size,"mimeType":"image/png","publicationReceipt":publication,"origin":origin});
        let before = peak_kib();
        for _ in 0..3 {
            let mut delivery =
                ImageDelivery::new(&options, root.path(), root.path(), &images).unwrap();
            delivery
                .restore_saved(std::slice::from_ref(&receipt), &origin, false)
                .unwrap();
            delivery.require_complete(&[]).unwrap();
        }
        let streamed = peak_kib();
        let buffered = std::fs::read(&destination).unwrap();
        std::hint::black_box(&buffered);
        let control = peak_kib();
        println!(
            "PUBLIC image recovery memory: {}",
            json!({"fileBytes":size,"beforeKiB":before,"streamedPeakKiB":streamed,"streamedDeltaKiB":streamed.saturating_sub(before),"wholeFileControlPeakKiB":control,"wholeFileAdditionalDeltaKiB":control.saturating_sub(streamed)})
        );
        assert_eq!(buffered.len() as u64, size);
        assert!(
            streamed.saturating_sub(before) < 16 * 1024,
            "Saved image verification buffered the image"
        );
        assert!(
            control.saturating_sub(streamed) > 48 * 1024,
            "Whole-file control did not establish the measurement sensitivity"
        );
    }
    #[test]
    fn sequential_destinations_restore_only_the_original_global_output_position() {
        let root = tempfile::tempdir().unwrap();
        let images =
            [json!({"url":"https://chatgpt.com/backend-api/estuary/content?id=file_public"})];
        let options = json!({"generateImage":"PUBLIC.png"});
        let origin = json!({"mode":"local","targetId":"TARGET-PUBLIC","assistantMessageId":"assistant-public-2","assistantTurnId":"turn-public-2"});
        let mut candidate = images[0].clone();
        candidate["origin"] = origin.clone();
        let mut delivery =
            ImageDelivery::after_previous(&options, root.path(), root.path(), &images, 1).unwrap();
        let receipt = delivery
            .publish(
                b"\x89PNG\r\n\x1a\nPUBLIC second image",
                &candidate,
                &CancellationToken::new(),
            )
            .unwrap();
        assert_eq!(receipt["path"], json!(root.path().join("PUBLIC.2.png")));
        ImageDelivery::after_previous(&options, root.path(), root.path(), &images, 1)
            .unwrap()
            .restore_saved(std::slice::from_ref(&receipt), &origin, false)
            .unwrap();
        assert!(
            ImageDelivery::new(&options, root.path(), root.path(), &images)
                .unwrap()
                .restore_saved(std::slice::from_ref(&receipt), &origin, false)
                .is_err()
        );
    }
    #[test]
    fn recovery_reuses_only_verified_native_publications_from_the_same_response() {
        let root = tempfile::tempdir().unwrap();
        let images = vec![
            json!({"url":"https://chatgpt.com/backend-api/estuary/content?id=file_public"}),
            json!({"url":"https://chatgpt.com/backend-api/estuary/content?id=file_public_2"}),
        ];
        let options = json!({"generateImage":"PUBLIC.png"});
        let origin = json!({"mode":"local","targetId":"TARGET-PUBLIC","assistantMessageId":"assistant-public","assistantTurnId":"turn-public"});
        let cancel = CancellationToken::new();
        let bytes = b"\x89PNG\r\n\x1a\nPUBLIC original image";
        let mut first = ImageDelivery::new(&options, root.path(), root.path(), &images).unwrap();
        let mut candidate = images[0].clone();
        candidate["origin"] = origin.clone();
        let receipt = first.publish(bytes, &candidate, &cancel).unwrap();
        let mut resumed = ImageDelivery::new(&options, root.path(), root.path(), &images).unwrap();
        resumed
            .restore_saved(std::slice::from_ref(&receipt), &origin, false)
            .unwrap();
        candidate = images[1].clone();
        candidate["origin"] = origin.clone();
        resumed.publish(bytes, &candidate, &cancel).unwrap();
        resumed.require_complete(&[]).unwrap();
        assert_eq!(
            std::fs::read(root.path().join("PUBLIC.png")).unwrap(),
            bytes
        );
        assert!(root.path().join("PUBLIC.2.png").exists());
        for (index, mut invalid) in [receipt.clone(), receipt.clone()].into_iter().enumerate() {
            if index == 0 {
                invalid["origin"]["assistantMessageId"] = json!("foreign-assistant");
            } else {
                invalid["sha256"] = json!("PUBLIC corrupt digest");
            }
            assert!(
                ImageDelivery::new(&options, root.path(), root.path(), &images)
                    .unwrap()
                    .restore_saved(&[invalid], &origin, false)
                    .is_err()
            );
        }
        std::fs::rename(
            root.path().join("PUBLIC.png"),
            root.path().join("PUBLIC-original.png"),
        )
        .unwrap();
        std::fs::write(root.path().join("PUBLIC.png"), bytes).unwrap();
        assert!(
            ImageDelivery::new(&options, root.path(), root.path(), &images)
                .unwrap()
                .restore_saved(&[receipt], &origin, false)
                .is_err()
        );
    }
    #[test]
    fn sibling_destinations_preserve_supplied_extensions_and_detect_missing_ones() {
        for (base, index, extension, expected) in [
            ("picture.png", 0, "jpg", "picture.png"),
            ("picture.png", 1, "jpg", "picture.2.png"),
            ("picture", 0, "png", "picture.png"),
            ("picture", 1, "jpg", "picture.2.jpg"),
            (".picture", 1, "png", ".picture.2.png"),
            ("picture.", 1, "png", "picture.2."),
        ] {
            assert_eq!(
                sibling_path(Path::new(base), index, extension).unwrap(),
                Path::new(expected)
            );
        }
    }
    #[test]
    fn explicit_destinations_never_replace_and_multiple_images_have_distinct_receipts() {
        let root = tempfile::tempdir().unwrap();
        let images = vec![
            json!({"url":"https://chatgpt.com/backend-api/estuary/content?id=file_public"}),
            json!({"url":"https://chatgpt.com/backend-api/estuary/content?id=file_public_2"}),
        ];
        let options = json!({"generateImage":"PUBLIC.png","outputPath":"ignored.png"});
        let mut delivery = ImageDelivery::new(&options, root.path(), root.path(), &images).unwrap();
        let cancel = CancellationToken::new();
        let bytes = b"\x89PNG\r\n\x1a\nPUBLIC image bytes";
        delivery.publish(bytes, &images[0], &cancel).unwrap();
        delivery.publish(bytes, &images[1], &cancel).unwrap();
        delivery.require_complete(&[]).unwrap();
        assert!(
            root.path().join("PUBLIC.png").exists()
                && root.path().join("PUBLIC.2.png").exists()
                && !root.path().join("ignored.png").exists()
        );
        let mut collision =
            ImageDelivery::new(&options, root.path(), root.path(), &images).unwrap();
        assert!(
            collision
                .publish(b"\xff\xd8\xffPUBLIC different bytes", &images[0], &cancel)
                .is_err()
        );
        assert_eq!(
            std::fs::read(root.path().join("PUBLIC.png")).unwrap(),
            bytes
        );
        assert!(delivery.require_complete(&[json!({"code":"remote-artifact-transfer-failed","details":{"artifactKind":"image"}})]).is_err());
        delivery
            .require_complete(&[
                json!({"code":"remote-artifact-transfer-failed","details":{"artifactKind":"file"}}),
            ])
            .unwrap();
    }
    #[test]
    fn automatic_images_share_the_first_unique_base_and_missing_delivery_cannot_complete() {
        let root = tempfile::tempdir().unwrap();
        let image = json!({"url":"https://chatgpt.com/backend-api/estuary/content?id=file_public"});
        let bytes = b"\x89PNG\r\n\x1a\nPUBLIC image";
        let cancel = CancellationToken::new();
        std::fs::write(root.path().join("file_public.png"), b"PUBLIC existing").unwrap();
        let mut delivery = ImageDelivery::new(
            &json!({}),
            root.path(),
            root.path(),
            &[image.clone(), image.clone()],
        )
        .unwrap();
        let first = delivery.publish(bytes, &image, &cancel).unwrap();
        let second = delivery.publish(bytes, &image, &cancel).unwrap();
        assert_ne!(first["path"], second["path"]);
        assert_eq!(
            sibling_path(Path::new(first["path"].as_str().unwrap()), 1, "png").unwrap(),
            Path::new(second["path"].as_str().unwrap())
        );
        let missing = ImageDelivery::new(
            &json!({"generateImage":"PUBLIC.png"}),
            root.path(),
            root.path(),
            &[],
        )
        .unwrap();
        assert!(missing.require_complete(&[]).is_err());
    }
}
