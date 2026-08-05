#![no_main]

use libfuzzer_sys::fuzz_target;
use plurx_pgs::{inspect_sup, ParserLimits};
use std::cell::RefCell;
use std::io::{Seek, SeekFrom, Write};

const MAX_FUZZ_BYTES: usize = 1024 * 1024;

thread_local! {
    static INPUT: RefCell<tempfile::NamedTempFile> = RefCell::new(
        tempfile::NamedTempFile::new().expect("create reusable fuzz input")
    );
}

fn fuzz_limits() -> ParserLimits {
    ParserLimits {
        max_sup_bytes: MAX_FUZZ_BYTES as u64,
        max_display_sets: 4096,
        max_segments_per_display_set: 256,
        max_payload_bytes_per_display_set: MAX_FUZZ_BYTES,
        max_canvas_width: 4096,
        max_canvas_height: 2160,
        max_canvas_pixels: 8_847_360,
        max_objects_per_composition: 64,
        max_object_rgba_bytes: 8 * 1024 * 1024,
        max_object_rle_bytes: MAX_FUZZ_BYTES,
        max_cached_objects: 256,
        max_cached_pixel_bytes: 16 * 1024 * 1024,
        max_palettes: 64,
    }
}

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_FUZZ_BYTES {
        return;
    }

    INPUT.with(|input| {
        let mut input = input.borrow_mut();
        let file = input.as_file_mut();
        if file.set_len(0).is_err()
            || file.seek(SeekFrom::Start(0)).is_err()
            || file.write_all(data).is_err()
            || file.flush().is_err()
        {
            return;
        }

        // Malformed input is expected. A panic, abort, sanitizer finding, or
        // allocation beyond the reduced profile is a fuzz failure.
        let _ = inspect_sup(input.path(), &fuzz_limits());
    });
});
