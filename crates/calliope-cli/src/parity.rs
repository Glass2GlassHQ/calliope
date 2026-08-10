//! `gst-parity`: run one gst-launch line through real GStreamer and through
//! g2g, then diff what each of them built. GStreamer is introspected from the
//! `GST_DEBUG_DUMP_DOT_DIR` graph dump of the run itself; g2g from
//! `g2g-launch --run-json`, which runs the line and reports the caps each edge
//! carried, so both readings are post-data. g2g is run separately for its
//! artifact.
//!
//! When the line ends in a `filesink location=`, each engine is pointed at its
//! own file under the workdir and the two artifacts are hashed and compared.
//!
//! The two engines name elements after different things (gst after the factory,
//! g2g after the Rust type), so the pairing is fed `g2g-inspect --gst-map`; the
//! elements it cannot pair share no link and so compare nothing.
//!
//! A g2g-launch too old for `--run-json` falls back to `--validate-json`, whose
//! caps are the solver's, chosen before the run. Then a pipeline whose geometry
//! only arrives with the stream (a demuxed file) shows g2g's placeholder against
//! gst's real geometry, which the summary labels rather than tries to reconcile.

use std::path::Path;

use anyhow::{Context, Result, bail};
use calliope_core::engine::{binary, probe_first_line};
use calliope_core::pipeline_diff::{self, Verdict};
use calliope_core::report::{ParityEngine, ParityReport};
use calliope_core::runner::whole_file_md5;

/// Compare one launch line across both engines.
pub fn run(pipeline: &str, workdir: &Path) -> Result<ParityReport> {
    let tokens: Vec<String> = pipeline.split_whitespace().map(str::to_string).collect();
    if tokens.is_empty() {
        bail!("empty pipeline");
    }
    let gst_version = probe_first_line(
        "gstreamer",
        &binary("CALLIOPE_GST_LAUNCH", "gst-launch-1.0"),
        &["--version"],
    )
    .context("gst-launch-1.0 not available")?
    .version;
    let g2g_version = probe_first_line(
        "g2g",
        &binary("CALLIOPE_G2G_LAUNCH", "g2g-launch"),
        &["--help"],
    )
    .context("g2g-launch not available (point CALLIOPE_G2G_LAUNCH at a build)")?
    .version;
    if !calliope_adapter_g2g::validate::supports_validate_json() {
        bail!(
            "this g2g-launch has no --validate-json; build one with the tooling-json feature and \
             point CALLIOPE_G2G_LAUNCH at it"
        );
    }

    let gst_dir = workdir.join("gstreamer");
    let g2g_dir = workdir.join("g2g");
    std::fs::create_dir_all(&gst_dir)?;
    std::fs::create_dir_all(&g2g_dir)?;
    let gst_artifact = gst_dir.join("out.bin");
    let g2g_artifact = g2g_dir.join("out.bin");
    let gst_args = retarget_filesink(&tokens, &gst_artifact);
    let g2g_args = retarget_filesink(&tokens, &g2g_artifact);
    let writes_artifact = gst_args.is_some() && g2g_args.is_some();
    let gst_args = gst_args.unwrap_or_else(|| tokens.clone());
    let g2g_args = g2g_args.unwrap_or_else(|| tokens.clone());

    let dot_dir = gst_dir.join("dot");
    let gst_run = calliope_adapter_gst::dot::run_with_dot_dump(&gst_args, &dot_dir)?;
    let gst_graph = calliope_adapter_gst::dot::graph_from_dump(&dot_dir)
        .context("reading the gstreamer graph dump")?;

    let (g2g_graph, g2g_caps_source) = if calliope_adapter_g2g::validate::supports_run_json() {
        (
            calliope_adapter_g2g::validate::observed_graph(&g2g_args)
                .context("running the pipeline through g2g")?,
            "g2g-launch --run-json (caps observed while running)",
        )
    } else {
        (
            calliope_adapter_g2g::validate::negotiated_graph(&g2g_args)
                .context("negotiating the pipeline with g2g")?,
            "g2g-launch --validate-json (negotiation before the run)",
        )
    };
    let g2g_run = run_g2g(&g2g_args)?;

    let synonyms = calliope_adapter_g2g::validate::name_synonyms();
    let diff = pipeline_diff::diff("gstreamer", &gst_graph, "g2g", &g2g_graph, &synonyms);
    let artifact = |path: &Path| -> (Option<String>, Option<u64>) {
        if !writes_artifact {
            return (None, None);
        }
        (
            whole_file_md5(path).ok(),
            std::fs::metadata(path).map(|m| m.len()).ok(),
        )
    };
    let (gst_md5, gst_len) = artifact(&gst_artifact);
    let (g2g_md5, g2g_len) = artifact(&g2g_artifact);
    let artifact_matched = match (&gst_md5, &g2g_md5) {
        (Some(left), Some(right)) => Some(left == right),
        _ => None,
    };

    Ok(ParityReport {
        pipeline: pipeline.to_string(),
        left: ParityEngine {
            engine: "gstreamer".into(),
            version: gst_version,
            graph: gst_graph,
            caps_source: "graph dump leaving PLAYING (after data flowed)".into(),
            ran_ok: gst_run.status.success(),
            artifact_md5: gst_md5,
            artifact_len: gst_len,
        },
        right: ParityEngine {
            engine: "g2g".into(),
            version: g2g_version,
            graph: g2g_graph,
            caps_source: g2g_caps_source.into(),
            ran_ok: g2g_run.status.success(),
            artifact_md5: g2g_md5,
            artifact_len: g2g_len,
        },
        diff,
        artifact_matched,
    })
}

