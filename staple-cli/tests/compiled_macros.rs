#![cfg(unix)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "compiled-macros-{label}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("tempdir should be created");
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn repo_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crate sits under workspace root")
}

fn copy_dir_all(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).expect("destination directory should be created");
    for entry in fs::read_dir(src).expect("source directory should be readable") {
        let entry = entry.expect("directory entry should be readable");
        let name = entry.file_name();
        if name == "target" || name == "build" {
            continue;
        }
        let src_path = entry.path();
        let dst_path = dst.join(&name);
        if entry.file_type().expect("file type").is_dir() {
            copy_dir_all(&src_path, &dst_path);
        } else {
            fs::copy(&src_path, &dst_path).expect("file should copy");
        }
    }
}

/// A `staple` command with `cache_home` as its cache. The cache starts as a
/// copy of the shared one, so it holds the standard library's package library
/// but nothing another test built: package fingerprints don't depend on where
/// a package lives, so tests sharing one cache would see each other's builds.
fn staple_cmd(cwd: &Path, cache_home: &Path) -> Command {
    ensure_shared_stdlib();
    if !cache_home.exists() {
        copy_tree(shared_cache(), cache_home);
    }
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_staple"));
    cmd.current_dir(cwd)
        .env("XDG_CACHE_HOME", cache_home)
        .env("STAPLE_MACRO_STATS", "1");
    cmd
}

/// Copies every file under `src` to `dst`.
fn copy_tree(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).expect("destination directory should be created");
    for entry in fs::read_dir(src).expect("source directory should be readable") {
        let entry = entry.expect("directory entry should be readable");
        let dst_path = dst.join(entry.file_name());
        if entry.file_type().expect("file type").is_dir() {
            copy_tree(&entry.path(), &dst_path);
        } else {
            fs::copy(entry.path(), &dst_path).expect("file should copy");
        }
    }
}

/// The cache the standard library's package library is built into once for
/// the whole binary. Tests copy it rather than write to it.
fn shared_cache() -> &'static Path {
    use std::sync::OnceLock;
    static CACHE: OnceLock<PathBuf> = OnceLock::new();
    CACHE.get_or_init(|| {
        let path = Path::new(env!("CARGO_TARGET_TMPDIR")).join("compiled-macros-shared-cache");
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("shared macro cache should be created");
        path
    })
}

/// Builds the standard library's package library once, by expanding a trivial
/// package that depends on it.
fn ensure_shared_stdlib() {
    use std::sync::OnceLock;
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        let temp = TempDir::new("shared-stdlib");
        fs::create_dir_all(temp.0.join("src")).unwrap();
        fs::write(
            temp.0.join("staple.kdl"),
            "package \"shared_stdlib\" {\n    entry \"src/main.sta\"\n}\n",
        )
        .unwrap();
        fs::write(temp.0.join("src/main.sta"), "pub def main = () => 0\n").unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_staple"))
            .current_dir(&temp.0)
            .env("XDG_CACHE_HOME", shared_cache())
            .args(["check", "--macro-engine=compiled", "--stdlib"])
            .arg(repo_root().join("stdlib"))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "standard library macro build failed: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    });
}

fn package_libraries(stats: &str) -> Vec<String> {
    stats
        .lines()
        .filter_map(|line| line.trim_start().strip_prefix("package #"))
        .filter_map(|rest| rest.split_once(": ").map(|(_, name)| name.to_owned()))
        .collect()
}

fn closure_libraries(stats: &str) -> Vec<String> {
    stats
        .lines()
        .filter_map(|line| line.trim_start().strip_prefix("lib #"))
        .filter_map(|rest| rest.split_once(": ").map(|(_, names)| names.to_owned()))
        .collect()
}

