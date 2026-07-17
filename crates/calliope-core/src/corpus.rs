//! Corpus manifest + on-demand fetcher. Vectors are never committed:
//! conformance suites carry per-suite licensing, so the manifest records
//! url + checksum + license and content downloads into a local cache.
//!
//! A vector verifies by `sha256` or `md5` (conformance suites, e.g. via the
//! Fluster importer, publish MD5). When `archive-member` is set the download
//! is a zip and that member is extracted to the cache path.

use std::io::Read;
use std::path::{Path, PathBuf};

use md5::Md5;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{Error, Result};

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct Manifest {
    #[serde(default)]
    pub vector: Vec<Vector>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct Vector {
    /// cache-relative id, e.g. "jvt/AUD_MW_E.264"
    pub id: String,
    pub url: String,
    /// at least one of sha256 / md5 is required (checked at load)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub md5: Option<String>,
    /// when set, `url` is a zip and this member is extracted to the cache path
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archive_member: Option<String>,
    /// expected decoded-output MD5 (Fluster `result`), reserved for a future
    /// engine-independent oracle; unused today
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decoded_md5: Option<String>,
    pub license: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub notes: String,
}

impl Manifest {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)?;
        let manifest: Manifest =
            toml::from_str(&text).map_err(|e| Error::Parse(format!("{}: {e}", path.display())))?;
        for v in &manifest.vector {
            v.validate()?;
        }
        Ok(manifest)
    }

    pub fn get(&self, id: &str) -> Result<&Vector> {
        self.vector
            .iter()
            .find(|v| v.id == id)
            .ok_or_else(|| Error::Corpus(format!("vector '{id}' not in manifest")))
    }
}

impl Vector {
    fn validate(&self) -> Result<()> {
        if self.id.starts_with('/') || self.id.split('/').any(|c| c == "..") {
            return Err(Error::Corpus(format!(
                "vector id escapes the cache: {}",
                self.id
            )));
        }
        if self.sha256.is_none() && self.md5.is_none() {
            return Err(Error::Corpus(format!(
                "vector '{}' has no sha256 / md5 checksum",
                self.id
            )));
        }
        Ok(())
    }

    fn verify_checksum(&self, bytes: &[u8]) -> Result<()> {
        if let Some(want) = &self.sha256 {
            let got = hex::encode(Sha256::digest(bytes));
            if !got.eq_ignore_ascii_case(want) {
                return Err(Error::Corpus(format!(
                    "{}: sha256 mismatch, want {want} got {got}",
                    self.id
                )));
            }
        }
        if let Some(want) = &self.md5 {
            let got = hex::encode(Md5::digest(bytes));
            if !got.eq_ignore_ascii_case(want) {
                return Err(Error::Corpus(format!(
                    "{}: md5 mismatch, want {want} got {got}",
                    self.id
                )));
            }
        }
        Ok(())
    }
}

/// $CALLIOPE_CACHE, else $XDG_CACHE_HOME/calliope, else ~/.cache/calliope
pub fn cache_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("CALLIOPE_CACHE") {
        return PathBuf::from(dir);
    }
    if let Ok(xdg) = std::env::var("XDG_CACHE_HOME") {
        return PathBuf::from(xdg).join("calliope");
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".cache/calliope")
}

/// resolve a vector to a local path, downloading + verifying if missing
pub async fn fetch(vector: &Vector, cache: &Path) -> Result<PathBuf> {
    let dest = cache.join(&vector.id);
    if dest.exists() {
        return Ok(dest);
    }
    let body = reqwest::get(&vector.url)
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| Error::Corpus(format!("{}: {e}", vector.id)))?
        .bytes()
        .await
        .map_err(|e| Error::Corpus(format!("{}: {e}", vector.id)))?;
    materialize(vector, &body, &dest)
}

