use std::collections::{HashMap, HashSet};
use std::fmt;
use std::ops::Range;
use std::path::{Path, PathBuf};

use crate::parser::parse_with_syntax_ids;
use crate::{Item, Module, SourceLocation, Span, SyntaxId, UseDeclaration, Visibility};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ModuleId(pub usize);

#[derive(Debug, Clone)]
pub struct SourceModule {
    pub id: ModuleId,
    pub path: PathBuf,
    pub syntax: Module,
    pub parent: Option<ModuleId>,
    pub name: Option<String>,
    pub visibility: Visibility,
    pub qualified_name: String,
}

/// A structured failure produced while loading an editor-owned source file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadDiagnostic {
    pub source: Option<PathBuf>,
    pub range: Option<Range<usize>>,
    pub location: Option<SourceLocation>,
    pub message: String,
}

impl LoadDiagnostic {
    fn compiler(message: impl Into<String>) -> Self {
        Self {
            source: None,
            range: None,
            location: None,
            message: message.into(),
        }
    }

    fn source(
        path: impl Into<PathBuf>,
        range: Option<Range<usize>>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            source: Some(path.into()),
            range,
            location: None,
            message: message.into(),
        }
    }
}

impl fmt::Display for LoadDiagnostic {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(source) = &self.source {
            write!(formatter, "{}: ", source.display())?;
        }
        formatter.write_str(&self.message)?;
        if let Some(location) = self.location {
            write!(
                formatter,
                " at line {}, column {}",
                location.line, location.column
            )
        } else if let Some(range) = &self.range {
            write!(formatter, " at byte {}", range.start)
        } else {
            Ok(())
        }
    }
}

impl std::error::Error for LoadDiagnostic {}

#[derive(Debug, Clone)]
pub struct Program {
    entry: ModuleId,
    standard_library_core: Option<ModuleId>,
    standard_library_cinterop: Option<ModuleId>,
    standard_library_io: Option<ModuleId>,
    modules: Vec<SourceModule>,
    imported_modules: HashMap<SyntaxId, ModuleId>,
    child_modules: HashMap<SyntaxId, ModuleId>,
    children: Vec<HashMap<String, ModuleId>>,
    initialization_order: Vec<ModuleId>,
}

impl Program {
    pub(crate) fn single(module: Module) -> Self {
        let mut program = Self {
            entry: ModuleId(0),
            standard_library_core: None,
            standard_library_cinterop: None,
            standard_library_io: None,
            modules: vec![SourceModule {
                id: ModuleId(0),
                path: PathBuf::from("<memory>.sta"),
                syntax: module,
                parent: None,
                name: None,
                visibility: Visibility::Private,
                qualified_name: "<memory>".to_owned(),
            }],
            imported_modules: HashMap::new(),
            child_modules: HashMap::new(),
            children: vec![HashMap::new()],
            initialization_order: vec![ModuleId(0)],
        };
        program.collect_single_submodules(ModuleId(0));
        program.resolve_single_inline_imports();
        program.initialization_order =
            initialization_order(&program.modules, &program.imported_modules);
        program
    }

