use std::collections::{HashMap, HashSet, VecDeque};
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
    pub default_features: bool,
    pub features: Vec<String>,
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
    pub features: HashMap<String, Vec<FeatureMember>>,
    pub default_features: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeatureMember {
    Local(String),
    Dependency { alias: String, feature: String },
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FeatureSelection {
    pub features: Vec<String>,
    pub all_features: bool,
    pub no_default_features: bool,
}

pub type ActiveFeatures = HashMap<PackageId, HashSet<String>>;

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
    dependency_options: HashMap<String, (bool, Vec<String>)>,
    features: HashMap<String, Vec<FeatureMember>>,
    default_features: Vec<String>,
}

pub fn load_package_graph(manifest: &Path) -> Result<PackageGraph, String> {
    let manifest = canonical_file(manifest, "manifest")?;
    let mut packages = Vec::new();
    let mut loaded = HashMap::new();
    let mut stack = Vec::new();
    let root = load_recursive(&manifest, false, &mut packages, &mut loaded, &mut stack)?;
    let graph = PackageGraph { root, packages };
    validate_features(&graph)?;
    Ok(graph)
}

fn validate_features(graph: &PackageGraph) -> Result<(), String> {
    for package in &graph.packages {
        for feature in &package.default_features {
            if !package.features.contains_key(feature) {
                return Err(format!(
                    "{}: unknown feature `{feature}`",
                    package.manifest.display()
                ));
            }
        }
        for (name, members) in &package.features {
            for member in members {
                match member {
                    FeatureMember::Local(feature) if !package.features.contains_key(feature) => {
                        return Err(format!(
                            "{}: feature `{name}` references unknown feature `{feature}`",
                            package.manifest.display()
                        ));
                    }
                    FeatureMember::Dependency { alias, feature } => {
                        let dependency = package
                            .dependencies
                            .iter()
                            .find(|dependency| &dependency.alias == alias)
                            .ok_or_else(|| {
                                format!(
                                    "{}: feature `{name}` references unknown dependency `{alias}`",
                                    package.manifest.display()
                                )
                            })?;
                        if !graph
                            .package(dependency.package)
                            .features
                            .contains_key(feature)
                        {
                            return Err(format!(
                                "{}: feature `{name}` references unknown feature `{alias}/{feature}`",
                                package.manifest.display()
                            ));
                        }
                    }
                    _ => {}
                }
            }
        }
        for dependency in &package.dependencies {
            for feature in &dependency.features {
                if !graph
                    .package(dependency.package)
                    .features
                    .contains_key(feature)
                {
                    return Err(format!(
                        "{}: dependency `{}` enables unknown feature `{feature}`",
                        package.manifest.display(),
                        dependency.alias
                    ));
                }
            }
        }
    }
    Ok(())
}

