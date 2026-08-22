use std::collections::{HashMap, HashSet};
use std::fmt;
use std::ops::Range;
use std::path::{Path, PathBuf};

use crate::parser::parse_with_syntax_ids;
use crate::{
    BlockExpression, Expression, Item, Module, SourceLocation, Span, Submodule,
    Syntax, SyntaxId, TokenKind, UseDeclaration, UseKind, Visibility,
};

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
    pub companion: bool,
}

/// The two module targets a bare dotted `use` declaration may denote.
///
/// `namespace` is the complete path, while `item_module` is the path without
/// its final component. The latter's public interface decides whether the
/// final component is an imported item.
#[derive(Debug, Clone)]
pub(crate) struct DottedImport {
    pub namespace: Option<ModuleId>,
    pub item_module: Option<ModuleId>,
    pub span: Span,
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
    standard_library_syntax: Option<ModuleId>,
    standard_library_cinterop: Option<ModuleId>,
    standard_library_io: Option<ModuleId>,
    modules: Vec<SourceModule>,
    imported_modules: HashMap<SyntaxId, ModuleId>,
    dotted_imports: HashMap<SyntaxId, DottedImport>,
    resolved_use_kinds: HashMap<SyntaxId, UseKind>,
    additional_imported_namespaces: HashMap<SyntaxId, ModuleId>,
    root_qualified_modules: HashMap<(ModuleId, String), ModuleId>,
    child_modules: HashMap<SyntaxId, ModuleId>,
    children: Vec<HashMap<String, ModuleId>>,
    initialization_order: Vec<ModuleId>,
}