    fn collect_single_submodules(&mut self, parent: ModuleId) {
        let declarations = self.modules[parent.0]
            .syntax
            .items
            .iter()
            .filter_map(|item| match item {
                Item::Submodule(module) => Some(module.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        for declaration in declarations {
            let id = ModuleId(self.modules.len());
            let qualified_name = format!(
                "{}.{}",
                self.modules[parent.0].qualified_name, declaration.name
            );
            self.modules.push(SourceModule {
                id,
                path: self.modules[parent.0].path.clone(),
                syntax: declaration.module,
                parent: Some(parent),
                name: Some(declaration.name.clone()),
                visibility: declaration.visibility,
                qualified_name,
            });
            self.children.push(HashMap::new());
            self.children[parent.0].insert(declaration.name, id);
            self.child_modules.insert(declaration.syntax.id, id);
            self.collect_single_submodules(id);
        }
    }

    fn resolve_single_inline_imports(&mut self) {
        let uses = self
            .modules
            .iter()
            .flat_map(|module| {
                module
                    .syntax
                    .items
                    .iter()
                    .filter_map(move |item| match item {
                        Item::UseDeclaration(declaration) => Some((module.id, declaration.clone())),
                        _ => None,
                    })
            })
            .collect::<Vec<_>>();
        for (source, declaration) in uses {
            let mut target = source;
            let mut index = 0;
            if declaration.path.first().is_some_and(|part| part == "super") {
                while declaration
                    .path
                    .get(index)
                    .is_some_and(|part| part == "super")
                {
                    let Some(parent) = self.parent_module(target) else {
                        break;
                    };
                    target = parent;
                    index += 1;
                }
            } else if let Some(first) = declaration.path.first()
                && let Some(child) = self.child_named(source, first)
            {
                target = child;
                index = 1;
            } else {
                continue;
            }
            let mut valid = index > 0;
            for part in &declaration.path[index..] {
                let Some(child) = self.child_named(target, part) else {
                    valid = false;
                    break;
                };
                target = child;
            }
            if valid {
                self.imported_modules.insert(declaration.syntax.id, target);
            }
        }
    }

    /// Registers inline modules introduced after parsing, such as macro output.
    pub(crate) fn rebuild_generated_inline_modules(&mut self) {
        let mut parent = 0;
        while parent < self.modules.len() {
            let declarations = self.modules[parent]
                .syntax
                .items
                .iter()
                .filter_map(|item| match item {
                    Item::Submodule(module)
                        if !self.child_modules.contains_key(&module.syntax.id) =>
                    {
                        Some(module.clone())
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();
            for declaration in declarations {
                let id = ModuleId(self.modules.len());
                let qualified_name = format!(
                    "{}.{}",
                    self.modules[parent].qualified_name, declaration.name
                );
                self.modules.push(SourceModule {
                    id,
                    path: self.modules[parent].path.clone(),
                    syntax: declaration.module,
                    parent: Some(ModuleId(parent)),
                    name: Some(declaration.name.clone()),
                    visibility: declaration.visibility,
                    qualified_name,
                });
                self.children.push(HashMap::new());
                self.children[parent].insert(declaration.name, id);
                self.child_modules.insert(declaration.syntax.id, id);
            }
            parent += 1;
        }
        self.resolve_single_inline_imports();
        self.initialization_order = initialization_order(&self.modules, &self.imported_modules);
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

    pub fn standard_library_io(&self) -> Option<ModuleId> {
        self.standard_library_io
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

    pub fn child_module(&self, submodule_syntax: SyntaxId) -> Option<ModuleId> {
        self.child_modules.get(&submodule_syntax).copied()
    }

    pub fn child_named(&self, module: ModuleId, name: &str) -> Option<ModuleId> {
        self.children[module.0].get(name).copied()
    }

    pub fn parent_module(&self, module: ModuleId) -> Option<ModuleId> {
        self.modules[module.0].parent
    }

    pub fn initialization_order(&self) -> &[ModuleId] {
        &self.initialization_order
    }

    pub(crate) fn modules_mut(&mut self) -> &mut [SourceModule] {
        &mut self.modules
    }
}

#[derive(Default)]
pub struct ProgramLoader {
    modules: Vec<SourceModule>,
    paths: HashMap<PathBuf, ModuleId>,
    imported_modules: HashMap<SyntaxId, ModuleId>,
    child_modules: HashMap<SyntaxId, ModuleId>,
    children: Vec<HashMap<String, ModuleId>>,
    loaded_imports: HashSet<ModuleId>,
    next_syntax_id: usize,
    module_root: Option<PathBuf>,
    package_entry: Option<ModuleId>,
    standard_library_root: Option<PathBuf>,
    standard_library_core: Option<ModuleId>,
    standard_library_cinterop: Option<ModuleId>,
    standard_library_io: Option<ModuleId>,
}

impl ProgramLoader {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_standard_library_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.standard_library_root = Some(root.into());
        self
    }

    /// Resolves non-standard file modules from an explicit package source root.
    ///
    /// Without this setting, file modules remain relative to the entry file's
    /// directory for compatibility with standalone compilation.
    pub fn with_module_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.module_root = Some(root.into());
        self
    }

    pub fn load_path(mut self, entry: &Path) -> Result<Program, String> {
        self.load_path_diagnostic(entry)
            .map_err(|error| error.to_string())
    }

    fn load_path_diagnostic(&mut self, entry: &Path) -> Result<Program, LoadDiagnostic> {
        let entry = canonical_file(entry).map_err(LoadDiagnostic::compiler)?;
        let root = match &self.module_root {
            Some(root) => {
                canonical_directory(root, "module root").map_err(LoadDiagnostic::compiler)?
            }
            None => entry.parent().unwrap_or_else(|| Path::new(".")).to_owned(),
        };
        if !entry.starts_with(&root) {
            return Err(LoadDiagnostic::compiler(format!(
                "entry module `{}` is outside module root `{}`",
                entry.display(),
                root.display()
            )));
        }
        self.module_root = Some(root.clone());
        let source = std::fs::read_to_string(&entry).map_err(|error| {
            LoadDiagnostic::source(
                &entry,
                None,
                format!("could not read `{}`: {error}", entry.display()),
            )
        })?;
        let entry_id = self.insert_source(entry, &source)?;
        self.package_entry = Some(entry_id);
        self.load_imports(entry_id, &root)?;
        self.load_standard_library()?;
        Ok(self.finish_ref(entry_id))
    }

    pub fn load_source(mut self, source: &str, module_root: &Path) -> Result<Program, String> {
        self.load_source_diagnostic(source, module_root)
            .map_err(|error| error.to_string())
    }

    fn load_source_diagnostic(
        &mut self,
        source: &str,
        module_root: &Path,
    ) -> Result<Program, LoadDiagnostic> {
        let root = std::fs::canonicalize(module_root).map_err(|error| {
            LoadDiagnostic::compiler(format!(
                "could not resolve module root `{}`: {error}",
                module_root.display()
            ))
        })?;
        self.module_root = Some(root.clone());
        let path = root.join("<stdin>.sta");
        let entry = self.insert_source(path, source)?;
        self.package_entry = Some(entry);
        self.load_imports(entry, &root)?;
        self.load_standard_library()?;
        Ok(self.finish_ref(entry))
    }

    /// Loads an entry module from in-memory text while retaining its real path.
    /// Imported modules are loaded from the configured module root, or from the
    /// entry module's directory when no root was configured.
    pub fn load_source_at(mut self, path: &Path, source: &str) -> Result<Program, LoadDiagnostic> {
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        let root = std::fs::canonicalize(parent).map_err(|error| {
            LoadDiagnostic::source(
                path,
                None,
                format!(
                    "could not resolve module root `{}`: {error}",
                    parent.display()
                ),
            )
        })?;
        let module_root = match &self.module_root {
            Some(module_root) => canonical_directory(module_root, "module root")
                .map_err(|error| LoadDiagnostic::source(path, None, error))?,
            None => root.clone(),
        };
        let file_name = path
            .file_name()
            .ok_or_else(|| LoadDiagnostic::source(path, None, "source path has no file name"))?;
        let entry_path = root.join(file_name);
        if !entry_path.starts_with(&module_root) {
            return Err(LoadDiagnostic::source(
                path,
                None,
                format!(
                    "entry module `{}` is outside module root `{}`",
                    entry_path.display(),
                    module_root.display()
                ),
            ));
        }
        self.module_root = Some(module_root.clone());
        let entry = self.insert_source(entry_path, source)?;
        self.package_entry = Some(entry);
        self.load_imports(entry, &module_root)?;
        self.load_standard_library()?;
        Ok(self.finish_ref(entry))
    }

    fn load_file(&mut self, path: &Path) -> Result<ModuleId, LoadDiagnostic> {
        if let Some(id) = self.paths.get(path) {
            return Ok(*id);
        }
        let source = std::fs::read_to_string(path).map_err(|error| {
            LoadDiagnostic::source(
                path,
                None,
                format!("could not read `{}`: {error}", path.display()),
            )
        })?;
        let id = self.insert_source(path.to_owned(), &source)?;
        let root = self.module_root.clone().expect("module root is set");
        self.load_imports(id, &root)?;
        Ok(id)
    }

    fn insert_source(&mut self, path: PathBuf, source: &str) -> Result<ModuleId, LoadDiagnostic> {
        let syntax = parse_with_syntax_ids(
            source,
            &mut self.next_syntax_id,
            &path.display().to_string(),
        )
        .map_err(|error| LoadDiagnostic {
            source: Some(path.clone()),
            range: Some(error.offset..error.offset),
            location: Some(error.location),
            message: error.message,
        })?;
        let id = ModuleId(self.modules.len());
        self.paths.insert(path.clone(), id);
        let qualified_name = path.display().to_string();
        self.modules.push(SourceModule {
            id,
            path: path.clone(),
            syntax,
            parent: None,
            name: None,
            visibility: Visibility::Private,
            qualified_name: qualified_name.clone(),
        });
        self.children.push(HashMap::new());
        self.insert_submodules(id, path, qualified_name)?;
        Ok(id)
    }

    fn insert_submodules(
        &mut self,
        parent: ModuleId,
        path: PathBuf,
        parent_name: String,
    ) -> Result<(), LoadDiagnostic> {
        let declarations = self.modules[parent.0]
            .syntax
            .items
            .iter()
            .filter_map(|item| match item {
                Item::Submodule(module) => Some(module.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        for declaration in declarations {
            if self.children[parent.0].contains_key(&declaration.name) {
                return Err(load_diagnostic_at(
                    &declaration.syntax.span,
                    format!("duplicate submodule definition of `{}`", declaration.name),
                ));
            }
            let id = ModuleId(self.modules.len());
            let qualified_name = format!("{parent_name}.{}", declaration.name);
            self.modules.push(SourceModule {
                id,
                path: path.clone(),
                syntax: declaration.module,
                parent: Some(parent),
                name: Some(declaration.name.clone()),
                visibility: declaration.visibility,
                qualified_name: qualified_name.clone(),
            });
            self.children.push(HashMap::new());
            self.children[parent.0].insert(declaration.name, id);
            self.child_modules.insert(declaration.syntax.id, id);
            self.insert_submodules(id, path.clone(), qualified_name)?;
        }
        Ok(())
    }

    fn load_imports(&mut self, module: ModuleId, root: &Path) -> Result<(), LoadDiagnostic> {
        if !self.loaded_imports.insert(module) {
            return Ok(());
        }
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
            match self.resolve_import(module, &declaration, root) {
                Ok(imported) => {
                    self.imported_modules
                        .insert(declaration.syntax.id, imported);
                }
                Err(_) if self.can_defer_inline_import(module, &declaration) => {}
                Err(error) => return Err(error),
            }
        }
        let children = self.children[module.0]
            .values()
            .copied()
            .collect::<Vec<_>>();
        for child in children {
            self.load_imports(child, root)?;
        }
        Ok(())
    }

    fn can_defer_inline_import(&self, module: ModuleId, declaration: &UseDeclaration) -> bool {
        let [name] = declaration.path.as_slice() else {
            return false;
        };
        self.modules[module.0]
            .syntax
            .items
            .iter()
            .take_while(|item| {
                !matches!(item, Item::UseDeclaration(use_) if use_.syntax.id == declaration.syntax.id)
            })
            .any(|item| {
                let syntax = match item {
                    Item::VisibilityMacroInvocation(invocation) => &invocation.syntax,
                    Item::Statement(statement) => match statement.as_ref() {
                        crate::Statement::Expression(expression) => expression.syntax(),
                        _ => return false,
                    },
                    _ => return false,
                };
                syntax
                    .tokens()
                    .iter()
                    .any(|token| token.kind == crate::TokenKind::Identifier && token.text == *name)
            })
    }

    fn resolve_import(
        &mut self,
        module: ModuleId,
        declaration: &UseDeclaration,
        root: &Path,
    ) -> Result<ModuleId, LoadDiagnostic> {
        let parts = &declaration.path;
        if parts.first().is_some_and(|part| part == "super") {
            let mut target = module;
            let mut index = 0;
            while parts.get(index).is_some_and(|part| part == "super") {
                target = self.modules[target.0].parent.ok_or_else(|| {
                    load_diagnostic_at(&declaration.syntax.span, "`super` has no parent module")
                })?;
                index += 1;
            }
            return self.traverse_children(target, &parts[index..], true, declaration);
        }

        if parts.first().is_some_and(|part| part == "package") {
            let entry = self.package_entry.ok_or_else(|| {
                load_diagnostic_at(&declaration.syntax.span, "package entry module is not set")
            })?;
            if parts.len() == 1 {
                return Ok(entry);
            }
            return self.resolve_file_import(&parts[1..], root, declaration);
        }

        if let Some(first) = parts.first()
            && let Some(child) = self.children[module.0].get(first).copied()
        {
            return self.traverse_children(child, &parts[1..], true, declaration);
        }

        let import_root = if parts.first().is_some_and(|part| part == "std") {
            self.resolve_standard_library_root()?
        } else {
            root.to_owned()
        };
        self.resolve_file_import(parts, &import_root, declaration)
    }

    fn resolve_file_import(
        &mut self,
        parts: &[String],
        import_root: &Path,
        declaration: &UseDeclaration,
    ) -> Result<ModuleId, LoadDiagnostic> {
        for prefix_len in (1..=parts.len()).rev() {
            let mut path = import_root.to_owned();
            for component in &parts[..prefix_len] {
                path.push(component);
            }
            path.set_extension("sta");
            let Ok(path) = std::fs::canonicalize(&path) else {
                continue;
            };
            let file = self.load_file(&path)?;
            return self.traverse_children(file, &parts[prefix_len..], false, declaration);
        }
        Err(load_diagnostic_at(
            &declaration.syntax.span,
            format!("could not resolve module `{}`", declaration.path.join(".")),
        ))
    }

    fn traverse_children(
        &self,
        mut module: ModuleId,
        parts: &[String],
        allow_private: bool,
        declaration: &UseDeclaration,
    ) -> Result<ModuleId, LoadDiagnostic> {
        for part in parts {
            let child = self.children[module.0].get(part).copied().ok_or_else(|| {
                load_diagnostic_at(
                    &declaration.syntax.span,
                    format!("module has no submodule named `{part}`"),
                )
            })?;
            if !allow_private && self.modules[child.0].visibility != Visibility::Public {
                return Err(load_diagnostic_at(
                    &declaration.syntax.span,
                    format!("submodule `{part}` is private"),
                ));
            }
            module = child;
        }
        Ok(module)
    }

    fn load_standard_library(&mut self) -> Result<(), LoadDiagnostic> {
        let root = self.resolve_standard_library_root()?;
        let core = canonical_file(&root.join("std/core.sta")).map_err(LoadDiagnostic::compiler)?;
        self.standard_library_core = Some(self.load_file(&core)?);
        let cinterop =
            canonical_file(&root.join("std/cinterop.sta")).map_err(LoadDiagnostic::compiler)?;
        self.standard_library_cinterop = Some(self.load_file(&cinterop)?);
        self.standard_library_io = canonical_file(&root.join("std/io.sta"))
            .ok()
            .and_then(|io| self.paths.get(&io).copied());
        Ok(())
    }

    fn resolve_standard_library_root(&mut self) -> Result<PathBuf, LoadDiagnostic> {
        if let Some(root) = &self.standard_library_root {
            return canonical_directory(root, "standard library").map_err(LoadDiagnostic::compiler);
        }
        if let Some(root) = std::env::var_os("STAPLE_STDLIB") {
            let root = PathBuf::from(root);
            let root = canonical_directory(&root, "standard library from `STAPLE_STDLIB`")
                .map_err(LoadDiagnostic::compiler)?;
            self.standard_library_root = Some(root.clone());
            return Ok(root);
        }
        let executable_root = std::env::current_exe().ok().and_then(|executable| {
            executable
                .parent()
                .and_then(Path::parent)
                .map(|prefix| prefix.join("lib/staple/stdlib"))
        });
        if let Some(root) = executable_root.as_deref()
            && let Ok(root) = canonical_directory(root, "standard library")
        {
            self.standard_library_root = Some(root.clone());
            return Ok(root);
        }
        if let Some(root) = default_standard_library_root()
            && let Ok(root) = canonical_directory(&root, "standard library")
        {
            self.standard_library_root = Some(root.clone());
            return Ok(root);
        }

        let searched = [executable_root, default_standard_library_root()]
            .into_iter()
            .flatten()
            .map(|path| format!("`{}`", path.display()))
            .collect::<Vec<_>>()
            .join(" and ");
        let searched = if searched.is_empty() {
            "the default locations".to_owned()
        } else {
            searched
        };
        Err(LoadDiagnostic::compiler(format!(
            "could not locate the Staple standard library in {searched}; run `install-stdlib.sh` from the Staple source tree, pass `--stdlib <path>`, or set `STAPLE_STDLIB`"
        )))
    }

    fn finish_ref(&mut self, entry: ModuleId) -> Program {
        let loader = std::mem::take(self);
        loader.finish(entry)
    }

    fn finish(self, entry: ModuleId) -> Program {
        let initialization_order = initialization_order(&self.modules, &self.imported_modules);
        Program {
            entry,
            standard_library_core: self.standard_library_core,
            standard_library_cinterop: self.standard_library_cinterop,
            standard_library_io: self.standard_library_io,
            modules: self.modules,
            imported_modules: self.imported_modules,
            child_modules: self.child_modules,
            children: self.children,
            initialization_order,
        }
    }
}

/// Returns the per-user standard-library installation path.
///
/// The installation script uses the same location by default.
pub fn default_standard_library_root() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(|home| standard_library_root_for_home(Path::new(&home)))
}

fn standard_library_root_for_home(home: &Path) -> PathBuf {
    home.join(".local/lib/staple/stdlib")
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

fn load_diagnostic_at(span: &Span, message: impl Into<String>) -> LoadDiagnostic {
    let (source, range, location) = match span {
        Span::User {
            source,
            range,
            location,
        } => (
            source.as_deref().map(PathBuf::from),
            Some(range.clone()),
            *location,
        ),
        Span::Compiler => (None, None, None),
    };
    LoadDiagnostic {
        source,
        range,
        location,
        message: message.into(),
    }
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
            members.sort_by(|left, right| {
                modules[left.0]
                    .path
                    .cmp(&modules[right.0].path)
                    .then_with(|| {
                        modules[left.0]
                            .qualified_name
                            .cmp(&modules[right.0].qualified_name)
                    })
            });
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_at_reports_a_structured_missing_import() {
        let entry = std::env::temp_dir().join("staple-loader-structured-test.sta");
        let error = ProgramLoader::new()
            .with_standard_library_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("stdlib"))
            .load_source_at(&entry, "use module_that_does_not_exist\n")
            .unwrap_err();
        assert_eq!(
            error.source.as_deref().and_then(Path::file_name),
            entry.file_name()
        );
        assert!(error.range.is_some());
        assert!(error.message.contains("module_that_does_not_exist"));
    }

    #[test]
    fn user_standard_library_has_a_stable_default_location() {
        assert_eq!(
            standard_library_root_for_home(Path::new("/home/staple")),
            PathBuf::from("/home/staple/.local/lib/staple/stdlib")
        );
    }
}
