//! Backend-neutral behavioral contract for the durable store boundary.
//!
//! Every scenario receives only `Arc<dyn Store>` and runs against both SQLite
//! modes. Replicated backends join the same factory list in later milestones.

use std::collections::BTreeSet;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;

use plurx_core::domain::{
    scopes, ArtworkAttempt, ItemEdit, ItemKind, ItemSort, LibraryKind, MetadataPatch, NewItem,
    NewLibrary, NewOfflinePackage, OfflineCreateOutcome, OfflineLeaseOutcome, ProbeResult,
    TraktAuth,
};
use plurx_core::store::{OutboxEntry, SqliteStore, Store};

const SETTINGS_METHODS: &[&str] = &["ping", "get_setting", "put_setting", "instance_id"];
const USER_METHODS: &[&str] = &[
    "count_users",
    "create_user",
    "get_user",
    "get_user_by_username",
    "list_users",
    "delete_user",
    "count_admins",
    "set_password",
    "set_admin",
    "delete_tokens_for_user",
    "create_token",
    "user_for_token",
    "delete_token",
];
const LIBRARY_METHODS: &[&str] = &[
    "create_library",
    "update_library",
    "delete_library",
    "set_library_schedule",
    "mark_library_scanned",
    "get_library",
    "list_libraries",
];
const MEDIA_METHODS: &[&str] = &[
    "item_by_external_id",
    "find_movie",
    "find_show",
    "find_season",
    "find_episode",
    "find_child_item",
    "insert_item",
    "get_item",
    "get_item_children",
    "list_top_items_in_genre",
    "list_top_items",
    "recently_added",
    "search_items",
    "apply_metadata",
    "items_needing_metadata",
    "episodes_for_show",
    "items_needing_artwork",
    "items_missing_artwork",
    "items_missing_genres",
    "update_item_fields",
    "set_nfo_seeded",
    "get_file_by_path",
    "upsert_file",
    "get_file",
    "media_shape",
    "files_for_item",
    "child_counts",
    "item_max_heights",
    "item_media_facts",
    "set_file_audio_offset",
    "get_file_probe_json",
    "merge_file_probe_chapters",
    "files_missing_probe",
    "library_file_paths",
    "delete_files",
    "prune_empty_items",
];
const WATCH_METHODS: &[&str] = &[
    "watch_state",
    "watch_map",
    "put_progress",
    "put_progress_at",
    "set_watched",
    "set_watched_tree",
    "watch_rollup",
    "watch_rollups",
    "continue_watching",
    "next_up",
    "apply_remote_watch",
];
const TRAKT_METHODS: &[&str] = &[
    "get_trakt_auth",
    "list_trakt_auth",
    "put_trakt_auth",
    "delete_trakt_auth",
    "update_trakt_tokens",
    "set_trakt_sync",
    "trakt_sync_candidates",
];
const API_KEY_METHODS: &[&str] = &[
    "create_api_key",
    "list_api_keys",
    "api_key_for_hash",
    "touch_api_key",
    "delete_api_key",
    "set_api_key_disabled",
];
const OUTBOX_METHODS: &[&str] = &[
    "enqueue_watched",
    "due_watched",
    "settle_watched",
    "watched_outbox_counts",
];
const CACHE_METHODS: &[&str] = &[
    "cache_hit",
    "claim_cache_entry",
    "touch_cache_claim",
    "complete_cache_entry",
    "touch_cache_entry",
    "cache_by_age",
    "stale_cache_claims",
    "all_cache_rows",
    "forget_cache_entry",
    "cache_bytes",
];
const OFFLINE_METHODS: &[&str] = &[
    "create_offline_package",
    "offline_package_for_user",
    "renew_offline_package_for_user",
    "offline_activity_packages",
    "offline_package_stats",
    "reset_interrupted_offline_packages",
    "claim_next_offline_package",
    "requeue_offline_package",
    "set_offline_package_recipe",
    "update_offline_progress",
    "fail_offline_package",
    "put_offline_lease",
    "offline_package_for_lease",
    "mark_offline_package_ready",
    "delete_offline_package",
    "expire_offline_packages",
];

struct StoreFixture {
    name: &'static str,
    store: Arc<dyn Store>,
    _directory: Option<tempfile::TempDir>,
}

