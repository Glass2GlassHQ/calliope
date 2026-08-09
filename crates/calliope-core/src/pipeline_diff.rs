//! Engine-neutral model of a built pipeline (elements, links, negotiated caps)
//! and the diff between two of them. Each adapter parses its engine's
//! introspection dump into a [`PipelineGraph`]; nothing here knows which engine
//! produced one.
//!
//! The diff is approximate by construction: engines name elements differently,
//! auto-plug different helper elements, and model different caps fields. Only a
//! caps conflict on a link both engines built is treated as a real difference;
//! everything else is reported as informational.

use std::collections::BTreeMap;
use std::fmt;

use serde::Serialize;

/// One element of a built pipeline, named as its own engine names it
/// (`videoconvert0`, `VideoConvert`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Element {
    pub name: String,
}

/// A link between two elements, by index into [`PipelineGraph::elements`].
/// `caps` is None when the engine reported no negotiated caps for that hop
/// (a gst pad still on ANY, an unnegotiated byte stream).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Link {
    pub from: usize,
    pub to: usize,
    pub caps: Option<Caps>,
}

/// What one engine says it built: the post-autoplug element set and every link
/// with the caps that got negotiated on it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct PipelineGraph {
    pub elements: Vec<Element>,
    pub links: Vec<Link>,
}

impl PipelineGraph {
    pub fn element_name(&self, index: usize) -> &str {
        self.elements
            .get(index)
            .map_or("<unknown>", |e| e.name.as_str())
    }

    fn link_label(&self, link: &Link) -> String {
        format!(
            "{} -> {}",
            self.element_name(link.from),
            self.element_name(link.to)
        )
    }
}

/// A fixed caps structure: the media type plus its fields, both engines'
/// spellings normalized (no `(type)` markers, no quotes, no padding).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Caps {
    pub media_type: String,
    pub fields: BTreeMap<String, String>,
}

impl Caps {
    /// Parse a gst-style caps string (`video/x-raw, width=(int)320, format=I420`).
    /// None for an absent or wildcard (`ANY`) caps.
    pub fn parse(text: &str) -> Option<Caps> {
        let text = text.trim();
        if text.is_empty() || text.eq_ignore_ascii_case("ANY") {
            return None;
        }
        let mut parts = text.split(',');
        let media_type = parts.next()?.trim().to_string();
        if media_type.is_empty() {
            return None;
        }
        let mut fields = BTreeMap::new();
        for part in parts {
            let Some((key, value)) = part.split_once('=') else {
                continue;
            };
            fields.insert(key.trim().to_string(), normalize_value(value));
        }
        Some(Caps { media_type, fields })
    }
}

impl fmt::Display for Caps {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.media_type)?;
        for (key, value) in &self.fields {
            write!(f, ",{key}={value}")?;
        }
        Ok(())
    }
}

/// Strip a gst type marker (`(int)320`) and any quoting from a field value.
fn normalize_value(value: &str) -> String {
    let value = value.trim();
    let value = match value.strip_prefix('(') {
        Some(rest) => rest.split_once(')').map_or(value, |(_, v)| v),
        None => value,
    };
    value.trim().trim_matches('"').to_string()
}

/// Comparable form of an element name: lowercase, alphanumerics only, so
/// `avdec_h264-0` and `AvdecH264` land on the same string.
fn canonical(name: &str) -> String {
    name.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// Do two engines' names denote the same element? Equal canonically, or equal
/// once one side's trailing instance number is dropped (`videoconvert0` vs
/// `VideoConvert`). Digits are never stripped blindly, so `avdec_h264` keeps its
/// codec number.
pub fn element_names_match(left: &str, right: &str) -> bool {
    let (left, right) = (canonical(left), canonical(right));
    if left == right {
        return true;
    }
    let instance_suffix = |long: &str, short: &str| {
        !short.is_empty()
            && long
                .strip_prefix(short)
                .is_some_and(|rest| rest.chars().all(|c| c.is_ascii_digit()))
    };
    instance_suffix(&left, &right) || instance_suffix(&right, &left)
}

/// How close two engines' pipelines are. Approximate: see the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Verdict {
    /// same elements, same links, same caps on every link
    Match,
    /// only differences the harness cannot judge: a different element set or
    /// naming, or caps fields one engine does not model
    Informational,
    /// a media type or field conflict on a link both engines built
    Differs,
}

