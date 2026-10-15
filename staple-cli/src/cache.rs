//! Fingerprints and the on-disk object cache behind incremental compilation.
//!
//! Every non-root package is emitted as its own object, named by a
//! fingerprint of everything that affects how that package lowers: the
//! compiler, the target, the package's sources and resolved features, and the
//! fingerprints of its dependencies. A cached object is therefore valid exactly
//! when its file name matches.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use staple_compiler::LoweredPackage;
use staple_project::{ActiveFeatures, PackageGraph, PackageId};

const FNV_OFFSET: u128 = 0x6c62272e07bb014262b821756295c58d;
const FNV_PRIME: u128 = 0x0000000001000000000000000000013B;

/// A 128-bit FNV-1a hash. Stable across platforms and releases, unlike
/// `std::hash`, so fingerprints can name files that outlive the process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Fingerprint(u128);

impl fmt::Display for Fingerprint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{:032x}", self.0)
    }
}

pub struct Hasher(u128);

impl Hasher {
    pub fn new() -> Self {
        Self(FNV_OFFSET)
    }

    pub fn write(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.0 = (self.0 ^ u128::from(byte)).wrapping_mul(FNV_PRIME);
        }
    }

    /// A length-prefixed string, so adjacent fields cannot run together.
    pub fn write_str(&mut self, text: &str) {
        self.write_framed(text.as_bytes());
    }

    pub fn write_framed(&mut self, bytes: &[u8]) {
        self.write(&(bytes.len() as u64).to_le_bytes());
        self.write(bytes);
    }

    pub fn write_fingerprint(&mut self, fingerprint: Fingerprint) {
        self.write(&fingerprint.0.to_le_bytes());
    }

    pub fn finish(&self) -> Fingerprint {
        Fingerprint(self.0)
    }
}

/// The compiler's identity: its version and the bytes of the running
/// executable. Hashing the executable is the expensive part, so the result is
/// remembered per process and on disk, keyed by the executable's size and
/// modification time.
pub fn compiler_identity() -> Result<Fingerprint, String> {
    static IDENTITY: OnceLock<Result<Fingerprint, String>> = OnceLock::new();
    IDENTITY.get_or_init(compute_compiler_identity).clone()
}

fn compute_compiler_identity() -> Result<Fingerprint, String> {
    let executable = std::env::current_exe()
        .map_err(|error| format!("could not locate the running compiler: {error}"))?;
    let metadata = std::fs::metadata(&executable)
        .map_err(|error| format!("could not inspect `{}`: {error}", executable.display()))?;
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |duration| duration.as_nanos());
    let mut location = Hasher::new();
    location.write(executable.as_os_str().as_encoded_bytes());
    let memo = user_cache_root().map(|root| {
        root.join(format!(
            "identity-{}-{modified:x}-{}",
            metadata.len(),
            location.finish()
        ))
    });
    if let Some(memo) = &memo
        && let Ok(text) = std::fs::read_to_string(memo)
        && let Ok(value) = u128::from_str_radix(text.trim(), 16)
    {
        return Ok(Fingerprint(value));
    }

    let bytes = std::fs::read(&executable)
        .map_err(|error| format!("could not read `{}`: {error}", executable.display()))?;
    let mut hasher = Hasher::new();
    hasher.write_str(env!("CARGO_PKG_VERSION"));
    hasher.write_framed(&bytes);
    let identity = hasher.finish();
    if let Some(memo) = memo
        && let Some(parent) = memo.parent()
        && std::fs::create_dir_all(parent).is_ok()
    {
        // Best effort: a missing memo only costs another hash next time.
        let _ = write_atomically(&memo, identity.to_string().as_bytes());
    }
    Ok(identity)
}

fn write_atomically(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    let temporary = temporary_sibling(path);
    std::fs::write(&temporary, contents)?;
    std::fs::rename(&temporary, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&temporary);
    })
}

fn temporary_sibling(path: &Path) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let name = path.file_name().unwrap_or_else(|| OsStr::new("object"));
    path.with_file_name(format!(
        ".{}.tmp-{}-{nonce}",
        name.to_string_lossy(),
        std::process::id()
    ))
}

/// The name LLVM gives the target: `--target` if given, else the host triple.
pub fn target_triple(target: Option<&str>) -> String {
    match target {
        Some(target) => target.to_owned(),
        None => inkwell::targets::TargetMachine::get_default_triple()
            .as_str()
            .to_string_lossy()
            .into_owned(),
    }
}

/// `$XDG_CACHE_HOME/staple`, else `~/.cache/staple`.
pub fn user_cache_root() -> Option<PathBuf> {
    if let Some(cache) = std::env::var_os("XDG_CACHE_HOME").filter(|cache| !cache.is_empty()) {
        return Some(PathBuf::from(cache).join("staple"));
    }
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(|home| PathBuf::from(home).join(".cache/staple"))
}

