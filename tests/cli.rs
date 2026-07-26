use std::fs;
use std::path::Path;
use std::process::Command;

use sha1::{Digest, Sha1};
use tempfile::tempdir;

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

fn prepare_cue_root(root: &Path) {
    fs::create_dir(root.join("work")).unwrap();
    fs::create_dir(root.join("dats")).unwrap();
    let payload = b"payload";
    let source_cue = b"FILE \"download.bin\" BINARY\n";
    let expected_cue = b"FILE \"Game.bin\" BINARY\n";
    let dat = format!(
        r#"<?xml version="1.0"?>
<datafile>
  <header>
    <name>System</name>
    <date>2026-07-24 13-57-31</date>
  </header>
  <game name="Game">{}{}</game>
</datafile>"#,
        rom("Game.cue", expected_cue),
        rom("Game.bin", payload),
    );
    fs::write(root.join("dats/catalog.dat"), dat).unwrap();
    fs::write(root.join("work/download.bin"), payload).unwrap();
    fs::write(root.join("work/template.cue"), source_cue).unwrap();
}

#[test]
fn no_argument_uses_current_directory() {
    let root = tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_romero"))
        .current_dir(root.path())
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(root.path().join("library").is_dir());
    assert!(root.path().join("work").is_dir());
    assert!(root.path().join("dats").is_dir());
    let progress = String::from_utf8_lossy(&output.stderr);
    assert!(progress.contains("Loading DAT catalogs"));
    assert!(progress.contains("Auditing library"));
    assert!(progress.contains("Processing work directory"));
    assert!(progress.contains("Writing missing-game reports"));
}

#[test]
fn direct_yaml_path_is_rejected_as_a_non_directory_root() {
    let root = tempdir().unwrap();
    let config = root.path().join("romero.yaml");
    fs::write(&config, "").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_romero"))
        .arg(&config)
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("root is not a directory"));
}

#[test]
fn nonexistent_root_is_rejected() {
    let parent = tempdir().unwrap();
    let missing = parent.path().join("missing");

    let output = Command::new(env!("CARGO_BIN_EXE_romero"))
        .arg(&missing)
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("root does not exist"));
}

#[test]
fn verbose_adds_cache_and_per_file_promotion_details() {
    let default_root = tempdir().unwrap();
    prepare_cue_root(default_root.path());
    let default_output = Command::new(env!("CARGO_BIN_EXE_romero"))
        .arg(default_root.path())
        .output()
        .unwrap();
    assert!(
        default_output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&default_output.stderr)
    );
    let default_stderr = String::from_utf8_lossy(&default_output.stderr);
    assert!(default_stderr.contains("Hashing work/download.bin"));
    assert!(default_stderr.contains("Promoting game: System / Game"));
    for hidden in [
        "Hash saved:",
        "Cache committed:",
        "Cache hit:",
        "Moving ROM:",
        "Writing rewritten CUE:",
        "Removing rewritten CUE source:",
    ] {
        assert!(
            !default_stderr.contains(hidden),
            "default stderr unexpectedly contained {hidden}: {default_stderr}"
        );
    }

    let verbose_root = tempdir().unwrap();
    prepare_cue_root(verbose_root.path());
    let verbose_output = Command::new(env!("CARGO_BIN_EXE_romero"))
        .arg("--verbose")
        .arg(verbose_root.path())
        .output()
        .unwrap();
    assert!(
        verbose_output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&verbose_output.stderr)
    );
    let verbose_stderr = String::from_utf8_lossy(&verbose_output.stderr);
    assert!(verbose_stderr.contains("Hash saved: work/download.bin"));
    assert!(verbose_stderr.contains("Cache committed: run complete"));
    assert!(verbose_stderr.contains("Moving ROM: work/download.bin -> library/System/Game.bin"));
    assert!(
        verbose_stderr
            .contains("Writing rewritten CUE: work/template.cue -> library/System/Game.cue")
    );
    assert!(verbose_stderr.contains("Removing rewritten CUE source: work/template.cue"));
    assert_eq!(default_output.stdout, verbose_output.stdout);

    let cached_output = Command::new(env!("CARGO_BIN_EXE_romero"))
        .arg("--verbose")
        .arg(verbose_root.path())
        .output()
        .unwrap();
    assert!(cached_output.status.success());
    assert!(String::from_utf8_lossy(&cached_output.stderr).contains("Cache hit:"));
}

#[test]
fn incomplete_games_explain_every_expected_rom_without_work_prefixes() {
    let root = tempdir().unwrap();
    fs::create_dir(root.path().join("work")).unwrap();
    fs::create_dir(root.path().join("dats")).unwrap();
    fs::create_dir_all(root.path().join("library/System")).unwrap();
    let dat = format!(
        r#"<?xml version="1.0"?>
<datafile>
  <header>
    <name>System</name>
    <date>2026-07-24 13-57-31</date>
  </header>
  <game name="Source">{}</game>
  <game name="Game">{}{}{}{}{}</game>
</datafile>"#,
        rom("Source.bin", b"library"),
        rom("Bad.bin", b"expected bad"),
        rom("Exact.bin", b"exact"),
        rom("Library.bin", b"library"),
        rom("Missing.bin", b"missing"),
        rom("Renamed.bin", b"renamed"),
    );
    fs::write(root.path().join("dats/catalog.dat"), dat).unwrap();
    fs::write(root.path().join("library/System/Source.bin"), b"library").unwrap();
    fs::write(root.path().join("work/Bad.bin"), b"wrong").unwrap();
    fs::write(root.path().join("work/Exact.bin"), b"exact").unwrap();
    fs::write(root.path().join("work/download.bin"), b"renamed").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_romero"))
        .arg(root.path())
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let content_matching = stderr
        .find("Content matching")
        .expect("content matching stage is visible");
    let incomplete = stderr
        .find("Incomplete: System / Game")
        .expect("incomplete content result is visible");
    assert!(content_matching < incomplete);
    assert!(!stderr.contains("CUE matching"));
    assert!(stderr.contains(concat!(
        "Incomplete: System / Game\n",
        "  Bad.bin [MISMATCH]\n",
        "  Exact.bin [OK]\n",
        "  Library.bin [LIBRARY]\n",
        "  Missing.bin [MISSING]\n",
        "  Renamed.bin -> download.bin [OK]\n",
    )));
    assert!(!stdout.contains("Incomplete:"));
    assert!(!stdout.contains("Leftover game:"));
    assert!(!stderr.contains("Renamed.bin -> work/download.bin"));
    assert!(!stderr.contains("match)"));
    assert!(!stderr.contains("matches)"));
    assert!(!stderr.contains('\x1b'));

    let colored_output = Command::new(env!("CARGO_BIN_EXE_romero"))
        .env_remove("NO_COLOR")
        .env("CLICOLOR_FORCE", "1")
        .arg(root.path())
        .output()
        .unwrap();
    assert!(colored_output.status.success());
    let colored_stderr = String::from_utf8_lossy(&colored_output.stderr);
    assert!(colored_stderr.contains("\x1b[96mIncomplete:\x1b[0m System / Game"));
    assert!(colored_stderr.contains("Exact.bin \x1b[32m[OK]\x1b[0m"));
    assert!(colored_stderr.contains("Library.bin \x1b[33m[LIBRARY]\x1b[0m"));
    assert!(colored_stderr.contains("Bad.bin \x1b[38;5;208m[MISMATCH]\x1b[0m"));
    assert!(colored_stderr.contains("Missing.bin \x1b[31m[MISSING]\x1b[0m"));
    assert!(!String::from_utf8_lossy(&colored_output.stdout).contains("Incomplete:"));
}
