use libpgs::pgs::{CompositionState, PcsData, PgsSegment};
use plurx_pgs::{inspect_sup, inspect_sup_file, normalize_sup, ParserLimits};
use std::fs::File;
use std::io::Write;

fn clear_display_set(pts: u32, state: CompositionState) -> Vec<u8> {
    let pcs = PgsSegment::from_pcs(
        u64::from(pts),
        0,
        &PcsData {
            video_width: 1920,
            video_height: 1080,
            composition_number: 0,
            composition_state: state,
            palette_only: false,
            palette_id: 0,
            objects: vec![],
        },
    );
    let end = PgsSegment::end_segment(u64::from(pts), 0);
    [pcs.to_bytes(), end.to_bytes()].concat()
}

fn write_sup(bytes: &[u8]) -> tempfile::NamedTempFile {
    let mut file = tempfile::NamedTempFile::new().expect("temporary SUP");
    file.write_all(bytes).expect("write SUP");
    file.flush().expect("flush SUP");
    file
}

#[test]
fn pts_wrap_is_unwrapped_across_the_32_bit_clock_boundary() {
    let before_wrap = u32::MAX - 45_000;
    let after_wrap = 45_000u32;
    let mut bytes = clear_display_set(before_wrap, CompositionState::EpochStart);
    bytes.extend(clear_display_set(after_wrap, CompositionState::Normal));
    let file = write_sup(&bytes);

    let track = normalize_sup(file.path(), &ParserLimits::default()).expect("valid PTS wrap");
    assert_eq!(track.compositions.len(), 2);
    assert_eq!(track.compositions[0].pts_90khz, u64::from(before_wrap));
    assert_eq!(
        track.compositions[1].pts_90khz,
        u32::MAX as u64 + 1 + u64::from(after_wrap)
    );
}

#[test]
fn epoch_continue_is_retained_by_the_owned_pcs_adapter() {
    let mut bytes = clear_display_set(90_000, CompositionState::EpochStart);
    bytes[PGS_HEADER_BYTES + 7] = 0xc0;
    let file = write_sup(&bytes);

    let track = normalize_sup(file.path(), &ParserLimits::default())
        .expect("0xC0 epoch-continue display set");
    assert_eq!(track.compositions.len(), 1);
    assert!(track.compositions[0].objects.is_empty());
}

#[cfg(unix)]
#[test]
fn already_open_input_keeps_one_identity_after_its_path_is_replaced() {
    let directory = tempfile::tempdir().expect("fixture directory");
    let path = directory.path().join("track.sup");
    std::fs::write(
        &path,
        clear_display_set(90_000, CompositionState::EpochStart),
    )
    .expect("write original SUP");
    let pinned = File::open(&path).expect("open original identity");

    std::fs::rename(&path, directory.path().join("original.sup")).expect("move original path");
    std::fs::write(&path, b"not a SUP stream").expect("replace path");

    let report = inspect_sup_file(pinned, &ParserLimits::default())
        .expect("the pinned handle remains the validated source");
    assert_eq!(report.display_sets, 1);
    assert!(inspect_sup(&path, &ParserLimits::default()).is_err());
}

const PGS_HEADER_BYTES: usize = 13;