/// Verify the downloaded `body` against the vector's checksum, extract the
/// archive member if any, and write the result to `dest` (write-then-rename so
/// an interrupted run never leaves a bad cache hit). Split out of [`fetch`] so
/// the verify / unzip path is testable without the network.
pub fn materialize(vector: &Vector, body: &[u8], dest: &Path) -> Result<PathBuf> {
    vector.verify_checksum(body)?;
    let payload = match &vector.archive_member {
        Some(member) => extract_zip_member(&vector.id, body, member)?,
        None => body.to_vec(),
    };
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = dest.with_extension("part");
    std::fs::write(&tmp, &payload)?;
    std::fs::rename(&tmp, dest)?;
    Ok(dest.to_path_buf())
}

fn extract_zip_member(id: &str, zip_bytes: &[u8], member: &str) -> Result<Vec<u8>> {
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(zip_bytes))
        .map_err(|e| Error::Corpus(format!("{id}: open zip: {e}")))?;
    let mut file = archive
        .by_name(member)
        .map_err(|e| Error::Corpus(format!("{id}: member '{member}' not in zip: {e}")))?;
    let mut out = Vec::with_capacity(file.size() as usize);
    file.read_to_end(&mut out)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn parses_manifest_and_rejects_escapes() {
        let good = r#"
            [[vector]]
            id = "jvt/AUD_MW_E.264"
            url = "https://example.org/AUD_MW_E.264"
            sha256 = "00"
            license = "ITU-T conformance"
        "#;
        let m: Manifest = toml::from_str(good).unwrap();
        assert_eq!(m.vector.len(), 1);
        m.vector[0].validate().unwrap();
        assert!(m.get("jvt/AUD_MW_E.264").is_ok());
        assert!(m.get("missing").is_err());

        let dir = std::env::temp_dir().join("calliope-corpus-test");
        std::fs::create_dir_all(&dir).unwrap();
        let bad = dir.join("bad.toml");
        std::fs::write(&bad, good.replace("jvt/", "../")).unwrap();
        assert!(Manifest::load(&bad).is_err());
    }

    #[test]
    fn rejects_vector_without_checksum() {
        let v = Vector {
            id: "x".into(),
            url: "u".into(),
            sha256: None,
            md5: None,
            archive_member: None,
            decoded_md5: None,
            license: "l".into(),
            notes: String::new(),
        };
        assert!(v.validate().is_err());
    }

    fn vector(md5: &str, archive_member: Option<&str>) -> Vector {
        Vector {
            id: "suite/clip.264".into(),
            url: "u".into(),
            sha256: None,
            md5: Some(md5.into()),
            archive_member: archive_member.map(str::to_string),
            decoded_md5: None,
            license: "conformance".into(),
            notes: String::new(),
        }
    }

    #[test]
    fn materialize_verifies_md5_and_writes_plain_file() {
        let body = b"raw elementary stream";
        let md5 = hex::encode(Md5::digest(body));
        let dir = std::env::temp_dir().join("calliope-materialize-plain");
        let _ = std::fs::remove_dir_all(&dir);
        let dest = dir.join("suite/clip.264");

        let out = materialize(&vector(&md5, None), body, &dest).unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), body);

        // a wrong checksum is rejected
        assert!(materialize(&vector("deadbeef", None), body, &dest).is_err());
    }

    #[test]
    fn materialize_extracts_zip_member() {
        // build a real zip in memory with the target member plus a decoy
        let mut buf = Vec::new();
        {
            let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            let opts: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            zip.start_file("AUD_MW_E.264", opts).unwrap();
            zip.write_all(b"the real vector bytes").unwrap();
            zip.start_file("readme.txt", opts).unwrap();
            zip.write_all(b"ignore me").unwrap();
            zip.finish().unwrap();
        }
        let md5 = hex::encode(Md5::digest(&buf));
        let dir = std::env::temp_dir().join("calliope-materialize-zip");
        let _ = std::fs::remove_dir_all(&dir);
        let dest = dir.join("jvt/AUD_MW_E.264");

        let out = materialize(&vector(&md5, Some("AUD_MW_E.264")), &buf, &dest).unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), b"the real vector bytes");

        // a missing member is a clear error, not a silent empty file
        assert!(materialize(&vector(&md5, Some("nope.264")), &buf, &dest).is_err());
    }
}
