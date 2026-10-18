use std::path::Path;

use calliope_core::engine::binary;
use calliope_core::produce::{Container, Validation, Validator};

use crate::run_tool;

// gpac logs every box parse error and warning with this prefix, at container@warning
const ISO_FILE_LOG_PREFIX: &str = "[iso file]";
const BRAND_LINE_PREFIX: &str = "Major Brand ";
// ISO/IEC 23000-19 brands a CMAF track file declares
const CMAF_BRANDS: [&str; 2] = ["cmfc", "cmf2"];

// MP4Box parses every box on -info, recovers from bad ones and still exits 0
pub fn validate(artifact: &Path, container: Container) -> Validation {
    let program = binary("CALLIOPE_MP4BOX", "MP4Box");
    let output = match run_tool(
        Validator::Mp4box,
        &program,
        &["-logs=container@warning:ncl", "-info"],
        artifact,
    ) {
        Ok(output) => output,
        Err(skipped) => return skipped,
    };
    let mut findings = findings(&output.stderr, container);
    if !output.status.success() {
        findings.push(format!("MP4Box exited with {}", output.status));
    }
    Validation::from_findings(Validator::Mp4box, findings)
}

fn findings(log: &str, container: Container) -> Vec<String> {
    let mut findings: Vec<String> = log
        .lines()
        .map(str::trim)
        .filter(|l| l.starts_with(ISO_FILE_LOG_PREFIX))
        .map(str::to_string)
        .collect();
    if container == Container::Cmaf {
        let brands = log
            .lines()
            .find_map(|l| l.trim().strip_prefix(BRAND_LINE_PREFIX))
            .unwrap_or_default();
        let declares_cmaf = brands
            .split(|c: char| c.is_whitespace() || c == ':')
            .any(|brand| CMAF_BRANDS.contains(&brand));
        if !declares_cmaf {
            findings.push(format!("no CMAF brand in ftyp: {brands}"));
        }
    }
    findings
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_truncated_file_reports_the_incomplete_box() {
        let log = include_str!("../testdata/mp4box-truncated.stderr");
        let findings = findings(log, Container::Fmp4);
        assert_eq!(findings.len(), 2, "{findings:?}");
        assert!(findings[0].contains("Incomplete box mdat"));
    }

    #[test]
    fn a_cmaf_track_needs_a_cmaf_brand() {
        let cmaf = include_str!("../testdata/mp4box-cmaf.stderr");
        assert!(findings(cmaf, Container::Cmaf).is_empty());
        let plain = include_str!("../testdata/mp4box-fragmented.stderr");
        assert!(findings(plain, Container::Fmp4).is_empty());
        assert_eq!(findings(plain, Container::Cmaf).len(), 1);
    }
}
