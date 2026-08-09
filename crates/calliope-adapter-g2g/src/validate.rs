//! glass2glass pipeline introspection: `g2g-launch --validate-json` parses and
//! negotiates a launch line without running it, and prints every node plus each
//! edge's negotiated caps. Parsed here into the engine-neutral
//! [`PipelineGraph`].

use calliope_core::engine::binary;
use calliope_core::pipeline_diff::{Caps, Element, Link, PipelineGraph};
use calliope_core::{Error, Result};

const VALIDATE_FLAG: &str = "--validate-json";

/// Is the configured g2g-launch new enough to dump the negotiated graph? Older
/// builds warn about the unknown flag and run the pipeline instead, so this is
/// checked before invoking it.
pub fn supports_validate_json() -> bool {
    let program = binary("CALLIOPE_G2G_LAUNCH", "g2g-launch");
    std::process::Command::new(program)
        .arg("--help")
        .output()
        .is_ok_and(|out| {
            let text = String::from_utf8_lossy(&out.stdout).into_owned()
                + &String::from_utf8_lossy(&out.stderr);
            text.contains(VALIDATE_FLAG)
        })
}

/// Negotiate `pipeline_args` and return the graph g2g would build. A pipeline
/// that fails to negotiate is an error carrying g2g's own explanation.
pub fn negotiated_graph(pipeline_args: &[String]) -> Result<PipelineGraph> {
    let program = binary("CALLIOPE_G2G_LAUNCH", "g2g-launch");
    let out = std::process::Command::new(&program)
        .arg(VALIDATE_FLAG)
        .args(pipeline_args)
        .output()
        .map_err(|e| Error::Engine {
            engine: "g2g".into(),
            message: format!("{program}: {e}"),
        })?;
    parse_validate_json(&String::from_utf8_lossy(&out.stdout))
}

/// Parse the `--validate-json` document into the neutral graph.
pub fn parse_validate_json(text: &str) -> Result<PipelineGraph> {
    let value: serde_json::Value = serde_json::from_str(text.trim())
        .map_err(|e| Error::Parse(format!("g2g --validate-json output: {e}")))?;
    if value["ok"] != true {
        return Err(Error::Engine {
            engine: "g2g".into(),
            message: format!("negotiation failed: {value}"),
        });
    }
    let elements = value["nodes"]
        .as_array()
        .ok_or_else(|| Error::Parse("g2g --validate-json: no nodes".into()))?
        .iter()
        .map(|node| Element {
            name: node["name"].as_str().unwrap_or("<unnamed>").to_string(),
        })
        .collect();
    let links = value["edges"]
        .as_array()
        .ok_or_else(|| Error::Parse("g2g --validate-json: no edges".into()))?
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

fn index(edge: &serde_json::Value, field: &str) -> Result<usize> {
    edge[field]
        .as_u64()
        .map(|i| i as usize)
        .ok_or_else(|| Error::Parse(format!("g2g --validate-json: edge without '{field}'")))
}

#[cfg(test)]
mod tests {
    use super::*;

    // A real `g2g-launch --validate-json videotestsrc num-buffers=5 !
    // videoconvert ! filesink location=/tmp/out.raw` document.
    const DUMP: &str = r#"{"edges":[{"caps":"video/x-raw,format=RGBA,width=320,height=240,framerate=30/1","from":0,"to":1},{"caps":"video/x-raw,format=RGBA,width=320,height=240,framerate=30/1","from":1,"to":2}],"nodes":[{"index":0,"name":"VideoTestSrc"},{"index":1,"name":"VideoConvert"},{"index":2,"name":"FileSink"}],"ok":true}"#;

    #[test]
    fn parses_nodes_and_per_edge_caps() {
        let graph = parse_validate_json(DUMP).unwrap();
        let names: Vec<&str> = graph.elements.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["VideoTestSrc", "VideoConvert", "FileSink"]);
        assert_eq!(graph.links.len(), 2);
        assert_eq!((graph.links[0].from, graph.links[0].to), (0, 1));
        let caps = graph.links[1].caps.as_ref().unwrap();
        assert_eq!(caps.media_type, "video/x-raw");
        assert_eq!(caps.fields["format"], "RGBA");
        assert_eq!(caps.fields["framerate"], "30/1");
    }

    #[test]
    fn a_failed_negotiation_is_an_error_carrying_g2gs_explanation() {
        let failed = r#"{"ok":false,"stage":"negotiate","failure":{"kind":"empty-link","upstream":0,"downstream":1}}"#;
        let err = parse_validate_json(failed).unwrap_err().to_string();
        assert!(err.contains("empty-link"), "{err}");
    }

    #[test]
    fn non_json_output_is_a_parse_error() {
        // What an older g2g-launch prints: it skips the unknown flag and runs.
        assert!(parse_validate_json("Setting pipeline to PLAYING\nDone\n").is_err());
    }
}