impl Program {
    pub(crate) fn single(module: Module) -> Self {
        let mut program = Self {
            entry: ModuleId(0),
            standard_library_core: None,
            standard_library_syntax: None,
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
                companion: false,
            }],
            imported_modules: HashMap::new(),
            dotted_imports: HashMap::new(),
            resolved_use_kinds: HashMap::new(),
            additional_imported_namespaces: HashMap::new(),
            root_qualified_modules: HashMap::new(),
            child_modules: HashMap::new(),
            children: vec![HashMap::new()],
            initialization_order: vec![ModuleId(0)],
        };
        program.collect_single_submodules(ModuleId(0));
        program.resolve_single_inline_imports();
        program.initialization_order = initialization_order(
            &program.modules,
            &program.imported_modules,
            &program.additional_imported_namespaces,
            &program.root_qualified_modules,
        );
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
            if declaration.companion
                && let Some(existing) = self.children[parent.0].get(&declaration.name).copied()
                && self.modules[existing.0].companion
            {
                self.modules[existing.0]
                    .syntax
                    .items
                    .extend(declaration.module.items);
                self.child_modules.insert(declaration.syntax.id, existing);
                self.collect_single_submodules(existing);
                continue;
            }
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
                companion: declaration.companion,
            });
            self.children.push(HashMap::new());
            self.children[parent.0].insert(declaration.name, id);
            self.child_modules.insert(declaration.syntax.id, id);
            self.collect_single_submodules(id);
        }

        let mut block_declarations = Vec::new();
        find_block_submodules(&self.modules[parent.0].syntax.items, &mut block_declarations);
        for declaration in block_declarations {
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
                companion: false,
            });
            self.children.push(HashMap::new());
            self.child_modules.insert(declaration.syntax.id, id);
            self.collect_single_submodules(id);
        }
    }

    fn resolve_single_inline_imports(&mut self) {
        let mut uses = self
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
        for module in &self.modules {
            let mut block_declarations = Vec::new();
            find_block_use_declarations(&module.syntax.items, &mut block_declarations);
            uses.extend(
                block_declarations
                    .into_iter()
                    .map(|declaration| (module.id, declaration)),
            );
        }
        for (source, declaration) in uses {
            if declaration.kind == UseKind::Dotted
                && self.dotted_imports.contains_key(&declaration.syntax.id)
            {
                continue;
            }
            let namespace = self.resolve_single_inline_path(source, &declaration.path);
            if declaration.kind == UseKind::Dotted {
                let item_module = self
                    .resolve_single_inline_path(source, &declaration.path[..declaration.path.len() - 1]);
                self.dotted_imports.insert(
                    declaration.syntax.id,
                    DottedImport {
                        namespace,
                        item_module,
                        span: declaration.syntax.span.clone(),
                    },
                );
                if let Some(imported) = namespace.or(item_module) {
                    self.imported_modules.insert(declaration.syntax.id, imported);
                }
            } else if let Some(imported) = namespace {
                self.imported_modules.insert(declaration.syntax.id, imported);
            }
        }
    }

    fn resolve_single_inline_path(&self, source: ModuleId, parts: &[String]) -> Option<ModuleId> {
        let mut target = source;
        let mut index = 0;
        if parts.first().is_some_and(|part| part == "super") {
            while parts.get(index).is_some_and(|part| part == "super") {
                target = self.parent_module(target)?;
                index += 1;
            }
        } else if let Some(first) = parts.first()
            && let Some(child) = self.child_named(source, first)
        {
            target = child;
            index = 1;
        } else {
            return None;
        }
        for part in &parts[index..] {
            target = self.child_named(target, part)?;
        }
        Some(target)
    }

    /// Registers inline modules introduced after parsing, such as macro output.
    pub(crate) fn rebuild_generated_inline_modules(&mut self) {
        let mut parent = 0;
        while parent < self.modules.len() {
            let mut declarations = self.modules[parent]
                .syntax
                .items
                .iter()
                .filter_map(|item| match item {
                    Item::Submodule(module)
                        if !self.child_modules.contains_key(&module.syntax.id) =>
                    {
                        Some((module.clone(), true))
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();
            let mut block_declarations = Vec::new();
            find_block_submodules(
                &self.modules[parent].syntax.items,
                &mut block_declarations,
            );
            declarations.extend(
                block_declarations
                    .into_iter()
                    .filter(|declaration| {
                        !self.child_modules.contains_key(&declaration.syntax.id)
                    })
                    .map(|declaration| (declaration, false)),
            );
            for (declaration, top_level) in declarations {
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
                    companion: declaration.companion,
                });
                self.children.push(HashMap::new());
                if top_level {
                    self.children[parent].insert(declaration.name, id);
                }
                self.child_modules.insert(declaration.syntax.id, id);
            }
            parent += 1;
        }
        self.resolve_single_inline_imports();
        self.initialization_order = initialization_order(
            &self.modules,
            &self.imported_modules,
            &self.additional_imported_namespaces,
            &self.root_qualified_modules,
        );
    }

    pub fn entry(&self) -> ModuleId {
        self.entry
    }

    pub fn standard_library_core(&self) -> Option<ModuleId> {
        self.standard_library_core
    }

    pub fn standard_library_syntax(&self) -> Option<ModuleId> {
        self.standard_library_syntax
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

    /// Returns the resolved import interpretation when one is available.
    pub fn use_kind<'a>(&'a self, declaration: &'a UseDeclaration) -> &'a UseKind {
        self.resolved_use_kinds
            .get(&declaration.syntax.id)
            .unwrap_or(&declaration.kind)
    }

    pub(crate) fn dotted_import(&self, use_syntax: SyntaxId) -> Option<DottedImport> {
        self.dotted_imports.get(&use_syntax).cloned()
    }

    /// Resolves bare dotted imports once public interfaces are available.
    /// Returns source syntax IDs for imports that denote both a namespace and
    /// a public item.
    pub(crate) fn resolve_dotted_imports(
        &mut self,
        item_availability: impl Fn(ModuleId, &str, Option<ModuleId>) -> (bool, bool),
    ) -> Vec<Span> {
        let declarations = self
            .modules
            .iter()
            .flat_map(|module| {
                module
                    .syntax
                    .items
                    .iter()
                    .filter_map(|item| match item {
                        Item::UseDeclaration(declaration) => Some(declaration.clone()),
                        _ => None,
                    })
            })
            .chain(self.modules.iter().flat_map(|module| {
                let mut declarations = Vec::new();
                find_block_use_declarations(&module.syntax.items, &mut declarations);
                declarations
            }))
            .filter(|declaration| declaration.kind == UseKind::Dotted)
            .collect::<Vec<_>>();
        let mut ambiguous = Vec::new();
        for declaration in declarations {
            let Some(candidates) = self.dotted_import(declaration.syntax.id) else {
                continue;
            };
            let item = declaration
                .path
                .last()
                .expect("dotted import has a final component");
            let (public_item, conflicts_with_namespace) = candidates
                .item_module
                .map(|module| item_availability(module, item, candidates.namespace))
                .unwrap_or((false, false));
            match (
                candidates.namespace,
                candidates.item_module,
                public_item,
                conflicts_with_namespace,
            ) {
                (Some(_), Some(_), true, true) => ambiguous.push(candidates.span),
                (Some(namespace), Some(item_module), true, false) => {
                    self.imported_modules
                        .insert(declaration.syntax.id, item_module);
                    self.additional_imported_namespaces
                        .insert(declaration.syntax.id, namespace);
                    self.resolved_use_kinds.insert(
                        declaration.syntax.id,
                        UseKind::Selected(vec![item.clone()]),
                    );
                }
                (Some(namespace), _, false, _) => {
                    self.imported_modules.insert(declaration.syntax.id, namespace);
                    self.resolved_use_kinds
                        .insert(declaration.syntax.id, UseKind::Namespace);
                }
                (None, Some(item_module), _, _) => {
                    self.imported_modules
                        .insert(declaration.syntax.id, item_module);
                    self.resolved_use_kinds.insert(
                        declaration.syntax.id,
                        UseKind::Selected(vec![item.clone()]),
                    );
                }
                (Some(namespace), None, _, _) => {
                    self.imported_modules.insert(declaration.syntax.id, namespace);
                    self.resolved_use_kinds
                        .insert(declaration.syntax.id, UseKind::Namespace);
                }
                (None, None, _, _) => {}
            }
        }
        self.initialization_order = initialization_order(
            &self.modules,
            &self.imported_modules,
            &self.additional_imported_namespaces,
            &self.root_qualified_modules,
        );
        ambiguous
    }

    pub(crate) fn imported_modules(&self) -> &HashMap<SyntaxId, ModuleId> {
        &self.imported_modules
    }

    pub(crate) fn resolved_use_kinds(&self) -> &HashMap<SyntaxId, UseKind> {
        &self.resolved_use_kinds
    }

    pub(crate) fn additional_imported_namespaces(&self) -> &HashMap<SyntaxId, ModuleId> {
        &self.additional_imported_namespaces
    }

    pub(crate) fn root_qualified_modules(
        &self,
        module: ModuleId,
    ) -> impl Iterator<Item = (&str, ModuleId)> {
        self.root_qualified_modules
            .iter()
            .filter_map(move |((source, namespace), target)| {
                (*source == module).then_some((namespace.as_str(), *target))
            })
    }

    pub fn child_module(&self, submodule_syntax: SyntaxId) -> Option<ModuleId> {
        self.child_modules.get(&submodule_syntax).copied()
    }

    pub(crate) fn child_modules(&self) -> &HashMap<SyntaxId, ModuleId> {
        &self.child_modules
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
    dotted_imports: HashMap<SyntaxId, DottedImport>,
    root_qualified_modules: HashMap<(ModuleId, String), ModuleId>,
    child_modules: HashMap<SyntaxId, ModuleId>,
    children: Vec<HashMap<String, ModuleId>>,
    loaded_imports: HashSet<ModuleId>,
    next_syntax_id: usize,
    module_root: Option<PathBuf>,
    package_entry: Option<ModuleId>,
    standard_library_root: Option<PathBuf>,
    standard_library_core: Option<ModuleId>,
    standard_library_syntax: Option<ModuleId>,
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
        // A configured module root is an explicit package boundary. For the
        // legacy implicit-root mode, `main.sta` likewise denotes a package;
        // arbitrary standalone files must not cause their entire containing
        // directory (notably the system temp directory) to be indexed.
        let discover_companions = self.module_root.is_some()
            || entry.file_name().is_some_and(|name| name == "main.sta");
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
        if discover_companions {
            self.discover_companions()?;
        }
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

    fn discover_companions(&mut self) -> Result<(), LoadDiagnostic> {
        let mut roots = Vec::new();
        let mut seen_files = HashSet::new();
        if let Some(root) = &self.module_root {
            roots.push(root.clone());
        }
        if let Some(root) = &self.standard_library_root {
            roots.push(root.clone());
        }
        for root in roots {
            let mut files = Vec::new();
            collect_staple_files(&root, &mut files);
            files.sort();
            for path in files {
                let canonical = std::fs::canonicalize(&path).unwrap_or(path.clone());
                if self.paths.contains_key(&canonical) || !seen_files.insert(canonical.clone()) {
                    continue;
                }
                let source = std::fs::read_to_string(&canonical).map_err(|error| {
                    LoadDiagnostic::source(&canonical, None, format!("could not read `{}`: {error}", canonical.display()))
                })?;
                let syntax = parse_with_syntax_ids(
                    &source,
                    &mut self.next_syntax_id,
                    &canonical.display().to_string(),
                )
                .map_err(|error| LoadDiagnostic {
                    source: Some(canonical.clone()),
                    range: Some(error.offset..error.offset),
                    location: Some(error.location),
                    message: error.message,
                })?;
                let companions = syntax.items.into_iter().filter_map(|item| match item {
                    Item::Submodule(module) if module.companion => Some(module),
                    _ => None,
                }).collect::<Vec<_>>();
                for companion in companions {
                    let owner = self.modules.iter().find(|module| {
                        !module.companion
                            && module.path.starts_with(&root)
                            && module.syntax.items.iter().any(|item| matches!(
                                item,
                                Item::TypeDeclaration(declaration) if declaration.name == companion.name
                            ))
                    }).map(|module| module.id);
                    let Some(owner) = owner else { continue };
                    self.modules[owner.0].syntax.items.push(Item::Submodule(companion.clone()));
                    self.insert_discovered_companion(owner, canonical.clone(), companion)?;
                }
            }
        }
        Ok(())
    }

    fn insert_discovered_companion(
        &mut self,
        parent: ModuleId,
        path: PathBuf,
        declaration: Submodule,
    ) -> Result<(), LoadDiagnostic> {
        if let Some(existing) = self.children[parent.0].get(&declaration.name).copied() {
            if !self.modules[existing.0].companion {
                return Err(load_diagnostic_at(
                    &declaration.syntax.span,
                    format!("companion `{}` conflicts with an ordinary module", declaration.name),
                ));
            }
            self.modules[existing.0].syntax.items.extend(declaration.module.items);
            self.child_modules.insert(declaration.syntax.id, existing);
            return Ok(());
        }
        let id = ModuleId(self.modules.len());
        let qualified_name = format!("{}.{}", self.modules[parent.0].qualified_name, declaration.name);
        self.modules.push(SourceModule {
            id,
            path: path.clone(),
            syntax: declaration.module,
            parent: Some(parent),
            name: Some(declaration.name.clone()),
            visibility: Visibility::Public,
            qualified_name: qualified_name.clone(),
            companion: true,
        });
        self.children.push(HashMap::new());
        self.children[parent.0].insert(declaration.name, id);
        self.child_modules.insert(declaration.syntax.id, id);
        self.insert_submodules(id, path, qualified_name)
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
            companion: false,
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
            if declaration.companion
                && let Some(existing) = self.children[parent.0].get(&declaration.name).copied()
                && self.modules[existing.0].companion
            {
                self.modules[existing.0]
                    .syntax
                    .items
                    .extend(declaration.module.items);
                self.child_modules.insert(declaration.syntax.id, existing);
                self.insert_submodules(existing, path.clone(), format!("{parent_name}.{}", declaration.name))?;
                continue;
            }
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
                companion: declaration.companion,
            });
            self.children.push(HashMap::new());
            self.children[parent.0].insert(declaration.name, id);
            self.child_modules.insert(declaration.syntax.id, id);
            self.insert_submodules(id, path.clone(), qualified_name)?;
        }

        let mut block_declarations = Vec::new();
        find_block_submodules(&self.modules[parent.0].syntax.items, &mut block_declarations);
        for declaration in block_declarations {
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
                companion: false,
            });
            self.children.push(HashMap::new());
            self.child_modules.insert(declaration.syntax.id, id);
            self.insert_submodules(id, path.clone(), qualified_name)?;
        }
        Ok(())
    }

    fn load_imports(&mut self, module: ModuleId, root: &Path) -> Result<(), LoadDiagnostic> {
        if !self.loaded_imports.insert(module) {
            return Ok(());
        }
        let mut uses = self.modules[module.0]
            .syntax
            .items
            .iter()
            .filter_map(|item| match item {
                Item::UseDeclaration(declaration) => Some(declaration.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        find_block_use_declarations(&self.modules[module.0].syntax.items, &mut uses);
        for declaration in uses {
            let resolution = if declaration.kind == UseKind::Dotted {
                self.resolve_dotted_import(module, &declaration, root)
            } else {
                self.resolve_import(module, &declaration, root).map(Some)
            };
            match resolution {
                Ok(imported) => {
                    if let Some(imported) = imported {
                        self.imported_modules
                            .insert(declaration.syntax.id, imported);
                    }
                }
                Err(_) if self.can_defer_inline_import(module, &declaration) => {}
                Err(error) => return Err(error),
            }
        }
        self.load_root_qualified_references(module, root)?;
        let children = self.children[module.0]
            .values()
            .copied()
            .collect::<Vec<_>>();
        for child in children {
            self.load_imports(child, root)?;
        }
        Ok(())
    }

    fn resolve_dotted_import(
        &mut self,
        module: ModuleId,
        declaration: &UseDeclaration,
        root: &Path,
    ) -> Result<Option<ModuleId>, LoadDiagnostic> {
        let namespace = self.resolve_import(module, declaration, root);
        let mut item_declaration = declaration.clone();
        item_declaration
            .path
            .pop()
            .expect("dotted import has a final component");
        let item_module = self.resolve_import(module, &item_declaration, root);
        let namespace = namespace.ok();
        let item_module = item_module.ok();
        if namespace.is_none() && item_module.is_none() {
            return Err(self
                .resolve_import(module, declaration, root)
                .err()
                .or_else(|| self.resolve_import(module, &item_declaration, root).err())
                .expect("failed dotted import has a diagnostic"));
        }
        self.dotted_imports.insert(
            declaration.syntax.id,
            DottedImport {
                namespace,
                item_module,
                span: declaration.syntax.span.clone(),
            },
        );
        Ok(namespace.or(item_module))
    }

    fn load_root_qualified_references(
        &mut self,
        module: ModuleId,
        root: &Path,
    ) -> Result<(), LoadDiagnostic> {
        let mut ignored = Vec::new();
        for item in &self.modules[module.0].syntax.items {
            root_scan_ignored_ranges(item, &mut ignored);
        }
        let tokens = self.modules[module.0]
            .syntax
            .syntax
            .tokens()
            .iter()
            .filter(|token| {
                !token.kind.is_trivia()
                    && !ignored
                        .iter()
                        .any(|range| range.contains(&token.span.start))
            })
            .cloned()
            .collect::<Vec<_>>();
        let mut index = 0;
        while index < tokens.len() {
            let token = &tokens[index];
            // Binder dependency aliases can become additional recognized roots
            // here once the loader receives a per-package alias-to-root table.
            // Keep aliases logical: do not merge dependency files into `root`.
            if token.kind != TokenKind::Identifier
                || !matches!(token.text.as_str(), "std" | "package")
            {
                index += 1;
                continue;
            }
            let mut parts = vec![token.text.clone()];
            let mut end = index + 1;
            while tokens
                .get(end)
                .is_some_and(|token| token.kind == TokenKind::Dot)
                && tokens
                    .get(end + 1)
                    .is_some_and(|token| token.kind == TokenKind::Identifier)
            {
                parts.push(tokens[end + 1].text.clone());
                end += 2;
            }
            if tokens
                .get(end)
                .is_some_and(|token| token.kind == TokenKind::Dot)
                && tokens.get(end + 1).is_some_and(|token| {
                    matches!(
                        token.kind,
                        TokenKind::Operator
                            | TokenKind::Colon
                            | TokenKind::Equals
                            | TokenKind::Star
                            | TokenKind::Plus
                            | TokenKind::Minus
                            | TokenKind::Slash
                    )
                })
            {
                parts.push(tokens[end + 1].text.clone());
                end += 2;
            }
            if parts.len() >= 2 {
                let span = root_reference_span(
                    &self.modules[module.0].syntax.syntax.span,
                    token.span.start..tokens[end - 1].span.end,
                );
                let (namespace, target) =
                    self.resolve_root_qualified_reference(module, &parts, root, span)?;
                self.root_qualified_modules
                    .insert((module, namespace), target);
            }
            index = end;
        }
        Ok(())
    }

    fn resolve_root_qualified_reference(
        &mut self,
        module: ModuleId,
        parts: &[String],
        root: &Path,
        span: Span,
    ) -> Result<(String, ModuleId), LoadDiagnostic> {
        let minimum = if parts.first().is_some_and(|part| part == "std") {
            2
        } else {
            1
        };
        let mut last_error = None;
        for prefix_len in (minimum..parts.len()).rev() {
            let declaration = UseDeclaration {
                syntax: Syntax::synthetic(SyntaxId::COMPILER, span.clone()),
                visibility: Visibility::Private,
                path: parts[..prefix_len].to_vec(),
                kind: UseKind::Namespace,
            };
            match self.resolve_import(module, &declaration, root) {
                Ok(target) => return Ok((parts[..prefix_len].join("."), target)),
                Err(error) => last_error = Some(error),
            }
        }
        Err(last_error.unwrap_or_else(|| {
            load_diagnostic_at(
                &span,
                format!(
                    "could not resolve root-qualified item `{}`",
                    parts.join(".")
                ),
            )
        }))
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
                    Item::Expression(expression) => expression.syntax(),
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

        // A future Binder dependency root should be dispatched here, before
        // inline-child and package-file lookup, using the current package's
        // dependency alias map. The resulting ModuleId keeps versioned package
        // identities separate even when their internal module paths coincide.

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
        let syntax =
            canonical_file(&root.join("std/syntax.sta")).map_err(LoadDiagnostic::compiler)?;
        self.standard_library_syntax = Some(self.load_file(&syntax)?);
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
        let initialization_order = initialization_order(
            &self.modules,
            &self.imported_modules,
            &HashMap::new(),
            &self.root_qualified_modules,
        );
        Program {
            entry,
            standard_library_core: self.standard_library_core,
            standard_library_syntax: self.standard_library_syntax,
            standard_library_cinterop: self.standard_library_cinterop,
            standard_library_io: self.standard_library_io,
            modules: self.modules,
            imported_modules: self.imported_modules,
            dotted_imports: self.dotted_imports,
            resolved_use_kinds: HashMap::new(),
            additional_imported_namespaces: HashMap::new(),
            root_qualified_modules: self.root_qualified_modules,
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

fn collect_staple_files(directory: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_staple_files(&path, files);
        } else if path.extension().is_some_and(|extension| extension == "sta") {
            files.push(path);
        }
    }
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

fn root_reference_span(source_span: &Span, range: Range<usize>) -> Span {
    match source_span {
        Span::User { source, .. } => Span::User {
            source: source.clone(),
            range,
            location: None,
        },
        Span::Compiler => Span::Compiler,
    }
}

fn root_scan_ignored_ranges(item: &Item, ranges: &mut Vec<Range<usize>>) {
    match item {
        Item::UseDeclaration(declaration) => ranges.push(declaration.syntax.span.to_range()),
        Item::Submodule(declaration) => ranges.push(declaration.syntax.span.to_range()),
        Item::Modified(modified) => root_scan_ignored_ranges(&modified.item, ranges),
        Item::VisibilitySplice(splice) => root_scan_ignored_ranges(&splice.item, ranges),
        _ => {}
    }
}

/// Finds `mod` declarations nested inside block expressions anywhere in
/// `items`, without descending into an `Item::Submodule`'s own body (that
/// case is already handled by the caller's top-level scan and recursion).
fn find_block_submodules(items: &[Item], out: &mut Vec<Submodule>) {
    for item in items {
        find_block_submodules_in_item(item, out);
    }
}

fn find_block_submodules_in_item(item: &Item, out: &mut Vec<Submodule>) {
    match item {
        Item::Modified(modified) => find_block_submodules_in_item(&modified.item, out),
        Item::VisibilityMacroInvocation(invocation) => {
            find_block_submodules_in_expression(&invocation.expression, out)
        }
        Item::VisibilitySplice(splice) => find_block_submodules_in_item(&splice.item, out),
        Item::RepeatedItemSplice(_)
        | Item::UseDeclaration(_)
        | Item::Submodule(_)
        | Item::TypeDeclaration(_) => {}
        Item::ExternBlock(block) => {
            for binding in &block.bindings {
                if let Some(value) = &binding.value {
                    find_block_submodules_in_expression(value, out);
                }
            }
        }
        Item::MacroDeclaration(declaration) => {
            if let Some(value) = &declaration.value {
                find_block_submodules_in_expression(value, out);
            }
        }
        Item::TraitDeclaration(declaration) => {
            for member in &declaration.members {
                if let Some(default) = &member.default {
                    find_block_submodules_in_expression(default, out);
                }
            }
        }
        Item::TraitImplementation(implementation) => {
            for member in &implementation.members {
                find_block_submodules_in_expression(&member.value, out);
            }
        }
        item @ (Item::Binding(_)
        | Item::PatternBinding(_)
        | Item::Assignment(_)
        | Item::Return(_)
        | Item::Break(_)
        | Item::Continue(_)
        | Item::Expression(_)) => find_block_submodules_in_statement(item, out),
    }
}

fn find_block_submodules_in_statement(statement: &Item, out: &mut Vec<Submodule>) {
    match statement {
        Item::Binding(binding) => {
            if let Some(value) = &binding.value {
                find_block_submodules_in_expression(value, out);
            }
        }
        Item::PatternBinding(binding) => {
            find_block_submodules_in_expression(&binding.value, out)
        }
        Item::Assignment(assignment) => {
            find_block_submodules_in_expression(&assignment.target, out);
            find_block_submodules_in_expression(&assignment.value, out);
        }
        Item::Return(statement) => find_block_submodules_in_expression(&statement.value, out),
        Item::Break(statement) => {
            if let Some(value) = &statement.value {
                find_block_submodules_in_expression(value, out);
            }
        }
        Item::Continue(_) => {}
        Item::Expression(expression) => find_block_submodules_in_expression(expression, out),
        Item::Submodule(submodule) => out.push(submodule.clone()),
        Item::TypeDeclaration(_) => {}
        Item::UseDeclaration(_) => {}
        _ => {}
    }
}

fn find_block_submodules_in_expression(expression: &Expression, out: &mut Vec<Submodule>) {
    match expression {
        Expression::Function(function) => {
            find_block_submodules_in_expression(&function.body, out)
        }
        Expression::Satisfies(satisfies) => {
            find_block_submodules_in_expression(&satisfies.value, out)
        }
        Expression::Match(match_) => {
            find_block_submodules_in_expression(&match_.subject, out);
            for arm in &match_.arms {
                find_block_submodules_in_expression(&arm.body, out);
            }
        }
        Expression::Loop(loop_) => find_block_submodules_in_block(&loop_.body, out),
        Expression::Resource(_) => {}
        Expression::With(with) => {
            find_block_submodules_in_expression(&with.value, out);
            find_block_submodules_in_block(&with.body, out);
        }
        Expression::Block(block) => find_block_submodules_in_block(block, out),
        Expression::Product(product) => {
            for element in &product.elements {
                find_block_submodules_in_expression(&element.value, out);
            }
        }
        Expression::Call(call) => {
            find_block_submodules_in_expression(&call.callee, out);
            find_block_submodules_in_expression(&call.argument, out);
        }
        Expression::Access(access) => find_block_submodules_in_expression(&access.value, out),
        Expression::Index(index) => {
            find_block_submodules_in_expression(&index.value, out);
            find_block_submodules_in_expression(&index.index, out);
        }
        Expression::SyntaxArgument(_)
        | Expression::VisibilityArgument(_)
        | Expression::Quote(_)
        | Expression::Splice(_)
        | Expression::Name(_)
        | Expression::String(_)
        | Expression::CString(_)
        | Expression::Integer(_)
        | Expression::Float(_) => {}
    }
}

fn find_block_submodules_in_block(block: &BlockExpression, out: &mut Vec<Submodule>) {
    for statement in &block.items {
        find_block_submodules_in_statement(statement, out);
    }
}

/// Finds `use` declarations nested inside block expressions anywhere in
/// `items`, without descending into an `Item::Submodule`'s own body (that
/// submodule is a separate flat `SourceModule`, visited independently by
/// the caller's own recursion into it).
fn find_block_use_declarations(items: &[Item], out: &mut Vec<UseDeclaration>) {
    for item in items {
        find_block_use_declarations_in_item(item, out);
    }
}

fn find_block_use_declarations_in_item(item: &Item, out: &mut Vec<UseDeclaration>) {
    match item {
        Item::Modified(modified) => find_block_use_declarations_in_item(&modified.item, out),
        Item::VisibilityMacroInvocation(invocation) => {
            find_block_use_declarations_in_expression(&invocation.expression, out)
        }
        Item::VisibilitySplice(splice) => find_block_use_declarations_in_item(&splice.item, out),
        Item::RepeatedItemSplice(_)
        | Item::UseDeclaration(_)
        | Item::Submodule(_)
        | Item::TypeDeclaration(_) => {}
        Item::ExternBlock(block) => {
            for binding in &block.bindings {
                if let Some(value) = &binding.value {
                    find_block_use_declarations_in_expression(value, out);
                }
            }
        }
        Item::MacroDeclaration(declaration) => {
            if let Some(value) = &declaration.value {
                find_block_use_declarations_in_expression(value, out);
            }
        }
        Item::TraitDeclaration(declaration) => {
            for member in &declaration.members {
                if let Some(default) = &member.default {
                    find_block_use_declarations_in_expression(default, out);
                }
            }
        }
        Item::TraitImplementation(implementation) => {
            for member in &implementation.members {
                find_block_use_declarations_in_expression(&member.value, out);
            }
        }
        item @ (Item::Binding(_)
        | Item::PatternBinding(_)
        | Item::Assignment(_)
        | Item::Return(_)
        | Item::Break(_)
        | Item::Continue(_)
        | Item::Expression(_)) => find_block_use_declarations_in_statement(item, out),
    }
}

fn find_block_use_declarations_in_statement(statement: &Item, out: &mut Vec<UseDeclaration>) {
    match statement {
        Item::Binding(binding) => {
            if let Some(value) = &binding.value {
                find_block_use_declarations_in_expression(value, out);
            }
        }
        Item::PatternBinding(binding) => {
            find_block_use_declarations_in_expression(&binding.value, out)
        }
        Item::Assignment(assignment) => {
            find_block_use_declarations_in_expression(&assignment.target, out);
            find_block_use_declarations_in_expression(&assignment.value, out);
        }
        Item::Return(statement) => {
            find_block_use_declarations_in_expression(&statement.value, out)
        }
        Item::Break(statement) => {
            if let Some(value) = &statement.value {
                find_block_use_declarations_in_expression(value, out);
            }
        }
        Item::Continue(_) => {}
        Item::Expression(expression) => {
            find_block_use_declarations_in_expression(expression, out)
        }
        Item::Submodule(_) => {}
        Item::TypeDeclaration(_) => {}
        Item::UseDeclaration(declaration) => out.push(declaration.clone()),
        _ => {}
    }
}

fn find_block_use_declarations_in_expression(expression: &Expression, out: &mut Vec<UseDeclaration>) {
    match expression {
        Expression::Function(function) => {
            find_block_use_declarations_in_expression(&function.body, out)
        }
        Expression::Satisfies(satisfies) => {
            find_block_use_declarations_in_expression(&satisfies.value, out)
        }
        Expression::Match(match_) => {
            find_block_use_declarations_in_expression(&match_.subject, out);
            for arm in &match_.arms {
                find_block_use_declarations_in_expression(&arm.body, out);
            }
        }
        Expression::Loop(loop_) => find_block_use_declarations_in_block(&loop_.body, out),
        Expression::Resource(_) => {}
        Expression::With(with) => {
            find_block_use_declarations_in_expression(&with.value, out);
            find_block_use_declarations_in_block(&with.body, out);
        }
        Expression::Block(block) => find_block_use_declarations_in_block(block, out),
        Expression::Product(product) => {
            for element in &product.elements {
                find_block_use_declarations_in_expression(&element.value, out);
            }
        }
        Expression::Call(call) => {
            find_block_use_declarations_in_expression(&call.callee, out);
            find_block_use_declarations_in_expression(&call.argument, out);
        }
        Expression::Access(access) => {
            find_block_use_declarations_in_expression(&access.value, out)
        }
        Expression::Index(index) => {
            find_block_use_declarations_in_expression(&index.value, out);
            find_block_use_declarations_in_expression(&index.index, out);
        }
        Expression::SyntaxArgument(_)
        | Expression::VisibilityArgument(_)
        | Expression::Quote(_)
        | Expression::Splice(_)
        | Expression::Name(_)
        | Expression::String(_)
        | Expression::CString(_)
        | Expression::Integer(_)
        | Expression::Float(_) => {}
    }
}

fn find_block_use_declarations_in_block(block: &BlockExpression, out: &mut Vec<UseDeclaration>) {
    for statement in &block.items {
        find_block_use_declarations_in_statement(statement, out);
    }
}

fn initialization_order(
    modules: &[SourceModule],
    imports: &HashMap<SyntaxId, ModuleId>,
    additional_imports: &HashMap<SyntaxId, ModuleId>,
    root_qualified_modules: &HashMap<(ModuleId, String), ModuleId>,
) -> Vec<ModuleId> {
    let mut edges = vec![Vec::new(); modules.len()];
    for module in modules {
        for item in &module.syntax.items {
            if let Item::UseDeclaration(declaration) = item
                && let Some(imported) = imports.get(&declaration.syntax.id)
            {
                edges[module.id.0].push(*imported);
            }
            if let Item::UseDeclaration(declaration) = item
                && let Some(imported) = additional_imports.get(&declaration.syntax.id)
            {
                edges[module.id.0].push(*imported);
            }
        }
        let mut block_declarations = Vec::new();
        find_block_use_declarations(&module.syntax.items, &mut block_declarations);
        for declaration in &block_declarations {
            if let Some(imported) = imports.get(&declaration.syntax.id) {
                edges[module.id.0].push(*imported);
            }
            if let Some(imported) = additional_imports.get(&declaration.syntax.id) {
                edges[module.id.0].push(*imported);
            }
        }
        edges[module.id.0].sort();
        edges[module.id.0].dedup();
    }
    // Binder dependency aliases can feed this same logical-root map in the
    // future; initialization should depend on resolved package identities, not
    // on how their source directories happen to be laid out.
    for ((source, _), target) in root_qualified_modules {
        edges[source.0].push(*target);
        edges[source.0].sort();
        edges[source.0].dedup();
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
