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
