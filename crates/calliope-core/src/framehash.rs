//! Per-frame hashes are the interchange format between engines, adopting
//! ffmpeg's framemd5: lowercase hex MD5 of each decoded frame's packed
//! planes. Engines either emit it natively (ffmpeg) or dump raw frames that
//! we chunk and hash here to the same layout.

use std::io::Read;
use std::path::Path;

use md5::{Digest, Md5};

use crate::{Error, Result};

/// parse ffmpeg `-f framemd5` output, keeping stream 0 in order
pub fn parse_framemd5(text: &str) -> Vec<String> {
    text.lines()
        .filter(|l| !l.trim_start().starts_with('#') && !l.trim().is_empty())
        .filter_map(|l| {
            let mut fields = l.split(',').map(str::trim);
            let stream = fields.next()?;
            if stream != "0" {
                return None;
            }
            fields.next_back().map(str::to_string)
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChecksumLine {
    // None for a frame g2g sent without a pts
    pub pts_ns: Option<u64>,
    pub digest: String,
}

const NANOSECONDS_PER_SECOND: u64 = 1_000_000_000;
const NANOSECOND_DIGITS: usize = 9;
// g2g's checksumsink prints its unset pts, u64::MAX nanoseconds, in the same h:mm:ss form
const UNSET_PTS_NS: u64 = u64::MAX;
// GST_TIME_FORMAT of GST_CLOCK_TIME_NONE, what gstreamer's checksumsink prints for an unset pts
const GSTREAMER_UNSET_PTS: &str = "99:99:99.999999999";

// parse checksumsink output, one `h:mm:ss.nnnnnnnnn <digest>` line per buffer
pub fn parse_checksum_lines(text: &str) -> Result<Vec<ChecksumLine>> {
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let bad = || Error::Parse(format!("not a checksumsink line: {line:?}"));
            let (time, digest) = line.trim().split_once(' ').ok_or_else(bad)?;
            let digest = digest.trim().to_string();
            if time == GSTREAMER_UNSET_PTS {
                return Ok(ChecksumLine {
                    pts_ns: None,
                    digest,
                });
            }
            let (clock, fraction) = time.split_once('.').ok_or_else(bad)?;
            let mut fields = clock.split(':').map(str::parse::<u64>);
            let (Some(Ok(hours)), Some(Ok(minutes)), Some(Ok(seconds)), None) =
                (fields.next(), fields.next(), fields.next(), fields.next())
            else {
                return Err(bad());
            };
            if fraction.len() != NANOSECOND_DIGITS {
                return Err(bad());
            }
            let nanoseconds: u64 = fraction.parse().map_err(|_| bad())?;
            let pts_ns = hours
                .checked_mul(60)
                .and_then(|total| total.checked_add(minutes)?.checked_mul(60))
                .and_then(|total| {
                    total
                        .checked_add(seconds)?
                        .checked_mul(NANOSECONDS_PER_SECOND)
                })
                .and_then(|ns| ns.checked_add(nanoseconds))
                .ok_or_else(bad)?;
            Ok(ChecksumLine {
                pts_ns: (pts_ns != UNSET_PTS_NS).then_some(pts_ns),
                digest,
            })
        })
        .collect()
}

/// hash a concatenated raw-frame dump in `frame_size` chunks
pub fn hash_raw_dump(path: &Path, frame_size: usize) -> Result<Vec<String>> {
    let mut file = std::fs::File::open(path)?;
    let mut buf = vec![0u8; frame_size];
    let mut hashes = Vec::new();
    loop {
        let mut filled = 0;
        while filled < frame_size {
            let n = file.read(&mut buf[filled..])?;
            if n == 0 {
                break;
            }
            filled += n;
        }
        if filled == 0 {
            return Ok(hashes);
        }
        if filled < frame_size {
            return Err(Error::Parse(format!(
                "{}: trailing partial frame ({filled} of {frame_size} bytes); wrong geometry or truncated output",
                path.display()
            )));
        }
        hashes.push(hex::encode(Md5::digest(&buf)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_framemd5_output() {
        let text = "\
#format: frame checksums
#version: 2
#hash: MD5
#tb 0: 1/25
#media_type 0: video
#stream#, dts,        pts, duration,     size, hash
0,          0,          0,        1,    38016, aabbccdd
0,          1,          1,        1,    38016, 11223344
1,          0,          0,        1,     4096, ffff0000
";
        assert_eq!(parse_framemd5(text), vec!["aabbccdd", "11223344"]);
    }

    #[test]
    fn parses_checksumsink_lines() {
        // g2g-launch output for mpegts/h264small.ts, then g2g's and gstreamer's lines for an unset pts
        let text = "\
0:00:01.400000000 6a0be7e55243dd6b37263e6a5cbd8a41
0:00:01.466666666 4955408b486846926dcb7c44dbb0096c
5124095:34:33.709551615 47b10f9bdab3cdd6da48154388dcae11
99:99:99.999999999 162ac65135511373ea7d9de6e096d89e
";
        let lines = parse_checksum_lines(text).unwrap();
        let pts: Vec<Option<u64>> = lines.iter().map(|l| l.pts_ns).collect();
        assert_eq!(pts, [Some(1_400_000_000), Some(1_466_666_666), None, None]);
        assert_eq!(lines[1].digest, "4955408b486846926dcb7c44dbb0096c");
        for bad in [
            "0:00:01.4 d",
            "00:01.400000000 d",
            "0:00:01.400000000",
            "99999999999999999:00:00.000000000 d",
        ] {
            assert!(parse_checksum_lines(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn hashes_raw_dump_per_frame() {
        let dir = std::env::temp_dir().join("calliope-framehash-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("dump.yuv");

        // two identical frames then a different one
        let mut data = vec![7u8; 8];
        data.extend_from_slice(&[7u8; 8]);
        data.extend_from_slice(&[9u8; 8]);
        std::fs::write(&path, &data).unwrap();

        let hashes = hash_raw_dump(&path, 8).unwrap();
        assert_eq!(hashes.len(), 3);
        assert_eq!(hashes[0], hashes[1]);
        assert_ne!(hashes[0], hashes[2]);

        // truncated trailing frame must fail, not silently drop
        std::fs::write(&path, [7u8; 12]).unwrap();
        assert!(hash_raw_dump(&path, 8).is_err());
    }
}
