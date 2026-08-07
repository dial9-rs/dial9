//! Binary-level CLI integration tests for `dial9 trace-shape`.
//!
//! These tests exercise clap parsing, subcommand dispatch, process exit status,
//! and diagnostic output by invoking the compiled binary via `std::process::Command`.

use std::process::Command;

/// Path to the compiled `dial9` binary under the workspace target directory.
fn dial9_bin() -> std::path::PathBuf {
    // `cargo test` sets CARGO_BIN_EXE_dial9 when there's a [[bin]] named "dial9"
    // in the same package. Fallback to target/debug/dial9.
    if let Ok(bin) = std::env::var("CARGO_BIN_EXE_dial9") {
        return bin.into();
    }
    let mut path = std::env::current_exe().unwrap();
    // Navigate from test binary to workspace target/debug
    path.pop(); // remove test binary name
    if path.ends_with("deps") {
        path.pop();
    }
    path.push("dial9");
    path
}

/// Create a minimal valid trace using the real encoder.
fn make_test_trace() -> Vec<u8> {
    use dial9_trace_format::encoder::Encoder;
    use dial9_trace_format::schema::FieldDef;
    use dial9_trace_format::types::{FieldType, FieldValue};

    let mut enc = Encoder::new();
    let schema = enc
        .register_schema("TestEvent", vec![FieldDef::new("value", FieldType::Varint)])
        .unwrap();
    enc.write_event(&schema, 1_000, &[FieldValue::Varint(42)])
        .unwrap();
    enc.finish()
}

#[test]
fn help_exits_zero() {
    let output = Command::new(dial9_bin())
        .arg("--help")
        .output()
        .expect("failed to run dial9");
    assert!(
        output.status.success(),
        "dial9 --help failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("trace-shape"),
        "help should list trace-shape subcommand"
    );
}

