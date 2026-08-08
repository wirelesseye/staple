use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::parser::parse_with_syntax_ids;
use crate::{Item, Module, SyntaxId, UseDeclaration};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ModuleId(pub usize);

#[derive(Debug, Clone)]
pub struct SourceModule {
    pub id: ModuleId,
    pub path: PathBuf,
    pub syntax: Module,
}

#[derive(Debug, Clone)]
pub struct Program {
    entry: ModuleId,
    standard_library_core: Option<ModuleId>,
    standard_library_cinterop: Option<ModuleId>,
    modules: Vec<SourceModule>,
    imported_modules: HashMap<SyntaxId, ModuleId>,
    initialization_order: Vec<ModuleId>,
}

impl Program {
    pub(crate) fn single(module: Module) -> Self {
        Self {
            entry: ModuleId(0),
            standard_library_core: None,
            standard_library_cinterop: None,
            modules: vec![SourceModule {
                id: ModuleId(0),
                path: PathBuf::from("<memory>.sta"),
                syntax: module,
            }],
            imported_modules: HashMap::new(),
            initialization_order: vec![ModuleId(0)],
        }
    }

    pub fn entry(&self) -> ModuleId {
        self.entry
    }

    pub fn standard_library_core(&self) -> Option<ModuleId> {
        self.standard_library_core
    }

    pub fn standard_library_cinterop(&self) -> Option<ModuleId> {
        self.standard_library_cinterop
    }

    pub fn modules(&self) -> &[SourceModule] {
        &self.modules
    }

    pub fn module(&self, id: ModuleId) -> &SourceModule {
        &self.modules[id.0]
    }

    pub fn imported_module(&self, use_syntax: SyntaxId) -> Option<ModuleId> {
        self.imported_modules.get(&use_syntax).copied()
    }

    pub fn initialization_order(&self) -> &[ModuleId] {
        &self.initialization_order
    }
}

#[derive(Default)]
pub struct ProgramLoader {
    modules: Vec<SourceModule>,
    paths: HashMap<PathBuf, ModuleId>,
    imported_modules: HashMap<SyntaxId, ModuleId>,
    next_syntax_id: usize,
    module_root: Option<PathBuf>,
    standard_library_root: Option<PathBuf>,
    standard_library_core: Option<ModuleId>,
    standard_library_cinterop: Option<ModuleId>,
}

