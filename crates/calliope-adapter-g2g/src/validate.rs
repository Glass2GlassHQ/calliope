//! glass2glass pipeline introspection. `g2g-launch --validate-json` parses and
//! negotiates a launch line without running it; `--run-json` runs it to EOS and
//! prints the same document with the caps each edge actually carried. Both give
//! every node plus per-edge caps, parsed here into the engine-neutral
//! [`PipelineGraph`].
//!
//! `g2g-inspect --gst-map` supplies the element-name synonyms, since g2g names a
//! graph node after the Rust type (`NalParse0`) where gst names it after the
//! factory (`h264parse0`).

use calliope_core::engine::binary;
use calliope_core::pipeline_diff::{Caps, Element, Link, NameSynonyms, PipelineGraph};
use calliope_core::{Error, Result};

const VALIDATE_FLAG: &str = "--validate-json";
const RUN_FLAG: &str = "--run-json";
/// `g2g-inspect`'s element-name synonym dump, not a `g2g-launch` flag.
const GST_MAP_FLAG: &str = "--gst-map";

/// Does the configured g2g-launch advertise `flag`? Older builds warn about an
/// unknown flag and run the pipeline instead, so this is checked before
/// invoking it.
fn help_mentions(flag: &str) -> bool {
    let program = binary("CALLIOPE_G2G_LAUNCH", "g2g-launch");
    std::process::Command::new(program)
        .arg("--help")
        .output()
        .is_ok_and(|out| {
            let text = String::from_utf8_lossy(&out.stdout).into_owned()
                + &String::from_utf8_lossy(&out.stderr);
            text.contains(flag)
        })
}

/// Is the configured g2g-launch new enough to dump the negotiated graph?
pub fn supports_validate_json() -> bool {
    help_mentions(VALIDATE_FLAG)
}

/// Is it new enough to dump the graph with the caps observed while running? The
/// two dumps have the same shape, so this only decides which one to ask for.
pub fn supports_run_json() -> bool {
    help_mentions(RUN_FLAG)
}

/// Negotiate `pipeline_args` and return the graph g2g would build. A pipeline
/// that fails to negotiate is an error carrying g2g's own explanation.
pub fn negotiated_graph(pipeline_args: &[String]) -> Result<PipelineGraph> {
    dump_graph(VALIDATE_FLAG, pipeline_args)
}

/// Run `pipeline_args` to EOS and return the graph with each edge's caps as
/// observed while it ran, so a stream whose geometry only arrives with the data
/// reports what it really carried instead of the solver's placeholder.
pub fn observed_graph(pipeline_args: &[String]) -> Result<PipelineGraph> {
    dump_graph(RUN_FLAG, pipeline_args)
}

fn dump_graph(flag: &str, pipeline_args: &[String]) -> Result<PipelineGraph> {
    let program = binary("CALLIOPE_G2G_LAUNCH", "g2g-launch");
    let out = std::process::Command::new(&program)
        .arg(flag)
        .args(pipeline_args)
        .output()
        .map_err(|e| Error::Engine {
            engine: "g2g".into(),
            message: format!("{program}: {e}"),
        })?;
    parse_graph_json(&String::from_utf8_lossy(&out.stdout))
}

