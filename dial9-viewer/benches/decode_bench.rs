use criterion::{Criterion, criterion_group, criterion_main};
use dial9_viewer::ingest::decode::decode_samples;

fn load_demo_trace() -> Vec<u8> {
    let data = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/ui/demo-trace.bin")).unwrap();
    let mut dec = flate2::read::GzDecoder::new(data.as_slice());
    let mut buf = Vec::new();
    std::io::Read::read_to_end(&mut dec, &mut buf).unwrap();
    buf
}

fn bench_decode(c: &mut Criterion) {
    let data = load_demo_trace();
    c.bench_function("decode_samples_demo_trace", |b| {
        b.iter(|| decode_samples(&data, "bench/demo-trace.bin").unwrap());
    });
}

criterion_group!(benches, bench_decode);
criterion_main!(benches);