pub fn resolve_features(
    graph: &PackageGraph,
    selection: &FeatureSelection,
) -> Result<ActiveFeatures, String> {
    let root = graph.root_package();
    for feature in &selection.features {
        if !root.features.contains_key(feature) {
            return Err(format!(
                "package `{}` does not declare feature `{feature}`",
                root.name
            ));
        }
    }
    let mut active: ActiveFeatures = graph
        .packages
        .iter()
        .enumerate()
        .map(|(index, _)| (PackageId(index), HashSet::new()))
        .collect();
    let mut queue = VecDeque::new();
    if !selection.no_default_features {
        for feature in &root.default_features {
            queue.push_back((graph.root, feature.clone()));
        }
    }
    for feature in &selection.features {
        queue.push_back((graph.root, feature.clone()));
    }
    if selection.all_features {
        for feature in root.features.keys() {
            queue.push_back((graph.root, feature.clone()));
        }
    }
    for package in &graph.packages {
        for dependency in &package.dependencies {
            let target = graph.package(dependency.package);
            if dependency.default_features {
                for feature in &target.default_features {
                    queue.push_back((dependency.package, feature.clone()));
                }
            }
            for feature in &dependency.features {
                queue.push_back((dependency.package, feature.clone()));
            }
        }
    }
    while let Some((package_id, feature)) = queue.pop_front() {
        if !active.get_mut(&package_id).unwrap().insert(feature.clone()) {
            continue;
        }
        for member in &graph.package(package_id).features[&feature] {
            match member {
                FeatureMember::Local(feature) => queue.push_back((package_id, feature.clone())),
                FeatureMember::Dependency { alias, feature } => {
                    let dependency = graph
                        .package(package_id)
                        .dependencies
                        .iter()
                        .find(|dependency| &dependency.alias == alias)
                        .unwrap();
                    queue.push_back((dependency.package, feature.clone()));
                }
            }
        }
    }
    Ok(active)
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
        features: parsed.features.clone(),
        default_features: parsed.default_features.clone(),
    });
    stack.push(manifest.clone());
    let mut dependencies = Vec::new();
    for (alias, directory) in parsed.dependencies {
        let target_manifest = directory.join("binder.kdl");
        let target = load_recursive(&target_manifest, true, packages, loaded, stack)?;
        let (default_features, features) = parsed
            .dependency_options
            .get(&alias)
            .cloned()
            .unwrap_or_else(|| (true, Vec::new()));
        dependencies.push(Dependency {
            alias,
            package: target,
            default_features,
            features,
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
    let mut features = None;
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
                "features" => {
                    if features.is_some() {
                        return Err(format!("{}: duplicate `features` node", manifest.display()));
                    }
                    features = Some(parse_features(child, manifest)?);
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
    let (dependencies, dependency_options) = dependencies.unwrap_or_default();
    let (features, default_features) = features.unwrap_or_default();
    Ok(ParsedPackage {
        name,
        kind,
        manifest: manifest.to_owned(),
        directory: directory.clone(),
        root,
        entry,
        dependencies: dependencies
            .into_iter()
            .map(|(alias, path)| (alias, directory.join(path)))
            .collect(),
        dependency_options,
        features,
        default_features,
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

type ParsedDependencies = (Vec<(String, PathBuf)>, HashMap<String, (bool, Vec<String>)>);
fn parse_dependencies(node: &KdlNode, manifest: &Path) -> Result<ParsedDependencies, String> {
    if !node.entries().is_empty() {
        return Err(format!(
            "{}: `dependencies` does not accept values or properties",
            manifest.display()
        ));
    }
    let mut result = Vec::new();
    let mut options = HashMap::new();
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
            let mut path = None;
            let mut default_features = true;
            for value in dependency.entries() {
                match value.name().map(|name| name.value()) {
                    Some("path") if path.is_none() => path = value.value().as_string(),
                    Some("default-features") => {
                        default_features = value.value().as_bool().ok_or_else(|| {
                            format!(
                                "{}: dependency `{alias}` `default-features` must be a boolean",
                                manifest.display()
                            )
                        })?
                    }
                    _ => {
                        return Err(format!(
                            "{}: dependency `{alias}` accepts only `path` and `default-features` properties",
                            manifest.display()
                        ));
                    }
                }
            }
            let path = path.ok_or_else(|| {
                format!(
                    "{}: dependency `{alias}` requires one string `path` property",
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
            let mut enabled = Vec::new();
            if let Some(children) = dependency.children() {
                for child in children.nodes() {
                    if child.name().value() != "features" || child.children().is_some() {
                        return Err(format!(
                            "{}: dependency `{alias}` accepts only a `features` child",
                            manifest.display()
                        ));
                    }
                    for entry in child.entries() {
                        let feature = entry
                            .value()
                            .as_string()
                            .filter(|_| entry.name().is_none())
                            .ok_or_else(|| {
                                format!(
                                    "{}: dependency `{alias}` features must be strings",
                                    manifest.display()
                                )
                            })?;
                        validate_feature_name(feature)
                            .map_err(|error| format!("{}: {error}", manifest.display()))?;
                        enabled.push(feature.to_owned());
                    }
                }
            }
            result.push((alias.to_owned(), path));
            options.insert(alias.to_owned(), (default_features, enabled));
        }
    }
    Ok((result, options))
}

fn parse_features(
    node: &KdlNode,
    manifest: &Path,
) -> Result<(HashMap<String, Vec<FeatureMember>>, Vec<String>), String> {
    if !node.entries().is_empty() {
        return Err(format!(
            "{}: `features` does not accept values or properties",
            manifest.display()
        ));
    }
    let mut features = HashMap::new();
    let mut defaults = Vec::new();
    if let Some(children) = node.children() {
        for feature in children.nodes() {
            let name = feature.name().value();
            if feature.children().is_some() {
                return Err(format!(
                    "{}: feature `{name}` does not accept children",
                    manifest.display()
                ));
            }
            if name != "default" {
                validate_feature_name(name)
                    .map_err(|error| format!("{}: {error}", manifest.display()))?;
                if features.contains_key(name) {
                    return Err(format!(
                        "{}: duplicate feature `{name}`",
                        manifest.display()
                    ));
                }
            }
            let mut members = Vec::new();
            for entry in feature.entries() {
                let value = entry
                    .value()
                    .as_string()
                    .filter(|_| entry.name().is_none())
                    .ok_or_else(|| {
                        format!(
                            "{}: feature `{name}` members must be strings",
                            manifest.display()
                        )
                    })?;
                members.push(
                    if let Some((alias, dependency_feature)) = value.split_once('/') {
                        validate_dependency_alias(alias)
                            .map_err(|error| format!("{}: {error}", manifest.display()))?;
                        validate_feature_name(dependency_feature)
                            .map_err(|error| format!("{}: {error}", manifest.display()))?;
                        FeatureMember::Dependency {
                            alias: alias.to_owned(),
                            feature: dependency_feature.to_owned(),
                        }
                    } else {
                        validate_feature_name(value)
                            .map_err(|error| format!("{}: {error}", manifest.display()))?;
                        FeatureMember::Local(value.to_owned())
                    },
                );
            }
            if name == "default" {
                defaults = members
                    .into_iter()
                    .map(|member| match member {
                        FeatureMember::Local(name) => Ok(name),
                        _ => Err(format!(
                            "{}: `default` may contain only local features",
                            manifest.display()
                        )),
                    })
                    .collect::<Result<_, _>>()?;
            } else {
                features.insert(name.to_owned(), members);
            }
        }
    }
    Ok((features, defaults))
}

pub fn validate_feature_name(name: &str) -> Result<(), String> {
    if name.is_empty()
        || !name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
    {
        return Err(format!(
            "feature name `{name}` must contain only ASCII letters, numbers, `_`, or `-`"
        ));
    }
    Ok(())
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
