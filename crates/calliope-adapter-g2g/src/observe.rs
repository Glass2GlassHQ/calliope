use std::time::Duration;

use futures::StreamExt;
use serde_json::Value;
use tokio_tungstenite::tungstenite::Message;

const OBSERVE_HOST: &str = "127.0.0.1";
const SINK_ROLE: &str = "sink";
// g2g names its decoder elements after the type: FfmpegVideoDec, Dav1dDec, NvDec, MfDecode
const DECODER_NAME_PART: &str = "Dec";
const TELEMETRY_TYPE: &str = "telemetry";
const EVENT_TYPE: &str = "event";
const CHECKSUM_KIND: &str = "checksum";
const STREAM_START_KIND: &str = "stream-start";
// g2g-launch binds the port only once the graph is built
const RECONNECT_DELAY: Duration = Duration::from_millis(250);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TelemetryReading {
    // packets into every sink since the process started, frames plus a few control packets
    pub sink_packets: u64,
    // median process() time of the slowest decoder element
    pub decoder_p50_ns: Option<u64>,
}

// one buffer a checksumsink digested
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChecksumReading {
    // the sink's instance name, its launch `name=` when it has one
    pub element: String,
    pub pts_ns: u64,
    pub digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObserveMessage {
    Telemetry(TelemetryReading),
    Checksum(ChecksumReading),
    // a source is about to send its first packet, one per source each time a graph starts
    StreamStart,
}

pub fn observe_args(launch_line: &str, port: u16) -> Vec<String> {
    vec![
        "-q".to_string(),
        "--observe".to_string(),
        port.to_string(),
        launch_line.to_string(),
    ]
}

pub fn parse_observe_message(text: &str) -> Option<ObserveMessage> {
    let message: Value = serde_json::from_str(text).ok()?;
    if message["type"] == TELEMETRY_TYPE {
        return parse_telemetry(&message).map(ObserveMessage::Telemetry);
    }
    if message["type"] == EVENT_TYPE && message["kind"] == STREAM_START_KIND {
        return Some(ObserveMessage::StreamStart);
    }
    if message["type"] == EVENT_TYPE && message["kind"] == CHECKSUM_KIND {
        return Some(ObserveMessage::Checksum(ChecksumReading {
            element: message["element"].as_str()?.to_string(),
            pts_ns: message["pts_ns"].as_u64()?,
            digest: message["digest"].as_str()?.to_string(),
        }));
    }
    None
}

fn parse_telemetry(message: &Value) -> Option<TelemetryReading> {
    let nodes = message["nodes"].as_array()?;
    let edges = message["edges"].as_array()?;
    let sink_ids: Vec<u64> = nodes
        .iter()
        .filter(|node| node["role"] == SINK_ROLE)
        .filter_map(|node| node["id"].as_u64())
        .collect();
    let sink_packets = edges
        .iter()
        .filter(|edge| edge["to"].as_u64().is_some_and(|to| sink_ids.contains(&to)))
        .filter_map(|edge| edge["packets"].as_u64())
        .sum();
    let decoder_p50_ns = nodes
        .iter()
        .filter(|node| {
            node["name"]
                .as_str()
                .is_some_and(|name| name.contains(DECODER_NAME_PART))
        })
        .filter(|node| {
            node["proc"]["count"]
                .as_u64()
                .is_some_and(|count| count > 0)
        })
        .filter_map(|node| node["proc"]["p50_ns"].as_u64())
        .max();
    Some(TelemetryReading {
        sink_packets,
        decoder_p50_ns,
    })
}

// never returns: reconnects whenever the run restarts its server, the caller aborts it
pub async fn follow_observe(port: u16, mut on_message: impl FnMut(ObserveMessage)) {
    let url = format!("ws://{OBSERVE_HOST}:{port}/");
    loop {
        if let Ok((mut socket, _)) = tokio_tungstenite::connect_async(url.as_str()).await {
            while let Some(Ok(message)) = socket.next().await {
                let Message::Text(text) = message else {
                    continue;
                };
                if let Some(message) = parse_observe_message(text.as_str()) {
                    on_message(message);
                }
            }
        }
        tokio::time::sleep(RECONNECT_DELAY).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // shape from g2g's dashboard wire protocol: source -> decoder -> clocksync -> fakesink
    const TELEMETRY: &str = r#"{"type":"telemetry","uptime_ns":5000000000,
        "nodes":[
          {"id":0,"name":"RtspSrc0","role":"source","proc":null},
          {"id":1,"name":"FfmpegDec0","role":"transform","proc":{"count":120,"p50_ns":2500000,"p99_ns":9000000}},
          {"id":2,"name":"ClockSync0","role":"transform","proc":{"count":119,"p50_ns":33000000,"p99_ns":34000000}},
          {"id":3,"name":"FakeSink0","role":"sink","proc":{"count":118,"p50_ns":1000,"p99_ns":2000}}],
        "edges":[
          {"from":0,"to":1,"packets":125,"bytes":900000},
          {"from":1,"to":2,"packets":122,"bytes":0},
          {"from":2,"to":3,"packets":121,"bytes":0}],
        "journey":null}"#;

    fn telemetry(text: &str) -> TelemetryReading {
        match parse_observe_message(text) {
            Some(ObserveMessage::Telemetry(reading)) => reading,
            other => panic!("expected telemetry, got {other:?}"),
        }
    }

    #[test]
    fn reads_sink_packets_and_decoder_latency() {
        let reading = telemetry(TELEMETRY);
        assert_eq!(reading.sink_packets, 121);
        assert_eq!(reading.decoder_p50_ns, Some(2_500_000));
    }

    #[test]
    fn ignores_other_events_and_untimed_decoders() {
        assert_eq!(
            parse_observe_message(r#"{"type":"event","kind":"eos"}"#),
            None
        );
        let untimed = TELEMETRY.replace(r#""count":120"#, r#""count":0"#);
        assert_eq!(telemetry(&untimed).decoder_p50_ns, None);
    }

    // recorded from `g2g-launch -q --observe <port> filesrc location=clip.ts ! tsdemux ! h264parse ! avdec_h264 ! clocksync ! checksumsink name=video`
    const CHECKSUM_EVENT: &str = r#"{"digest":"78fa86525f962ffea7bee0c80aaae2939d83c2ec","element":"video","kind":"checksum","pts_ns":2700000000,"type":"event"}"#;

    #[test]
    fn reads_stream_start_events() {
        assert_eq!(
            parse_observe_message(r#"{"kind":"stream-start","type":"event"}"#),
            Some(ObserveMessage::StreamStart)
        );
    }

    #[test]
    fn reads_checksum_events() {
        assert_eq!(
            parse_observe_message(CHECKSUM_EVENT),
            Some(ObserveMessage::Checksum(ChecksumReading {
                element: "video".into(),
                pts_ns: 2_700_000_000,
                digest: "78fa86525f962ffea7bee0c80aaae2939d83c2ec".into(),
            }))
        );
    }
}