/// One matched link's caps comparison. `conflicts` and `media_type_differs` are
/// real disagreements; the `fields_only_*` lists are informational, since an
/// engine that does not model a field cannot disagree about it.
#[derive(Debug, Clone, Serialize)]
pub struct LinkCapsDiff {
    pub link: String,
    pub left_caps: Option<String>,
    pub right_caps: Option<String>,
    pub media_type_differs: bool,
    pub conflicts: Vec<FieldConflict>,
    pub fields_only_left: Vec<String>,
    pub fields_only_right: Vec<String>,
}

impl LinkCapsDiff {
    pub fn is_real_difference(&self) -> bool {
        self.media_type_differs || !self.conflicts.is_empty()
    }

    pub fn is_informational(&self) -> bool {
        !self.fields_only_left.is_empty() || !self.fields_only_right.is_empty()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct FieldConflict {
    pub field: String,
    pub left: String,
    pub right: String,
}

/// The whole comparison of two engines' pipelines.
#[derive(Debug, Clone, Serialize)]
pub struct PipelineDiff {
    pub left_engine: String,
    pub right_engine: String,
    /// elements only one engine built (auto-plug differences, unmatched naming)
    pub elements_only_left: Vec<String>,
    pub elements_only_right: Vec<String>,
    /// links only one engine built, between elements both of them have
    pub links_only_left: Vec<String>,
    pub links_only_right: Vec<String>,
    /// caps comparison for every link both engines built
    pub links: Vec<LinkCapsDiff>,
    pub verdict: Verdict,
}

/// Compare two engines' pipelines: match elements by name, then match links
/// between matched elements, then compare the caps on each matched link.
pub fn diff(
    left_engine: &str,
    left: &PipelineGraph,
    right_engine: &str,
    right: &PipelineGraph,
) -> PipelineDiff {
    let (left_to_right, right_to_left) = match_elements(left, right);

    let unmatched = |graph: &PipelineGraph, map: &[Option<usize>]| -> Vec<String> {
        map.iter()
            .enumerate()
            .filter(|(_, m)| m.is_none())
            .map(|(i, _)| graph.element_name(i).to_string())
            .collect()
    };

    // A link is shared when both its endpoints matched and the other engine has
    // a link between the counterparts.
    let mapped = |link: &Link, map: &[Option<usize>]| -> Option<(usize, usize)> {
        Some((map[link.from]?, map[link.to]?))
    };
    let mut links = Vec::new();
    let mut links_only_left = Vec::new();
    let mut right_matched = vec![false; right.links.len()];
    for link in &left.links {
        let counterpart = mapped(link, &left_to_right).and_then(|(from, to)| {
            right
                .links
                .iter()
                .enumerate()
                .find(|(i, r)| !right_matched[*i] && r.from == from && r.to == to)
        });
        match counterpart {
            Some((index, other)) => {
                right_matched[index] = true;
                links.push(caps_diff(left.link_label(link), &link.caps, &other.caps));
            }
            None => links_only_left.push(left.link_label(link)),
        }
    }
    let links_only_right = right
        .links
        .iter()
        .enumerate()
        .filter(|(i, _)| !right_matched[*i])
        .map(|(_, l)| right.link_label(l))
        .collect::<Vec<_>>();

    let elements_only_left = unmatched(left, &left_to_right);
    let elements_only_right = unmatched(right, &right_to_left);
    let real = links.iter().any(LinkCapsDiff::is_real_difference);
    let informational = !elements_only_left.is_empty()
        || !elements_only_right.is_empty()
        || !links_only_left.is_empty()
        || !links_only_right.is_empty()
        || links.iter().any(LinkCapsDiff::is_informational);
    let verdict = match (real, informational) {
        (true, _) => Verdict::Differs,
        (false, true) => Verdict::Informational,
        (false, false) => Verdict::Match,
    };

    PipelineDiff {
        left_engine: left_engine.to_string(),
        right_engine: right_engine.to_string(),
        elements_only_left,
        elements_only_right,
        links_only_left,
        links_only_right,
        links,
        verdict,
    }
}

/// Greedily pair the two element lists by name, first match wins. Returns the
/// left->right and right->left index maps.
fn match_elements(
    left: &PipelineGraph,
    right: &PipelineGraph,
) -> (Vec<Option<usize>>, Vec<Option<usize>>) {
    let mut left_to_right = vec![None; left.elements.len()];
    let mut right_to_left = vec![None; right.elements.len()];
    for (i, element) in left.elements.iter().enumerate() {
        let found = right.elements.iter().enumerate().find(|(j, candidate)| {
            right_to_left[*j].is_none() && element_names_match(&element.name, &candidate.name)
        });
        if let Some((j, _)) = found {
            left_to_right[i] = Some(j);
            right_to_left[j] = Some(i);
        }
    }
    (left_to_right, right_to_left)
}

fn caps_diff(link: String, left: &Option<Caps>, right: &Option<Caps>) -> LinkCapsDiff {
    let mut diff = LinkCapsDiff {
        link,
        left_caps: left.as_ref().map(Caps::to_string),
        right_caps: right.as_ref().map(Caps::to_string),
        media_type_differs: false,
        conflicts: Vec::new(),
        fields_only_left: Vec::new(),
        fields_only_right: Vec::new(),
    };
    let only = |caps: &Caps| -> Vec<String> {
        caps.fields
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect()
    };
    match (left, right) {
        // One engine reported no caps for this hop: nothing to conflict with,
        // so the other side's fields are informational.
        (Some(left), None) => diff.fields_only_left = only(left),
        (None, Some(right)) => diff.fields_only_right = only(right),
        (None, None) => {}
        (Some(left), Some(right)) => {
            diff.media_type_differs = left.media_type != right.media_type;
            for (field, value) in &left.fields {
                match right.fields.get(field) {
                    Some(other) if other != value => diff.conflicts.push(FieldConflict {
                        field: field.clone(),
                        left: value.clone(),
                        right: other.clone(),
                    }),
                    Some(_) => {}
                    None => diff.fields_only_left.push(format!("{field}={value}")),
                }
            }
            for (field, value) in &right.fields {
                if !left.fields.contains_key(field) {
                    diff.fields_only_right.push(format!("{field}={value}"));
                }
            }
        }
    }
    diff
}

#[cfg(test)]
mod tests {
    use super::*;

    fn graph(names: &[&str], links: &[(usize, usize, &str)]) -> PipelineGraph {
        PipelineGraph {
            elements: names
                .iter()
                .map(|n| Element {
                    name: (*n).to_string(),
                })
                .collect(),
            links: links
                .iter()
                .map(|(from, to, caps)| Link {
                    from: *from,
                    to: *to,
                    caps: Caps::parse(caps),
                })
                .collect(),
        }
    }

    #[test]
    fn caps_parse_normalizes_type_markers_and_padding() {
        let caps = Caps::parse("video/x-raw, width=(int)320 , format=(string)I420").unwrap();
        assert_eq!(caps.media_type, "video/x-raw");
        assert_eq!(caps.fields["width"], "320");
        assert_eq!(caps.fields["format"], "I420");
        // Fields render in a stable order so two engines' strings compare.
        assert_eq!(caps.to_string(), "video/x-raw,format=I420,width=320");
        // A wildcard / empty pad is not caps.
        assert!(Caps::parse("ANY").is_none());
        assert!(Caps::parse("  ").is_none());
    }

    #[test]
    fn element_names_match_across_engine_spellings() {
        assert!(element_names_match("videoconvert0", "VideoConvert"));
        assert!(element_names_match("avdec_h264-0", "AvdecH264"));
        assert!(element_names_match("typefind", "TypeFind"));
        // A trailing codec number is part of the name, not an instance index.
        assert!(!element_names_match("avdec_h264-0", "AvdecH265"));
        assert!(!element_names_match("videoconvert0", "videoscale0"));
    }

    #[test]
    fn identical_pipelines_match() {
        let left = graph(
            &["videotestsrc0", "videoconvert0", "filesink0"],
            &[
                (0, 1, "video/x-raw, format=I420, width=320"),
                (1, 2, "video/x-raw, format=I420, width=320"),
            ],
        );
        let right = graph(
            &["VideoTestSrc", "VideoConvert", "FileSink"],
            &[
                (0, 1, "video/x-raw,width=320,format=I420"),
                (1, 2, "video/x-raw,width=320,format=I420"),
            ],
        );
        let d = diff("gstreamer", &left, "g2g", &right);
        assert_eq!(d.verdict, Verdict::Match);
        assert_eq!(d.links.len(), 2);
        assert!(d.elements_only_left.is_empty() && d.elements_only_right.is_empty());
    }

    #[test]
    fn a_caps_field_conflict_on_a_shared_link_is_a_real_difference() {
        let left = graph(
            &["videotestsrc0", "filesink0"],
            &[(0, 1, "video/x-raw, format=A444_16LE, width=320")],
        );
        let right = graph(
            &["VideoTestSrc", "FileSink"],
            &[(0, 1, "video/x-raw, format=RGBA, width=320")],
        );
        let d = diff("gstreamer", &left, "g2g", &right);
        assert_eq!(d.verdict, Verdict::Differs);
        let link = &d.links[0];
        assert_eq!(link.link, "videotestsrc0 -> filesink0");
        assert_eq!(link.conflicts.len(), 1);
        assert_eq!(link.conflicts[0].field, "format");
        assert_eq!(link.conflicts[0].left, "A444_16LE");
        assert_eq!(link.conflicts[0].right, "RGBA");
        assert!(!link.media_type_differs);
    }

    #[test]
    fn a_media_type_difference_is_a_real_difference() {
        let left = graph(&["a0", "b0"], &[(0, 1, "video/x-h264, width=320")]);
        let right = graph(&["a", "b"], &[(0, 1, "video/x-raw, width=320")]);
        let d = diff("gstreamer", &left, "g2g", &right);
        assert!(d.links[0].media_type_differs);
        assert_eq!(d.verdict, Verdict::Differs);
    }

    #[test]
    fn fields_only_one_engine_models_stay_informational() {
        let left = graph(
            &["a0", "b0"],
            &[(0, 1, "video/x-raw, format=I420, pixel-aspect-ratio=1/1")],
        );
        let right = graph(&["a", "b"], &[(0, 1, "video/x-raw, format=I420")]);
        let d = diff("gstreamer", &left, "g2g", &right);
        assert_eq!(d.verdict, Verdict::Informational);
        assert_eq!(d.links[0].fields_only_left, ["pixel-aspect-ratio=1/1"]);
        assert!(d.links[0].conflicts.is_empty());
    }

    #[test]
    fn extra_autoplugged_elements_are_reported_as_topology_differences() {
        // gst auto-plugs a typefind + parser the g2g graph does not have.
        let left = graph(
            &["filesrc0", "typefind", "h264parse0", "filesink0"],
            &[(0, 1, ""), (1, 2, ""), (2, 3, "video/x-h264")],
        );
        let right = graph(&["FileSrc", "FileSink"], &[(0, 1, "video/x-h264")]);
        let d = diff("gstreamer", &left, "g2g", &right);
        assert_eq!(d.elements_only_left, ["typefind", "h264parse0"]);
        assert!(d.elements_only_right.is_empty());
        // No link survives the pairing: the right graph's only link is between
        // matched elements the left graph does not link directly.
        assert_eq!(
            d.links_only_left,
            [
                "filesrc0 -> typefind",
                "typefind -> h264parse0",
                "h264parse0 -> filesink0"
            ]
        );
        assert_eq!(d.links_only_right, ["FileSrc -> FileSink"]);
        assert_eq!(d.verdict, Verdict::Informational);
    }

    #[test]
    fn an_unnegotiated_hop_is_informational_not_a_conflict() {
        let left = graph(&["a0", "b0"], &[(0, 1, "ANY")]);
        let right = graph(&["a", "b"], &[(0, 1, "video/x-h264, width=320")]);
        let d = diff("gstreamer", &left, "g2g", &right);
        assert_eq!(d.verdict, Verdict::Informational);
        assert!(d.links[0].left_caps.is_none());
        assert_eq!(d.links[0].fields_only_right, ["width=320"]);
    }
}