fn sqlite_fixtures() -> Vec<StoreFixture> {
    let directory = tempfile::tempdir().expect("file-backed contract directory");
    let file_store =
        SqliteStore::open(&directory.path().join("plurx.db")).expect("file-backed contract store");
    vec![
        StoreFixture {
            name: "memory",
            store: Arc::new(SqliteStore::open_in_memory().expect("in-memory contract store")),
            _directory: None,
        },
        StoreFixture {
            name: "file",
            store: Arc::new(file_store),
            _directory: Some(directory),
        },
    ]
}

async fn for_each_sqlite_backend<F, Fut>(mut contract: F)
where
    F: FnMut(Arc<dyn Store>, &'static str) -> Fut,
    Fut: Future<Output = ()>,
{
    for fixture in sqlite_fixtures() {
        contract(Arc::clone(&fixture.store), fixture.name).await;
    }
}

#[test]
fn contract_inventory_matches_every_store_method() {
    let source = include_str!("../src/store/mod.rs");
    let declared = source
        .lines()
        .filter_map(|line| line.strip_prefix("    async fn "))
        .filter_map(|line| line.split_once('(').map(|(name, _)| name))
        .collect::<BTreeSet<_>>();
    let covered = [
        SETTINGS_METHODS,
        USER_METHODS,
        LIBRARY_METHODS,
        MEDIA_METHODS,
        WATCH_METHODS,
        TRAKT_METHODS,
        API_KEY_METHODS,
        OUTBOX_METHODS,
        CACHE_METHODS,
        OFFLINE_METHODS,
    ]
    .into_iter()
    .flatten()
    .copied()
    .collect::<BTreeSet<_>>();

    assert_eq!(declared.len(), 114, "review the M1a method count");
    assert_eq!(
        covered, declared,
        "every trait method needs a parity scenario"
    );
}

#[tokio::test]
async fn settings_contract_runs_through_dyn_store() {
    for_each_sqlite_backend(|store, backend| async move {
        store.ping().await.expect("ping");
        assert_eq!(store.get_setting("contract.key").await.expect("get"), None);
        store
            .put_setting("contract.key", "first")
            .await
            .expect("insert setting");
        store
            .put_setting("contract.key", "second")
            .await
            .expect("update setting");
        assert_eq!(
            store.get_setting("contract.key").await.expect("get"),
            Some("second".to_owned()),
            "backend {backend}"
        );
        uuid::Uuid::parse_str(&store.instance_id().await.expect("instance id"))
            .expect("new instance ids are UUIDs");
    })
    .await;
}

#[tokio::test]
async fn user_contract_runs_through_dyn_store() {
    for_each_sqlite_backend(|store, backend| async move {
        assert_eq!(store.count_users().await.expect("count"), 0);
        let admin = store
            .create_user("Admin", "hash-1", true)
            .await
            .expect("create admin");
        let viewer = store
            .create_user("Viewer", "hash-2", false)
            .await
            .expect("create viewer");
        assert_eq!(store.count_users().await.expect("count"), 2);
        assert_eq!(store.count_admins().await.expect("admins"), 1);
        assert_eq!(
            store
                .get_user(admin.id)
                .await
                .expect("get")
                .expect("admin")
                .username,
            "Admin"
        );
        assert_eq!(
            store
                .get_user_by_username("viewer")
                .await
                .expect("lookup")
                .expect("viewer")
                .id,
            viewer.id
        );
        assert_eq!(store.list_users().await.expect("list").len(), 2);
        assert!(store
            .set_password(viewer.id, "hash-3")
            .await
            .expect("password"));
        assert!(store.set_admin(viewer.id, true).await.expect("promote"));
        assert_eq!(store.count_admins().await.expect("admins"), 2);

        store
            .create_token("token-one", viewer.id, Some("contract"))
            .await
            .expect("token");
        store
            .create_token("token-two", viewer.id, None)
            .await
            .expect("token");
        assert_eq!(
            store
                .user_for_token("token-one")
                .await
                .expect("resolve")
                .expect("token user")
                .id,
            viewer.id,
            "backend {backend}"
        );
        assert!(store.delete_token("token-one").await.expect("delete token"));
        assert_eq!(
            store
                .delete_tokens_for_user(viewer.id)
                .await
                .expect("delete user tokens"),
            1
        );
        assert!(store.delete_user(admin.id).await.expect("delete user"));
    })
    .await;
}

#[tokio::test]
async fn api_key_contract_runs_through_dyn_store() {
    for_each_sqlite_backend(|store, backend| async move {
        let key = store
            .create_api_key(
                "automation",
                "key-hash",
                &[
                    scopes::SCAN_TRIGGER.to_owned(),
                    scopes::STATUS_READ.to_owned(),
                ],
            )
            .await
            .expect("create key");
        assert_eq!(store.list_api_keys().await.expect("list").len(), 1);
        assert_eq!(
            store
                .api_key_for_hash("key-hash")
                .await
                .expect("lookup")
                .expect("key")
                .id,
            key.id,
            "backend {backend}"
        );
        store.touch_api_key(key.id).await.expect("touch");
        assert!(store
            .set_api_key_disabled(key.id, true)
            .await
            .expect("disable"));
        assert!(store.delete_api_key(key.id).await.expect("delete"));
    })
    .await;
}

#[tokio::test]
async fn library_contract_runs_through_dyn_store() {
    for_each_sqlite_backend(|store, backend| async move {
        let library = store
            .create_library(&NewLibrary {
                name: "Contract Movies".into(),
                kind: LibraryKind::Movies,
                paths: vec![PathBuf::from("/contract/movies")],
                anime: false,
            })
            .await
            .expect("create library");
        assert_eq!(
            store
                .get_library(library.id)
                .await
                .expect("get")
                .expect("library")
                .name,
            "Contract Movies"
        );
        let updated = store
            .update_library(
                library.id,
                &NewLibrary {
                    name: "Contract Films".into(),
                    kind: LibraryKind::Movies,
                    paths: vec![PathBuf::from("/contract/films")],
                    anime: false,
                },
            )
            .await
            .expect("update")
            .expect("updated library");
        let scheduled = store
            .set_library_schedule(updated.id, 15, 1_440)
            .await
            .expect("schedule")
            .expect("scheduled library");
        assert_eq!(
            (
                scheduled.scan_interval_mins,
                scheduled.refresh_interval_mins
            ),
            (15, 1_440)
        );
        store
            .mark_library_scanned(scheduled.id, true)
            .await
            .expect("mark scanned");
        assert_eq!(store.list_libraries().await.expect("list").len(), 1);
        assert!(
            store.delete_library(scheduled.id).await.expect("delete"),
            "backend {backend}"
        );
    })
    .await;
}

#[tokio::test]
async fn media_contract_runs_through_dyn_store() {
    for_each_sqlite_backend(|store, backend| async move {
        let movies = store
            .create_library(&NewLibrary {
                name: "Media Contract Movies".into(),
                kind: LibraryKind::Movies,
                paths: vec![PathBuf::from("/contract/movies")],
                anime: false,
            })
            .await
            .expect("movie library");
        let shows = store
            .create_library(&NewLibrary {
                name: "Media Contract Shows".into(),
                kind: LibraryKind::Shows,
                paths: vec![PathBuf::from("/contract/shows")],
                anime: false,
            })
            .await
            .expect("show library");
        let home = store
            .create_library(&NewLibrary {
                name: "Media Contract Home".into(),
                kind: LibraryKind::Home,
                paths: vec![PathBuf::from("/contract/home")],
                anime: false,
            })
            .await
            .expect("home library");

        let movie = store
            .insert_item(&NewItem {
                library_id: movies.id,
                kind: ItemKind::Movie,
                parent_id: None,
                title: "The Contract Movie".into(),
                year: Some(2024),
                season_number: None,
                episode_number: None,
            })
            .await
            .expect("movie");
        let empty_movie = store
            .insert_item(&NewItem {
                library_id: movies.id,
                kind: ItemKind::Movie,
                parent_id: None,
                title: "Empty Contract Movie".into(),
                year: Some(2023),
                season_number: None,
                episode_number: None,
            })
            .await
            .expect("empty movie");
        let show = store
            .insert_item(&NewItem {
                library_id: shows.id,
                kind: ItemKind::Show,
                parent_id: None,
                title: "Contract Show".into(),
                year: Some(2024),
                season_number: None,
                episode_number: None,
            })
            .await
            .expect("show");
        let season = store
            .insert_item(&NewItem {
                library_id: shows.id,
                kind: ItemKind::Season,
                parent_id: Some(show),
                title: "Season 1".into(),
                year: None,
                season_number: Some(1),
                episode_number: None,
            })
            .await
            .expect("season");
        let episode = store
            .insert_item(&NewItem {
                library_id: shows.id,
                kind: ItemKind::Episode,
                parent_id: Some(season),
                title: "Pilot".into(),
                year: None,
                season_number: Some(1),
                episode_number: Some(1),
            })
            .await
            .expect("episode");
        let folder = store
            .insert_item(&NewItem {
                library_id: home.id,
                kind: ItemKind::Folder,
                parent_id: None,
                title: "Trips".into(),
                year: None,
                season_number: None,
                episode_number: None,
            })
            .await
            .expect("home folder");

        assert_eq!(
            store
                .find_movie(movies.id, "The Contract Movie", Some(2024))
                .await
                .expect("find movie")
                .expect("movie")
                .id,
            movie
        );
        assert_eq!(
            store
                .find_show(shows.id, "Contract Show", Some(2024))
                .await
                .expect("find show")
                .expect("show")
                .id,
            show
        );
        assert_eq!(
            store
                .find_season(show, 1)
                .await
                .expect("find season")
                .expect("season")
                .id,
            season
        );
        assert_eq!(
            store
                .find_episode(season, 1)
                .await
                .expect("find episode")
                .expect("episode")
                .id,
            episode
        );
        assert_eq!(
            store
                .find_child_item(home.id, None, ItemKind::Folder, "Trips")
                .await
                .expect("find child")
                .expect("folder")
                .id,
            folder
        );
        assert_eq!(
            store.get_item(movie).await.expect("get").expect("movie").id,
            movie
        );
        assert_eq!(
            store.get_item_children(show).await.expect("children").len(),
            1
        );

        store
            .apply_metadata(
                movie,
                &MetadataPatch {
                    overview: Some("A backend-neutral movie".into()),
                    tmdb_id: Some(42),
                    imdb_id: Some("tt0000042".into()),
                    runtime_ms: Some(7_200_000),
                    genres: Some(vec!["Drama".into()]),
                    enriched: true,
                    artwork: Some(ArtworkAttempt::Failed("contract fixture".into())),
                    ..Default::default()
                },
            )
            .await
            .expect("movie metadata");
        store
            .apply_metadata(
                show,
                &MetadataPatch {
                    tmdb_id: Some(84),
                    enriched: true,
                    ..Default::default()
                },
            )
            .await
            .expect("show metadata");
        assert_eq!(
            store
                .item_by_external_id(ItemKind::Movie, Some(42), None)
                .await
                .expect("external id")
                .expect("external movie")
                .id,
            movie
        );
        assert!(store
            .items_needing_metadata(Some(movies.id), false, None)
            .await
            .expect("metadata queue")
            .iter()
            .any(|item| item.id == empty_movie));
        assert_eq!(
            store.episodes_for_show(show).await.expect("episodes").len(),
            1
        );
        assert!(store
            .items_needing_artwork(home.id, false, None)
            .await
            .expect("home artwork")
            .iter()
            .any(|item| item.id == folder));
        assert!(store
            .items_missing_artwork(Some(movies.id), 0, 10)
            .await
            .expect("missing artwork")
            .iter()
            .any(|item| item.id == movie));
        assert!(store
            .items_missing_genres(0, 10)
            .await
            .expect("missing genres")
            .iter()
            .any(|item| item.id == show));
        let edited = store
            .update_item_fields(
                folder,
                &ItemEdit {
                    title: Some("Edited Trips".into()),
                    tags: Some(vec!["family".into()]),
                    ..Default::default()
                },
            )
            .await
            .expect("edit")
            .expect("edited folder");
        assert_eq!(edited.title, "Edited Trips");
        store.set_nfo_seeded(folder).await.expect("NFO stamp");

        let movie_file = store
            .upsert_file(
                movie,
                "/contract/movies/movie.mkv",
                1_000,
                10,
                &ProbeResult {
                    duration_ms: Some(7_200_000),
                    container: Some("mkv".into()),
                    video_codec: Some("hevc".into()),
                    width: Some(3_840),
                    height: Some(2_160),
                    bitrate: Some(20_000_000),
                    raw_json: Some(r#"{"format":{"filename":"movie.mkv"}}"#.into()),
                    ..Default::default()
                },
            )
            .await
            .expect("movie file");
        let empty_file = store
            .upsert_file(
                empty_movie,
                "/contract/movies/empty.mkv",
                2_000,
                20,
                &ProbeResult::default(),
            )
            .await
            .expect("unprobed file");
        let episode_file = store
            .upsert_file(
                episode,
                "/contract/shows/pilot.mkv",
                3_000,
                30,
                &ProbeResult {
                    duration_ms: Some(3_600_000),
                    container: Some("mkv".into()),
                    video_codec: Some("h264".into()),
                    height: Some(1_080),
                    ..Default::default()
                },
            )
            .await
            .expect("episode file");
        assert_eq!(
            store
                .get_file_by_path("/contract/movies/movie.mkv")
                .await
                .expect("file by path")
                .expect("file")
                .id,
            movie_file
        );
        assert_eq!(
            store
                .get_file(movie_file)
                .await
                .expect("file")
                .expect("file")
                .id,
            movie_file
        );
        assert!(store.media_shape().await.expect("media shape").probed >= 2);
        assert_eq!(store.files_for_item(movie).await.expect("files").len(), 1);
        assert_eq!(
            store
                .child_counts(&[show])
                .await
                .expect("counts")
                .get(&show),
            Some(&1)
        );
        assert_eq!(
            store
                .item_max_heights(&[movie])
                .await
                .expect("heights")
                .get(&movie),
            Some(&2_160)
        );
        assert!(store
            .item_media_facts(&[movie])
            .await
            .expect("facts")
            .contains_key(&movie));
        store
            .set_file_audio_offset(movie_file, 125)
            .await
            .expect("audio offset");
        assert!(store
            .get_file_probe_json(movie_file)
            .await
            .expect("probe JSON")
            .expect("probe JSON")
            .contains("movie.mkv"));
        store
            .merge_file_probe_chapters(movie_file, r#"[{"start_time":"0.0","end_time":"10.0"}]"#)
            .await
            .expect("merge chapters");
        assert!(store
            .get_file_probe_json(movie_file)
            .await
            .expect("probe JSON")
            .expect("probe JSON")
            .contains("chapters"));
        assert!(store
            .files_missing_probe(Some(movies.id))
            .await
            .expect("missing probes")
            .iter()
            .any(|file| file.id == empty_file));
        assert_eq!(
            store
                .library_file_paths(movies.id)
                .await
                .expect("paths")
                .len(),
            2
        );

        let genre_page = store
            .list_top_items_in_genre(movies.id, ItemSort::Title, 0, 20, Some("drama"))
            .await
            .expect("genre page");
        assert!(genre_page.items.iter().any(|item| item.id == movie));
        assert_eq!(
            store
                .list_top_items(movies.id, ItemSort::Added, 0, 20)
                .await
                .expect("page")
                .total,
            2
        );
        assert!(!store
            .recently_added(None, 20)
            .await
            .expect("recent")
            .is_empty());
        assert!(store
            .search_items("Contract", 20)
            .await
            .expect("search")
            .iter()
            .any(|item| item.item.id == movie));

        assert_eq!(
            store
                .delete_files(&[empty_file])
                .await
                .expect("delete files"),
            1
        );
        assert!(store.prune_empty_items(movies.id).await.expect("prune") >= 1);
        assert!(store
            .get_file(episode_file)
            .await
            .expect("episode file")
            .is_some());
        assert!(
            store
                .get_item(movie)
                .await
                .expect("movie remains")
                .is_some(),
            "backend {backend}"
        );
    })
    .await;
}

#[tokio::test]
async fn watch_contract_runs_through_dyn_store() {
    for_each_sqlite_backend(|store, backend| async move {
        let user = store
            .create_user("watch-contract", "hash", false)
            .await
            .expect("user");
        let movies = store
            .create_library(&NewLibrary {
                name: "Watch Contract Movies".into(),
                kind: LibraryKind::Movies,
                paths: vec![PathBuf::from("/watch/movies")],
                anime: false,
            })
            .await
            .expect("movies");
        let shows = store
            .create_library(&NewLibrary {
                name: "Watch Contract Shows".into(),
                kind: LibraryKind::Shows,
                paths: vec![PathBuf::from("/watch/shows")],
                anime: false,
            })
            .await
            .expect("shows");
        let movie = store
            .insert_item(&NewItem {
                library_id: movies.id,
                kind: ItemKind::Movie,
                parent_id: None,
                title: "Watch Contract Movie".into(),
                year: Some(2024),
                season_number: None,
                episode_number: None,
            })
            .await
            .expect("movie");
        let show = store
            .insert_item(&NewItem {
                library_id: shows.id,
                kind: ItemKind::Show,
                parent_id: None,
                title: "Watch Contract Show".into(),
                year: Some(2024),
                season_number: None,
                episode_number: None,
            })
            .await
            .expect("show");
        let season = store
            .insert_item(&NewItem {
                library_id: shows.id,
                kind: ItemKind::Season,
                parent_id: Some(show),
                title: "Season 1".into(),
                year: None,
                season_number: Some(1),
                episode_number: None,
            })
            .await
            .expect("season");
        let mut episodes = Vec::new();
        for number in 1..=2 {
            let episode = store
                .insert_item(&NewItem {
                    library_id: shows.id,
                    kind: ItemKind::Episode,
                    parent_id: Some(season),
                    title: format!("Episode {number}"),
                    year: None,
                    season_number: Some(1),
                    episode_number: Some(number),
                })
                .await
                .expect("episode");
            store
                .upsert_file(
                    episode,
                    &format!("/watch/shows/e{number}.mkv"),
                    1_000,
                    i64::from(number),
                    &ProbeResult {
                        duration_ms: Some(1_000),
                        container: Some("mkv".into()),
                        ..Default::default()
                    },
                )
                .await
                .expect("episode file");
            episodes.push(episode);
        }
        store
            .upsert_file(
                movie,
                "/watch/movies/movie.mkv",
                1_000,
                1,
                &ProbeResult {
                    duration_ms: Some(10_000),
                    container: Some("mkv".into()),
                    ..Default::default()
                },
            )
            .await
            .expect("movie file");

        assert!(store
            .watch_state(user.id, movie)
            .await
            .expect("watch state")
            .is_none());
        assert!(store
            .watch_map(user.id, &[movie])
            .await
            .expect("watch map")
            .is_empty());
        store
            .put_progress(user.id, movie, 4_000, Some(10_000))
            .await
            .expect("progress");
        store
            .put_progress_at(user.id, episodes[0], 200, Some(1_000), Some(1))
            .await
            .expect("dated progress");
        assert!(store
            .watch_state(user.id, movie)
            .await
            .expect("watch state")
            .is_some());
        assert_eq!(
            store
                .watch_map(user.id, &[movie])
                .await
                .expect("watch map")
                .len(),
            1
        );
        assert!(store
            .continue_watching(user.id, 10)
            .await
            .expect("continue watching")
            .iter()
            .any(|item| item.item.id == movie));

        store
            .set_watched(user.id, movie, true)
            .await
            .expect("set watched");
        let changed = store
            .set_watched_tree(user.id, show, true)
            .await
            .expect("set watched tree");
        assert_eq!(changed.len(), 2);
        let rollup = store.watch_rollup(user.id, show).await.expect("rollup");
        assert_eq!((rollup.watched, rollup.leaves), (2, 2));
        assert_eq!(
            store
                .watch_rollups(user.id, &[show, season])
                .await
                .expect("rollups")
                .len(),
            2
        );

        store
            .set_watched_tree(user.id, show, false)
            .await
            .expect("clear tree");
        store
            .apply_remote_watch(user.id, episodes[0], true, 1_000, Some(1_000), 10)
            .await
            .expect("remote watch");
        assert!(
            store
                .next_up(user.id, 10)
                .await
                .expect("next up")
                .iter()
                .any(|item| item.item.id == episodes[1]),
            "backend {backend}"
        );
    })
    .await;
}

#[tokio::test]
async fn trakt_contract_runs_through_dyn_store() {
    for_each_sqlite_backend(|store, backend| async move {
        let user = store
            .create_user("trakt-contract", "hash", false)
            .await
            .expect("user");
        let library = store
            .create_library(&NewLibrary {
                name: "Trakt Contract Movies".into(),
                kind: LibraryKind::Movies,
                paths: vec![PathBuf::from("/trakt/movies")],
                anime: false,
            })
            .await
            .expect("library");
        let movie = store
            .insert_item(&NewItem {
                library_id: library.id,
                kind: ItemKind::Movie,
                parent_id: None,
                title: "Trakt Contract Movie".into(),
                year: Some(2024),
                season_number: None,
                episode_number: None,
            })
            .await
            .expect("movie");
        store
            .apply_metadata(
                movie,
                &MetadataPatch {
                    tmdb_id: Some(4242),
                    imdb_id: Some("tt0004242".into()),
                    runtime_ms: Some(7_200_000),
                    enriched: true,
                    ..Default::default()
                },
            )
            .await
            .expect("metadata");
        store
            .put_progress(user.id, movie, 1_000, Some(7_200_000))
            .await
            .expect("watch state");

        assert!(store.get_trakt_auth(user.id).await.expect("get").is_none());
        store
            .put_trakt_auth(&TraktAuth {
                user_id: user.id,
                access_token: "access-1".into(),
                refresh_token: "refresh-1".into(),
                expires_at: 100,
                trakt_username: Some("contract".into()),
                connected_at: 1,
                last_sync_at: 0,
                last_activities: None,
            })
            .await
            .expect("put auth");
        assert_eq!(store.list_trakt_auth().await.expect("list").len(), 1);
        store
            .update_trakt_tokens(user.id, "access-2", "refresh-2", 200)
            .await
            .expect("update tokens");
        store
            .set_trakt_sync(user.id, 50, Some(r#"{"movies":{"watched_at":50}}"#))
            .await
            .expect("set sync");
        assert!(
            store
                .trakt_sync_candidates(user.id)
                .await
                .expect("candidates")
                .iter()
                .any(|candidate| candidate.item_id == movie),
            "backend {backend}"
        );
        store.delete_trakt_auth(user.id).await.expect("delete auth");
        assert!(store.get_trakt_auth(user.id).await.expect("get").is_none());
    })
    .await;
}

#[tokio::test]
async fn watched_outbox_contract_runs_through_dyn_store() {
    for_each_sqlite_backend(|store, backend| async move {
        let id = store
            .enqueue_watched(r#"{"type":"movie","watched":true}"#)
            .await
            .expect("enqueue");
        let mut due = store.due_watched(10).await.expect("due");
        assert_eq!(due.len(), 1, "backend {backend}");
        let entry = due.pop().expect("entry");
        assert_eq!(entry.id, id);
        store
            .settle_watched(&OutboxEntry {
                attempts: 1,
                status: "ok".into(),
                ..entry
            })
            .await
            .expect("settle");
        assert_eq!(
            store.watched_outbox_counts().await.expect("counts"),
            (0, 1, 0)
        );
    })
    .await;
}

async fn seed_file(store: &Arc<dyn Store>, prefix: &str) -> (i64, i64) {
    let user = store
        .create_user(&format!("{prefix}-user"), "hash", false)
        .await
        .expect("seed user");
    let library = store
        .create_library(&NewLibrary {
            name: format!("{prefix} Library"),
            kind: LibraryKind::Movies,
            paths: vec![PathBuf::from(format!("/{prefix}"))],
            anime: false,
        })
        .await
        .expect("seed library");
    let item = store
        .insert_item(&NewItem {
            library_id: library.id,
            kind: ItemKind::Movie,
            parent_id: None,
            title: format!("{prefix} Movie"),
            year: Some(2024),
            season_number: None,
            episode_number: None,
        })
        .await
        .expect("seed item");
    let file = store
        .upsert_file(
            item,
            &format!("/{prefix}/movie.mkv"),
            10_000,
            1,
            &ProbeResult {
                duration_ms: Some(7_200_000),
                container: Some("mkv".into()),
                ..Default::default()
            },
        )
        .await
        .expect("seed file");
    (user.id, file)
}

#[tokio::test]
async fn transcode_cache_contract_runs_through_dyn_store() {
    for_each_sqlite_backend(|store, backend| async move {
        let (_, file) = seed_file(&store, "cache-contract").await;
        let node = "cache-node";
        assert!(store
            .cache_hit("recipe", node)
            .await
            .expect("miss")
            .is_none());
        assert!(store
            .claim_cache_entry("recipe", file, 1, node, "aa/recipe")
            .await
            .expect("claim"));
        assert!(!store
            .claim_cache_entry("recipe", file, 1, node, "bb/loser")
            .await
            .expect("duplicate claim"));
        store
            .touch_cache_claim("recipe", node)
            .await
            .expect("touch claim");
        assert_eq!(
            store
                .stale_cache_claims(node, i64::MAX)
                .await
                .expect("stale claims")
                .len(),
            1
        );
        assert_eq!(store.all_cache_rows(node).await.expect("all rows").len(), 1);
        store
            .complete_cache_entry("recipe", node, 4_096)
            .await
            .expect("complete");
        assert!(store
            .cache_hit("recipe", node)
            .await
            .expect("hit")
            .is_some());
        store
            .touch_cache_entry("recipe", node)
            .await
            .expect("touch entry");
        assert_eq!(store.cache_by_age(node, 10).await.expect("by age").len(), 1);
        assert_eq!(store.cache_bytes(node).await.expect("bytes"), 4_096);
        store
            .forget_cache_entry("recipe", node, "local")
            .await
            .expect("forget");
        assert!(
            store
                .all_cache_rows(node)
                .await
                .expect("all rows")
                .is_empty(),
            "backend {backend}"
        );
    })
    .await;
}

fn offline_request(id: &str, request_id: &str, user_id: i64, file_id: i64) -> NewOfflinePackage {
    NewOfflinePackage {
        id: id.into(),
        request_id: request_id.into(),
        user_id,
        file_id,
        node_id: "offline-node".into(),
        source_path: "/offline-contract/movie.mkv".into(),
        source_size: 10_000,
        source_mtime: 1,
        target_height: 1_080,
        output_width: Some(1_920),
        output_height: Some(1_080),
        audio_index: Some(0),
        audio_offset_ms: 0,
        subtitle_index: None,
        subtitle_language: None,
        subtitle_mode: "none".into(),
        estimated_bytes: 5_000,
        reserved_bytes: 5_000,
        expires_at: 10_000,
    }
}

#[tokio::test]
async fn offline_package_contract_runs_through_dyn_store() {
    for_each_sqlite_backend(|store, backend| async move {
        let (user_id, file_id) = seed_file(&store, "offline-contract").await;
        let first = offline_request("package-1", "request-1", user_id, file_id);
        assert!(matches!(
            store
                .create_offline_package(&first, 10, 100_000, 100_000)
                .await
                .expect("create package"),
            OfflineCreateOutcome::Created(_)
        ));
        assert!(matches!(
            store
                .create_offline_package(&first, 10, 100_000, 100_000)
                .await
                .expect("idempotent create"),
            OfflineCreateOutcome::Existing(_)
        ));
        assert!(store
            .offline_package_for_user(&first.id, user_id)
            .await
            .expect("package lookup")
            .is_some());
        assert!(store
            .renew_offline_package_for_user(&first.id, user_id, 20_000)
            .await
            .expect("renew package")
            .is_some());
        assert!(!store
            .offline_activity_packages("offline-node", 1, 0, 10)
            .await
            .expect("activity")
            .is_empty());
        let stats = store
            .offline_package_stats("offline-node", 1)
            .await
            .expect("stats");
        assert_eq!(stats.queued, 1);
        assert_eq!(
            store
                .reset_interrupted_offline_packages("offline-node")
                .await
                .expect("reset"),
            0
        );
        assert_eq!(
            store
                .claim_next_offline_package("offline-node")
                .await
                .expect("claim")
                .expect("package")
                .id,
            first.id
        );
        assert!(store
            .requeue_offline_package(&first.id)
            .await
            .expect("requeue"));
        store
            .claim_next_offline_package("offline-node")
            .await
            .expect("claim")
            .expect("package");
        assert!(store
            .set_offline_package_recipe(&first.id, "offline-recipe")
            .await
            .expect("set recipe"));
        assert!(store
            .update_offline_progress(&first.id, "video", 500)
            .await
            .expect("progress"));
        assert!(store
            .mark_offline_package_ready(&first.id, "offline-recipe", 4_000, 7_200_000)
            .await
            .expect("ready"));
        assert!(matches!(
            store
                .put_offline_lease(&first.id, user_id, "lease-hash", 30_000)
                .await
                .expect("lease"),
            OfflineLeaseOutcome::Created(_)
        ));
        assert!(matches!(
            store
                .put_offline_lease(&first.id, user_id, "lease-hash", 40_000)
                .await
                .expect("renew lease"),
            OfflineLeaseOutcome::Renewed(_)
        ));
        assert!(store
            .offline_package_for_lease("lease-hash", 1, 50_000)
            .await
            .expect("lease lookup")
            .is_some());

        let failed = offline_request("package-2", "request-2", user_id, file_id);
        store
            .create_offline_package(&failed, 10, 100_000, 100_000)
            .await
            .expect("create failed fixture");
        assert!(store
            .fail_offline_package(&failed.id, "video", "encoder", "contract failure")
            .await
            .expect("fail package"));

        let mut expired = offline_request("package-3", "request-3", user_id, file_id);
        expired.expires_at = 1;
        store
            .create_offline_package(&expired, 10, 100_000, 100_000)
            .await
            .expect("create expired fixture");
        assert!(store.expire_offline_packages(2).await.expect("expire") >= 1);
        assert!(
            store
                .delete_offline_package(&first.id, user_id)
                .await
                .expect("delete"),
            "backend {backend}"
        );
    })
    .await;
}