#[test]
fn trace_shape_help_exits_zero() {
    let output = Command::new(dial9_bin())
        .args(["trace-shape", "--help"])
        .output()
        .expect("failed to run dial9");
    assert!(
        output.status.success(),
        "dial9 trace-shape --help failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("extract") || stdout.contains("Extract"));
    assert!(stdout.contains("generate") || stdout.contains("Generate"));
    assert!(stdout.contains("synthesize") || stdout.contains("Synthesize"));
}

#[test]
fn synthesize_help_exits_zero() {
    let output = Command::new(dial9_bin())
        .args(["trace-shape", "synthesize", "--help"])
        .output()
        .expect("failed to run dial9");
    assert!(
        output.status.success(),
        "dial9 trace-shape synthesize --help failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("--repeat"));
}

#[test]
fn synthesize_nonexistent_source_exits_nonzero() {
    let tmp = std::env::temp_dir().join("dial9_cli_test_nonexistent");
    let output = Command::new(dial9_bin())
        .args([
            "trace-shape",
            "synthesize",
            "/nonexistent/path/trace.bin",
            tmp.to_str().unwrap(),
        ])
        .output()
        .expect("failed to run dial9");
    assert!(
        !output.status.success(),
        "should exit nonzero for missing source"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("nonexistent")
            || stderr.contains("No such file")
            || stderr.contains("stat"),
        "stderr should reference the missing file: {stderr}"
    );
}

#[test]
fn generate_nonexistent_shape_exits_nonzero() {
    let tmp = std::env::temp_dir().join("dial9_cli_test_gen_nonexistent");
    let output = Command::new(dial9_bin())
        .args([
            "trace-shape",
            "generate",
            "/nonexistent/path/shape.json",
            tmp.to_str().unwrap(),
        ])
        .output()
        .expect("failed to run dial9");
    assert!(
        !output.status.success(),
        "should exit nonzero for missing shape"
    );
}

#[test]
fn synthesize_roundtrip_exits_zero() {
    let tmp_dir = std::env::temp_dir().join("dial9_cli_test_synth_rt");
    std::fs::create_dir_all(&tmp_dir).unwrap();
    let source = tmp_dir.join("source.bin");
    let output_path = tmp_dir.join("synthetic.bin");

    std::fs::write(&source, make_test_trace()).unwrap();

    let output = Command::new(dial9_bin())
        .args([
            "trace-shape",
            "synthesize",
            source.to_str().unwrap(),
            output_path.to_str().unwrap(),
        ])
        .output()
        .expect("failed to run dial9");
    assert!(
        output.status.success(),
        "synthesize failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output_path.exists(), "output file should be created");
    assert!(
        std::fs::metadata(&output_path).unwrap().len() > 5,
        "output should contain more than just a header"
    );

    // Cleanup
    let _ = std::fs::remove_dir_all(&tmp_dir);
}

#[test]
fn extract_then_generate_roundtrip() {
    let tmp_dir = std::env::temp_dir().join("dial9_cli_test_extract_gen");
    std::fs::create_dir_all(&tmp_dir).unwrap();
    let source = tmp_dir.join("source.bin");
    let shape = tmp_dir.join("shape.json");
    let generated = tmp_dir.join("generated.bin");

    std::fs::write(&source, make_test_trace()).unwrap();

    // Extract
    let output = Command::new(dial9_bin())
        .args([
            "trace-shape",
            "extract",
            source.to_str().unwrap(),
            shape.to_str().unwrap(),
        ])
        .output()
        .expect("failed to run dial9");
    assert!(
        output.status.success(),
        "extract failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(shape.exists());

    // Generate
    let output = Command::new(dial9_bin())
        .args([
            "trace-shape",
            "generate",
            shape.to_str().unwrap(),
            generated.to_str().unwrap(),
        ])
        .output()
        .expect("failed to run dial9");
    assert!(
        output.status.success(),
        "generate failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(generated.exists());

    // Cleanup
    let _ = std::fs::remove_dir_all(&tmp_dir);
}

#[test]
fn synthesize_gzip_input_exits_zero() {
    use std::io::Write;
    let tmp_dir = std::env::temp_dir().join("dial9_cli_test_gz");
    std::fs::create_dir_all(&tmp_dir).unwrap();
    let source = tmp_dir.join("source.bin.gz");
    let output_path = tmp_dir.join("synthetic.bin");

    let trace = make_test_trace();
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(&trace).unwrap();
    let compressed = gz.finish().unwrap();
    std::fs::write(&source, &compressed).unwrap();

    let output = Command::new(dial9_bin())
        .args([
            "trace-shape",
            "synthesize",
            source.to_str().unwrap(),
            output_path.to_str().unwrap(),
        ])
        .output()
        .expect("failed to run dial9");
    assert!(
        output.status.success(),
        "synthesize with gzip input failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output_path.exists());

    // Cleanup
    let _ = std::fs::remove_dir_all(&tmp_dir);
}

#[test]
fn invalid_subcommand_exits_nonzero() {
    let output = Command::new(dial9_bin())
        .args(["trace-shape", "invalid-subcommand"])
        .output()
        .expect("failed to run dial9");
    assert!(
        !output.status.success(),
        "invalid subcommand should exit nonzero"
    );
}

#[test]
fn repeat_zero_exits_nonzero() {
    let tmp_dir = std::env::temp_dir().join("dial9_cli_test_repeat0");
    std::fs::create_dir_all(&tmp_dir).unwrap();
    let source = tmp_dir.join("source.bin");
    let output_path = tmp_dir.join("synthetic.bin");
    std::fs::write(&source, make_test_trace()).unwrap();

    let output = Command::new(dial9_bin())
        .args([
            "trace-shape",
            "synthesize",
            source.to_str().unwrap(),
            output_path.to_str().unwrap(),
            "--repeat",
            "0",
        ])
        .output()
        .expect("failed to run dial9");
    assert!(!output.status.success(), "repeat=0 should exit nonzero");

    // Cleanup
    let _ = std::fs::remove_dir_all(&tmp_dir);
}
