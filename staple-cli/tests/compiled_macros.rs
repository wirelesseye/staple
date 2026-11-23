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

fn staple_cmd(cwd: &Path, cache_home: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_staple"));
    cmd.current_dir(cwd)
        .env("XDG_CACHE_HOME", cache_home)
        .env("STAPLE_MACRO_STATS", "1");
    cmd
}

#[test]
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
