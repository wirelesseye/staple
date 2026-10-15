//! Incremental builds through the real `staple` binary: cached objects are
//! reused, invalidated by edits, and produce the same program as the
//! whole-program build.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::SystemTime;

fn repo_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap()
}

struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let path = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("incremental-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn write(&self, relative: &str, contents: &str) {
        let path = self.0.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    fn cache(&self) -> PathBuf {
        self.0.join("cache")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn staple(scratch: &Scratch, stdlib: &Path, directory: &Path, arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_staple"))
        .args(arguments)
        .current_dir(directory)
        .env("STAPLE_STDLIB", stdlib)
        .env("XDG_CACHE_HOME", scratch.cache())
        .env_remove("STAPLE_INCREMENTAL")
        .output()
        .unwrap()
}

fn succeeded(output: &Output) {
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

type Objects = BTreeMap<PathBuf, SystemTime>;

/// The `.o` files under `directory` (recursively) with their modification times.
fn objects(directory: &Path) -> Objects {
    let mut found = BTreeMap::new();
    let Ok(entries) = std::fs::read_dir(directory) else {
        return found;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            found.extend(objects(&path));
        } else if path.extension().is_some_and(|extension| extension == "o") {
            found.insert(path, entry.metadata().unwrap().modified().unwrap());
        }
    }
    found
}

fn names(objects: &Objects) -> Vec<String> {
    objects
        .keys()
        .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
        .collect()
}

fn copy_directory(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap().flatten() {
        let target = to.join(entry.file_name());
        if entry.path().is_dir() {
            copy_directory(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

/// Lets a re-emission show up as a newer modification time on coarse clocks.
fn tick() {
    std::thread::sleep(std::time::Duration::from_millis(1100));
}

#[test]
fn incremental_and_whole_program_runs_agree() {
    let scratch = Scratch::new("agree");
    let stdlib = repo_root().join("stdlib");
    let examples = repo_root().join("staple-compiler/examples");
    for example in [
        "hello_world.sta",
        "traits_and_generics.sta",
        "coroutines.sta",
    ] {
        let file = examples.join(example);
        let file = file.to_str().unwrap();
        let incremental = staple(&scratch, &stdlib, &examples, &["run", file]);
        let whole = staple(
            &scratch,
            &stdlib,
            &examples,
            &["run", "--no-incremental", file],
        );
        assert_eq!(incremental.status, whole.status, "{example}");
        assert_eq!(incremental.stdout, whole.stdout, "{example}");
        assert_eq!(incremental.stderr, whole.stderr, "{example}");
        assert!(!incremental.stdout.is_empty(), "{example} printed nothing");
    }
}

#[test]
fn the_environment_variable_selects_the_whole_program_build() {
    let scratch = Scratch::new("environment");
    let stdlib = repo_root().join("stdlib");
    let file = repo_root().join("staple-compiler/examples/hello_world.sta");
    let output = Command::new(env!("CARGO_BIN_EXE_staple"))
        .args(["run", file.to_str().unwrap()])
        .env("STAPLE_STDLIB", &stdlib)
        .env("XDG_CACHE_HOME", scratch.cache())
        .env("STAPLE_INCREMENTAL", "0")
        .output()
        .unwrap();
    succeeded(&output);
    assert_eq!(String::from_utf8_lossy(&output.stdout), "Hello, world!\n");
    assert!(objects(&scratch.cache()).is_empty());
}

#[test]
fn a_second_run_reuses_the_cached_objects_and_a_stdlib_edit_rebuilds_std() {
    let scratch = Scratch::new("reuse");
    let stdlib = scratch.0.join("stdlib");
    copy_directory(&repo_root().join("stdlib"), &stdlib);
    let file = repo_root().join("staple-compiler/examples/hello_world.sta");
    let arguments = ["run", file.to_str().unwrap()];

    succeeded(&staple(&scratch, &stdlib, &scratch.0, &arguments));
    let first = objects(&scratch.cache());
    let first_names = names(&first);
    assert_eq!(first_names.len(), 2, "{first_names:?}");
    assert!(first_names.iter().any(|name| name.starts_with("std-")));
    assert!(first_names.iter().any(|name| name.starts_with("runtime-")));

    tick();
    succeeded(&staple(&scratch, &stdlib, &scratch.0, &arguments));
    assert_eq!(objects(&scratch.cache()), first);

    let source = stdlib.join("std/io.sta");
    let mut text = std::fs::read_to_string(&source).unwrap();
    text.push_str("\n// edited\n");
    std::fs::write(&source, text).unwrap();
    succeeded(&staple(&scratch, &stdlib, &scratch.0, &arguments));
    let third = objects(&scratch.cache());
    assert_eq!(third.len(), 3, "{:?}", names(&third));
    // The runtime does not depend on the stdlib's sources.
    for (path, time) in &first {
        if path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("runtime-")
        {
            assert_eq!(third.get(path), Some(time));
        }
    }
}

#[test]
fn building_the_stdlib_installs_its_objects_and_later_builds_use_them() {
    let scratch = Scratch::new("stdlib");
    let stdlib = scratch.0.join("stdlib");
    copy_directory(&repo_root().join("stdlib"), &stdlib);
    let manifest = stdlib.join("staple.kdl");
    let output = staple(
        &scratch,
        &stdlib,
        &scratch.0,
        &[
            "build",
            "--manifest-path",
            manifest.to_str().unwrap(),
            "--stdlib",
            stdlib.to_str().unwrap(),
        ],
    );
    succeeded(&output);
    assert!(output.stdout.is_empty() && output.stderr.is_empty());
    let installed = objects(&stdlib.join("target"));
    let installed_names = names(&installed);
    assert_eq!(installed_names.len(), 2, "{installed_names:?}");
    assert!(installed_names.iter().any(|name| name.starts_with("std-")));
    assert!(
        installed_names
            .iter()
            .any(|name| name.starts_with("runtime-"))
    );

    let file = repo_root().join("staple-compiler/examples/hello_world.sta");
    let run = staple(
        &scratch,
        &stdlib,
        &scratch.0,
        &["run", file.to_str().unwrap()],
    );
    succeeded(&run);
    assert_eq!(String::from_utf8_lossy(&run.stdout), "Hello, world!\n");
    assert!(objects(&scratch.cache()).is_empty());
    assert_eq!(objects(&stdlib.join("target")), installed);
}

#[test]
fn library_builds_cache_their_own_and_their_dependencies_objects() {
    let scratch = Scratch::new("library");
    let stdlib = repo_root().join("stdlib");
    scratch.write("leaf/staple.kdl", "package \"leaf\" { kind \"library\" }\n");
    scratch.write(
        "leaf/src/values.sta",
        "pub mod\n\npub def answer = () => 40\n",
    );
    scratch.write(
        "util/staple.kdl",
        "package \"util\" { kind \"library\"; dependencies { leaf path=\"../leaf\" } }\n",
    );
    scratch.write(
        "util/src/math.sta",
        "pub mod\n\nuse leaf.values.answer\n\npub def more = () => answer () + 2\n",
    );
    scratch.write(
        "app/staple.kdl",
        "package \"app\" { dependencies { util path=\"../util\" } }\n",
    );
    scratch.write(
        "app/src/main.sta",
        "use std.io.println\nuse util.math.more\n\nprintln \"${more ()}\"\n",
    );
    let util = scratch.0.join("util");
    let leaf_target = scratch.0.join("leaf/target");
    let util_target = util.join("target");

    let build = staple(&scratch, &stdlib, &util, &["build"]);
    succeeded(&build);
    assert!(build.stdout.is_empty() && build.stderr.is_empty());
    let leaf = names(&objects(&leaf_target));
    let own = names(&objects(&util_target));
    assert_eq!(leaf.len(), 1, "{leaf:?}");
    assert!(leaf[0].starts_with("leaf-"));
    assert_eq!(own.len(), 2, "{own:?}");
    assert!(own.iter().any(|name| name.starts_with("util-")));
    assert!(own.iter().any(|name| name.starts_with("runtime-")));

    tick();
    let before = (objects(&leaf_target), objects(&util_target));
    succeeded(&staple(&scratch, &stdlib, &util, &["build"]));
    assert_eq!((objects(&leaf_target), objects(&util_target)), before);

    // Editing the dependency rebuilds it and its dependent, not the runtime.
    scratch.write(
        "leaf/src/values.sta",
        "pub mod\n\npub def answer = () => 41\n",
    );
    succeeded(&staple(&scratch, &stdlib, &util, &["build"]));
    let after = (objects(&leaf_target), objects(&util_target));
    assert_ne!(names(&after.0), names(&before.0));
    let added = names(&after.1)
        .into_iter()
        .filter(|name| !names(&before.1).contains(name))
        .collect::<Vec<_>>();
    assert_eq!(added.len(), 1, "{added:?}");
    assert!(added[0].starts_with("util-"));

    // An executable built on the cached objects behaves like the whole-program build.
    let app = scratch.0.join("app");
    let incremental = staple(&scratch, &stdlib, &app, &["run"]);
    let whole = staple(&scratch, &stdlib, &app, &["run", "--no-incremental"]);
    succeeded(&incremental);
    assert_eq!(String::from_utf8_lossy(&incremental.stdout), "43\n");
    assert_eq!(incremental.stdout, whole.stdout);
}

#[test]
fn the_install_script_prebuilds_the_stdlib_and_replaces_stale_objects() {
    if Command::new("sh").arg("-c").arg("true").output().is_err() {
        return;
    }
    let scratch = Scratch::new("install");
    let destination = scratch.0.join("installed");
    // A stale object from an earlier install must not survive.
    scratch.write("installed/target/old/stale.o", "");
    let install = || {
        Command::new("sh")
            .arg(repo_root().join("scripts/install-stdlib.sh"))
            .arg(&destination)
            .env("STAPLE", env!("CARGO_BIN_EXE_staple"))
            .env("XDG_CACHE_HOME", scratch.cache())
            .env_remove("STAPLE_INSTALL_OBJECTS")
            .output()
            .unwrap()
    };
    let output = install();
    succeeded(&output);
    assert!(destination.join("std").is_dir());
    assert!(!destination.join("target/old").exists());
    let installed = names(&objects(&destination.join("target")));
    assert_eq!(installed.len(), 2, "{installed:?}");

    succeeded(&install());
    assert_eq!(names(&objects(&destination.join("target"))), installed);

    let file = repo_root().join("staple-compiler/examples/hello_world.sta");
    let run = staple(
        &scratch,
        &destination,
        &scratch.0,
        &["run", file.to_str().unwrap()],
    );
    succeeded(&run);
    assert_eq!(String::from_utf8_lossy(&run.stdout), "Hello, world!\n");
    assert!(objects(&scratch.cache()).is_empty());
}
