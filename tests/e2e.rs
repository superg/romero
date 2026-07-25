use std::fs;
use std::io::Write;
use std::path::Path;

use romero::{ProgressEvent, run, run_with_progress};
use sha1::{Digest, Sha1};
use tempfile::tempdir;
use zip::ZipWriter;
use zip::write::SimpleFileOptions;

fn sha1(bytes: &[u8]) -> String {
    let mut hasher = Sha1::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

fn rom(name: &str, contents: &[u8]) -> String {
    format!(
        r#"<rom name="{name}" size="{}" sha1="{}"/>"#,
        contents.len(),
        sha1(contents)
    )
}

fn dat(system: &str, games: &str) -> String {
    format!(
        r#"<?xml version="1.0"?>
<datafile>
  <header>
    <name>{system}</name>
    <date>2026-07-24 13-57-31</date>
  </header>
  {games}
</datafile>"#
    )
}

fn prepare_roots(root: &Path) {
    fs::create_dir_all(root.join("work")).unwrap();
    fs::create_dir_all(root.join("dats")).unwrap();
}

#[test]
fn reconciles_cue_game_and_reuses_cache_after_root_relocation() {
    let temporary = tempdir().unwrap();
    let first_root = temporary.path().join("first");
    fs::create_dir(&first_root).unwrap();
    prepare_roots(&first_root);

    let bin = b"raw disc bytes";
    let source_cue = b"FILE \"download.bin\" BINARY\r\n";
    let final_cue = b"FILE \"Final Game.bin\" BINARY\r\n";
    let game = format!(
        r#"<game name="Final Game">{}{}</game>"#,
        rom("Final Game.cue", final_cue),
        rom("Final Game.bin", bin)
    );
    fs::write(
        first_root.join("dats/catalog.dat"),
        dat("Sony - PlayStation", &game),
    )
    .unwrap();
    fs::write(first_root.join("work/download.bin"), bin).unwrap();
    fs::write(first_root.join("work/template.cue"), source_cue).unwrap();

    let first = run(&first_root).unwrap();
    assert_eq!(first.promotions, 1);
    assert_eq!(
        fs::read(first_root.join("library/Sony - PlayStation/Final Game.bin")).unwrap(),
        bin
    );
    assert_eq!(
        fs::read(first_root.join("library/Sony - PlayStation/Final Game.cue")).unwrap(),
        final_cue
    );
    assert_eq!(
        fs::read(first_root.join("library/Sony - PlayStation.miss")).unwrap(),
        b""
    );
    assert!(first_root.join(".romero.sqlite3").is_file());

    let relocated = temporary.path().join("relocated");
    fs::rename(&first_root, &relocated).unwrap();
    let second = run(&relocated).unwrap();
    assert_eq!(second.cache_misses, 0);
    assert_eq!(second.cache_hits, 2);
    assert_eq!(second.complete_games, 1);
}

#[test]
fn promotes_multiple_complete_cue_games_sequentially() {
    let root = tempdir().unwrap();
    prepare_roots(root.path());
    let first_bin = b"first disc bytes";
    let second_bin = b"second disc bytes";
    let first_source_cue = b"FILE \"first-download.bin\" BINARY\n";
    let second_source_cue = b"FILE \"second-download.bin\" BINARY\n";
    let first_cue = b"FILE \"First.bin\" BINARY\n";
    let second_cue = b"FILE \"Second.bin\" BINARY\n";
    let games = format!(
        r#"<game name="First Game">{}{}</game>
<game name="Second Game">{}{}</game>"#,
        rom("First.cue", first_cue),
        rom("First.bin", first_bin),
        rom("Second.cue", second_cue),
        rom("Second.bin", second_bin),
    );
    fs::write(root.path().join("dats/catalog.dat"), dat("System", &games)).unwrap();
    fs::write(root.path().join("work/first-download.bin"), first_bin).unwrap();
    fs::write(
        root.path().join("work/first-template.cue"),
        first_source_cue,
    )
    .unwrap();
    fs::write(root.path().join("work/second-download.bin"), second_bin).unwrap();
    fs::write(
        root.path().join("work/second-template.cue"),
        second_source_cue,
    )
    .unwrap();

    let mut events = Vec::new();
    let summary = run_with_progress(root.path(), |event| events.push(event.clone()))
        .expect("valid CUE games");

    assert_eq!(summary.promotions, 2);
    assert_eq!(summary.complete_games, 2);
    assert_eq!(summary.missing_games, 0);
    assert_eq!(
        fs::read(root.path().join("library/System/First.bin")).unwrap(),
        first_bin
    );
    assert_eq!(
        fs::read(root.path().join("library/System/First.cue")).unwrap(),
        first_cue
    );
    assert_eq!(
        fs::read(root.path().join("library/System/Second.bin")).unwrap(),
        second_bin
    );
    assert_eq!(
        fs::read(root.path().join("library/System/Second.cue")).unwrap(),
        second_cue
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, ProgressEvent::PromotingGame { .. }))
            .count(),
        2
    );
    assert!(
        fs::read_dir(root.path().join("work"))
            .unwrap()
            .next()
            .is_none()
    );
}

