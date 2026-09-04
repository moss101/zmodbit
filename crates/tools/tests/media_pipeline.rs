//! Media pipeline integration tests (M2.10, docs/25): real PNG/JPEG/PDF/
//! text detection, budgets, digests, provenance, bounded fs.read, object
//! store retrieval.

use modbit_tools::media::{
    detect_type, minimal_jpeg, minimal_pdf, minimal_png, read_file_bounded, MediaError,
    MediaPipeline, BOUNDED_READ_BYTES, MAX_MEDIA_BYTES,
};

#[test]
fn magic_bytes_detect_png_jpeg_pdf_and_text() {
    let png = minimal_png();
    let (t, mime) = detect_type(&png).unwrap();
    assert_eq!(t, modbit_tools::media::MediaType::Png);
    assert_eq!(mime, "image/png");

    let (t, mime) = detect_type(&minimal_jpeg()).unwrap();
    assert_eq!(t, modbit_tools::media::MediaType::Jpeg);
    assert_eq!(mime, "image/jpeg");

    let (t, mime) = detect_type(&minimal_pdf()).unwrap();
    assert_eq!(t, modbit_tools::media::MediaType::Pdf);
    assert_eq!(mime, "application/pdf");

    let (t, mime) = detect_type(b"just some text").unwrap();
    assert_eq!(t, modbit_tools::media::MediaType::Text);
    assert_eq!(mime, "text/plain");
}

#[test]
fn pipeline_ingests_real_media_with_provenance_and_digest() {
    let store_root = {
        let mut p = std::env::temp_dir();
        p.push(format!("modbit-m210-{}", uuid::Uuid::now_v7().simple()));
        p
    };
    let pipeline = MediaPipeline::new(&store_root, MAX_MEDIA_BYTES).unwrap();

    let png = minimal_png();
    let envelope = pipeline.ingest("media-1", "desktop-upload", &png).unwrap();
    assert_eq!(envelope.media_type, modbit_tools::media::MediaType::Png);
    assert_eq!(envelope.mime, "image/png");
    assert_eq!(envelope.byte_length, png.len());
    assert_eq!(envelope.source, "desktop-upload");
    assert_eq!(envelope.trust_label, "untrusted-until-scanned");

    // Digest is verifiable: the object store returns the same bytes.
    let round_trip = pipeline.read_bounded(&envelope).unwrap().1;
    assert_eq!(round_trip, png);
}

#[test]
fn budget_rejects_oversized_media() {
    let store_root = {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "modbit-m210-budget-{}",
            uuid::Uuid::now_v7().simple()
        ));
        p
    };
    let pipeline = MediaPipeline::new(&store_root, 1024).unwrap();
    let oversized = vec![b'P'; 2048];
    match pipeline.ingest("media-big", "api", &oversized) {
        Err(MediaError::TooLarge { size, budget }) => {
            assert_eq!(size, 2048);
            assert_eq!(budget, 1024);
        }
        other => panic!("expected TooLarge, got {other:?}"),
    }
}

#[test]
fn fs_read_returns_bounded_typed_results() {
    let store_root = {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "modbit-m210-fsread-{}",
            uuid::Uuid::now_v7().simple()
        ));
        p
    };
    let pipeline = MediaPipeline::new(&store_root, MAX_MEDIA_BYTES).unwrap();

    // A real PNG file on disk read through the fs.read path.
    let file = std::env::temp_dir().join(format!(
        "modbit-m210-img-{}.png",
        uuid::Uuid::now_v7().simple()
    ));
    std::fs::write(&file, minimal_png()).unwrap();
    let (envelope, bounded) =
        read_file_bounded(&pipeline, "desktop-upload", &file, "media-fs-1").unwrap();
    assert_eq!(envelope.mime, "image/png");
    assert_eq!(bounded.len(), minimal_png().len());
    std::fs::remove_file(file).ok();

    // A text file: bounded read caps the returned window.
    let big_text = std::env::temp_dir().join(format!(
        "modbit-m210-txt-{}.txt",
        uuid::Uuid::now_v7().simple()
    ));
    std::fs::write(&big_text, vec![b'z'; BOUNDED_READ_BYTES + 4096]).unwrap();
    let (envelope, bounded_text) =
        read_file_bounded(&pipeline, "desktop-upload", &big_text, "media-text").unwrap();
    assert_eq!(envelope.media_type, modbit_tools::media::MediaType::Text);
    assert_eq!(bounded_text.len(), BOUNDED_READ_BYTES);
    std::fs::remove_file(big_text).ok();
}

#[test]
fn empty_and_unknown_media_are_rejected() {
    let store_root = {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "modbit-m210-empty-{}",
            uuid::Uuid::now_v7().simple()
        ));
        p
    };
    let pipeline = MediaPipeline::new(&store_root, MAX_MEDIA_BYTES).unwrap();
    assert!(matches!(
        pipeline.ingest("m", "src", &[]),
        Err(MediaError::Empty)
    ));
    assert!(matches!(
        pipeline.ingest("m", "src", &[0xFF, 0xFE, 0x00, 0x01]),
        Err(MediaError::UnknownType)
    ));
}
