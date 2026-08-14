#![cfg(unix)]

use libpgs::pgs::{CompositionState, PcsData, PgsSegment};
use plurx_pgs::{inspect_sup, inspect_sup_file, ParserLimits};
use std::fs::File;

fn clear_display_set(pts: u32) -> Vec<u8> {
    let pcs = PgsSegment::from_pcs(
        u64::from(pts),
        0,
        &PcsData {
            video_width: 1920,
            video_height: 1080,
            composition_number: 0,
            composition_state: CompositionState::EpochStart,
            palette_only: false,
            palette_id: 0,
            objects: vec![],
        },
    );
    let end = PgsSegment::end_segment(u64::from(pts), 0);
    [pcs.to_bytes(), end.to_bytes()].concat()
}

#[test]
fn already_open_input_keeps_one_identity_after_its_path_is_replaced() {
    let directory = tempfile::tempdir().expect("fixture directory");
    let path = directory.path().join("track.sup");
    std::fs::write(&path, clear_display_set(90_000)).expect("write original SUP");
    let pinned = File::open(&path).expect("open original identity");

    std::fs::rename(&path, directory.path().join("original.sup")).expect("move original path");
    std::fs::write(&path, b"not a SUP stream").expect("replace path");

    let report = inspect_sup_file(pinned, &ParserLimits::default())
        .expect("the pinned handle remains the validated source");
    assert_eq!(report.display_sets, 1);
    assert!(inspect_sup(&path, &ParserLimits::default()).is_err());
}