#[test]
fn each_completed_hash_survives_interruption_before_the_next_file() {
    use std::panic::{AssertUnwindSafe, catch_unwind};

    let root = tempdir().unwrap();
    prepare_roots(root.path());
    fs::write(root.path().join("work/first.bin"), b"first").unwrap();
    fs::write(root.path().join("work/second.bin"), b"second").unwrap();

    let interrupted = catch_unwind(AssertUnwindSafe(|| {
        let _ = run_with_progress(root.path(), |event| {
            if matches!(
                event,
                ProgressEvent::Hashing { path, .. }
                    if path == Path::new("work/second.bin")
            ) {
                panic!("simulated interruption");
            }
        });
    }));
    assert!(interrupted.is_err());

    let mut resumed_events = Vec::new();
    let resumed =
        run_with_progress(root.path(), |event| resumed_events.push(event.clone())).unwrap();

    assert_eq!(resumed.cache_hits, 1);
    assert_eq!(resumed.cache_misses, 1);
    assert!(resumed_events.contains(&ProgressEvent::CacheHit {
        path: Path::new("work/first.bin").to_path_buf(),
    }));
    assert!(resumed_events.contains(&ProgressEvent::Hashing {
        path: Path::new("work/second.bin").to_path_buf(),
        size: 6,
    }));
    assert!(resumed_events.contains(&ProgressEvent::HashSaved {
        path: Path::new("work/second.bin").to_path_buf(),
    }));
    assert!(!resumed_events.iter().any(|event| {
        matches!(
            event,
            ProgressEvent::Hashing { path, .. }
                if path == Path::new("work/first.bin")
        )
    }));
}

#[test]
fn loads_dat_entries_from_zip_without_extracting_them() {
    let root = tempdir().unwrap();
    prepare_roots(root.path());
    let payload = b"zip catalog payload";
    let game = format!(
        r#"<game name="Archive Game">{}</game>"#,
        rom("Archive Game.rom", payload)
    );
    let archive_path = root.path().join("dats/catalogs.zip");
    let archive = fs::File::create(&archive_path).unwrap();
    let mut writer = ZipWriter::new(archive);
    writer
        .start_file(
            "nested/catalog.dat",
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated),
        )
        .unwrap();
    writer
        .write_all(dat("Archive System", &game).as_bytes())
        .unwrap();
    writer.finish().unwrap();
    fs::write(root.path().join("work/unrecognized-name.rom"), payload).unwrap();

    run(root.path()).unwrap();

    assert_eq!(
        fs::read(root.path().join("library/Archive System/Archive Game.rom")).unwrap(),
        payload
    );
    assert!(!root.path().join("nested").exists());
}

#[test]
fn quarantines_unknown_library_directory_with_contents_intact() {
    let root = tempdir().unwrap();
    prepare_roots(root.path());
    fs::write(root.path().join("outside.bin"), b"outside").unwrap();
    fs::create_dir_all(root.path().join("library/Unknown/nested")).unwrap();
    fs::write(
        root.path().join("library/Unknown/nested/evidence.bin"),
        b"evidence",
    )
    .unwrap();
    fs::create_dir(root.path().join("work/Unknown")).unwrap();

    let summary = run(root.path()).unwrap();

    assert_eq!(summary.quarantined_directories, 1);
    assert_eq!(
        fs::read(root.path().join("work/Unknown.1/nested/evidence.bin")).unwrap(),
        b"evidence"
    );
    assert!(!root.path().join("library/Unknown").exists());
    assert_eq!(
        fs::read(root.path().join("outside.bin")).unwrap(),
        b"outside"
    );
}

