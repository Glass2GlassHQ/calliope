use std::time::Duration;

use futures::StreamExt;
use serde_json::Value;
use tokio_tungstenite::tungstenite::Message;

const OBSERVE_HOST: &str = "127.0.0.1";
const SINK_ROLE: &str = "sink";
const TELEMETRY_TYPE: &str = "telemetry";
// g2g-launch binds the port only once the graph is built
const RECONNECT_DELAY: Duration = Duration::from_millis(250);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TelemetryReading {
    // packets into every sink since the process started, frames plus a few control packets
    pub sink_packets: u64,
    // median process() time of the elements feeding a sink, the decoder in `... ! dec ! fakesink`
    pub sink_feeder_p50_ns: Option<u64>,
}

pub fn observe_args(launch_line: &str, port: u16) -> Vec<String> {
    vec![
        "-q".to_string(),
        "--observe".to_string(),
        port.to_string(),
        launch_line.to_string(),
    ]
}

pub fn parse_telemetry(text: &str) -> Option<TelemetryReading> {
    let message: Value = serde_json::from_str(text).ok()?;
    if message["type"] != TELEMETRY_TYPE {
        return None;
    }
    let nodes = message["nodes"].as_array()?;
    let edges = message["edges"].as_array()?;
    let sink_ids: Vec<u64> = nodes
        .iter()
        .filter(|node| node["role"] == SINK_ROLE)
        .filter_map(|node| node["id"].as_u64())
        .collect();
    let sink_edges: Vec<&Value> = edges
        .iter()
        .filter(|edge| edge["to"].as_u64().is_some_and(|to| sink_ids.contains(&to)))
        .collect();
    let sink_packets = sink_edges
        .iter()
        .filter_map(|edge| edge["packets"].as_u64())
        .sum();
    let feeder_ids: Vec<u64> = sink_edges
        .iter()
        .filter_map(|edge| edge["from"].as_u64())
        .collect();
    let sink_feeder_p50_ns = nodes
        .iter()
        .filter(|node| {
            node["id"]
                .as_u64()
                .is_some_and(|id| feeder_ids.contains(&id))
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
        sink_feeder_p50_ns,
    })
}

// never returns: reconnects whenever the run restarts its server, the caller aborts it
pub async fn follow_telemetry(port: u16, mut on_reading: impl FnMut(TelemetryReading)) {
    let url = format!("ws://{OBSERVE_HOST}:{port}/");
    loop {
        if let Ok((mut socket, _)) = tokio_tungstenite::connect_async(url.as_str()).await {
            while let Some(Ok(message)) = socket.next().await {
                let Message::Text(text) = message else {
                    continue;
                };
                if let Some(reading) = parse_telemetry(text.as_str()) {
                    on_reading(reading);
                }
            }
        }
        tokio::time::sleep(RECONNECT_DELAY).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // shape from g2g's dashboard wire protocol: source -> decoder -> fakesink
    const TELEMETRY: &str = r#"{"type":"telemetry","uptime_ns":5000000000,
        "nodes":[
          {"id":0,"name":"RtspSrc0","role":"source","proc":null},
          {"id":1,"name":"FfmpegDec0","role":"transform","proc":{"count":120,"p50_ns":2500000,"p99_ns":9000000}},
          {"id":2,"name":"FakeSink0","role":"sink","proc":{"count":118,"p50_ns":1000,"p99_ns":2000}}],
        "edges":[
          {"from":0,"to":1,"packets":125,"bytes":900000},
          {"from":1,"to":2,"packets":121,"bytes":0}],
        "journey":null}"#;

    #[test]
    fn reads_sink_packets_and_decoder_latency() {
        let reading = parse_telemetry(TELEMETRY).expect("telemetry message");
        assert_eq!(reading.sink_packets, 121);
        assert_eq!(reading.sink_feeder_p50_ns, Some(2_500_000));
    }

    #[test]
    fn ignores_events_and_untimed_feeders() {
        assert_eq!(parse_telemetry(r#"{"type":"event","kind":"eos"}"#), None);
        let untimed = TELEMETRY.replace(r#""count":120"#, r#""count":0"#);
        let reading = parse_telemetry(&untimed).expect("telemetry message");
        assert_eq!(reading.sink_feeder_p50_ns, None);
    }
}
