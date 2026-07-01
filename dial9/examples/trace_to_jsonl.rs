//! Convert a dial9 binary trace to JSONL (one JSON object per line).
//!
//! Usage:
//!   cargo run --example trace_to_jsonl --features analysis -- <input.bin> [output.jsonl]
//!
//! If output is omitted, writes to stdout.

use dial9_trace_format::decoder::Decoder;
use std::io::{BufWriter, Write};

fn main() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: trace_to_jsonl <input.bin> [output.jsonl]");
        std::process::exit(1);
    }

    let data = std::fs::read(&args[1])?;
    let mut decoder =
        Decoder::new(&data).ok_or_else(|| std::io::Error::other("invalid trace header"))?;

    let out: Box<dyn Write> = if let Some(path) = args.get(2) {
        Box::new(std::fs::File::create(path)?)
    } else {
        Box::new(std::io::stdout().lock())
    };
    let mut w = BufWriter::new(out);

    let mut count = 0u64;
    decoder
        .for_each_event(|raw| {
            // Deserialize as a generic JSON value to preserve all fields
            let ev: serde_json::Value = raw.deserialize().expect("deserialize");
            serde_json::to_writer(&mut w, &ev).expect("write json");
            w.write_all(b"\n").expect("write newline");
            count += 1;
        })
        .map_err(|e| std::io::Error::other(e.to_string()))?;

    w.flush()?;
    eprintln!("{count} events written");
    Ok(())
}
