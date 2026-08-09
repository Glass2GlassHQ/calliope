//! GStreamer pipeline introspection: run a launch line with
//! `GST_DEBUG_DUMP_DOT_DIR` set and parse the resulting graph dump into the
//! engine-neutral [`PipelineGraph`]. The dump written on the way out of PLAYING
//! carries the post-autoplug element set and each pad link's negotiated caps,
//! which `-v` only reports for the links that changed caps while running.
//!
//! Bins are flattened: a bin's ghost pads and proxy pads are walked through, so
//! `decodebin` disappears and the elements it auto-plugged become the graph, the
//! same shape an engine that resolves its decoder chain up front reports.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use calliope_core::engine::binary;
use calliope_core::pipeline_diff::{Caps, Element, Link, PipelineGraph};
use calliope_core::{Error, Result};

/// Run `pipeline_args` through gst-launch with a dot dump into `dot_dir`
/// (created if absent). Returns the finished process output; the caller judges
/// the exit status and then reads the dump with [`graph_from_dump`].
pub fn run_with_dot_dump(pipeline_args: &[String], dot_dir: &Path) -> Result<std::process::Output> {
    std::fs::create_dir_all(dot_dir)?;
    let program = binary("CALLIOPE_GST_LAUNCH", "gst-launch-1.0");
    std::process::Command::new(&program)
        .arg("-q")
        .args(pipeline_args)
        .env("GST_DEBUG_DUMP_DOT_DIR", dot_dir)
        .output()
        .map_err(|e| Error::Engine {
            engine: "gstreamer".into(),
            message: format!("{program}: {e}"),
        })
}

/// Parse the most negotiated dump in `dot_dir`: the one written leaving PLAYING,
/// else the one written entering it, else the newest.
pub fn graph_from_dump(dot_dir: &Path) -> Result<PipelineGraph> {
    let path = newest_playing_dump(dot_dir)?;
    parse_dot(&std::fs::read_to_string(&path)?)
}

fn newest_playing_dump(dot_dir: &Path) -> Result<PathBuf> {
    let mut dumps: Vec<PathBuf> = std::fs::read_dir(dot_dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "dot"))
        .collect();
    // Names start with the elapsed timestamp, so lexical order is time order.
    dumps.sort();
    let by_state = |suffix: &str| {
        dumps
            .iter()
            .rev()
            .find(|p| p.to_string_lossy().ends_with(suffix))
            .cloned()
    };
    by_state(".PLAYING_PAUSED.dot")
        .or_else(|| by_state(".PAUSED_PLAYING.dot"))
        .or_else(|| dumps.last().cloned())
        .ok_or_else(|| {
            Error::Parse(format!(
                "no dot dump in {}: GST_DEBUG_DUMP_DOT_DIR needs a gstreamer built with debug support",
                dot_dir.display()
            ))
        })
}

/// One element cluster in the dump: the pad-id prefix its pads carry, its
/// instance name, the nesting depth of its body, and whether it holds other
/// elements (a bin, which gets flattened away).
#[derive(Debug)]
struct Cluster {
    prefix: String,
    name: String,
    depth: isize,
    is_bin: bool,
}

