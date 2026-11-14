//! Tests of the image library on the made-up sample image
//! (`memory_manager::sample`), written to temporary files.

/// The sample images as files, built once per test run.
fn sample_file(name: &str) -> std::path::PathBuf {
    static DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    let dir = DIR.get_or_init(|| {
        use memory_manager::sample;
        let dir = std::env::temp_dir().join(format!("memory-manager-sample-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("original.bin"), sample::radio_like()).unwrap();
        std::fs::write(dir.join("edited.bin"), sample::edited_9999()).unwrap();
        std::fs::write(dir.join("markers.bin"), sample::with_presets_on_every_band()).unwrap();
        dir
    });
    dir.join(name)
}

fn original_path() -> std::path::PathBuf {
    sample_file("original.bin")
}

fn edited_path() -> std::path::PathBuf {
    sample_file("edited.bin")
}

fn before_markers_path() -> std::path::PathBuf {
    sample_file("edited.bin")
}

fn markers_path() -> std::path::PathBuf {
    sample_file("markers.bin")
}

include!("shared/image_tests.rs");

#[test]
fn the_demo_image_is_marked_and_cannot_pass_for_a_radio_image() {
    use memory_manager::sample;
    let demo = sample::demo();
    assert!(!demo.has_product_string());
    assert_eq!(&demo.as_bytes()[0x0F50..0x0F60], sample::DEMO_MARKER);
    assert!(EepromImage::from_bytes(&sample::radio_like()).unwrap().has_product_string());
    // Apart from the marker it is the sample with presets on every band.
    let with_presets = sample::with_presets_on_every_band();
    let differing: Vec<usize> = diff(demo.as_bytes(), &with_presets);
    assert!(differing.iter().all(|at| (0x0F50..0x0F60).contains(at)));
}
