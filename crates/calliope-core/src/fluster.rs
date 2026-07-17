//! Importer for [Fluster](https://github.com/fluendo/fluster) decoder
//! conformance test-suite JSON into calliope's corpus manifest. Fluster's
//! suite files list official vectors (JVT / JCT-VC / AV1 / ...) with a source
//! URL, the archive's MD5, the vector's path inside the archive, and the
//! expected decoded checksum. We map each to a [`Vector`]; the decoded checksum
//! is captured (`decoded_md5`) for a future engine-independent oracle.
//!
//! Fluster is LGPL, like calliope; its suite JSONs are consumed from a local
//! checkout, not vendored (the vectors themselves have per-suite licensing and
//! are fetched on demand).

use std::path::Path;

use serde::Deserialize;

use crate::corpus::Vector;
use crate::{Error, Result};

#[derive(Debug, Deserialize)]
struct Suite {
    name: String,
    codec: String,
    #[serde(default)]
    test_vectors: Vec<TestVector>,
}

#[derive(Debug, Deserialize)]
struct TestVector {
    name: String,
    source: String,
    source_checksum: String,
    input_file: String,
    #[serde(default)]
    output_format: Option<String>,
    #[serde(default)]
    result: Option<String>,
}

/// Parse one Fluster suite JSON into corpus vectors.
pub fn import_suite_json(text: &str) -> Result<Vec<Vector>> {
    let suite: Suite =
        serde_json::from_str(text).map_err(|e| Error::Parse(format!("fluster suite: {e}")))?;
    suite
        .test_vectors
        .iter()
        .map(|tv| suite.to_vector(tv))
        .collect()
}

/// Import every `*.json` suite under `dir` (recursively), returning all vectors.
pub fn import_dir(dir: &Path) -> Result<Vec<Vector>> {
    let mut vectors = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d)? {
            let path = entry?.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "json") {
                let text = std::fs::read_to_string(&path)?;
                vectors.extend(import_suite_json(&text)?);
            }
        }
    }
    vectors.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(vectors)
}

impl Suite {
    fn to_vector(&self, tv: &TestVector) -> Result<Vector> {
        // `input_file` is the vector's path inside the archive; keep it verbatim
        // as the zip member and use its basename for the cache filename.
        let basename = tv
            .input_file
            .rsplit(['/', '\\'])
            .next()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| Error::Parse(format!("fluster {}: empty input_file", tv.name)))?;
        let id = format!(
            "fluster/{}/{}/{}",
            sanitize(&self.name),
            sanitize(&tv.name),
            basename
        );
        let is_zip = tv.source.to_ascii_lowercase().ends_with(".zip");
        Ok(Vector {
            id,
            url: tv.source.clone(),
            sha256: None,
            md5: Some(tv.source_checksum.clone()),
            archive_member: is_zip.then(|| tv.input_file.clone()),
            decoded_md5: tv.result.clone(),
            // only carry the golden format when it is one we can reproduce
            output_format: tv
                .output_format
                .as_deref()
                .and_then(crate::scenario::PixelFormat::from_pix_fmt),
            license: format!("{} conformance (Fluster suite {})", self.codec, self.name),
            notes: String::new(),
        })
    }
}

/// keep an id path component safe: no separators, no `..`
fn sanitize(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| {
            if c == '/' || c == '\\' || c.is_whitespace() {
                '_'
            } else {
                c
            }
        })
        .collect();
    if cleaned == ".." {
        "__".into()
    } else {
        cleaned
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
      "name": "JVT-AVC_V1",
      "codec": "H.264",
      "description": "JVT AVC version 1",
      "test_vectors": [
        {
          "name": "AUD_MW_E",
          "source": "https://www.itu.int/.../AUD_MW_E.zip",
          "source_checksum": "7132c9cf7bc85fdde62add5ec25ea532",
          "input_file": "AUD_MW_E.264",
          "profile": "Constrained Baseline",
          "output_format": "yuv420p",
          "result": "e96fe5054de0329a8868d06003375cdb"
        },
        {
          "name": "BA1_Sony_D",
          "source": "https://example.org/BA1_Sony_D.264",
          "source_checksum": "aabbcc",
          "input_file": "BA1_Sony_D.264",
          "result": "ddeeff"
        }
      ]
    }"#;

    #[test]
    fn maps_zip_and_direct_vectors() {
        let v = import_suite_json(SAMPLE).unwrap();
        assert_eq!(v.len(), 2);

        let zip = &v[0];
        assert_eq!(zip.id, "fluster/JVT-AVC_V1/AUD_MW_E/AUD_MW_E.264");
        assert_eq!(zip.md5.as_deref(), Some("7132c9cf7bc85fdde62add5ec25ea532"));
        // .zip source -> extract the member
        assert_eq!(zip.archive_member.as_deref(), Some("AUD_MW_E.264"));
        assert_eq!(
            zip.decoded_md5.as_deref(),
            Some("e96fe5054de0329a8868d06003375cdb")
        );
        assert!(zip.license.contains("H.264 conformance"));

        // a direct (non-zip) source needs no archive member
        let direct = &v[1];
        assert_eq!(direct.archive_member, None);
        assert_eq!(direct.md5.as_deref(), Some("aabbcc"));
    }

    #[test]
    fn imported_vectors_validate_in_a_manifest() {
        // round-trip through TOML the way `corpus import` writes them
        let vectors = import_suite_json(SAMPLE).unwrap();
        let manifest = crate::corpus::Manifest { vector: vectors };
        let toml = toml::to_string(&manifest).unwrap();
        let reloaded: crate::corpus::Manifest = toml::from_str(&toml).unwrap();
        assert_eq!(reloaded.vector.len(), 2);
        assert!(
            reloaded
                .get("fluster/JVT-AVC_V1/AUD_MW_E/AUD_MW_E.264")
                .is_ok()
        );
    }
}