/// Parse a `gst-launch` dot dump into the engine-neutral graph.
pub fn parse_dot(text: &str) -> Result<PipelineGraph> {
    let clusters = parse_clusters(text);
    if clusters.is_empty() {
        return Err(Error::Parse("dot dump has no element clusters".into()));
    }
    let pad_links = parse_pad_links(text);

    // Pad -> owning cluster, longest prefix wins (a bin's prefix is a prefix of
    // nothing else, but the longest match is the right rule regardless).
    let owner = |pad: &str| -> Option<usize> {
        clusters
            .iter()
            .enumerate()
            .filter(|(_, c)| {
                pad.starts_with(&c.prefix) && pad[c.prefix.len()..].starts_with("_node_")
            })
            .max_by_key(|(_, c)| c.prefix.len())
            .map(|(i, _)| i)
    };
    let is_leaf = |cluster: usize| !clusters[cluster].is_bin;

    // Leaf elements become the graph's nodes, in dump order.
    let mut node_of_cluster = HashMap::new();
    let mut elements = Vec::new();
    for (i, cluster) in clusters.iter().enumerate() {
        if is_leaf(i) {
            node_of_cluster.insert(i, elements.len());
            elements.push(Element {
                name: cluster.name.clone(),
            });
        }
    }

    let mut adjacency: HashMap<&str, Vec<(&str, Option<Caps>)>> = HashMap::new();
    for (from, to, caps) in &pad_links {
        adjacency
            .entry(from.as_str())
            .or_default()
            .push((to.as_str(), caps.clone()));
    }

    let mut links: Vec<Link> = Vec::new();
    for (from, to, caps) in &pad_links {
        let Some(src) = owner(from).filter(|c| is_leaf(*c)) else {
            continue;
        };
        let mut reached = Vec::new();
        walk_to_leaf_pads(
            to,
            caps.clone(),
            &adjacency,
            &owner,
            &is_leaf,
            &mut HashSet::new(),
            &mut reached,
        );
        for (dst, caps) in reached {
            let link = Link {
                from: node_of_cluster[&src],
                to: node_of_cluster[&dst],
                caps,
            };
            if !links.iter().any(|l| l.from == link.from && l.to == link.to) {
                links.push(link);
            }
        }
    }

    Ok(PipelineGraph { elements, links })
}

/// Follow `pad` forward until every path reaches a pad owned by a leaf element,
/// walking through bin ghost pads and proxy pads. `caps` is the first caps seen
/// on the way, since the ghost-pad hops of a link repeat (or omit) them.
fn walk_to_leaf_pads<'a>(
    pad: &'a str,
    caps: Option<Caps>,
    adjacency: &HashMap<&'a str, Vec<(&'a str, Option<Caps>)>>,
    owner: &impl Fn(&str) -> Option<usize>,
    is_leaf: &impl Fn(usize) -> bool,
    seen: &mut HashSet<&'a str>,
    out: &mut Vec<(usize, Option<Caps>)>,
) {
    if !seen.insert(pad) {
        return;
    }
    if let Some(cluster) = owner(pad).filter(|c| is_leaf(*c)) {
        out.push((cluster, caps));
        return;
    }
    for (next, next_caps) in adjacency.get(pad).into_iter().flatten() {
        walk_to_leaf_pads(
            next,
            caps.clone().or_else(|| next_caps.clone()),
            adjacency,
            owner,
            is_leaf,
            seen,
            out,
        );
    }
}

/// Collect every element cluster, marking as a bin any that holds another.
fn parse_clusters(text: &str) -> Vec<Cluster> {
    let mut clusters: Vec<Cluster> = Vec::new();
    let mut open: Vec<usize> = Vec::new();
    let mut depth: isize = 0;
    for line in text.lines() {
        if let Some(prefix) = subgraph_id(line).and_then(element_prefix) {
            if let Some(parent) = open.last() {
                clusters[*parent].is_bin = true;
            }
            open.push(clusters.len());
            clusters.push(Cluster {
                prefix,
                name: String::new(),
                depth: depth + 1,
                is_bin: false,
            });
        } else if let Some(index) = open.last().copied() {
            // An element's own label is the first `label=` line in its body; the
            // pad subgraphs and edges inside it sit at a deeper level.
            let trimmed = line.trim_start();
            if clusters[index].name.is_empty()
                && clusters[index].depth == depth
                && trimmed.starts_with("label=")
                && let Some(label) = attr_value(trimmed, "label")
            {
                clusters[index].name = instance_name(&label);
            }
        }
        depth += net_braces(line);
        while open.last().is_some_and(|i| depth < clusters[*i].depth) {
            open.pop();
        }
    }
    clusters
}

