use std::fs::{self, File, FileTimes};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[test]
fn search_sorts_before_limit_and_preserves_path_default() {
    let root = std::env::temp_dir().join(format!(
        "lightbooru-cli-sort-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(root.join("a")).unwrap();
    fs::create_dir_all(root.join("b")).unwrap();
    for (name, seconds) in [("a/z.jpg", 2), ("b/a.jpg", 1), ("b/b.jpg", 3)] {
        File::create(root.join(name))
            .unwrap()
            .set_times(FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(seconds)))
            .unwrap();
        fs::write(root.join(format!("{name}.json")), r#"{"tags":["cat"]}"#).unwrap();
    }
    for (sort, expected) in [
        (None, "a/z.jpg"),
        (Some("filename"), "b/a.jpg"),
        (Some("mtime-desc"), "b/b.jpg"),
        (Some("mtime-asc"), "b/a.jpg"),
    ] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_booructl"));
        command
            .arg("--base")
            .arg(&root)
            .args(["search", "cat", "--limit", "1"]);
        if let Some(sort) = sort {
            command.args(["--sort", sort]);
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            String::from_utf8(output.stdout).unwrap().trim(),
            root.join(expected).to_str().unwrap()
        );
    }
    fs::remove_dir_all(root).unwrap();
}
