use std::process::{Command, Stdio};

use plurx_core::domain::{ItemKind, LibraryKind, NewItem, NewLibrary};
use plurx_core::metadata::book::{
    enrich_library, materialize_item_cover, CoverMaterializationWorkers,
};
use plurx_core::scan::probe::probe;
use plurx_core::store::{LibraryStore, MediaStore, SqliteStore};

fn ffmpeg() -> String {
    std::env::var("PLURX_FFMPEG")
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "ffmpeg".to_owned())
}

fn require_ffmpeg() {
    let bin = ffmpeg();
    let available = Command::new(&bin)
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok();
    assert!(
        available,
        "this test needs ffmpeg; running `{bin}` failed. Install ffmpeg or set PLURX_FFMPEG"
    );
}

fn run(command: &mut Command) {
    let output = command.output().expect("running ffmpeg");
    assert!(
        output.status.success(),
        "{command:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
async fn audiobook_refresh_adopts_its_embedded_cover() {
    require_ffmpeg();
    let media = tempfile::tempdir().expect("media");
    let artwork = tempfile::tempdir().expect("artwork");
    let cover = media.path().join("cover.jpg");
    let audiobook = media.path().join("One Second After.mp3");
    run(Command::new(ffmpeg())
        .args(["-y", "-v", "error", "-f", "lavfi", "-i"])
        .arg("color=c=navy:s=32x48:d=0.1")
        .args(["-frames:v", "1"])
        .arg(&cover));
    run(Command::new(ffmpeg())
        .args(["-y", "-v", "error", "-f", "lavfi", "-i"])
        .arg("sine=frequency=440:duration=0.2")
        .arg("-i")
        .arg(&cover)
        .args([
            "-map",
            "0:a:0",
            "-map",
            "1:v:0",
            "-c:a",
            "libmp3lame",
            "-c:v",
            "mjpeg",
            "-id3v2_version",
            "3",
            "-metadata:s:v",
            "comment=Cover (front)",
            "-disposition:v",
            "attached_pic",
        ])
        .arg(&audiobook));

    let store = SqliteStore::open_in_memory().expect("store");
    let library = store
        .create_library(&NewLibrary {
            name: "Books".into(),
            kind: LibraryKind::Books,
            paths: vec![media.path().to_path_buf()],
            anime: false,
        })
        .await
        .expect("library");
    let item_id = store
        .insert_item(&NewItem {
            library_id: library.id,
            kind: ItemKind::Audiobook,
            parent_id: None,
            title: "One Second After".into(),
            year: None,
            season_number: None,
            episode_number: None,
        })
        .await
        .expect("audiobook");
    let facts = probe(&audiobook).await.expect("probe attached cover");
    store
        .upsert_file(
            item_id,
            audiobook.to_str().expect("UTF-8 fixture path"),
            i64::try_from(std::fs::metadata(&audiobook).expect("metadata").len())
                .expect("small fixture"),
            1,
            &facts,
        )
        .await
        .expect("file row");

    let report = enrich_library(&store, artwork.path(), library.id, true, Some(&[item_id])).await;
    assert_eq!(report.inspected, 1);
    assert_eq!(report.updated, 1);
    assert_eq!(report.errors, 0);
    let item = store
        .get_item(item_id)
        .await
        .expect("item")
        .expect("audiobook disappeared");
    let poster = item.poster_path.expect("embedded poster path");
    assert!(artwork.path().join(&poster).is_file());
    assert_eq!(item.book_metadata_source, None);
    assert!(item.artwork_attempted_at.is_some());
    assert_eq!(item.artwork_error, None);

    std::fs::remove_file(artwork.path().join(&poster)).expect("remove local materialization");
    assert_eq!(
        materialize_item_cover(
            &store,
            artwork.path(),
            item_id,
            std::slice::from_ref(&poster),
            &CoverMaterializationWorkers::default(),
        )
        .await
        .expect("rebuild embedded audiobook cover"),
        Some(true)
    );
    assert!(artwork.path().join(&poster).is_file());
    let after = store
        .get_item(item_id)
        .await
        .expect("item after materialization")
        .expect("audiobook disappeared after materialization");
    assert_eq!(after.poster_path.as_deref(), Some(poster.as_str()));
    assert_eq!(after.book_metadata_source, item.book_metadata_source);
    assert_eq!(after.artwork_attempted_at, item.artwork_attempted_at);
}