impl ProgramLoader {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_standard_library_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.standard_library_root = Some(root.into());
        self
    }

    pub fn load_path(mut self, entry: &Path) -> Result<Program, String> {
        let entry = canonical_file(entry)?;
        self.module_root = Some(entry.parent().unwrap_or_else(|| Path::new(".")).to_owned());
        let entry_id = self.load_file(&entry)?;
        self.load_standard_library()?;
        Ok(self.finish(entry_id))
    }

    pub fn load_source(mut self, source: &str, module_root: &Path) -> Result<Program, String> {
        let root = std::fs::canonicalize(module_root).map_err(|error| {
            format!(
                "could not resolve module root `{}`: {error}",
                module_root.display()
            )
        })?;
        self.module_root = Some(root.clone());
        let path = root.join("<stdin>.sta");
        let entry = self.insert_source(path, source)?;
        self.load_imports(entry, &root)?;
        self.load_standard_library()?;
        Ok(self.finish(entry))
    }

    fn load_file(&mut self, path: &Path) -> Result<ModuleId, String> {
        if let Some(id) = self.paths.get(path) {
            return Ok(*id);
        }
        let source = std::fs::read_to_string(path)
            .map_err(|error| format!("could not read `{}`: {error}", path.display()))?;
        let id = self.insert_source(path.to_owned(), &source)?;
        let root = self.module_root.clone().expect("module root is set");
        self.load_imports(id, &root)?;
        Ok(id)
    }

    fn insert_source(&mut self, path: PathBuf, source: &str) -> Result<ModuleId, String> {
        let syntax = parse_with_syntax_ids(
            source,
            &mut self.next_syntax_id,
            &path.display().to_string(),
        )
        .map_err(|error| format!("{}: {error}", path.display()))?;
        let id = ModuleId(self.modules.len());
        self.paths.insert(path.clone(), id);
        self.modules.push(SourceModule { id, path, syntax });
        Ok(id)
    }

    fn load_imports(&mut self, module: ModuleId, root: &Path) -> Result<(), String> {
        let uses = self.modules[module.0]
            .syntax
            .items
            .iter()
            .filter_map(|item| match item {
                Item::UseDeclaration(declaration) => Some(declaration.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        for declaration in uses {
            let import_root = if declaration.path.first().is_some_and(|part| part == "std") {
                self.resolve_standard_library_root()?
            } else {
                root.to_owned()
            };
            let path = use_path(&import_root, &declaration);
            let path = canonical_file(&path).map_err(|message| match &declaration.syntax.span {
                crate::Span::User {
                    location: Some(location),
                    ..
                } => format!(
                    "{} at line {}, column {}",
                    message, location.line, location.column
                ),
                span => format!("{} at byte {}", message, span.to_range().start),
            })?;
            let imported = self.load_file(&path)?;
            self.imported_modules
                .insert(declaration.syntax.id, imported);
        }
        Ok(())
    }

    fn load_standard_library(&mut self) -> Result<(), String> {
        let root = self.resolve_standard_library_root()?;
        let core = canonical_file(&root.join("std/core.sta"))?;
        self.standard_library_core = Some(self.load_file(&core)?);
        let cinterop = canonical_file(&root.join("std/cinterop.sta"))?;
        self.standard_library_cinterop = Some(self.load_file(&cinterop)?);
        Ok(())
    }

    fn resolve_standard_library_root(&mut self) -> Result<PathBuf, String> {
        if let Some(root) = &self.standard_library_root {
            return canonical_directory(root, "standard library");
        }
        if let Some(root) = std::env::var_os("STAPLE_STDLIB") {
            let root = PathBuf::from(root);
            let root = canonical_directory(&root, "standard library from `STAPLE_STDLIB`")?;
            self.standard_library_root = Some(root.clone());
            return Ok(root);
        }
        let executable = std::env::current_exe()
            .map_err(|error| format!("could not locate the Staple standard library: {error}"))?;
        let prefix = executable
            .parent()
            .and_then(Path::parent)
            .ok_or_else(|| "could not determine the Stapler installation prefix".to_owned())?;
        let root = prefix.join("lib/staple/stdlib");
        canonical_directory(&root, "standard library")
            .map_err(|error| format!("{error}; pass `--stdlib <path>` or set `STAPLE_STDLIB`"))
    }

    fn finish(self, entry: ModuleId) -> Program {
        let initialization_order = initialization_order(&self.modules, &self.imported_modules);
        Program {
            entry,
            standard_library_core: self.standard_library_core,
            standard_library_cinterop: self.standard_library_cinterop,
            modules: self.modules,
            imported_modules: self.imported_modules,
            initialization_order,
        }
    }
}

fn canonical_directory(path: &Path, description: &str) -> Result<PathBuf, String> {
    std::fs::canonicalize(path).map_err(|error| {
        format!(
            "could not resolve {description} `{}`: {error}",
            path.display()
        )
    })
}

fn canonical_file(path: &Path) -> Result<PathBuf, String> {
    std::fs::canonicalize(path)
        .map_err(|error| format!("could not resolve module `{}`: {error}", path.display()))
}

fn use_path(root: &Path, declaration: &UseDeclaration) -> PathBuf {
    let mut path = root.to_owned();
    for component in &declaration.path {
        path.push(component);
    }
    path.set_extension("sta");
    path
}

fn initialization_order(
    modules: &[SourceModule],
    imports: &HashMap<SyntaxId, ModuleId>,
) -> Vec<ModuleId> {
    let mut edges = vec![Vec::new(); modules.len()];
    for module in modules {
        for item in &module.syntax.items {
            if let Item::UseDeclaration(declaration) = item
                && let Some(imported) = imports.get(&declaration.syntax.id)
            {
                edges[module.id.0].push(*imported);
            }
        }
        edges[module.id.0].sort();
        edges[module.id.0].dedup();
    }

    fn visit(node: usize, edges: &[Vec<ModuleId>], seen: &mut [bool], order: &mut Vec<usize>) {
        if std::mem::replace(&mut seen[node], true) {
            return;
        }
        for dependency in &edges[node] {
            visit(dependency.0, edges, seen, order);
        }
        order.push(node);
    }
    let mut finish = Vec::new();
    let mut seen = vec![false; modules.len()];
    for node in 0..modules.len() {
        visit(node, &edges, &mut seen, &mut finish);
    }

    let mut reverse = vec![Vec::new(); modules.len()];
    for (node, dependencies) in edges.iter().enumerate() {
        for dependency in dependencies {
            reverse[dependency.0].push(ModuleId(node));
        }
    }
    let mut component_of = vec![usize::MAX; modules.len()];
    let mut components: Vec<Vec<ModuleId>> = Vec::new();
    fn collect(
        node: usize,
        reverse: &[Vec<ModuleId>],
        component: usize,
        component_of: &mut [usize],
        members: &mut Vec<ModuleId>,
    ) {
        if component_of[node] != usize::MAX {
            return;
        }
        component_of[node] = component;
        members.push(ModuleId(node));
        for next in &reverse[node] {
            collect(next.0, reverse, component, component_of, members);
        }
    }
    for node in finish.into_iter().rev() {
        if component_of[node] == usize::MAX {
            let component = components.len();
            let mut members = Vec::new();
            collect(node, &reverse, component, &mut component_of, &mut members);
            members.sort_by(|left, right| modules[left.0].path.cmp(&modules[right.0].path));
            components.push(members);
        }
    }

    let mut component_edges = vec![HashSet::new(); components.len()];
    for (node, dependencies) in edges.iter().enumerate() {
        for dependency in dependencies {
            let from = component_of[node];
            let to = component_of[dependency.0];
            if from != to {
                component_edges[from].insert(to);
            }
        }
    }
    fn order_components(
        component: usize,
        edges: &[HashSet<usize>],
        seen: &mut HashSet<usize>,
        order: &mut Vec<usize>,
    ) {
        if !seen.insert(component) {
            return;
        }
        let mut dependencies = edges[component].iter().copied().collect::<Vec<_>>();
        dependencies.sort_unstable();
        for dependency in dependencies {
            order_components(dependency, edges, seen, order);
        }
        order.push(component);
    }
    let mut component_order = Vec::new();
    let mut component_seen = HashSet::new();
    for component in 0..components.len() {
        order_components(
            component,
            &component_edges,
            &mut component_seen,
            &mut component_order,
        );
    }
    component_order
        .into_iter()
        .flat_map(|component| components[component].iter().copied())
        .collect()
}
