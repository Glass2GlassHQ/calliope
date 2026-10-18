// importer for a directory tree of real-world media samples (ffmpeg fate-suite, gst-integration-testsuites medias)

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::corpus::{Media, Vector};
use crate::{Error, Result};

// gst-validate's expected decoded frames sit beside the inputs as png directories
const GST_REFERENCE_FRAMES_SUFFIX: &str = "_reference_frames";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleTree {
    FateSuite,
    GstMedias,
}

impl SampleTree {
    // cache-relative directory, so a tree fetched into the cache needs no download
    pub fn id_prefix(self) -> &'static str {
        match self {
            Self::FateSuite => "fate-suite",
            Self::GstMedias => "gst-medias",
        }
    }

    fn url_base(self) -> &'static str {
        match self {
            Self::FateSuite => "https://fate-suite.ffmpeg.org",
            Self::GstMedias => {
                "https://gitlab.freedesktop.org/gstreamer/gst-integration-testsuites/-/raw/main"
            }
        }
    }

    // directories of generated outputs rather than samples
    fn skips_dir(self, name: &str) -> bool {
        self == Self::GstMedias && name.ends_with(GST_REFERENCE_FRAMES_SUFFIX)
    }

    fn license(self) -> &'static str {
        match self {
            Self::FateSuite => {
                "FFmpeg FATE sample suite, redistributable test data (fate-suite.ffmpeg.org)"
            }
            Self::GstMedias => {
                "GStreamer gst-integration-testsuites medias, no license stated, fetched not redistributed"
            }
        }
    }
}

// one vector per file `probe` reports as media, ids sorted
pub fn import_tree(
    root: &Path,
    tree: SampleTree,
    mut probe: impl FnMut(&Path) -> Result<Option<Media>>,
) -> Result<Vec<Vector>> {
    let mut vectors = Vec::new();
    for path in sample_files(root, tree)? {
        let Some(media) = probe(&path)? else {
            continue;
        };
        let relative = path
            .strip_prefix(root)
            .expect("walked under root")
            .to_str()
            .ok_or_else(|| Error::Corpus(format!("non-utf8 sample path {}", path.display())))?
            .replace('\\', "/");
        vectors.push(Vector {
            id: format!("{}/{relative}", tree.id_prefix()),
            url: format!("{}/{relative}", tree.url_base()),
            sha256: Some(hex::encode(Sha256::digest(std::fs::read(&path)?))),
            md5: None,
            archive_member: None,
            decoded_md5: None,
            output_format: None,
            license: tree.license().into(),
            notes: String::new(),
            media: Some(media),
        });
    }
    vectors.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(vectors)
}

// every regular file under `root`, skipping hidden entries such as `.git` and generated outputs
fn sample_files(root: &Path, tree: SampleTree) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                if !tree.skips_dir(&name) {
                    stack.push(entry.path());
                }
            } else if file_type.is_file() {
                files.push(entry.path());
            }
        }
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::corpus::{AudioStream, Manifest, VideoStream};

    fn write(root: &Path, relative: &str, bytes: &[u8]) {
        let path = root.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }

    // stands in for ffprobe: classifies by extension
    fn fake_probe(path: &Path) -> Result<Option<Media>> {
        let extension = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        Ok(match extension {
            "h264" => Some(Media {
                container: "h264".into(),
                video: Some(VideoStream {
                    codec: "h264".into(),
                    width: 176,
                    height: 144,
                    pix_fmt: Some("yuv420p".into()),
                }),
                audio: None,
            }),
            "wav" => Some(Media {
                container: "wav".into(),
                video: None,
                audio: Some(AudioStream {
                    codec: "pcm_s16le".into(),
                    sample_rate: 8000,
                    channels: 1,
                }),
            }),
            _ => None,
        })
    }

    #[test]
    fn keeps_media_files_and_skips_the_rest() {
        let root = std::env::temp_dir().join("calliope-sample-tree");
        let _ = std::fs::remove_dir_all(&root);
        write(&root, "h264/clip.h264", b"video bytes");
        write(&root, "wav/tone.wav", b"audio bytes");
        write(&root, "md5sum", b"not media");
        write(&root, ".git/objects/clip.h264", b"hidden");

        let vectors = import_tree(&root, SampleTree::FateSuite, fake_probe).unwrap();
        let ids: Vec<&str> = vectors.iter().map(|v| v.id.as_str()).collect();
        assert_eq!(
            ids,
            ["fate-suite/h264/clip.h264", "fate-suite/wav/tone.wav"]
        );

        let clip = &vectors[0];
        assert_eq!(clip.url, "https://fate-suite.ffmpeg.org/h264/clip.h264");
        assert_eq!(
            clip.sha256.as_deref(),
            Some(hex::encode(Sha256::digest(b"video bytes")).as_str())
        );
        assert!(clip.license.contains("fate-suite"));
        assert_eq!(
            clip.media.as_ref().unwrap().video.as_ref().unwrap().codec,
            "h264"
        );
    }

    #[test]
    fn imported_vectors_round_trip_through_the_manifest() {
        let root = std::env::temp_dir().join("calliope-sample-tree-manifest");
        let _ = std::fs::remove_dir_all(&root);
        write(&root, "mp4/a.h264", b"x");
        write(&root, "flac/b.wav", b"y");
        write(
            &root,
            "mp4/a_mp4_reference_frames/0.h264",
            b"expected frame",
        );
        let vectors = import_tree(&root, SampleTree::GstMedias, fake_probe).unwrap();
        let manifest = Manifest { vector: vectors };
        let path = root.join("vectors.toml");
        std::fs::write(&path, toml::to_string(&manifest).unwrap()).unwrap();
        let reloaded = Manifest::load(&path).unwrap();
        assert_eq!(reloaded.vector.len(), 2);
        let audio = reloaded.get("gst-medias/flac/b.wav").unwrap();
        assert_eq!(audio.media, manifest.vector[0].media);
    }
}
