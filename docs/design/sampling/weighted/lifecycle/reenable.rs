fn main() {
    let recorder = dial9_core::recorder::recorder(
        dial9_core::buffer::MemoryBuffer::new(1 << 20).unwrap()
    ).build();
    let stop = recorder.handle().disable_callback();
    assert!(recorder.handle().is_enabled());
    stop();
    assert!(!recorder.handle().is_enabled());
    recorder.handle().enable();
    assert!(recorder.handle().is_enabled());
    println!("A sampler stop callback disables recording, but Handle::enable re-enables it.");
}