fn run_g2g(pipeline_args: &[String]) -> Result<std::process::Output> {
    let program = binary("CALLIOPE_G2G_LAUNCH", "g2g-launch");
    std::process::Command::new(&program)
        .arg("-q")
        .args(pipeline_args)
        .output()
        .with_context(|| format!("running {program}"))
}

/// Point the line's trailing `filesink location=` at `path`, so each engine
/// writes its own artifact. None when the line does not end in a filesink with
/// a location: then there is no artifact to compare.
fn retarget_filesink(tokens: &[String], path: &Path) -> Option<Vec<String>> {
    let last_element = tokens.iter().rposition(|t| t == "!")? + 1;
    if tokens.get(last_element)? != "filesink" {
        return None;
    }
    let location = tokens[last_element..]
        .iter()
        .position(|t| t.starts_with("location="))?
        + last_element;
    let mut retargeted = tokens.to_vec();
    retargeted[location] = format!("location={}", path.display());
    Some(retargeted)
}

/// Print the comparison: the two element sets, then every shared link's caps,
/// then the artifact and the verdict.
pub fn print_summary(report: &ParityReport) {
    let diff = &report.diff;
    println!("pipeline: {}", report.pipeline);
    // The engine versions ride the JSON report; g2g-launch has no --version, so
    // its probe line is the whole usage banner and reads as noise here.
    for engine in [&report.left, &report.right] {
        println!(
            "  {:<10} {} elements, {} links{}",
            engine.engine,
            engine.graph.elements.len(),
            engine.graph.links.len(),
            if engine.ran_ok { "" } else { "  (run failed)" }
        );
    }
    let list = |label: &str, items: &[String]| {
        if !items.is_empty() {
            println!("{label}: {}", items.join(", "));
        }
    };
    list("elements only in gstreamer", &diff.elements_only_left);
    list("elements only in g2g", &diff.elements_only_right);
    list("links only in gstreamer", &diff.links_only_left);
    list("links only in g2g", &diff.links_only_right);

    if !diff.links.is_empty() {
        println!("caps per shared link:");
        println!(
            "  (gstreamer: {}; g2g: {})",
            report.left.caps_source, report.right.caps_source
        );
    }
    for link in &diff.links {
        println!("  {}", link.link);
        println!(
            "    gstreamer {}",
            link.left_caps.as_deref().unwrap_or("(none)")
        );
        println!(
            "    g2g       {}",
            link.right_caps.as_deref().unwrap_or("(none)")
        );
        if link.media_type_differs {
            println!("    CONFLICT  media type");
        }
        for conflict in &link.conflicts {
            println!(
                "    CONFLICT  {}: gstreamer={} g2g={}",
                conflict.field, conflict.left, conflict.right
            );
        }
        list("    only gstreamer", &link.fields_only_left);
        list("    only g2g", &link.fields_only_right);
    }

    if let Some(matched) = report.artifact_matched {
        println!(
            "artifact: gstreamer {} ({} bytes), g2g {} ({} bytes) -> {}",
            report.left.artifact_md5.as_deref().unwrap_or("-"),
            report.left.artifact_len.unwrap_or(0),
            report.right.artifact_md5.as_deref().unwrap_or("-"),
            report.right.artifact_len.unwrap_or(0),
            if matched { "identical" } else { "DIFFERENT" }
        );
    }
    let verdict = match diff.verdict {
        Verdict::Match => "match",
        Verdict::Informational => "informational differences only",
        Verdict::Differs => "differs",
    };
    println!("verdict: {verdict} (approximate: only caps conflicts on shared links count)");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens(line: &str) -> Vec<String> {
        line.split_whitespace().map(str::to_string).collect()
    }

    #[test]
    fn retargets_only_a_trailing_filesink_location() {
        let out = Path::new("/w/out.bin");
        let retargeted = retarget_filesink(
            &tokens("videotestsrc num-buffers=5 ! filesink location=out.raw sync=false"),
            out,
        )
        .expect("trailing filesink is retargeted");
        assert_eq!(retargeted.last().unwrap(), "sync=false");
        assert!(retargeted.contains(&"location=/w/out.bin".to_string()));
        // The earlier filesrc location is untouched.
        let retargeted = retarget_filesink(
            &tokens("filesrc location=in.h264 ! filesink location=o"),
            out,
        )
        .unwrap();
        assert_eq!(retargeted[1], "location=in.h264");
        assert_eq!(retargeted[4], "location=/w/out.bin");
        // Nothing to compare when the line does not end in a filesink.
        assert!(retarget_filesink(&tokens("videotestsrc ! fakesink"), out).is_none());
        assert!(retarget_filesink(&tokens("videotestsrc ! filesink"), out).is_none());
    }

    fn binary_missing(env_var: &str, default: &str, arg: &str) -> bool {
        std::process::Command::new(binary(env_var, default))
            .arg(arg)
            .output()
            .is_err()
    }

    #[test]
    fn diffs_a_trivial_line_through_both_engines() {
        if binary_missing("CALLIOPE_GST_LAUNCH", "gst-launch-1.0", "--version") {
            eprintln!("skipping: gst-launch-1.0 not installed");
            return;
        }
        if !calliope_adapter_g2g::validate::supports_validate_json() {
            eprintln!("skipping: no g2g-launch with --validate-json (set CALLIOPE_G2G_LAUNCH)");
            return;
        }
        let workdir = std::env::temp_dir().join("calliope-gst-parity-test");
        let _ = std::fs::remove_dir_all(&workdir);
        let report = run(
            "videotestsrc num-buffers=30 ! videoconvert ! filesink location=out.raw",
            &workdir,
        )
        .expect("parity run");

        assert!(report.left.ran_ok, "gstreamer run failed");
        assert!(report.right.ran_ok, "g2g run failed");
        // Both engines built the same three elements and two links, so the
        // pairing is complete and every link's caps got compared.
        assert_eq!(report.right.graph.elements.len(), 3);
        assert!(report.diff.elements_only_left.is_empty());
        assert!(report.diff.elements_only_right.is_empty());
        assert_eq!(report.diff.links.len(), 2);
        assert!(report.diff.links_only_left.is_empty());
        assert!(report.diff.links_only_right.is_empty());
        // Each shared link carries both engines' negotiated caps.
        for link in &report.diff.links {
            assert!(link.left_caps.is_some(), "{link:?}");
            assert!(link.right_caps.is_some(), "{link:?}");
        }
        // Both engines wrote an artifact that got hashed.
        assert!(report.left.artifact_md5.is_some());
        assert!(report.right.artifact_md5.is_some());
        assert!(report.artifact_matched.is_some());
    }

    /// A stream whose geometry only arrives with the data. g2g negotiates a
    /// 16x16 placeholder for the parser's output and refines it once it reads an
    /// SPS, so the run dump is what makes this line comparable at all: both
    /// engines have to report the clip's real 176x144 on that link.
    ///
    /// `name=parse` on the parser so the pairing holds without a `g2g-inspect`
    /// to read the synonym table from; the engines otherwise name it
    /// differently (`h264parse0` against g2g's `NalParse0`).
    #[test]
    fn a_refined_caps_line_agrees_on_the_real_geometry() {
        if binary_missing("CALLIOPE_GST_LAUNCH", "gst-launch-1.0", "--version") {
            eprintln!("skipping: gst-launch-1.0 not installed");
            return;
        }
        if !calliope_adapter_g2g::validate::supports_run_json() {
            eprintln!("skipping: no g2g-launch with --run-json (set CALLIOPE_G2G_LAUNCH)");
            return;
        }
        let clip = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../local-corpus/testsrc-176x144.h264")
            .canonicalize();
        let Ok(clip) = clip else {
            eprintln!("skipping: local-corpus missing (run tools/gen-local-corpus.sh)");
            return;
        };
        let workdir = std::env::temp_dir().join("calliope-gst-parity-refined-test");
        let _ = std::fs::remove_dir_all(&workdir);
        let report = run(
            &format!(
                "filesrc location={} ! h264parse name=parse ! fakesink",
                clip.display()
            ),
            &workdir,
        )
        .expect("parity run");

        assert!(report.left.ran_ok, "gstreamer run failed");
        assert!(report.right.ran_ok, "g2g run failed");
        assert!(
            report.right.caps_source.contains("--run-json"),
            "g2g's caps should come from the run, got {}",
            report.right.caps_source
        );

        let parsed = report
            .diff
            .links
            .iter()
            .find(|l| l.link.starts_with("parse ->"))
            .expect("the parser's output link is shared");
        let g2g_caps = parsed.right_caps.as_deref().expect("g2g reported caps");
        assert!(
            g2g_caps.contains("width=176") && g2g_caps.contains("height=144"),
            "the run dump carries the clip's geometry, not the negotiation \
             placeholder, got {g2g_caps}"
        );
        // The geometry both engines model agrees, so nothing on this link is a
        // real difference; gst simply models more fields.
        assert!(parsed.conflicts.is_empty(), "{parsed:?}");
        assert!(!parsed.media_type_differs);
    }
}
