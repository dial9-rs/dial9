//! Tests for `dial9 report serve <dir>` — a thin static-file server for
//! agent-generated report folders. Reports embed iframes that fetch trace
//! files via HTTP, which doesn't work over `file://` due to browser
//! restrictions. This server fills that gap.

use assert2::check;
use dial9_viewer::report_serve_router;

async fn start_server(dir: std::path::PathBuf) -> String {
    let app = report_serve_router(&dir);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn serves_html_files_from_report_dir() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("report.html"), b"<h1>hello</h1>").unwrap();
    let base = start_server(tmp.path().to_path_buf()).await;

    let resp = reqwest::get(format!("{base}/report.html")).await.unwrap();
    check!(resp.status().as_u16() == 200);
    let body = resp.text().await.unwrap();
    check!(body == "<h1>hello</h1>");
}

#[tokio::test]
async fn serves_binary_assets() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("traces")).unwrap();
    let bytes: Vec<u8> = (0u8..=255).collect();
    std::fs::write(tmp.path().join("traces/test.bin"), &bytes).unwrap();
    let base = start_server(tmp.path().to_path_buf()).await;

    let resp = reqwest::get(format!("{base}/traces/test.bin"))
        .await
        .unwrap();
    check!(resp.status().as_u16() == 200);
    let got = resp.bytes().await.unwrap();
    check!(got.as_ref() == bytes.as_slice());
}

#[tokio::test]
async fn serves_index_at_root() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("index.html"), b"<h1>root</h1>").unwrap();
    let base = start_server(tmp.path().to_path_buf()).await;

    let resp = reqwest::get(format!("{base}/")).await.unwrap();
    check!(resp.status().as_u16() == 200);
    let body = resp.text().await.unwrap();
    check!(body.contains("root"));
}

#[tokio::test]
async fn returns_404_for_missing() {
    let tmp = tempfile::tempdir().unwrap();
    let base = start_server(tmp.path().to_path_buf()).await;

    let resp = reqwest::get(format!("{base}/does-not-exist.html"))
        .await
        .unwrap();
    check!(resp.status().as_u16() == 404);
}