/// Parse a `--validate-json` / `--run-json` document into the neutral graph.
/// The run dump adds a per-edge `caps_source`, which the neutral graph does not
/// model: which dump was read is recorded on the parity report instead.
pub fn parse_graph_json(text: &str) -> Result<PipelineGraph> {
    let value: serde_json::Value = serde_json::from_str(text.trim())
        .map_err(|e| Error::Parse(format!("g2g graph dump: {e}")))?;
    if value["ok"] != true {
        return Err(Error::Engine {
            engine: "g2g".into(),
            message: format!("negotiation failed: {value}"),
        });
    }
    let elements = value["nodes"]
        .as_array()
        .ok_or_else(|| Error::Parse("g2g graph dump: no nodes".into()))?
        .iter()
        .map(|node| Element {
            name: node["name"].as_str().unwrap_or("<unnamed>").to_string(),
        })
        .collect();
    let links = value["edges"]
        .as_array()
        .ok_or_else(|| Error::Parse("g2g graph dump: no edges".into()))?
        .iter()
        .map(|edge| {
            Ok(Link {
                from: index(edge, "from")?,
                to: index(edge, "to")?,
                caps: edge["caps"].as_str().and_then(Caps::parse),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(PipelineGraph { elements, links })
}

/// The gst-name/g2g-name pairs `g2g-inspect --gst-map` prints, so a parity diff
/// pairs the elements the two engines name differently (`h264parse` against
/// `NalParse`). An inspect binary that is missing, or too old for the flag,
/// gives an empty table: those elements then stay unpaired, as they were before.
pub fn name_synonyms() -> NameSynonyms {
    let program = binary("CALLIOPE_G2G_INSPECT", "g2g-inspect");
    let out = std::process::Command::new(&program)
        .arg(GST_MAP_FLAG)
        .output();
    match out {
        Ok(out) if out.status.success() => {
            NameSynonyms::parse_tsv(&String::from_utf8_lossy(&out.stdout))
        }
        _ => NameSynonyms::default(),
    }
}

fn index(edge: &serde_json::Value, field: &str) -> Result<usize> {
    edge[field]
        .as_u64()
        .map(|i| i as usize)
        .ok_or_else(|| Error::Parse(format!("g2g graph dump: edge without '{field}'")))
}

#[cfg(test)]
mod tests {
    use super::*;

    // A real `g2g-launch --validate-json videotestsrc num-buffers=5 !
    // videoconvert ! filesink location=/tmp/out.raw` document.
    const DUMP: &str = r#"{"edges":[{"caps":"video/x-raw,format=RGBA,width=320,height=240,framerate=30/1","from":0,"to":1},{"caps":"video/x-raw,format=RGBA,width=320,height=240,framerate=30/1","from":1,"to":2}],"nodes":[{"index":0,"name":"VideoTestSrc"},{"index":1,"name":"VideoConvert"},{"index":2,"name":"FileSink"}],"ok":true}"#;

    #[test]
    fn parses_nodes_and_per_edge_caps() {
        let graph = parse_graph_json(DUMP).unwrap();
        let names: Vec<&str> = graph.elements.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["VideoTestSrc", "VideoConvert", "FileSink"]);
        assert_eq!(graph.links.len(), 2);
        assert_eq!((graph.links[0].from, graph.links[0].to), (0, 1));
        let caps = graph.links[1].caps.as_ref().unwrap();
        assert_eq!(caps.media_type, "video/x-raw");
        assert_eq!(caps.fields["format"], "RGBA");
        assert_eq!(caps.fields["framerate"], "30/1");
    }

    // A real `g2g-launch --run-json filesrc location=clip.h264 ! h264parse !
    // fakesink` document: the parser's output link refined mid-run, the byte
    // stream feeding it never did.
    const RUN_DUMP: &str = r#"{"edges":[{"caps":"video/x-h264,width=16,height=16,framerate=1/1","caps_source":"negotiated","from":0,"to":1},{"caps":"video/x-h264,width=320,height=240,framerate=30/1","caps_source":"runtime","from":1,"to":2}],"nodes":[{"index":0,"name":"FileSrc0"},{"index":1,"name":"NalParse0"},{"index":2,"name":"FakeSink0"}],"ok":true}"#;

    #[test]
    fn the_run_dump_parses_with_the_caps_that_crossed() {
        let graph = parse_graph_json(RUN_DUMP).unwrap();
        let names: Vec<&str> = graph.elements.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["FileSrc0", "NalParse0", "FakeSink0"]);
        // The refined link carries the real geometry, not the 16x16 placeholder
        // the same line negotiates before it runs.
        let refined = graph.links[1].caps.as_ref().unwrap();
        assert_eq!(refined.fields["width"], "320");
        assert_eq!(refined.fields["height"], "240");
    }

    #[test]
    fn a_failed_negotiation_is_an_error_carrying_g2gs_explanation() {
        let failed = r#"{"ok":false,"stage":"negotiate","failure":{"kind":"empty-link","upstream":0,"downstream":1}}"#;
        let err = parse_graph_json(failed).unwrap_err().to_string();
        assert!(err.contains("empty-link"), "{err}");
    }

    #[test]
    fn non_json_output_is_a_parse_error() {
        // What an older g2g-launch prints: it skips the unknown flag and runs.
        assert!(parse_graph_json("Setting pipeline to PLAYING\nDone\n").is_err());
    }
}