/// Every pad-to-pad edge in the dump, as (source pad, target pad, caps). The
/// invisible edges that only lay out an element's own pads are skipped.
fn parse_pad_links(text: &str) -> Vec<(String, String, Option<Caps>)> {
    let mut links = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        let Some((from, rest)) = line.split_once(" -> ") else {
            continue;
        };
        if from.contains(char::is_whitespace) {
            continue;
        }
        let (to, attrs) = rest
            .find(['[', ' ', ';'])
            .map_or((rest, ""), |i| (&rest[..i], &rest[i..]));
        if attrs.contains("style=\"invis\"") {
            continue;
        }
        // A link whose two pads carry different caps labels the head pad instead.
        let label = attr_value(attrs, "label")
            .filter(|l| !l.trim().is_empty())
            .or_else(|| attr_value(attrs, "headlabel"));
        let caps = label.and_then(|l| Caps::parse(&dot_label_to_caps(&l)));
        links.push((from.to_string(), to.to_string(), caps));
    }
    links
}

/// `video/x-raw\l  width: 160\l` -> `video/x-raw,width=160`.
fn dot_label_to_caps(label: &str) -> String {
    let mut parts = label
        .split("\\l")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| match s.split_once(": ") {
            Some((field, value)) => format!("{}={}", field.trim(), value.trim()),
            None => s.to_string(),
        });
    let mut caps = parts.next().unwrap_or_default();
    for field in parts {
        caps.push(',');
        caps.push_str(&field);
    }
    caps
}

/// `subgraph <id> {` -> the id.
fn subgraph_id(line: &str) -> Option<&str> {
    line.trim().strip_prefix("subgraph ")?.strip_suffix(" {")
}

/// An element cluster id is `cluster_<pad prefix>` ending in the element's
/// address; the pad-group subgraphs inside it end in `_sink` / `_src` instead.
fn element_prefix(id: &str) -> Option<String> {
    let prefix = id.strip_prefix("cluster_")?;
    let (_, address) = prefix.rsplit_once("_0x")?;
    if address.is_empty() || !address.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some(prefix.to_string())
}

/// An element label is `<type>\n<instance name>\n<state>...`.
fn instance_name(label: &str) -> String {
    let mut lines = label.split("\\n");
    let first = lines.next().unwrap_or_default();
    lines.next().unwrap_or(first).trim().to_string()
}

/// The value of `name="..."` in `text`, honoring backslash escapes.
fn attr_value(text: &str, name: &str) -> Option<String> {
    let mut rest = text;
    loop {
        let start = rest.find(name)?;
        let before_is_word = rest[..start]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_');
        let after = &rest[start + name.len()..];
        if !before_is_word && after.starts_with("=\"") {
            let body = &after[2..];
            let mut value = String::new();
            let mut chars = body.chars();
            while let Some(c) = chars.next() {
                match c {
                    '\\' => {
                        value.push(c);
                        if let Some(next) = chars.next() {
                            value.push(next);
                        }
                    }
                    '"' => return Some(value),
                    _ => value.push(c),
                }
            }
            return Some(value);
        }
        rest = &rest[start + name.len()..];
    }
}

