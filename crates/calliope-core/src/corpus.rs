//! Corpus manifest + on-demand fetcher. Vectors are never committed:
//! conformance suites carry per-suite licensing, so the manifest records
//! url + sha256 + license and content downloads into a local cache.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::{Error, Result};

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct Manifest {
    #[serde(default)]
    pub vector: Vec<Vector>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct Vector {
    /// cache-relative id, e.g. "jvt/AUD_MW_E.264"
    pub id: String,
    pub url: String,
    pub sha256: String,
    pub license: String,
    #[serde(default)]
    pub notes: String,
}

impl Manifest {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)?;
        let manifest: Manifest =
            toml::from_str(&text).map_err(|e| Error::Parse(format!("{}: {e}", path.display())))?;
        for v in &manifest.vector {
            if v.id.starts_with('/') || v.id.split('/').any(|c| c == "..") {
                return Err(Error::Corpus(format!(
                    "vector id escapes the cache: {}",
                    v.id
                )));
            }
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
    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    let body = reqwest::get(&vector.url)
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| Error::Corpus(format!("{}: {e}", vector.id)))?
        .bytes()
        .await
        .map_err(|e| Error::Corpus(format!("{}: {e}", vector.id)))?;

    let digest = hex::encode(Sha256::digest(&body));
    if !digest.eq_ignore_ascii_case(&vector.sha256) {
        return Err(Error::Corpus(format!(
            "{}: sha256 mismatch, expected {} got {digest}",
            vector.id, vector.sha256
        )));
    }

    // write-then-rename so an interrupted fetch never leaves a bad cache hit
    let tmp = dest.with_extension("part");
    tokio::fs::write(&tmp, &body).await?;
    tokio::fs::rename(&tmp, &dest).await?;
    Ok(dest)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(m.get("jvt/AUD_MW_E.264").is_ok());
        assert!(m.get("missing").is_err());

        let dir = std::env::temp_dir().join("calliope-corpus-test");
        std::fs::create_dir_all(&dir).unwrap();
        let bad = dir.join("bad.toml");
        std::fs::write(&bad, good.replace("jvt/", "../")).unwrap();
        assert!(Manifest::load(&bad).is_err());
    }

    #[test]
    fn empty_manifest_is_valid() {
        let m: Manifest = toml::from_str("").unwrap();
        assert!(m.vector.is_empty());
    }
}
