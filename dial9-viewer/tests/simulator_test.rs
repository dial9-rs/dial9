use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use dial9_viewer::simulator::{SimulatorConfig, SimulatorTraceMode, build_simulator_app};
use serde_json::Value;
use tower::ServiceExt;

async fn get(app: &Router, uri: &str) -> (StatusCode, Vec<u8>) {
    let response = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), 64 * 1024 * 1024)
        .await
        .unwrap()
        .to_vec();
    (status, body)
}

fn json(body: &[u8]) -> Value {
    serde_json::from_slice(body).unwrap()
}

fn final_sse_data(body: &[u8]) -> &str {
    let text = std::str::from_utf8(body).unwrap();
    text.lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .next_back()
        .unwrap_or_else(|| panic!("SSE response contained no data event: {text}"))
}

// Flamegraph trees can exceed serde_json's default recursion limit. These
// generated responses contain each numeric field once, so inspect only the
// shallow counters needed by this integration test.
fn response_u64(json: &str, field: &str) -> u64 {
    let needle = format!("\"{field}\":");
    let start = json.find(&needle).unwrap() + needle.len();
    let value = &json[start..];
    let end = value
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(value.len());
    value[..end].parse().unwrap()
}

#[tokio::test]
async fn demo_simulator_drives_browser_objects_and_aggregate_views_without_s3() {
    let app = build_simulator_app(
        SimulatorConfig::builder()
            .trace_mode(SimulatorTraceMode::DemoReplay)
            .hosts(1)
            .build(),
    )
    .await
    .unwrap();

    let (status, body) = get(&app, "/api/config").await;
    assert_eq!(status, StatusCode::OK);
    let config = json(&body);
    assert_eq!(config["default_bucket"], "dial9-simulator");
    assert_eq!(config["default_prefix"], "traces");
    assert_eq!(config["source_layout"], "time-partitioned");
    assert_eq!(config["supports_byo_credentials"], false);
    assert_eq!(config["aggregation_enabled"], true);

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let (status, body) = get(
        &app,
        &format!(
            "/api/services?bucket=dial9-simulator&from={}&to={}",
            now - 180,
            now + 60
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let services = json(&body);
    assert_eq!(
        services["services"],
        serde_json::json!(["simulated-service"])
    );
    assert!(services["service_metadata"][0].get("host_count").is_none());
    assert!(services["service_metadata"][0]["layout_hint"].is_string());

    let (status, body) = get(
        &app,
        &format!(
            "/api/browse?bucket=dial9-simulator&service=simulated-service&from={}&to={}",
            now - 180,
            now + 60
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let browse = json(&body);
    let objects = browse["objects"].as_array().unwrap();
    assert!(
        objects.len() >= 4,
        "the requested four-minute window should have virtual coverage"
    );
    let key = objects[0]["key"].as_str().unwrap();
    assert!(key.starts_with("traces/version=1/date="));
    assert!(key.contains("/service=simulated-service/time="));
    assert!(key.contains("/instance=host-001/"));

    let (status, object) = get(
        &app,
        &format!(
            "/api/object?bucket=dial9-simulator&key={}",
            urlencoding::encode(key)
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        object.starts_with(&[0x1f, 0x8b]),
        "object should remain gzip"
    );

    let epoch_secs = key
        .rsplit('/')
        .next()
        .unwrap()
        .split('-')
        .next()
        .unwrap()
        .parse::<i64>()
        .unwrap();

    // viewer.html resolves the compact selection scope by re-listing with the
    // key-derived prefix, even when that prefix is also the server default.
    let (status, body) = get(
        &app,
        &format!(
            "/api/browse?bucket=dial9-simulator&prefix=traces\
             &service=simulated-service&from={epoch_secs}&to={}",
            epoch_secs + 60
        )
        .replace(' ', ""),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let scope_browse = json(&body);
    assert!(
        scope_browse["objects"]
            .as_array()
            .unwrap()
            .iter()
            .any(|object| object["key"] == key),
        "scope re-list should return the selected virtual trace"
    );

    let scope = format!(
        "bucket=dial9-simulator&prefix=traces&service=simulated-service&host=host-001\
         &start_ns={}&end_ns={}&max_files=1",
        epoch_secs * 1_000_000_000,
        (epoch_secs + 60) * 1_000_000_000
    )
    .replace(' ', "");

    let (status, body) = get(&app, &format!("/api/flamegraph?{scope}")).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let flamegraph = final_sse_data(&body);
    assert!(response_u64(flamegraph, "total_samples") > 0);
    assert!(
        flamegraph.contains("\"children\":[{"),
        "flamegraph tree should be non-trivial"
    );
    assert_eq!(response_u64(flamegraph, "fold_errors"), 0);

    let (status, body) = get(&app, &format!("/api/span-stats?{scope}")).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let spans: Value = serde_json::from_str(final_sse_data(&body)).unwrap();
    assert!(
        !spans["span_types"].as_array().unwrap().is_empty(),
        "demo replay should expose aggregated spans"
    );

    let (status, body) = get(&app, &format!("/api/tokio-stats?{scope}")).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let tokio_stats: Value = serde_json::from_str(final_sse_data(&body)).unwrap();
    assert!(tokio_stats["total_polls"].as_u64().unwrap() > 0);
}
