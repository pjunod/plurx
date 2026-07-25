//! EXIF capture dates for photos in home libraries.
//!
//! This is priority 1 of the recorded-date ladder for stills
//! (docs/HOMEVIDEO-PLAN.md §4.4): the camera's own idea of when the shutter
//! fired beats anything a filename or an mtime can tell us — mtime in
//! particular lies the moment a folder is copied off an SD card.
//!
//! JPEG and TIFF only. HEIC stores its EXIF inside an ISO-BMFF box structure
//! that `kamadak-exif` doesn't walk, and hand-rolling that parse is a rabbit
//! hole for v1 — HEIC photos fall through to the filename/mtime rungs.

use std::path::Path;

/// `DateTimeOriginal` (falling back to `DateTimeDigitized`, then the file's
/// `DateTime`) as `YYYY-MM-DDTHH:MM:SS`, or `None` when there is no usable
/// EXIF. Never errors: a corrupt header is a missing date, not a scan failure.
pub fn date_for(path: &Path) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    let mut reader = std::io::BufReader::new(file);
    let exif = exif::Reader::new().read_from_container(&mut reader).ok()?;

    for tag in [
        exif::Tag::DateTimeOriginal,
        exif::Tag::DateTimeDigitized,
        exif::Tag::DateTime,
    ] {
        let Some(field) = exif
            .get_field(tag, exif::In::PRIMARY)
            .or_else(|| exif.get_field(tag, exif::In::THUMBNAIL))
        else {
            continue;
        };
        if let Some(date) = normalize(&field.display_value().to_string()) {
            return Some(date);
        }
    }
    None
}

/// EXIF writes dates as "2019:06:14 12:34:56". Convert to ISO 8601 and reject
/// the placeholder zeros cameras emit when their clock was never set.
fn normalize(raw: &str) -> Option<String> {
    let raw = raw.trim();
    let (date, time) = match raw.split_once(' ') {
        Some((d, t)) => (d, Some(t)),
        None => (raw, None),
    };
    let mut parts = date.split(&[':', '-'][..]);
    let (y, m, d) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() || y.len() != 4 || m.len() != 2 || d.len() != 2 {
        return None;
    }
    if !(y.chars().chain(m.chars()).chain(d.chars())).all(|c| c.is_ascii_digit()) {
        return None;
    }
    if y == "0000" || m == "00" || d == "00" {
        return None;
    }
    let time = time
        .map(str::trim)
        .filter(|t| t.len() == 8 && t.as_bytes()[2] == b':' && t.as_bytes()[5] == b':');
    Some(match time {
        Some(time) => format!("{y}-{m}-{d}T{time}"),
        None => format!("{y}-{m}-{d}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exif_timestamps_become_iso_8601() {
        assert_eq!(
            normalize("2019:06:14 12:34:56").as_deref(),
            Some("2019-06-14T12:34:56")
        );
        // Already-ISO dates (some writers) and date-only values.
        assert_eq!(normalize("2019-06-14").as_deref(), Some("2019-06-14"));
        assert_eq!(normalize("2019:06:14").as_deref(), Some("2019-06-14"));
    }

    #[test]
    fn a_camera_with_an_unset_clock_has_no_date() {
        assert_eq!(normalize("0000:00:00 00:00:00"), None);
        assert_eq!(normalize("    "), None);
        assert_eq!(normalize("garbage"), None);
    }

    #[test]
    fn unreadable_files_are_simply_dateless() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("not-a-photo.jpg");
        std::fs::write(&path, b"definitely not a jpeg").expect("write");
        assert_eq!(date_for(&path), None);
        assert_eq!(date_for(&dir.path().join("missing.jpg")), None);
    }
}