pub fn runtime_fingerprint(identity: Fingerprint, triple: &str) -> Fingerprint {
    let mut hasher = Hasher::new();
    hasher.write_str("runtime");
    hasher.write_fingerprint(identity);
    hasher.write_str(triple);
    hasher.finish()
}

/// The standard library's package: the target of the `std` alias every package
/// carries.
pub fn standard_library_package(graph: &PackageGraph) -> Option<PackageId> {
    graph
        .packages
        .iter()
        .flat_map(|package| &package.dependencies)
        .find(|dependency| dependency.alias == "std")
        .map(|dependency| dependency.package)
}

/// Fingerprints every package of `packages`, which lists dependencies before
/// the packages that use them.
pub fn package_fingerprints(
    graph: &PackageGraph,
    active: &ActiveFeatures,
    packages: &[LoweredPackage],
    identity: Fingerprint,
    triple: &str,
) -> Result<HashMap<PackageId, Fingerprint>, String> {
    let mut fingerprints = HashMap::new();
    for lowered in packages {
        let package = graph.package(lowered.id);
        let mut hasher = Hasher::new();
        hasher.write_str("package");
        hasher.write_fingerprint(identity);
        hasher.write_str(triple);
        hasher.write_str(&package.name);
        hasher.write_str(&format!("{:?}", package.kind));
        hasher.write(&[u8::from(package.prelude)]);

        let mut features = active
            .get(&lowered.id)
            .map(|features| features.iter().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        features.sort();
        hasher.write(&(features.len() as u64).to_le_bytes());
        for feature in &features {
            hasher.write_str(feature);
        }

        let mut files = Vec::new();
        collect_sources(package.source_root(), package, &mut files)
            .map_err(|error| format!("could not read package `{}`: {error}", package.name))?;
        files.sort();
        hasher.write(&(files.len() as u64).to_le_bytes());
        for (relative, path) in &files {
            let contents = std::fs::read(path)
                .map_err(|error| format!("could not read `{}`: {error}", path.display()))?;
            hasher.write_str(relative);
            hasher.write_framed(&contents);
        }

        hasher.write(&(lowered.dependencies.len() as u64).to_le_bytes());
        for dependency in &lowered.dependencies {
            let fingerprint = fingerprints.get(dependency).ok_or_else(|| {
                format!(
                    "package `{}` is not ordered after its dependency `{}`",
                    package.name,
                    graph.package(*dependency).name
                )
            })?;
            hasher.write_str(&graph.package(*dependency).name);
            hasher.write_fingerprint(*fingerprint);
        }
        fingerprints.insert(lowered.id, hasher.finish());
    }
    Ok(fingerprints)
}

/// The `.sta` files under `directory` with their `/`-separated paths relative
/// to the package's source root. Hidden directories and the package's own
/// `target` and `build` directories hold no sources.
fn collect_sources(
    directory: &Path,
    package: &staple_project::Package,
    files: &mut Vec<(String, PathBuf)>,
) -> std::io::Result<()> {
    let source_root = package.source_root();
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name();
        if entry.file_type()?.is_dir() {
            let hidden = name.to_string_lossy().starts_with('.');
            let generated =
                path == package.directory.join("target") || path == package.directory.join("build");
            if !hidden && !generated {
                collect_sources(&path, package, files)?;
            }
        } else if path.extension() == Some(OsStr::new("sta")) {
            let relative = path
                .strip_prefix(source_root)
                .unwrap_or(&path)
                .components()
                .map(|component| component.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/");
            files.push((relative, path));
        }
    }
    Ok(())
}

/// A directory objects are written to. `prune` removes the older objects of
/// the same name, so only directories owned by one project should set it: the
/// user cache is shared by unrelated projects whose packages may share names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Destination {
    pub directory: PathBuf,
    pub prune: bool,
}

/// Where to look for an object and where to put one that is missing. Reading
/// and writing are separate because the installed standard library is read
/// but not written by ordinary builds.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Placement {
    pub read: Vec<PathBuf>,
    pub write: Vec<Destination>,
}

fn object_name(name: &str, fingerprint: Fingerprint) -> String {
    format!("{name}-{fingerprint}.o")
}

