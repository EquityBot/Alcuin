//! Deterministic ZIP32 browser bundles, matching the maintained stored ZIP format.
use anyhow::{Result, ensure};
use std::collections::BTreeSet;

pub struct ZipEntry<'a> {
    pub path: &'a str,
    pub bytes: &'a [u8],
}

const fn crc_table() -> [u32; 256] {
    let mut table = [0; 256];
    let mut index = 0;
    while index < 256 {
        let mut value = index as u32;
        let mut bit = 0;
        while bit < 8 {
            value = if value & 1 != 0 {
                0xedb88320 ^ (value >> 1)
            } else {
                value >> 1
            };
            bit += 1;
        }
        table[index] = value;
        index += 1;
    }
    table
}
fn crc32(bytes: &[u8]) -> u32 {
    const TABLE: [u32; 256] = crc_table();
    let mut crc = u32::MAX;
    for &byte in bytes {
        crc = TABLE[((crc ^ u32::from(byte)) & 255) as usize] ^ (crc >> 8);
    }
    crc ^ u32::MAX
}
fn zip_path(path: &str, index: usize, seen: &mut BTreeSet<String>) -> String {
    let path = path.replace('\\', "/");
    let bytes = path.as_bytes();
    let path = if bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1..3] == *b":/" {
        &path[3..]
    } else {
        &path
    };
    let path = path
        .split('/')
        .filter(|segment| !matches!(*segment, "" | "." | ".."))
        .collect::<Vec<_>>()
        .join("/");
    let path = if path.is_empty() {
        format!("file-{}.txt", index + 1)
    } else {
        path
    };
    let extension = path.rfind('.').filter(|&index| index > 0);
    let (base, extension) = extension.map_or((path.as_str(), ""), |index| {
        (&path[..index], &path[index..])
    });
    let mut candidate = path.clone();
    let mut suffix = 2;
    while !seen.insert(candidate.clone()) {
        candidate = format!("{base}-{suffix}{extension}");
        suffix += 1;
    }
    candidate
}
fn u16_at(header: &mut [u8], offset: usize, value: u16) {
    header[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}
fn u32_at(header: &mut [u8], offset: usize, value: u32) {
    header[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}
fn zip32(value: usize, label: &str) -> Result<u32> {
    u32::try_from(value).map_err(|_| anyhow::anyhow!("{label} exceeds ZIP32 limits."))
}

pub fn stored_zip(entries: &[ZipEntry<'_>]) -> Result<Vec<u8>> {
    ensure!(
        entries.len() <= usize::from(u16::MAX),
        "Too many files for a ZIP32 browser bundle."
    );
    let mut seen = BTreeSet::new();
    let mut directory = vec![];
    let mut archive = vec![];
    for (index, entry) in entries.iter().enumerate() {
        let name = zip_path(entry.path, index, &mut seen).into_bytes();
        let name_len = u16::try_from(name.len())
            .map_err(|_| anyhow::anyhow!("ZIP file name exceeds ZIP16 limits."))?;
        let size = zip32(entry.bytes.len(), "ZIP entry size")?;
        let offset = zip32(archive.len(), "ZIP local header offset")?;
        let crc = crc32(entry.bytes);
        let mut local = [0u8; 30];
        u32_at(&mut local, 0, 0x04034b50);
        u16_at(&mut local, 4, 20);
        u16_at(&mut local, 6, 0x0800);
        u16_at(&mut local, 12, 0x0021);
        u32_at(&mut local, 14, crc);
        u32_at(&mut local, 18, size);
        u32_at(&mut local, 22, size);
        u16_at(&mut local, 26, name_len);
        archive.extend_from_slice(&local);
        archive.extend_from_slice(&name);
        archive.extend_from_slice(entry.bytes);
        zip32(archive.len(), "ZIP local data size")?;
        directory.push((name, size, offset, crc));
    }
    let central_offset = zip32(archive.len(), "ZIP central directory offset")?;
    for (name, size, offset, crc) in directory {
        let mut central = [0u8; 46];
        u32_at(&mut central, 0, 0x02014b50);
        u16_at(&mut central, 4, 20);
        u16_at(&mut central, 6, 20);
        u16_at(&mut central, 8, 0x0800);
        u16_at(&mut central, 14, 0x0021);
        u32_at(&mut central, 16, crc);
        u32_at(&mut central, 20, size);
        u32_at(&mut central, 24, size);
        u16_at(&mut central, 28, name.len() as u16);
        u32_at(&mut central, 42, offset);
        archive.extend_from_slice(&central);
        archive.extend_from_slice(&name);
        zip32(archive.len(), "ZIP central directory size")?;
    }
    let central_size = zip32(
        archive.len() - central_offset as usize,
        "ZIP central directory size",
    )?;
    let mut footer = [0u8; 22];
    u32_at(&mut footer, 0, 0x06054b50);
    u16_at(&mut footer, 8, entries.len() as u16);
    u16_at(&mut footer, 10, entries.len() as u16);
    u32_at(&mut footer, 12, central_size);
    u32_at(&mut footer, 16, central_offset);
    archive.extend_from_slice(&footer);
    Ok(archive)
}

#[cfg(test)]
mod tests {
    #[test]
    fn every_zip_byte_matches_the_pinned_reference() {
        use base64::{Engine as _, engine::general_purpose::STANDARD};
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../fixtures/browser-zip-reference.json")).unwrap();
        assert_eq!(
            fixture["sourceRevision"],
            "8b9eaf68e7612448fb3bf43eab0d63a42e73c325"
        );
        for case in fixture["cases"].as_array().unwrap() {
            let sources = case["entries"].as_array().unwrap();
            let contents: Vec<_> = sources
                .iter()
                .map(|entry| {
                    STANDARD
                        .decode(entry["contentBase64"].as_str().unwrap())
                        .unwrap()
                })
                .collect();
            let entries: Vec<_> = sources
                .iter()
                .zip(&contents)
                .map(|(entry, bytes)| super::ZipEntry {
                    path: entry["path"].as_str().unwrap(),
                    bytes,
                })
                .collect();
            assert_eq!(
                super::stored_zip(&entries).unwrap(),
                STANDARD
                    .decode(case["expectedBase64"].as_str().unwrap())
                    .unwrap(),
                "{}",
                case["name"]
            );
        }
    }
    use super::*;
    #[test]
    fn stored_zip_preserves_bytes_relative_paths_duplicate_names_and_utf8() {
        let bytes = stored_zip(&[
            ZipEntry {
                path: "C:\\PUBLIC\\..\\folder\\café.txt",
                bytes: "PUBLIC café 🦀".as_bytes(),
            },
            ZipEntry {
                path: "/PUBLIC/folder/café.txt",
                bytes: b"PUBLIC second",
            },
            ZipEntry {
                path: "../../",
                bytes: b"",
            },
        ])
        .unwrap();
        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(&bytes)).unwrap();
        assert_eq!(archive.len(), 3);
        for (index, name, expected) in [
            (0, "PUBLIC/folder/café.txt", "PUBLIC café 🦀"),
            (1, "PUBLIC/folder/café-2.txt", "PUBLIC second"),
            (2, "file-3.txt", ""),
        ] {
            let mut entry = archive.by_index(index).unwrap();
            assert_eq!(entry.name().unwrap(), name);
            assert_eq!(entry.compression(), zip::CompressionMethod::Stored);
            let mut actual = String::new();
            std::io::Read::read_to_string(&mut entry, &mut actual).unwrap();
            assert_eq!(actual, expected);
        }
        assert_eq!(&bytes[6..8], &0x0800u16.to_le_bytes());
        assert_eq!(&bytes[12..14], &0x0021u16.to_le_bytes());
    }
    #[test]
    fn zip_path_and_count_limits_refuse_before_file_data_is_serialised() {
        assert!(
            stored_zip(&[ZipEntry {
                path: &"x".repeat(65536),
                bytes: b"PUBLIC"
            }])
            .is_err()
        );
        assert_eq!(crc32(b"123456789"), 0xcbf43926);
        assert_eq!(stored_zip(&[]).unwrap().len(), 22);
    }
}
