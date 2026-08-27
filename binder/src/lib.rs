use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use kdl::{KdlDocument, KdlNode};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackageKind {
    Executable,
    Library,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PackageId(pub usize);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dependency {
    pub alias: String,
    pub package: PackageId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Package {
    pub name: String,
    pub kind: PackageKind,
    pub manifest: PathBuf,
    pub directory: PathBuf,
    pub root: PathBuf,
    pub entry: Option<PathBuf>,
    pub dependencies: Vec<Dependency>,
}

impl Package {
    pub fn source_root(&self) -> &Path {
        self.root.parent().expect("package root has a parent")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageGraph {
    pub root: PackageId,
    pub packages: Vec<Package>,
}

impl PackageGraph {
    pub fn root_package(&self) -> &Package {
        &self.packages[self.root.0]
    }

    pub fn package(&self, id: PackageId) -> &Package {
        &self.packages[id.0]
    }
}

#[derive(Debug)]
struct ParsedPackage {
    name: String,
    kind: PackageKind,
    manifest: PathBuf,
    directory: PathBuf,
    root: PathBuf,
    entry: Option<PathBuf>,
    dependencies: Vec<(String, PathBuf)>,
}

pub fn load_package_graph(manifest: &Path) -> Result<PackageGraph, String> {
    let manifest = canonical_file(manifest, "manifest")?;
    let mut packages = Vec::new();
    let mut loaded = HashMap::new();
    let mut stack = Vec::new();
    let root = load_recursive(&manifest, false, &mut packages, &mut loaded, &mut stack)?;
    Ok(PackageGraph { root, packages })
}

fn load_recursive(
    manifest: &Path,
    dependency: bool,
    packages: &mut Vec<Package>,
    loaded: &mut HashMap<PathBuf, PackageId>,
    stack: &mut Vec<PathBuf>,
) -> Result<PackageId, String> {
    let manifest = canonical_file(manifest, "manifest")?;
    if let Some(position) = stack.iter().position(|path| path == &manifest) {
        let mut cycle = stack[position..]
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>();
        cycle.push(manifest.display().to_string());
        return Err(format!("dependency cycle: {}", cycle.join(" -> ")));
    }
    if let Some(id) = loaded.get(&manifest) {
        return Ok(*id);
    }

    let parsed = parse_package(&manifest)?;
    if dependency && parsed.kind != PackageKind::Library {
        return Err(format!(
            "{}: dependency package `{}` must have kind \"library\"",
            parsed.manifest.display(),
            parsed.name
        ));
    }
    let id = PackageId(packages.len());
    loaded.insert(manifest.clone(), id);
    packages.push(Package {
        name: parsed.name.clone(),
        kind: parsed.kind,
        manifest: parsed.manifest.clone(),
        directory: parsed.directory.clone(),
        root: parsed.root.clone(),
        entry: parsed.entry.clone(),
        dependencies: Vec::new(),
    });
    stack.push(manifest.clone());
    let mut dependencies = Vec::new();
    for (alias, directory) in parsed.dependencies {
        let target_manifest = directory.join("binder.kdl");
        let target = load_recursive(&target_manifest, true, packages, loaded, stack)?;
        dependencies.push(Dependency {
            alias,
            package: target,
        });
    }
    stack.pop();
    packages[id.0].dependencies = dependencies;
    Ok(id)
}

fn parse_package(manifest: &Path) -> Result<ParsedPackage, String> {
    let source = std::fs::read_to_string(manifest)
        .map_err(|error| format!("could not read `{}`: {error}", manifest.display()))?;
    let document = source
        .parse::<KdlDocument>()
        .map_err(|error| format!("could not parse `{}`: {error}", manifest.display()))?;
    let [node] = document.nodes() else {
        return Err(format!(
            "{} must contain exactly one `package` node",
            manifest.display()
        ));
    };
    if node.name().value() != "package" {
        return Err(format!(
            "{}: unknown top-level node `{}`; expected `package`",
            manifest.display(),
            node.name().value()
        ));
    }
    let name = package_name(node, manifest)?;
    let mut kind = None;
    let mut root = None;
    let mut entry = None;
    let mut dependencies = None;
    if let Some(children) = node.children() {
        for child in children.nodes() {
            match child.name().value() {
                "kind" => parse_kind_child(child, manifest, &mut kind)?,
                "root" => parse_path_child(child, manifest, &mut root)?,
                "entry" => parse_path_child(child, manifest, &mut entry)?,
                "dependencies" => {
                    if dependencies.is_some() {
                        return Err(format!(
                            "{}: duplicate `dependencies` node",
                            manifest.display()
                        ));
                    }
                    dependencies = Some(parse_dependencies(child, manifest)?);
                }
                unknown => {
                    return Err(format!(
                        "{}: unknown package node `{unknown}`",
                        manifest.display()
                    ));
                }
            }
        }
    }
    let kind = kind.unwrap_or(PackageKind::Executable);
    let directory = manifest.parent().unwrap().to_owned();
    let root_setting = root.as_deref().unwrap_or_else(|| Path::new("src/root.sta"));
    validate_sta_path(root_setting, "root", manifest)?;
    let root_parent_setting = root_setting.parent().unwrap_or_else(|| Path::new("."));
    let root_parent =
        resolve_relative_directory(&directory, root_parent_setting, "root directory")?;
    let root = root_parent.join(root_setting.file_name().unwrap());
    let entry_setting = match (kind, entry.as_deref()) {
        (PackageKind::Executable, None) => Some(Path::new("src/main.sta")),
        (_, value) => value,
    };
    let entry = entry_setting
        .map(|setting| {
            validate_sta_path(setting, "entry", manifest)?;
            let entry = resolve_relative_file(&directory, setting, "entry")?;
            if !entry.starts_with(&root_parent) {
                return Err(format!(
                    "{}: entry `{}` is outside package root directory `{}`",
                    manifest.display(),
                    entry.display(),
                    root_parent.display()
                ));
            }
            Ok(entry)
        })
        .transpose()?;
    Ok(ParsedPackage {
        name,
        kind,
        manifest: manifest.to_owned(),
        directory: directory.clone(),
        root,
        entry,
        dependencies: dependencies
            .unwrap_or_default()
            .into_iter()
            .map(|(alias, path)| (alias, directory.join(path)))
            .collect(),
    })
}

fn parse_kind_child(
    node: &KdlNode,
    manifest: &Path,
    destination: &mut Option<PackageKind>,
) -> Result<(), String> {
    if destination.is_some() {
        return Err(format!("{}: duplicate `kind` node", manifest.display()));
    }
    if node.children().is_some() || node.entries().len() != 1 || node.entries()[0].name().is_some()
    {
        return Err(format!(
            "{}: `kind` requires exactly one string",
            manifest.display()
        ));
    }
    *destination = Some(match node.entries()[0].value().as_string() {
        Some("executable") => PackageKind::Executable,
        Some("library") => PackageKind::Library,
        Some(value) => {
            return Err(format!(
                "{}: unknown package kind `{value}`",
                manifest.display()
            ));
        }
        None => return Err(format!("{}: `kind` must be a string", manifest.display())),
    });
    Ok(())
}

fn parse_dependencies(node: &KdlNode, manifest: &Path) -> Result<Vec<(String, PathBuf)>, String> {
    if !node.entries().is_empty() {
        return Err(format!(
            "{}: `dependencies` does not accept values or properties",
            manifest.display()
        ));
    }
    let mut result = Vec::new();
    let mut seen = HashMap::new();
    if let Some(children) = node.children() {
        for dependency in children.nodes() {
            let alias = dependency.name().value();
            validate_dependency_alias(alias)
                .map_err(|error| format!("{}: {error}", manifest.display()))?;
            if seen.insert(alias.to_owned(), ()).is_some() {
                return Err(format!(
                    "{}: duplicate dependency alias `{alias}`",
                    manifest.display()
                ));
            }
            if dependency.children().is_some() || dependency.entries().len() != 1 {
                return Err(format!(
                    "{}: dependency `{alias}` requires exactly one `path` property",
                    manifest.display()
                ));
            }
            let value = &dependency.entries()[0];
            if value.name().map(|name| name.value()) != Some("path") {
                return Err(format!(
                    "{}: dependency `{alias}` requires a `path` property",
                    manifest.display()
                ));
            }
            let path = value.value().as_string().ok_or_else(|| {
                format!(
                    "{}: dependency `{alias}` path must be a string",
                    manifest.display()
                )
            })?;
            let path = PathBuf::from(path);
            if path.is_absolute() {
                return Err(format!(
                    "{}: dependency `{alias}` path must be relative",
                    manifest.display()
                ));
            }
            result.push((alias.to_owned(), path));
        }
    }
    Ok(result)
}

pub fn validate_dependency_alias(alias: &str) -> Result<(), String> {
    let mut characters = alias.chars();
    if !characters
        .next()
        .is_some_and(|character| character == '_' || character.is_alphabetic())
        || !characters.all(|character| character == '_' || character.is_alphanumeric())
        || matches!(alias, "_" | "std" | "package" | "super")
        || is_keyword(alias)
    {
        return Err(format!(
            "dependency alias `{alias}` must be a non-reserved Staple identifier"
        ));
    }
    Ok(())
}

fn is_keyword(value: &str) -> bool {
    matches!(
        value,
        "use"
            | "as"
            | "satisfies"
            | "pub"
            | "let"
            | "mut"
            | "move"
            | "signal"
            | "return"
            | "loop"
            | "break"
            | "continue"
            | "def"
            | "const"
            | "extern"
            | "type"
            | "mod"
            | "companion"
            | "macro"
            | "trait"
            | "impl"
            | "match"
            | "alias"
            | "opaque"
            | "where"
    )
}

pub fn validate_package_name(name: &str) -> Result<(), String> {
    let mut characters = name.chars();
    if !characters
        .next()
        .is_some_and(|character| character.is_ascii_alphabetic())
        || !characters
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
    {
        return Err(format!(
            "package name `{name}` must start with an ASCII letter and contain only letters, numbers, `_`, or `-`"
        ));
    }
    Ok(())
}

fn package_name(node: &KdlNode, manifest: &Path) -> Result<String, String> {
    let [entry] = node.entries() else {
        return Err(format!(
            "{}: `package` requires exactly one string name",
            manifest.display()
        ));
    };
    if entry.name().is_some() {
        return Err(format!(
            "{}: package properties are not supported",
            manifest.display()
        ));
    }
    let name = entry
        .value()
        .as_string()
        .ok_or_else(|| format!("{}: `package` name must be a string", manifest.display()))?;
    validate_package_name(name).map_err(|error| format!("{}: {error}", manifest.display()))?;
    Ok(name.to_owned())
}

fn parse_path_child(
    node: &KdlNode,
    manifest: &Path,
    destination: &mut Option<PathBuf>,
) -> Result<(), String> {
    let name = node.name().value();
    if destination.is_some() {
        return Err(format!("{}: duplicate `{name}` node", manifest.display()));
    }
    if node.children().is_some() || node.entries().len() != 1 || node.entries()[0].name().is_some()
    {
        return Err(format!(
            "{}: `{name}` requires exactly one string path",
            manifest.display()
        ));
    }
    let value = node.entries()[0]
        .value()
        .as_string()
        .ok_or_else(|| format!("{}: `{name}` path must be a string", manifest.display()))?;
    let path = PathBuf::from(value);
    if path.is_absolute() {
        return Err(format!(
            "{}: `{name}` path must be relative",
            manifest.display()
        ));
    }
    *destination = Some(path);
    Ok(())
}

fn validate_sta_path(path: &Path, label: &str, manifest: &Path) -> Result<(), String> {
    if path.extension() != Some(OsStr::new("sta")) {
        return Err(format!(
            "{}: {label} `{}` must use the `.sta` extension",
            manifest.display(),
            path.display()
        ));
    }
    Ok(())
}

fn resolve_relative_directory(
    parent: &Path,
    relative: &Path,
    label: &str,
) -> Result<PathBuf, String> {
    let resolved = std::fs::canonicalize(parent.join(relative)).map_err(|error| {
        format!(
            "could not resolve {label} `{}`: {error}",
            parent.join(relative).display()
        )
    })?;
    if !resolved.starts_with(parent) {
        return Err(format!(
            "{label} `{}` escapes `{}`",
            relative.display(),
            parent.display()
        ));
    }
    if !resolved.is_dir() {
        return Err(format!(
            "{label} `{}` is not a directory",
            resolved.display()
        ));
    }
    Ok(resolved)
}

fn resolve_relative_file(parent: &Path, relative: &Path, label: &str) -> Result<PathBuf, String> {
    let resolved = std::fs::canonicalize(parent.join(relative)).map_err(|error| {
        format!(
            "could not resolve {label} `{}`: {error}",
            parent.join(relative).display()
        )
    })?;
    if !resolved.starts_with(parent) {
        return Err(format!(
            "{label} `{}` escapes `{}`",
            relative.display(),
            parent.display()
        ));
    }
    if !resolved.is_file() {
        return Err(format!("{label} `{}` is not a file", resolved.display()));
    }
    Ok(resolved)
}

fn canonical_file(path: &Path, label: &str) -> Result<PathBuf, String> {
    let path = std::fs::canonicalize(path)
        .map_err(|error| format!("could not resolve {label} `{}`: {error}", path.display()))?;
    if !path.is_file() {
        return Err(format!("{label} `{}` is not a file", path.display()));
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    static NEXT: AtomicUsize = AtomicUsize::new(0);

    fn fixture() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "binder-graph-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(path.join("src")).unwrap();
        path
    }

    #[test]
    fn loads_recursive_renamed_dependencies() {
        let workspace = fixture();
        for package in ["app", "middle", "leaf"] {
            std::fs::create_dir_all(workspace.join(package).join("src")).unwrap();
        }
        std::fs::write(workspace.join("app/src/main.sta"), "42\n").unwrap();
        std::fs::write(
            workspace.join("app/binder.kdl"),
            "package \"app\" { dependencies { renamed path=\"../middle\" } }\n",
        )
        .unwrap();
        std::fs::write(
            workspace.join("middle/binder.kdl"),
            "package \"middle-lib\" { kind \"library\"; dependencies { leaf path=\"../leaf\" } }\n",
        )
        .unwrap();
        std::fs::write(
            workspace.join("leaf/binder.kdl"),
            "package \"leaf\" { kind \"library\" }\n",
        )
        .unwrap();
        let graph = load_package_graph(&workspace.join("app/binder.kdl")).unwrap();
        assert_eq!(graph.packages.len(), 3);
        assert_eq!(graph.root_package().dependencies[0].alias, "renamed");
        assert_eq!(
            graph
                .package(graph.root_package().dependencies[0].package)
                .name,
            "middle-lib"
        );
        let _ = std::fs::remove_dir_all(workspace);
    }

    #[test]
    fn library_entries_are_optional_and_executable_entries_default() {
        let workspace = fixture();
        std::fs::write(
            workspace.join("binder.kdl"),
            "package \"library\" { kind \"library\" }\n",
        )
        .unwrap();
        let graph = load_package_graph(&workspace.join("binder.kdl")).unwrap();
        assert_eq!(graph.root_package().entry, None);
        std::fs::write(workspace.join("src/main.sta"), "42\n").unwrap();
        std::fs::write(workspace.join("binder.kdl"), "package \"app\"\n").unwrap();
        let graph = load_package_graph(&workspace.join("binder.kdl")).unwrap();
        assert_eq!(
            graph.root_package().entry,
            Some(std::fs::canonicalize(workspace.join("src/main.sta")).unwrap())
        );
        let _ = std::fs::remove_dir_all(workspace);
    }

    #[test]
    fn rejects_dependency_cycles_and_non_library_targets() {
        let workspace = fixture();
        for package in ["a", "b"] {
            std::fs::create_dir_all(workspace.join(package).join("src")).unwrap();
        }
        std::fs::write(workspace.join("a/src/main.sta"), "42\n").unwrap();
        std::fs::write(workspace.join("b/src/main.sta"), "42\n").unwrap();
        std::fs::write(
            workspace.join("a/binder.kdl"),
            "package \"a\" { dependencies { b path=\"../b\" } }\n",
        )
        .unwrap();
        std::fs::write(workspace.join("b/binder.kdl"), "package \"b\"\n").unwrap();
        assert!(
            load_package_graph(&workspace.join("a/binder.kdl"))
                .unwrap_err()
                .contains("must have kind \"library\"")
        );
        std::fs::write(
            workspace.join("b/binder.kdl"),
            "package \"b\" { kind \"library\"; dependencies { a path=\"../a\" } }\n",
        )
        .unwrap();
        std::fs::write(
            workspace.join("a/binder.kdl"),
            "package \"a\" { kind \"library\"; dependencies { b path=\"../b\" } }\n",
        )
        .unwrap();
        assert!(
            load_package_graph(&workspace.join("a/binder.kdl"))
                .unwrap_err()
                .contains("dependency cycle")
        );
        let _ = std::fs::remove_dir_all(workspace);
    }
}