/// The cached object for `name`/`fingerprint`, calling `emit` to create it
/// (at the path it is given) when no read directory has one. `emit` writes a
/// temporary file that is renamed into place, so concurrent builds see either
/// no object or a complete one.
pub fn ensure_object(
    placement: &Placement,
    name: &str,
    fingerprint: Fingerprint,
    emit: &mut dyn FnMut(&Path) -> Result<(), String>,
) -> Result<PathBuf, String> {
    let file = object_name(name, fingerprint);
    for directory in &placement.read {
        let path = directory.join(&file);
        if path.is_file() {
            return Ok(path);
        }
    }
    let mut last_error = format!("no cache directory to store `{name}` in");
    for destination in &placement.write {
        let path = destination.directory.join(&file);
        let temporary = temporary_sibling(&path);
        // Creating the temporary file first finds an unwritable directory
        // before the compiler spends time on an object it cannot store.
        let probe = std::fs::create_dir_all(&destination.directory)
            .and_then(|()| std::fs::File::create_new(&temporary));
        if let Err(error) = probe {
            last_error = format!(
                "could not write to `{}`: {error}",
                destination.directory.display()
            );
            continue;
        }
        let result = emit(&temporary).and_then(|()| {
            std::fs::rename(&temporary, &path)
                .map_err(|error| format!("could not create `{}`: {error}", path.display()))
        });
        if let Err(error) = result {
            let _ = std::fs::remove_file(&temporary);
            return Err(error);
        }
        if destination.prune {
            prune_stale(&destination.directory, name, &file);
        }
        return Ok(path);
    }
    Err(last_error)
}

