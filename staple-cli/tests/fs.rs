//! Filesystem tests use only fresh temporary directories, through the real CLI.
#![cfg(unix)]
#[cfg(target_os = "linux")]
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::symlink;
use std::{fs, path::Path, process::Command};

#[test]
fn filesystem_round_trips_and_handle_lifetimes() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let scratch = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("fs-{}", std::process::id()));
    fs::create_dir_all(&scratch).unwrap();
    struct Cleanup(std::path::PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    let _cleanup = Cleanup(scratch.clone());
    for incremental in [false, true] {
        let directory = scratch.join(if incremental { "incremental" } else { "whole" });
        fs::create_dir_all(directory.join("outside")).unwrap();
        fs::create_dir_all(directory.join("tree")).unwrap();
        fs::create_dir_all(directory.join("invalid-names")).unwrap();
        fs::write(directory.join("outside/keep"), "safe").unwrap();
        fs::write(directory.join("invalid"), [0xff]).unwrap();
        #[cfg(target_os = "linux")]
        fs::write(
            directory
                .join("invalid-names")
                .join(std::ffi::OsString::from_vec(vec![0xff])),
            "",
        )
        .unwrap();
        symlink("../outside", directory.join("tree/link")).unwrap();
        let source = include_str!("fixtures/fs.sta").replace(
            "// INVALID_NAME_CHECK",
            if cfg!(target_os = "linux") {
                "require (match (read_dir \"invalid-names\") { InvalidUtf8 => True, _ => False })"
            } else {
                ""
            },
        );
        fs::write(directory.join("test.sta"), source).unwrap();
        let mut command = Command::new("sh");
        command
            .args(["-c", "ulimit -n 128; exec \"$@\"", "staple-fs"])
            .arg(env!("CARGO_BIN_EXE_staple"));
        command
            .args(["run", "test.sta", "--stdlib"])
            .arg(root.join("stdlib"))
            .current_dir(&directory)
            .env("XDG_CACHE_HOME", scratch.join("cache"));
        if !incremental {
            command.arg("--no-incremental");
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "incremental={incremental}: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&output.stdout), "fs ok\n");
        assert!(directory.join("outside/keep").exists());
    }
}
