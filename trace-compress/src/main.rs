use std::process::ExitCode;
use std::time::Instant;

fn usage() -> ExitCode {
    eprintln!(
        "usage:\n  d9tc analyze <trace.bin>\n  d9tc compress <trace.bin> <out.d9tc> [zstd-level]\n  d9tc decompress <in.d9tc> <out.bin>\n  d9tc bench <trace.bin> [zstd-level...]"
    );
    ExitCode::FAILURE
}

fn gzip_compress(data: &[u8], level: u32) -> Vec<u8> {
    use std::io::Write;
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::new(level));
    enc.write_all(data).unwrap();
    enc.finish().unwrap()
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(|s| s.as_str()) {
        Some("analyze") => {
            let Some(path) = args.get(2) else {
                return usage();
            };
            let data = std::fs::read(path).expect("read input");
            match dial9_trace_compress::analyze::analyze(&data) {
                Ok(report) => {
                    println!("{report}");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("{e}");
                    ExitCode::FAILURE
                }
            }
        }
        Some("streams") => {
            let Some(path) = args.get(2) else {
                return usage();
            };
            let level: i32 = args.get(3).map(|s| s.parse().unwrap()).unwrap_or(12);
            let data = std::fs::read(path).expect("read input");
            match dial9_trace_compress::compress::stream_report(&data, level) {
                Ok(report) => {
                    println!("{report}");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("{e}");
                    ExitCode::FAILURE
                }
            }
        }
        Some("compress") => {
            let (Some(inp), Some(outp)) = (args.get(2), args.get(3)) else {
                return usage();
            };
            let level: i32 = args
                .get(4)
                .map(|s| s.parse().unwrap())
                .unwrap_or(dial9_trace_compress::compress::DEFAULT_LEVEL);
            let data = std::fs::read(inp).expect("read input");
            let t = Instant::now();
            match dial9_trace_compress::compress::compress(&data, level) {
                Ok(c) => {
                    let dt = t.elapsed();
                    std::fs::write(outp, &c).expect("write output");
                    println!(
                        "{} -> {} bytes ({:.2}%) in {:.1} ms",
                        data.len(),
                        c.len(),
                        c.len() as f64 / data.len() as f64 * 100.0,
                        dt.as_secs_f64() * 1000.0
                    );
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("{e}");
                    ExitCode::FAILURE
                }
            }
        }
        Some("decompress") => {
            let (Some(inp), Some(outp)) = (args.get(2), args.get(3)) else {
                return usage();
            };
            let data = std::fs::read(inp).expect("read input");
            let t = Instant::now();
            match dial9_trace_compress::compress::decompress(&data) {
                Ok(raw) => {
                    let dt = t.elapsed();
                    std::fs::write(outp, &raw).expect("write output");
                    println!(
                        "{} -> {} bytes in {:.1} ms",
                        data.len(),
                        raw.len(),
                        dt.as_secs_f64() * 1000.0
                    );
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("{e}");
                    ExitCode::FAILURE
                }
            }
        }
        Some("bench") => {
            let Some(path) = args.get(2) else {
                return usage();
            };
            let data = std::fs::read(path).expect("read input");
            let levels: Vec<i32> = if args.len() > 3 {
                args[3..].iter().map(|s| s.parse().unwrap()).collect()
            } else {
                vec![3, 9, 12, 15, 17, 19]
            };
            println!("input: {} bytes", data.len());

            for lvl in [1u32, 6, 9] {
                let t = Instant::now();
                let c = gzip_compress(&data, lvl);
                let dt = t.elapsed();
                println!(
                    "gzip -{lvl}:      {:>9} bytes ({:.2}%)  {:>8.1} ms",
                    c.len(),
                    c.len() as f64 / data.len() as f64 * 100.0,
                    dt.as_secs_f64() * 1000.0
                );
            }

            for lvl in levels {
                let t = Instant::now();
                let c = match dial9_trace_compress::compress::compress(&data, lvl) {
                    Ok(c) => c,
                    Err(e) => {
                        eprintln!("compress failed: {e}");
                        return ExitCode::FAILURE;
                    }
                };
                let dt = t.elapsed();
                let t2 = Instant::now();
                let restored = match dial9_trace_compress::compress::decompress(&c) {
                    Ok(r) => r,
                    Err(e) => {
                        eprintln!("decompress failed: {e}");
                        return ExitCode::FAILURE;
                    }
                };
                let dt2 = t2.elapsed();
                let ok = restored == data;
                println!(
                    "d9tc zstd-{lvl:<2}: {:>9} bytes ({:.2}%)  {:>8.1} ms  (decomp {:.1} ms, round-trip {})",
                    c.len(),
                    c.len() as f64 / data.len() as f64 * 100.0,
                    dt.as_secs_f64() * 1000.0,
                    dt2.as_secs_f64() * 1000.0,
                    if ok { "OK" } else { "MISMATCH" }
                );
                if !ok {
                    return ExitCode::FAILURE;
                }
            }
            ExitCode::SUCCESS
        }
        _ => usage(),
    }
}