/// Removes `<name>-<fingerprint>.o` files other than `keep`.
fn prune_stale(directory: &Path, name: &str, keep: &str) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let file = entry.file_name();
        let Some(file) = file.to_str() else { continue };
        let is_object = file
            .strip_prefix(name)
            .and_then(|rest| rest.strip_prefix('-'))
            .and_then(|rest| rest.strip_suffix(".o"))
            .is_some_and(|hash| hash.len() == 32 && hash.bytes().all(|b| b.is_ascii_hexdigit()));
        if is_object && file != keep {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use staple_project::{Dependency, Package, PackageKind};
    use std::collections::HashSet;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "staple-cache-test-{}-{nonce}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn write(&self, relative: &str, contents: &str) {
            let path = self.0.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, contents).unwrap();
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn package(scratch: &Scratch, name: &str, dependencies: &[usize]) -> Package {
        let directory = scratch.0.join(name);
        Package {
            name: name.to_owned(),
            kind: PackageKind::Library,
            manifest: directory.join("staple.kdl"),
            directory: directory.clone(),
            root: directory.join("src/lib.sta"),
            entry: None,
            dependencies: dependencies
                .iter()
                .map(|&package| Dependency {
                    alias: format!("dep{package}"),
                    package: PackageId(package),
                    default_features: true,
                    features: Vec::new(),
                })
                .collect(),
            features: HashMap::new(),
            default_features: Vec::new(),
            prelude: false,
        }
    }

    /// `leaf` (0) <- `middle` (1) <- `app` (2).
    fn graph(scratch: &Scratch) -> (PackageGraph, Vec<LoweredPackage>) {
        scratch.write("leaf/src/lib.sta", "pub fn leaf() {}\n");
        scratch.write("middle/src/lib.sta", "pub fn middle() {}\n");
        scratch.write("app/src/lib.sta", "pub fn app() {}\n");
        let graph = PackageGraph {
            root: PackageId(2),
            packages: vec![
                package(scratch, "leaf", &[]),
                package(scratch, "middle", &[0]),
                package(scratch, "app", &[1]),
            ],
        };
        let packages = (0..3)
            .map(|index| LoweredPackage {
                id: PackageId(index),
                name: graph.packages[index].name.clone(),
                is_root: index == 2,
                dependencies: if index == 0 {
                    Vec::new()
                } else {
                    vec![PackageId(index - 1)]
                },
            })
            .collect();
        (graph, packages)
    }

    fn fingerprints(
        graph: &PackageGraph,
        packages: &[LoweredPackage],
        active: &ActiveFeatures,
    ) -> HashMap<PackageId, Fingerprint> {
        package_fingerprints(graph, active, packages, Fingerprint(1), "triple").unwrap()
    }

    #[test]
    fn hashes_match_published_fnv1a_vectors() {
        assert_eq!(
            Hasher::new().finish().to_string(),
            "6c62272e07bb014262b821756295c58d"
        );
        let mut hasher = Hasher::new();
        hasher.write(b"a");
        assert_eq!(
            hasher.finish().to_string(),
            "d228cb696f1a8caf78912b704e4a8964"
        );
    }

    #[test]
    fn fingerprints_are_stable() {
        let scratch = Scratch::new();
        let (graph, packages) = graph(&scratch);
        let none = ActiveFeatures::new();
        assert_eq!(
            fingerprints(&graph, &packages, &none),
            fingerprints(&graph, &packages, &none)
        );
    }

    #[test]
    fn a_source_change_rebuilds_the_package_and_its_dependents_only() {
        let scratch = Scratch::new();
        let (graph, packages) = graph(&scratch);
        let none = ActiveFeatures::new();
        let before = fingerprints(&graph, &packages, &none);
        scratch.write("middle/src/lib.sta", "pub fn middle() { }\n");
        let after = fingerprints(&graph, &packages, &none);
        assert_eq!(before[&PackageId(0)], after[&PackageId(0)]);
        assert_ne!(before[&PackageId(1)], after[&PackageId(1)]);
        assert_ne!(before[&PackageId(2)], after[&PackageId(2)]);
    }

    #[test]
    fn added_and_renamed_files_change_the_fingerprint() {
        let scratch = Scratch::new();
        let (graph, packages) = graph(&scratch);
        let none = ActiveFeatures::new();
        let before = fingerprints(&graph, &packages, &none)[&PackageId(0)];
        scratch.write("leaf/src/extra.sta", "");
        let added = fingerprints(&graph, &packages, &none)[&PackageId(0)];
        assert_ne!(before, added);
        std::fs::rename(
            scratch.0.join("leaf/src/extra.sta"),
            scratch.0.join("leaf/src/other.sta"),
        )
        .unwrap();
        assert_ne!(added, fingerprints(&graph, &packages, &none)[&PackageId(0)]);
    }

    #[test]
    fn features_target_and_compiler_change_the_fingerprint() {
        let scratch = Scratch::new();
        let (graph, packages) = graph(&scratch);
        let none = ActiveFeatures::new();
        let before = fingerprints(&graph, &packages, &none);
        let mut active = ActiveFeatures::new();
        active.insert(PackageId(0), HashSet::from(["fast".to_owned()]));
        let after = fingerprints(&graph, &packages, &active);
        assert_ne!(before[&PackageId(0)], after[&PackageId(0)]);
        assert_ne!(before[&PackageId(2)], after[&PackageId(2)]);

        let other =
            package_fingerprints(&graph, &none, &packages, Fingerprint(1), "other").unwrap();
        assert_ne!(before[&PackageId(0)], other[&PackageId(0)]);
        let other =
            package_fingerprints(&graph, &none, &packages, Fingerprint(2), "triple").unwrap();
        assert_ne!(before[&PackageId(0)], other[&PackageId(0)]);
    }

    #[test]
    fn a_missing_object_is_emitted_once_and_then_reused() {
        let scratch = Scratch::new();
        let placement = Placement {
            read: vec![scratch.0.join("cache")],
            write: vec![Destination {
                directory: scratch.0.join("cache"),
                prune: true,
            }],
        };
        let emitted = std::cell::Cell::new(0);
        let mut emit = |path: &Path| {
            emitted.set(emitted.get() + 1);
            std::fs::write(path, "object").map_err(|error| error.to_string())
        };
        let first = ensure_object(&placement, "pkg", Fingerprint(1), &mut emit).unwrap();
        let second = ensure_object(&placement, "pkg", Fingerprint(1), &mut emit).unwrap();
        assert_eq!(first, second);
        assert_eq!(emitted.get(), 1);

        // A new fingerprint replaces the pruned object and leaves no temporaries.
        let third = ensure_object(&placement, "pkg", Fingerprint(2), &mut emit).unwrap();
        assert_eq!(emitted.get(), 2);
        assert!(!first.exists());
        let names = std::fs::read_dir(scratch.0.join("cache"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        assert_eq!(names, vec![third]);
    }

    #[test]
    fn a_failed_emission_leaves_nothing_behind() {
        let scratch = Scratch::new();
        let placement = Placement {
            read: Vec::new(),
            write: vec![Destination {
                directory: scratch.0.join("cache"),
                prune: false,
            }],
        };
        let error = ensure_object(&placement, "pkg", Fingerprint(1), &mut |_| {
            Err("boom".to_owned())
        })
        .unwrap_err();
        assert_eq!(error, "boom");
        assert_eq!(
            std::fs::read_dir(scratch.0.join("cache")).unwrap().count(),
            0
        );
    }

    #[test]
    fn an_unwritable_destination_falls_through_to_the_next() {
        let scratch = Scratch::new();
        scratch.write("blocker", "a file, not a directory");
        let placement = Placement {
            read: Vec::new(),
            write: vec![
                Destination {
                    directory: scratch.0.join("blocker/inside"),
                    prune: false,
                },
                Destination {
                    directory: scratch.0.join("fallback"),
                    prune: false,
                },
            ],
        };
        let path = ensure_object(&placement, "pkg", Fingerprint(1), &mut |path| {
            std::fs::write(path, "object").map_err(|error| error.to_string())
        })
        .unwrap();
        assert!(path.starts_with(scratch.0.join("fallback")));
    }
}