#[test]
#[ignore = "builds macro libraries; run scripts/test-macro-libraries.sh"]
fn user_package_macro_used_in_same_module() {
    let temp = TempDir::new("same-module");
    let pkg = temp.0.join("pkg");
    let cache = temp.0.join("cache");
    fs::create_dir_all(pkg.join("src")).unwrap();
    fs::write(
        pkg.join("staple.kdl"),
        "package \"same_module\" {\n    entry \"src/main.sta\"\n}\n",
    )
    .unwrap();
    fs::write(
        pkg.join("src/main.sta"),
        "\
use std.syntax.*
use std.flow.if
use std.io.(print, println)

macro double_expr: Expr -> Expr = expr => {
    let doubled: Expr = parse_quote { ($expr) + ($expr) }
    doubled
}

pub def main = () => {
    let result = double_expr 21
    if (result == 42) { println \"42\" } else { println \"wrong\" }
}

main ()
",
    )
    .unwrap();

    let stdlib = repo_root().join("stdlib");

    let check = staple_cmd(&pkg, &cache)
        .args(["check", "--macro-engine=compiled", "--stdlib"])
        .arg(&stdlib)
        .output()
        .unwrap();
    assert!(
        check.status.success(),
        "check failed: stdout={} stderr={}",
        String::from_utf8_lossy(&check.stdout),
        String::from_utf8_lossy(&check.stderr)
    );
    let check_err = String::from_utf8_lossy(&check.stderr);
    assert!(
        check_err.contains("double_expr"),
        "expected double_expr in macro stats, got: {check_err}"
    );

    let expand = staple_cmd(&pkg, &cache)
        .args(["expand", "src/main.sta", "--macro-engine=compiled", "--stdlib"])
        .arg(&stdlib)
        .output()
        .unwrap();
    assert!(
        expand.status.success(),
        "expand failed: stdout={} stderr={}",
        String::from_utf8_lossy(&expand.stdout),
        String::from_utf8_lossy(&expand.stderr)
    );

    let build = staple_cmd(&pkg, &cache)
        .args(["build", "--macro-engine=compiled", "--stdlib"])
        .arg(&stdlib)
        .output()
        .unwrap();
    assert!(
        build.status.success(),
        "build failed: stdout={} stderr={}",
        String::from_utf8_lossy(&build.stdout),
        String::from_utf8_lossy(&build.stderr)
    );

    let run = staple_cmd(&pkg, &cache)
        .args(["run", "--macro-engine=compiled", "--stdlib"])
        .arg(&stdlib)
        .output()
        .unwrap();
    assert!(
        run.status.success(),
        "run failed: stdout={} stderr={}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&run.stdout), "42\n");
}

#[test]
#[ignore = "builds macro libraries; run scripts/test-macro-libraries.sh"]
fn user_package_macro_closure_uses_earlier_macro() {
    let temp = TempDir::new("stage-tower");
    let pkg = temp.0.join("pkg");
    let cache = temp.0.join("cache");
    fs::create_dir_all(pkg.join("src")).unwrap();
    fs::write(
        pkg.join("staple.kdl"),
        "package \"stage_tower\" {\n    entry \"src/main.sta\"\n}\n",
    )
    .unwrap();
    fs::write(
        pkg.join("src/main.sta"),
        "\
use std.syntax.*
use std.flow.if
use std.io.(print, println)

macro add_one: Expr -> Expr = expr => parse_quote { ($expr) + 1 }

macro double_plus_two: Expr -> Expr = expr => match (add_one 20 == 21) {
    True => parse_quote { ($expr) * 2 },
    False => parse_quote { 0 },
}

pub def main = () => {
    let result = double_plus_two 21
    if (result == 42) { println \"42\" } else { println \"wrong\" }
}

main ()
",
    )
    .unwrap();

    let stdlib = repo_root().join("stdlib");

    let run = staple_cmd(&pkg, &cache)
        .args(["run", "--macro-engine=compiled", "--stdlib"])
        .arg(&stdlib)
        .output()
        .unwrap();
    assert!(
        run.status.success(),
        "run failed: stdout={} stderr={}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&run.stdout), "42\n");
    // `double_plus_two` runs compiled, with `add_one`'s call in its body
    // expanded first. `add_one` is only a quotation, so it is a template the
    // parent expands, and gets no library.
    let stats = String::from_utf8_lossy(&run.stderr);
    let has_library = |name: &str| {
        stats
            .lines()
            .any(|line| line.trim_start().starts_with("lib #") && line.ends_with(&format!(": {name}")))
    };
    assert!(has_library("double_plus_two"), "{stats}");
    assert!(!has_library("add_one"), "{stats}");

    let compile_out = temp.0.join("standalone-bin");
    let compile = staple_cmd(&pkg, &cache)
        .args(["compile", "src/main.sta", "-o"])
        .arg(&compile_out)
        .args(["--macro-engine=compiled", "--stdlib"])
        .arg(&stdlib)
        .output()
        .unwrap();
    assert!(
        compile.status.success(),
        "compile failed: stdout={} stderr={}",
        String::from_utf8_lossy(&compile.stdout),
        String::from_utf8_lossy(&compile.stderr)
    );
    let compiled_run = Command::new(&compile_out).output().unwrap();
    assert!(compiled_run.status.success());
    assert_eq!(String::from_utf8_lossy(&compiled_run.stdout), "42\n");
}

#[test]
#[ignore = "builds macro libraries; run scripts/test-macro-libraries.sh"]
fn stdlib_builds_with_compiled_engine() {
    let temp = TempDir::new("stdlib-build");
    let stdlib_copy = temp.0.join("stdlib");
    let cache = temp.0.join("cache");
    copy_dir_all(&repo_root().join("stdlib"), &stdlib_copy);

    let build = staple_cmd(&stdlib_copy, &cache)
        .args(["build", "--macro-engine=compiled", "--stdlib"])
        .arg(&stdlib_copy)
        .output()
        .unwrap();
    assert!(
        build.status.success(),
        "stdlib build failed: stdout={} stderr={}",
        String::from_utf8_lossy(&build.stdout),
        String::from_utf8_lossy(&build.stderr)
    );
    let stderr = String::from_utf8_lossy(&build.stderr);
    assert!(
        stderr.contains("closure libraries built"),
        "expected macro stats in stderr, got: {stderr}"
    );
}

/// Writes a package named `name` whose entry module is `source`.
fn write_package(root: &Path, name: &str, source: &str) -> PathBuf {
    let pkg = root.join(name);
    fs::create_dir_all(pkg.join("src")).unwrap();
    fs::write(
        pkg.join("staple.kdl"),
        format!("package \"{name}\" {{\n    entry \"src/main.sta\"\n}}\n"),
    )
    .unwrap();
    fs::write(pkg.join("src/main.sta"), source).unwrap();
    pkg
}

#[test]
#[ignore = "builds macro libraries; run scripts/test-macro-libraries.sh"]
fn compiled_macros_are_checked_for_effects() {
    let temp = TempDir::new("effects");
    let pkg = write_package(
        &temp.0,
        "effects",
        "\
use std.syntax.*

extern \"c\" { getpid: () -> I32 }

def sneak = () => () :: {
    let _ = getpid ()
}

macro sneaky: Expr -> Expr = value => {
    sneak ()
    parse_quote { $value }
}

pub def main = () => {
    let _x = sneaky 1
}
",
    );
    let check = staple_cmd(&pkg, &temp.0.join("cache"))
        .args(["check", "--macro-engine=compiled", "--stdlib"])
        .arg(repo_root().join("stdlib"))
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&check.stderr);
    assert!(!check.status.success(), "check should fail: {stderr}");
    assert!(
        stderr.contains(
            "macro `sneaky` reaches `effects.main.sneak` -> `getpid`, a foreign function the \
             standard library doesn't declare, which macro code can't use"
        ),
        "{stderr}"
    );
    // The interpreter's own check doesn't report the macro a second time.
    assert!(!stderr.contains("compile-time name `getpid`"), "{stderr}");
}

#[test]
#[ignore = "builds macro libraries; run scripts/test-macro-libraries.sh"]
fn a_macro_closure_may_use_a_typegroup() {
    let temp = TempDir::new("typegroup");
    let pkg = write_package(
        &temp.0,
        "shapes",
        "\
use std.syntax.*
use std.typegroup.typegroup

pub typegroup Shape {
    Circle,
    Square,
}

def describe = () => U64 :: {
    let _shape: Shape = Shape.Circle
    3
}

macro choose: Expr -> Expr = value => {
    let _n = describe ()
    parse_quote { $value }
}

pub def main = () => {
    let _x = choose 1
}
",
    );
    let check = staple_cmd(&pkg, &temp.0.join("cache"))
        .args(["check", "--macro-engine=compiled", "--stdlib"])
        .arg(repo_root().join("stdlib"))
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&check.stderr);
    assert!(check.status.success(), "check failed: {stderr}");
    assert!(stderr.contains(": choose"), "{stderr}");
}

/// A package graph where `first` and `second` both use a macro from `macros`.
fn write_dependent_packages(root: &Path) {
    let write = |relative: &str, contents: &str| {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    };
    write(
        "macros/staple.kdl",
        "package \"macros\" { kind \"library\" }\n",
    );
    write(
        "macros/src/lib.sta",
        "pub mod\nuse std.clone.Clone\nuse std.syntax.*\n\
         pub macro double: Expr -> Expr = value => {\n\
             let same = Clone.clone value\n\
             parse_quote { ($same) + ($same) }\n\
         }\n",
    );
    for name in ["first", "second"] {
        write(
            &format!("{name}/staple.kdl"),
            &format!(
                "package \"{name}\" {{\n    kind \"library\"\n    \
                 dependencies {{ macros path=\"../macros\" }}\n}}\n"
            ),
        );
        write(
            &format!("{name}/src/lib.sta"),
            "pub mod\nuse std.syntax.*\nuse macros.lib.double\n\
             pub def doubled = (x: I32) => I32 :: double x\n",
        );
    }
    write(
        "app/staple.kdl",
        "package \"app\" {\n    entry \"src/main.sta\"\n    dependencies {\n        \
         first path=\"../first\"\n        second path=\"../second\"\n        \
         macros path=\"../macros\"\n    }\n}\n",
    );
    write(
        "app/src/main.sta",
        "use std.io.println\nuse first.lib.doubled\nuse macros.lib.double\n\
         pub def main = () => {\n\
             let a = doubled 21\n\
             let b = double 4\n\
             println \"${a + b}\"\n\
         }\n",
    );
}

#[test]
#[ignore = "builds package libraries; run scripts/test-macro-libraries.sh"]
fn dependents_share_a_package_library() {
    let temp = TempDir::new("package-shared");
    write_dependent_packages(&temp.0);
    let app = temp.0.join("app");
    let check = staple_cmd(&app, &temp.0.join("cache"))
        .args(["check", "--macro-engine=compiled", "--stdlib"])
        .arg(repo_root().join("stdlib"))
        .output()
        .unwrap();
    let stats = String::from_utf8_lossy(&check.stderr);
    assert!(check.status.success(), "check failed: {stats}");

    let packages = package_libraries(&stats);
    assert_eq!(
        packages.iter().filter(|name| *name == "macros").count(),
        1,
        "one package library for both dependents: {packages:?}"
    );
    assert!(packages.iter().any(|name| name == "first"), "{packages:?}");
    assert!(packages.iter().any(|name| name == "second"), "{packages:?}");
    // Dependencies are built into the user cache, never their own directory.
    for dependency in ["macros", "first", "second"] {
        assert!(
            !temp.0.join(dependency).join("target").exists(),
            "`{dependency}` was written to: {stats}"
        );
    }
}

#[test]
#[ignore = "builds package libraries; run scripts/test-macro-libraries.sh"]
fn a_changed_dependency_rebuilds_its_package_library_once() {
    let temp = TempDir::new("package-rebuilt");
    write_dependent_packages(&temp.0);
    let app = temp.0.join("app");
    let check = || {
        staple_cmd(&app, &temp.0.join("cache"))
            .args(["check", "--macro-engine=compiled", "--stdlib"])
            .arg(repo_root().join("stdlib"))
            .output()
            .unwrap()
    };
    let first = check();
    assert!(
        first.status.success(),
        "check failed: {}",
        String::from_utf8_lossy(&first.stderr)
    );

    let source = temp.0.join("macros/src/lib.sta");
    let mut changed = fs::read_to_string(&source).unwrap();
    changed.push_str("\n// changed\n");
    fs::write(&source, changed).unwrap();

    let second = check();
    let stats = String::from_utf8_lossy(&second.stderr);
    assert!(second.status.success(), "check failed: {stats}");
    let packages = package_libraries(&stats);
    assert_eq!(
        packages.iter().filter(|name| *name == "macros").count(),
        1,
        "the changed dependency is rebuilt once: {packages:?}"
    );
    assert!(packages.iter().any(|name| name == "first"), "{packages:?}");
    assert!(packages.iter().any(|name| name == "second"), "{packages:?}");
}

#[test]
#[ignore = "builds the standard library macro library; run scripts/test-macro-libraries.sh"]
fn a_prebuilt_standard_library_macro_library_is_used_without_a_build() {
    // The shared cache already holds the standard library's package library.
    ensure_shared_stdlib();
    let temp = TempDir::new("prebuilt-stdlib");
    let stdlib = temp.0.join("stdlib");
    copy_dir_all(&repo_root().join("stdlib"), &stdlib);

    let install = staple_cmd(&stdlib, &temp.0.join("install-cache"))
        .args(["build", "--macro-engine=compiled", "--stdlib"])
        .arg(&stdlib)
        .output()
        .unwrap();
    assert!(
        install.status.success(),
        "install failed: {}",
        String::from_utf8_lossy(&install.stderr)
    );
    let macros = fs::read_dir(stdlib.join("target"))
        .unwrap()
        .flatten()
        .map(|entry| entry.path().join("macros"))
        .find(|path| path.is_dir())
        .expect("the prebuilt macro library is installed");
    assert!(
        fs::read_dir(&macros).unwrap().flatten().any(|entry| {
            entry.path().extension().is_some_and(|extension| extension == "dylib"
                || extension == "so")
        }),
        "the installed macro directory holds a library: {}",
        macros.display()
    );

    let pkg = write_package(
        &temp.0,
        "prebuilt_user",
        "\
use std.syntax.*
use std.flow.if
use std.io.println
use std.clone.Clone

macro keep: Expr -> Expr = value => {
    let same = Clone.clone value
    parse_quote { $same }
}

pub def main = () => {
    let x = keep 1
    if (x == 1) { println \"ok\" } else { println \"no\" }
}
",
    );
    // A fresh cache: the standard library's library must come from the
    // install, not from rebuilding it.
    let fresh = temp.0.join("fresh-cache");
    let check = Command::new(env!("CARGO_BIN_EXE_staple"))
        .current_dir(&pkg)
        .env("XDG_CACHE_HOME", &fresh)
        .env("STAPLE_MACRO_STATS", "1")
        .args(["check", "--macro-engine=compiled", "--stdlib"])
        .arg(&stdlib)
        .output()
        .unwrap();
    let stats = String::from_utf8_lossy(&check.stderr);
    assert!(check.status.success(), "check failed: {stats}");
    let packages = package_libraries(&stats);
    assert!(packages.is_empty(), "the install is not rebuilt: {packages:?}");
    let closures = closure_libraries(&stats);
    assert!(
        closures.iter().any(|names| names.split(", ").any(|name| name == "keep")),
        "the user macro still gets a closure library: {closures:?}"
    );
    assert!(
        !closures.iter().any(|names| names.split(", ").any(|name| name == "if")),
        "the standard library's macros come from the install: {closures:?}"
    );
}

#[test]
#[ignore = "builds the standard library's package library; run via scripts/test-macro-libraries.sh"]
fn second_build_reuses_cache_and_closure_change_rebuilds_only_that_macro() {
    let temp = TempDir::new("cache-reuse");
    let cache = temp.0.join("cache");
    let stdlib = repo_root().join("stdlib");
    let pkg = write_package(
        &temp.0,
        "cache_reuse",
        "\
use std.syntax.*
use std.flow.if
use std.io.println
use std.clone.Clone

macro first_macro: Expr -> Expr = value => {
    let cloned = Clone.clone value
    parse_quote { ($cloned) + 1 }
}

macro second_macro: Expr -> Expr = value => {
    let cloned = Clone.clone value
    parse_quote { ($cloned) + 2 }
}

pub def main = () => {
    let a = first_macro 10
    let b = second_macro 10
    if (a + b == 23) { println \"ok\" } else { println \"no\" }
}
",
    );

    let first = staple_cmd(&pkg, &cache)
        .args(["check", "--macro-engine=compiled", "--stdlib"])
        .arg(&stdlib)
        .output()
        .unwrap();
    let first_stats = String::from_utf8_lossy(&first.stderr);
    assert!(first.status.success(), "first check failed: {first_stats}");
    let first_closures = closure_libraries(&first_stats);
    assert!(
        first_closures.iter().any(|n| n == "first_macro"),
        "expected first_macro in {first_closures:?}"
    );
    assert!(
        first_closures.iter().any(|n| n == "second_macro"),
        "expected second_macro in {first_closures:?}"
    );

    // Second build with identical sources loads every library from the cache.
    let second = staple_cmd(&pkg, &cache)
        .args(["check", "--macro-engine=compiled", "--stdlib"])
        .arg(&stdlib)
        .output()
        .unwrap();
    let second_stats = String::from_utf8_lossy(&second.stderr);
    assert!(second.status.success(), "second check failed: {second_stats}");
    assert!(
        package_libraries(&second_stats).is_empty(),
        "second build should not rebuild package libraries: {second_stats}"
    );
    assert!(
        closure_libraries(&second_stats).is_empty(),
        "second build should not rebuild closure libraries: {second_stats}"
    );

    // Changing only second_macro's closure rebuilds second_macro only and prunes its stale library.
    fs::write(
        pkg.join("src/main.sta"),
        "\
use std.syntax.*
use std.flow.if
use std.io.println
use std.clone.Clone

macro first_macro: Expr -> Expr = value => {
    let cloned = Clone.clone value
    parse_quote { ($cloned) + 1 }
}

// A longer body, so every later item moves.
macro second_macro: Expr -> Expr = value => {
    let cloned = Clone.clone value
    let doubled = Clone.clone cloned
    parse_quote { ($doubled) + 300 }
}

pub def main = () => {
    let a = first_macro 10
    let b = second_macro 10
    if (a + b == 321) { println \"ok\" } else { println \"no\" }
}
",
    )
    .unwrap();
    let third = staple_cmd(&pkg, &cache)
        .args(["check", "--macro-engine=compiled", "--stdlib"])
        .arg(&stdlib)
        .output()
        .unwrap();
    let third_stats = String::from_utf8_lossy(&third.stderr);
    assert!(third.status.success(), "third check failed: {third_stats}");
    let third_closures = closure_libraries(&third_stats);
    assert_eq!(
        third_closures,
        vec!["second_macro".to_owned()],
        "only second_macro should rebuild: {third_stats}"
    );

    // Editing only `main`, which no closure contains, rebuilds nothing, even
    // though its new `if` call takes an expansion mark before the macros'.
    let source = fs::read_to_string(pkg.join("src/main.sta")).unwrap();
    let source = source.replace(
        "pub def main = () => {\n",
        "pub def main = () => {\n    if (1 == 1) { println \"first\" } else { println \"never\" }\n",
    );
    fs::write(pkg.join("src/main.sta"), source).unwrap();
    let fourth = staple_cmd(&pkg, &cache)
        .args(["check", "--macro-engine=compiled", "--stdlib"])
        .arg(&stdlib)
        .output()
        .unwrap();
    let fourth_stats = String::from_utf8_lossy(&fourth.stderr);
    assert!(fourth.status.success(), "fourth check failed: {fourth_stats}");
    assert!(
        closure_libraries(&fourth_stats).is_empty(),
        "an edit outside every closure rebuilds nothing: {fourth_stats}"
    );
}

#[test]
#[ignore = "builds the standard library's package library; run via scripts/test-macro-libraries.sh"]
fn corrupt_manifest_rebuilds_closure_library() {
    let temp = TempDir::new("corrupt-manifest");
    let cache = temp.0.join("cache");
    let stdlib = repo_root().join("stdlib");
    let pkg = write_package(
        &temp.0,
        "corrupt_manifest",
        "\
use std.syntax.*
use std.flow.if
use std.io.println
use std.clone.Clone

macro keep_cached: Expr -> Expr = value => {
    let cloned = Clone.clone value
    parse_quote { $cloned }
}

pub def main = () => {
    let x = keep_cached 42
    if (x == 42) { println \"ok\" } else { println \"no\" }
}
",
    );

    let first = staple_cmd(&pkg, &cache)
        .args(["check", "--macro-engine=compiled", "--stdlib"])
        .arg(&stdlib)
        .output()
        .unwrap();
    let first_stats = String::from_utf8_lossy(&first.stderr);
    assert!(first.status.success(), "first check failed: {first_stats}");
    assert_eq!(closure_libraries(&first_stats), vec!["keep_cached".to_owned()]);

    // Corrupt every closure manifest under target/staple/*/macros.
    let mut corrupted = 0usize;
    let target_staple = pkg.join("target/staple");
    for entry in fs::read_dir(&target_staple).unwrap() {
        let macros_dir = entry.unwrap().path().join("macros");
        if !macros_dir.is_dir() {
            continue;
        }
        for file in fs::read_dir(&macros_dir).unwrap() {
            let path = file.unwrap().path();
            if path.extension().and_then(|e| e.to_str()) == Some("manifest")
                && path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.contains("keep_cached"))
            {
                fs::write(&path, b"corrupt-manifest").unwrap();
                corrupted += 1;
            }
        }
    }
    assert_eq!(corrupted, 1, "expected 1 keep_cached manifest to corrupt");

    let second = staple_cmd(&pkg, &cache)
        .args(["check", "--macro-engine=compiled", "--stdlib"])
        .arg(&stdlib)
        .output()
        .unwrap();
    let second_stats = String::from_utf8_lossy(&second.stderr);
    assert!(second.status.success(), "second check failed: {second_stats}");
    assert_eq!(
        closure_libraries(&second_stats),
        vec!["keep_cached".to_owned()],
        "corrupt manifest should force rebuild: {second_stats}"
    );
}

/// `staple expand` on a single file makes its calls through the server, and
/// `--tree-dump` gives the same tree with either engine.
#[test]
#[ignore = "builds macro libraries; run scripts/test-macro-libraries.sh"]
fn a_single_file_expands_the_same_with_either_engine() {
    let temp = TempDir::new("single-file");
    let source = temp.0.join("main.sta");
    fs::write(
        &source,
        "\
use std.syntax.*
use std.clone.Clone

macro keep: Expr -> Expr = value => {
    let same = Clone.clone value
    parse_quote { $same }
}

let a: I32 = if True { keep 1 } else { 2 }
let b: I32 = when { a > 1 => 3, True => keep 4 }
",
    )
    .unwrap();
    let expand = |engine: &str| {
        let output = staple_cmd(&temp.0, &temp.0.join("cache"))
            .args(["expand", "--tree-dump", &format!("--macro-engine={engine}"), "--stdlib"])
            .arg(repo_root().join("stdlib"))
            .arg(&source)
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        assert!(output.status.success(), "{engine} expand failed: {stderr}");
        (String::from_utf8_lossy(&output.stdout).into_owned(), stderr)
    };
    let (interpreted, _) = expand("interp");
    let (compiled, stats) = expand("compiled");
    assert!(stats.contains("calls: keep 2 in"), "{stats}");
    assert!(stats.contains("calls: if "), "{stats}");
    assert!(stats.contains("calls: when "), "{stats}");
    assert!(compiled.contains("Match"), "{compiled}");
    assert_eq!(interpreted, compiled);
}

/// Differences task 12's engine comparison found, each fixed in the compiled
/// engine: an item sequence after a bodyless type, an `@compiled_only` name
/// imported by a closure, iteration (an import from a companion, and a blanket
/// companion method), a generic `typegroup`, and the position of an error in
/// syntax a macro produced inside another macro's code.
#[test]
#[ignore = "builds macro libraries; run scripts/test-macro-libraries.sh"]
fn compiled_expansion_matches_the_interpreter_on_task_12_differences() {
    let temp = TempDir::new("task-12");
    let cache = temp.0.join("cache");
    let run = |name: &str, engine: &str, command: &[&str], source: &str| {
        let path = temp.0.join(name);
        fs::write(&path, source).unwrap();
        let output = staple_cmd(&temp.0, &cache)
            .args(command)
            .arg(format!("--macro-engine={engine}"))
            .arg("--stdlib")
            .arg(repo_root().join("stdlib"))
            .arg(&path)
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        (output.status.success(), stdout, stderr)
    };
    let sources = [
        (
            "bodyless.sta",
            "\
@derive_eq
type Enabled
@derive_eq
type Plain wrap I32
let same: Bool = Enabled == Enabled
",
        ),
        (
            "compiled_only.sta",
            "\
use std.syntax.(Syntax, Expr, Braced, Parse, ParseError, stream, is_empty, parse_quote)
macro first_of: move Braced<Syntax> -> Expr = move body => {
    let Braced (contents) = body
    let mut input = stream contents
    let first = match (Parse.parse.<Expr> input) {
        Ok value => value,
        ParseError (message) => panic message,
    }
    if (is_empty input) { parse_quote { $first } } else { panic \"trailing syntax\" }
}
let value: I32 = first_of { 1 }
",
        ),
        (
            "iteration.sta",
            "\
use std.syntax.*
macro size: move Sequence<Ident<String>> -> Expr = move xs => {
    let n = xs |> count
    if (n == 3) { parse_quote { 3 } } else { parse_quote { 0 } }
}
macro walk: move Sequence<Ident<String>> -> Expr = move xs => {
    let mut total = 0
    for _ in xs { total = total + 1 }
    if (total == 3) { parse_quote { 3 } } else { parse_quote { 0 } }
}
let a: I32 = size x y z
let b: I32 = walk x y z
",
        ),
        (
            "typegroup.sta",
            "\
trait Allowed T {}
impl Allowed I32 {}
typegroup Bounded<T = I32 where Allowed T> { Empty, Value wrap pub T, }
let value: Bounded = Bounded.Value 42
",
        ),
    ];
    for (name, source) in sources {
        let (ok, interpreted, stderr) = run(name, "interp", &["expand", "--tree-dump"], source);
        assert!(ok, "{name}: interpreter failed: {stderr}");
        let (ok, compiled, stderr) = run(name, "compiled", &["expand", "--tree-dump"], source);
        assert!(ok, "{name}: compiled engine failed: {stderr}");
        assert_eq!(interpreted, compiled, "{name}");
        let (ok, _, stderr) = run(name, "compiled", &["check"], source);
        assert!(ok, "{name}: compiled check failed: {stderr}");
    }
    let (ok, _, stderr) = run(
        "located.sta",
        "compiled",
        &["check"],
        "\
use std.syntax.*
macro m: Expr -> Expr = e => if (True) { nope } else { e }
let b: I32 = m 1
let c: I32 = m 2
",
    );
    assert!(!ok);
    assert!(stderr.contains("unknown name `nope` at line 2, column 42"), "{stderr}");
    // A library that fails to build is reported, and built, once.
    assert_eq!(stderr.matches("unknown name `nope`").count(), 1, "{stderr}");
}