/// Net `{` minus `}` on this line, ignoring braces inside quoted strings (a
/// caps property value can contain `{ avc, byte-stream }`).
fn net_braces(line: &str) -> isize {
    let mut depth = 0;
    let mut in_quotes = false;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' if in_quotes => {
                chars.next();
            }
            '"' => in_quotes = !in_quotes,
            '{' if !in_quotes => depth += 1,
            '}' if !in_quotes => depth -= 1,
            _ => {}
        }
    }
    depth
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(graph: &PipelineGraph) -> Vec<&str> {
        graph.elements.iter().map(|e| e.name.as_str()).collect()
    }

    fn link_labels(graph: &PipelineGraph) -> Vec<String> {
        graph
            .links
            .iter()
            .map(|l| {
                format!(
                    "{} -> {}",
                    graph.element_name(l.from),
                    graph.element_name(l.to)
                )
            })
            .collect()
    }

    #[test]
    fn parses_a_linear_pipeline_dump() {
        let graph = parse_dot(include_str!("../testdata/videotestsrc.dot")).unwrap();
        assert_eq!(
            names(&graph),
            ["filesink0", "videoconvert0", "videotestsrc0"]
        );
        assert_eq!(
            link_labels(&graph),
            [
                "videoconvert0 -> filesink0",
                "videotestsrc0 -> videoconvert0"
            ]
        );
        // Both links carry the caps videotestsrc negotiated.
        let caps = graph.links[0].caps.as_ref().unwrap();
        assert_eq!(caps.media_type, "video/x-raw");
        assert_eq!(caps.fields["format"], "A444_16LE");
        assert_eq!(caps.fields["width"], "320");
        assert_eq!(caps.fields["framerate"], "30/1");
    }

    #[test]
    fn flattens_decodebin_into_the_elements_it_autoplugged() {
        let graph = parse_dot(include_str!("../testdata/decodebin-h264.dot")).unwrap();
        // The bin itself is gone; what it plugged is the graph.
        assert!(
            !names(&graph).contains(&"decodebin0"),
            "{:?}",
            names(&graph)
        );
        for expected in [
            "filesrc0",
            "typefind",
            "h264parse0",
            "avdec_h264-0",
            "videoconvert0",
            "capsfilter0",
            "filesink0",
        ] {
            assert!(names(&graph).contains(&expected), "{:?}", names(&graph));
        }
        // The ghost / proxy pad hops are walked through, so the elements either
        // side of the bin boundary link directly.
        let links = link_labels(&graph);
        for expected in [
            "filesrc0 -> typefind",
            "typefind -> h264parse0",
            "avdec_h264-0 -> videoconvert0",
            "videoconvert0 -> capsfilter0",
            "capsfilter0 -> filesink0",
        ] {
            assert!(links.contains(&expected.to_string()), "{links:?}");
        }
    }

    #[test]
    fn carries_the_negotiated_caps_across_the_bin_boundary() {
        let graph = parse_dot(include_str!("../testdata/decodebin-h264.dot")).unwrap();
        let decoded = graph
            .links
            .iter()
            .find(|l| {
                graph.element_name(l.from) == "avdec_h264-0"
                    && graph.element_name(l.to) == "videoconvert0"
            })
            .expect("decoder output link");
        let caps = decoded.caps.as_ref().expect("negotiated caps");
        assert_eq!(caps.media_type, "video/x-raw");
        assert_eq!(caps.fields["format"], "Y42B");
        assert_eq!(caps.fields["height"], "120");

        // filesrc feeds an unnegotiated byte stream: the pad is still ANY.
        let source = graph
            .links
            .iter()
            .find(|l| graph.element_name(l.from) == "filesrc0")
            .expect("source link");
        assert!(source.caps.is_none());
    }

    #[test]
    fn a_braces_in_caps_property_does_not_break_cluster_nesting() {
        // capsfilter1 inside decodebin has `{ avc, byte-stream }` in its label;
        // counting those braces would close the bin early and leave it a leaf.
        let graph = parse_dot(include_str!("../testdata/decodebin-h264.dot")).unwrap();
        assert!(
            names(&graph).contains(&"capsfilter1"),
            "{:?}",
            names(&graph)
        );
        assert!(!names(&graph).contains(&"decodebin0"));
    }

    #[test]
    fn caps_label_becomes_a_caps_string() {
        assert_eq!(
            dot_label_to_caps("video/x-raw\\l               width: 160\\l  framerate: 25/1\\l"),
            "video/x-raw,width=160,framerate=25/1"
        );
        assert_eq!(dot_label_to_caps("ANY"), "ANY");
    }

    #[test]
    fn empty_dump_is_an_error() {
        assert!(parse_dot("digraph pipeline {\n}\n").is_err());
    }
}