#[test]
fn partial_configuration_uses_defaults_for_missing_paths() {
    let root = tempdir().unwrap();
    fs::write(root.path().join("romero.yaml"), "work_path: intake\n").unwrap();

    run(root.path()).unwrap();

    assert!(root.path().join("library").is_dir());
    assert!(root.path().join("intake").is_dir());
    assert!(root.path().join("dats").is_dir());
    assert!(root.path().join(".romero.sqlite3").is_file());
    assert!(!root.path().join("work").exists());
}

#[test]
fn invalid_configuration_fails_before_managed_directories_are_created() {
    let root = tempdir().unwrap();
    fs::write(
        root.path().join("romero.yaml"),
        "library_path: ../outside\n",
    )
    .unwrap();

    assert!(run(root.path()).is_err());
    assert!(!root.path().join("library").exists());
    assert!(!root.path().join("work").exists());
    assert!(!root.path().join("dats").exists());
}

#[test]
fn conflicting_dats_fail_before_missing_managed_directories_are_created() {
    let root = tempdir().unwrap();
    fs::create_dir(root.path().join("dats")).unwrap();
    let first = dat(
        "System",
        &format!(
            r#"<game name="First">{}</game>"#,
            rom("First.bin", b"first")
        ),
    );
    let second = dat(
        "System",
        &format!(
            r#"<game name="Second">{}</game>"#,
            rom("Second.bin", b"second")
        ),
    );
    fs::write(root.path().join("dats/first.dat"), first).unwrap();
    fs::write(root.path().join("dats/second.dat"), second).unwrap();

    assert!(run(root.path()).is_err());
    assert!(!root.path().join("library").exists());
    assert!(!root.path().join("work").exists());
    assert!(!root.path().join(".romero.sqlite3").exists());
}

#[test]
fn concurrent_run_fails_immediately_on_the_exclusive_cache_lock() {
    let root = tempdir().unwrap();
    run(root.path()).unwrap();
    let connection = rusqlite::Connection::open(root.path().join(".romero.sqlite3")).unwrap();
    connection.execute_batch("BEGIN EXCLUSIVE").unwrap();

    let error = run(root.path()).unwrap_err();

    assert!(error.to_string().contains("already running"));
}

#[cfg(unix)]
#[test]
fn moved_hash_remains_cached_after_a_post_promotion_report_failure() {
    use std::os::unix::fs::PermissionsExt as _;

    let root = tempdir().unwrap();
    prepare_roots(root.path());
    let payload = b"payload";
    let game = format!(r#"<game name="Game">{}</game>"#, rom("Game.bin", payload));
    fs::write(root.path().join("dats/catalog.dat"), dat("System", &game)).unwrap();
    run(root.path()).unwrap();
    assert_eq!(
        fs::read(root.path().join("library/System.miss")).unwrap(),
        b"Game\n"
    );

    fs::write(root.path().join("work/download.bin"), payload).unwrap();
    let library = root.path().join("library");
    fs::set_permissions(&library, fs::Permissions::from_mode(0o555)).unwrap();
    let failed = run(root.path());
    fs::set_permissions(&library, fs::Permissions::from_mode(0o755)).unwrap();

    assert!(failed.is_err());
    assert!(root.path().join("library/System/Game.bin").is_file());
    let recovered = run(root.path()).unwrap();
    assert_eq!(recovered.cache_hits, 1);
    assert_eq!(recovered.cache_misses, 0);
    assert_eq!(
        fs::read(root.path().join("library/System.miss")).unwrap(),
        b""
    );
}

#[cfg(unix)]
#[test]
fn dat_symlinks_and_managed_path_symlink_components_are_rejected() {
    use std::os::unix::fs::symlink;

    let dat_root = tempdir().unwrap();
    fs::create_dir(dat_root.path().join("dats")).unwrap();
    fs::write(dat_root.path().join("catalog.dat"), dat("System", "")).unwrap();
    symlink(
        dat_root.path().join("catalog.dat"),
        dat_root.path().join("dats/link.dat"),
    )
    .unwrap();
    assert!(run(dat_root.path()).is_err());
    assert!(!dat_root.path().join("library").exists());

    let config_root = tempdir().unwrap();
    let outside = tempdir().unwrap();
    symlink(outside.path(), config_root.path().join("linked")).unwrap();
    fs::write(
        config_root.path().join("romero.yaml"),
        "library_path: linked/library\n",
    )
    .unwrap();
    assert!(run(config_root.path()).is_err());
    assert!(!outside.path().join("library").exists());
}
