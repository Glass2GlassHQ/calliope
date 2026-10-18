use std::path::{Path, PathBuf};

const EXTINF_TAG: &str = "#EXTINF:";

// the segment uris of a media playlist, resolved against its directory
pub fn segments(playlist: &Path) -> Result<Vec<PathBuf>, String> {
    let text = std::fs::read_to_string(playlist)
        .map_err(|e| format!("read {}: {e}", playlist.display()))?;
    let dir = playlist.parent().unwrap_or(Path::new("."));
    let segments: Vec<PathBuf> = segment_uris(&text).map(|uri| dir.join(uri)).collect();
    if segments.is_empty() {
        return Err("the playlist lists no segments".into());
    }
    Ok(segments)
}

pub fn segment_uris(playlist: &str) -> impl Iterator<Item = &str> {
    playlist
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
}

// every EXTINF duration in seconds, the title after the comma ignored
pub fn segment_durations(playlist: &str) -> Vec<f64> {
    playlist
        .lines()
        .filter_map(|l| l.trim().strip_prefix(EXTINF_TAG))
        .filter_map(|rest| rest.split(',').next()?.trim().parse().ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_uris_and_durations() {
        let playlist = include_str!("../testdata/hls-fmp4-doubled.m3u8");
        assert_eq!(
            segment_uris(playlist).collect::<Vec<_>>(),
            ["seg00000.m4s", "seg00001.m4s"]
        );
        assert_eq!(segment_durations(playlist), [15.214, 8.81]);
    }
}
