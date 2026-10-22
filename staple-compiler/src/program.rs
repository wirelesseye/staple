use std::collections::{HashMap, HashSet};
use std::fmt;
use std::ops::Range;
use std::path::{Path, PathBuf};

use staple_syntax::{
    BlockExpression, Expression, Item, Module, SelectedImport, SourceLocation, Span, Submodule,
    Syntax, SyntaxId, TokenKind, UseDeclaration, UseKind, Visibility, parse_with_syntax_ids,
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
    /// A blanket `companion<T where Bound T> T` block. It belongs to every
    /// type satisfying its bounds, so it is never a named namespace.
    pub blanket: bool,
    /// A companion block whose target is not a nominal type: a structural
    /// type, or one the loader could not find. It is attached to its target
    /// by identity during resolution.
    pub detached: bool,
    /// The namespace of a nominal type with companion items: a module the
    /// loader makes for the type, with no source of its own, holding the
    /// members of every block for the type wherever the blocks are written.
    pub namespace: bool,
    /// For a companion block of a nominal type, the type's namespace.
    pub companion_namespace: Option<ModuleId>,
}

/// A source position: package-relative file path with 1-based line and column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourcePosition {
    pub file: String,
    pub line: usize,
    pub column: usize,
}

/// The 1-based line and column (counted in characters) of a byte offset in
/// `source`. `\r\n`, `\r` and `\n` each end a line.
pub(crate) fn source_location_at(source: &str, offset: usize) -> SourceLocation {
    let offset = offset.min(source.len());
    let bytes = source.as_bytes();
    let mut line = 1usize;
    let mut line_start = 0usize;
    let mut cursor = 0usize;
    while cursor < offset {
        match bytes[cursor] {
            b'\r' if bytes.get(cursor + 1) == Some(&b'\n') => {
                cursor += 2;
                line += 1;
                line_start = cursor;
            }
            b'\r' | b'\n' => {
                cursor += 1;
                line += 1;
                line_start = cursor;
            }
            _ => cursor += 1,
        }
    }
    SourceLocation {
        line,
        column: source[line_start..offset].chars().count() + 1,
    }
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
    executable_entry: Option<ModuleId>,
    package_graph: Option<staple_project::PackageGraph>,
    active_features: staple_project::ActiveFeatures,
    module_packages: Vec<Option<staple_project::PackageId>>,
    module_prelude: Vec<bool>,
    package_name: String,
    package_root: Option<ModuleId>,
    package_root_path: Option<PathBuf>,
    module_root: Option<PathBuf>,
    standard_library_core: Option<ModuleId>,
    standard_library_prelude: Option<ModuleId>,
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
    /// The next syntax identity no loaded node uses.
    next_syntax_id: usize,
    /// Imports a recovering loader could not resolve and left unbound.
    import_errors: Vec<LoadDiagnostic>,
}

impl Program {
    /// Imports a [`ProgramLoader::recovering_imports`] loader left unbound.
    pub fn import_errors(&self) -> &[LoadDiagnostic] {
        &self.import_errors
    }

    pub(crate) fn single(module: Module) -> Self {
        let visibility = module.visibility;
        // A module's own node is the last one its parse numbers.
        let next_syntax_id = module.syntax.id.0 + 1;
        let mut program = Self {
            entry: ModuleId(0),
            executable_entry: Some(ModuleId(0)),
            package_graph: None,
            active_features: HashMap::new(),
            module_packages: vec![None],
            module_prelude: vec![
                !module
                    .modifiers
                    .iter()
                    .any(|modifier| modifier.namespace.is_none() && modifier.name == "no_prelude"),
            ],
            package_name: "package".to_owned(),
            package_root: None,
            package_root_path: None,
            module_root: None,
            standard_library_core: None,
            standard_library_prelude: None,
            standard_library_syntax: None,
            standard_library_cinterop: None,
            standard_library_io: None,
            modules: vec![SourceModule {
                id: ModuleId(0),
                path: PathBuf::from("<memory>.sta"),
                syntax: module,
                parent: None,
                name: None,
                visibility,
                qualified_name: "<memory>".to_owned(),
                companion: false,
                blanket: false,
                detached: false,
                namespace: false,
                companion_namespace: None,
            }],
            imported_modules: HashMap::new(),
            dotted_imports: HashMap::new(),
            resolved_use_kinds: HashMap::new(),
            additional_imported_namespaces: HashMap::new(),
            root_qualified_modules: HashMap::new(),
            child_modules: HashMap::new(),
            children: vec![HashMap::new()],
            initialization_order: vec![ModuleId(0)],
            next_syntax_id,
            import_errors: Vec::new(),
        };
        program.collect_single_submodules(ModuleId(0));
        program.resolve_single_inline_imports();
        // Targets are found through imports, and imports through a type's
        // namespace need its companions grouped.
        let mut next_syntax_id = program.next_syntax_id;
        program.group_companions(&mut next_syntax_id);
        program.next_syntax_id = next_syntax_id;
        program.resolve_single_inline_imports();
        program.initialization_order = initialization_order(
            &program.modules,
            &program.imported_modules,
            &program.additional_imported_namespaces,
            &program.root_qualified_modules,
            &program.module_packages,
            &package_dependency_order(program.package_graph.as_ref()),
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
        for mut declaration in declarations {
            attach_companion_generics(&mut declaration, &mut self.next_syntax_id);
            let blanket = declaration.blanket_parameter().is_some();
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
                blanket,
                detached: false,
                namespace: false,
                companion_namespace: None,
            });
            self.module_packages.push(self.module_packages[parent.0]);
            self.children.push(HashMap::new());
            // A companion block is a module of its own, never reachable by
            // name; its members are reached through its target.
            if !declaration.companion {
                self.children[parent.0].insert(declaration.name, id);
            }
            self.child_modules.insert(declaration.syntax.id, id);
            self.collect_single_submodules(id);
        }

        let mut block_declarations = Vec::new();
        find_block_submodules(
            &self.modules[parent.0].syntax.items,
            &mut block_declarations,
        );
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
                blanket: false,
                detached: false,
                namespace: false,
                companion_namespace: None,
            });
            self.module_packages.push(self.module_packages[parent.0]);
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
                let item_module = self.resolve_single_inline_path(
                    source,
                    &declaration.path[..declaration.path.len() - 1],
                );
                self.dotted_imports.insert(
                    declaration.syntax.id,
                    DottedImport {
                        namespace,
                        item_module,
                        span: declaration.syntax.span.clone(),
                    },
                );
                if let Some(imported) = namespace.or(item_module) {
                    self.imported_modules
                        .insert(declaration.syntax.id, imported);
                }
            } else if let Some(imported) = namespace {
                self.imported_modules
                    .insert(declaration.syntax.id, imported);
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
    /// `next_syntax_id` continues the identities macro expansion allocated.
    pub(crate) fn rebuild_generated_inline_modules(&mut self, next_syntax_id: &mut usize) {
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
            find_block_submodules(&self.modules[parent].syntax.items, &mut block_declarations);
            declarations.extend(
                block_declarations
                    .into_iter()
                    .filter(|declaration| !self.child_modules.contains_key(&declaration.syntax.id))
                    .map(|declaration| (declaration, false)),
            );
            for (mut declaration, top_level) in declarations {
                attach_companion_generics(&mut declaration, next_syntax_id);
                let blanket = declaration.blanket_parameter().is_some();
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
                    blanket,
                    detached: false,
                    namespace: false,
                    companion_namespace: None,
                });
                self.module_packages.push(self.module_packages[parent]);
                self.children.push(HashMap::new());
                if top_level && !declaration.companion {
                    self.children[parent].insert(declaration.name, id);
                }
                self.child_modules.insert(declaration.syntax.id, id);
            }
            parent += 1;
        }
        // Macros such as `typegroup` declare types, so companion targets are
        // found again over the expanded items.
        self.group_companions(next_syntax_id);
        self.next_syntax_id = self.next_syntax_id.max(*next_syntax_id);
        self.resolve_single_inline_imports();
        self.initialization_order = initialization_order(
            &self.modules,
            &self.imported_modules,
            &self.additional_imported_namespaces,
            &self.root_qualified_modules,
            &self.module_packages,
            &package_dependency_order(self.package_graph.as_ref()),
        );
    }

    /// Groups `companion` blocks by the nominal types they extend; see
    /// `CompanionGrouping`.
    fn group_companions(&mut self, next_syntax_id: &mut usize) {
        let module_prelude = self.module_prelude.clone();
        CompanionGrouping {
            modules: &mut self.modules,
            children: &mut self.children,
            module_packages: &mut self.module_packages,
            next_syntax_id,
            child_modules: &self.child_modules,
            imported_modules: &self.imported_modules,
            dotted_imports: &self.dotted_imports,
            prelude: self.standard_library_prelude,
            uses_prelude: &|module: ModuleId| module_prelude.get(module.0).copied().unwrap_or(true),
        }
        .group();
        self.initialization_order = initialization_order(
            &self.modules,
            &self.imported_modules,
            &self.additional_imported_namespaces,
            &self.root_qualified_modules,
            &self.module_packages,
            &package_dependency_order(self.package_graph.as_ref()),
        );
    }

    /// Copies the expanded contents of flattened inline modules back into the
    /// declaring AST nodes. Resolution continues to use the flattened module
    /// table, while source-oriented consumers can walk the entry module without
    /// observing the pre-expansion inline-module snapshots.
    pub(crate) fn reattach_expanded_inline_modules(&mut self) {
        let expanded = self
            .child_modules
            .iter()
            .map(|(syntax, module)| (*syntax, self.modules[module.0].syntax.clone()))
            .collect::<HashMap<_, _>>();
        for module in &mut self.modules {
            reattach_inline_items(&mut module.syntax.items, &expanded);
        }
    }

    pub fn entry(&self) -> ModuleId {
        self.entry
    }

    /// The next syntax identity no loaded node uses.
    pub(crate) fn next_syntax_id(&self) -> usize {
        self.next_syntax_id
    }

    pub fn executable_entry(&self) -> Option<ModuleId> {
        self.executable_entry
    }

    pub fn package_graph(&self) -> Option<&staple_project::PackageGraph> {
        self.package_graph.as_ref()
    }
    pub fn active_features(&self) -> &staple_project::ActiveFeatures {
        &self.active_features
    }

    pub fn package_of(&self, module: ModuleId) -> Option<staple_project::PackageId> {
        self.module_packages.get(module.0).copied().flatten()
    }

    pub fn package_name(&self) -> &str {
        &self.package_name
    }

    pub fn package_root(&self) -> Option<ModuleId> {
        self.package_root
    }

    pub fn package_root_path(&self) -> Option<&Path> {
        self.package_root_path.as_deref()
    }

    /// The directory imported modules are resolved against, when one was
    /// configured (the package source root, or the entry file's directory).
    pub fn module_root(&self) -> Option<&Path> {
        self.module_root.as_deref()
    }

    /// The `use`-path-style dotted name of a module, e.g. `std.io`,
    /// `std.number.types`, or `example.tools.text` — the package name followed by
    /// the file path relative to the package source root (extension dropped,
    /// separators turned into dots) and then any enclosing inline `mod`
    /// segments. When there is no manifest at all, the package name is dropped
    /// and the result is just the module-root-relative path (`tools.text`).
    /// Returns `None` when neither a package nor a module root is known (a lone
    /// in-memory program), or when the file lies outside that root.
    pub fn module_dotted_name(&self, module: ModuleId) -> Option<String> {
        // Climb enclosing inline `mod` / block / companion submodules up to the
        // file-backed root; those carry a `name`, a file-backed module does not.
        let mut inline = Vec::new();
        let mut current = module;
        let file_root = loop {
            let source = self.module(current);
            match source.parent {
                Some(parent) => {
                    inline.push(source.name.clone()?);
                    current = parent;
                }
                None => break current,
            }
        };
        inline.reverse();

        let root = self.module(file_root);
        // Prefer the package graph; fall back to a configured package-root file,
        // then to the bare module root. Only the last of these carries no
        // package identity, so its name is left off (a relative path).
        let (package_name, source_root, package_root_path): (Option<&str>, &Path, Option<&Path>) =
            match self
                .package_of(file_root)
                .and_then(|id| self.package_graph.as_ref().map(|graph| graph.package(id)))
            {
                Some(package) => (
                    Some(&package.name),
                    package.source_root(),
                    Some(&package.root),
                ),
                None => match self.package_root_path.as_deref() {
                    Some(package_root_path) => (
                        Some(self.package_name()),
                        package_root_path.parent()?,
                        Some(package_root_path),
                    ),
                    // No manifest: a relative path rooted at the module root.
                    None => (None, self.module_root.as_deref()?, None),
                },
            };

        let mut segments: Vec<String> = package_name.map(str::to_owned).into_iter().collect();
        let is_package_root = self.package_root == Some(file_root)
            || package_root_path.is_some_and(|path| path == root.path);
        if !is_package_root {
            let relative = root
                .path
                .strip_prefix(source_root)
                .ok()
                .map(Path::to_path_buf)
                .or_else(|| {
                    let canonical = std::fs::canonicalize(&root.path).ok()?;
                    canonical
                        .strip_prefix(source_root)
                        .ok()
                        .map(Path::to_path_buf)
                })?;
            let relative = relative.with_extension("");
            let file_segments = relative
                .components()
                .filter_map(|component| match component {
                    std::path::Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
                    _ => None,
                })
                .collect::<Vec<_>>();
            // A `root.sta` sitting directly in the source root names the package
            // itself, not a `.root` submodule.
            if file_segments != ["root"] {
                segments.extend(file_segments);
            }
        }
        segments.extend(inline);
        if segments.is_empty() {
            return None;
        }
        Some(segments.join("."))
    }

    /// The stable, load-order-independent prefix used to mangle a module's
    /// symbols (`__staple_m{prefix}.{name}`, `__staple_init_m{prefix}`).
    ///
    /// This is the escaped [`Self::module_dotted_name`] — a package-qualified
    /// dotted path such as `std.io` or `example.tools.text`, derived from the
    /// package name and the file's location rather than its collection order.
    /// When no dotted name is available (a lone in-memory program, or a file
    /// outside any package root) it falls back to the positional module index,
    /// matching the historical scheme for those single-module cases.
    pub fn mangled_module_prefix(&self, module: ModuleId) -> String {
        match self.module_dotted_name(module) {
            Some(dotted) => dotted
                .split('.')
                .map(escape_symbol_segment)
                .collect::<Vec<_>>()
                .join("."),
            None => format!("m{}", module.0),
        }
    }

    pub fn standard_library_core(&self) -> Option<ModuleId> {
        self.standard_library_core
    }

    pub fn standard_library_prelude(&self) -> Option<ModuleId> {
        self.standard_library_prelude
    }

    pub fn module_uses_prelude(&self, module: ModuleId) -> bool {
        self.module_prelude.get(module.0).copied().unwrap_or(true)
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

    /// The path of a module's file as reported by `std.source.file` and
    /// `Location.file`: relative to the containing package's directory (or the
    /// module root when there is no package) when the file lies inside it,
    /// otherwise the module's own path.
    pub fn module_source_path(&self, id: ModuleId) -> String {
        let module = self.module(id);
        let directory = self
            .package_of(id)
            .and_then(|id| {
                self.package_graph()
                    .map(|graph| graph.package(id).directory.as_path())
            })
            .or_else(|| self.module_root());
        directory
            .and_then(|directory| {
                module
                    .path
                    .strip_prefix(directory)
                    .ok()
                    .map(Path::to_path_buf)
                    .or_else(|| {
                        let directory = std::fs::canonicalize(directory).ok()?;
                        module
                            .path
                            .strip_prefix(directory)
                            .ok()
                            .map(Path::to_path_buf)
                    })
            })
            .unwrap_or_else(|| module.path.clone())
            .display()
            .to_string()
    }

    /// The package-relative file path and 1-based line and column (columns
    /// count characters) of the start of a user span, using the same rules as
    /// the `std.source` macros. `None` when the span is not a user span or its
    /// source file is not part of this program.
    ///
    /// For a reference produced by macro expansion, pass the span recorded in
    /// `ResolvedModule::caller_location_span` rather than the reference's own
    /// span. Each call scans the program's modules; callers resolving many
    /// spans should cache the result per span.
    pub fn span_position(&self, span: &Span) -> Option<SourcePosition> {
        let Span::User {
            source: Some(source),
            range,
            location,
        } = span
        else {
            return None;
        };
        let module = self.modules.iter().find(|module| {
            matches!(
                &module.syntax.syntax.span,
                Span::User { source: Some(name), .. } if name == source
            )
        })?;
        let location = match location {
            Some(location) => *location,
            None => {
                let text: String = module
                    .syntax
                    .syntax
                    .tokens
                    .iter()
                    .map(|token| token.text.as_str())
                    .collect();
                source_location_at(&text, range.start)
            }
        };
        Some(SourcePosition {
            file: self.module_source_path(module.id),
            line: location.line,
            column: location.column,
        })
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
                module.syntax.items.iter().filter_map(|item| match item {
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
                        UseKind::Selected(vec![SelectedImport::new(item.clone())]),
                    );
                }
                (Some(namespace), _, false, _) => {
                    self.imported_modules
                        .insert(declaration.syntax.id, namespace);
                    self.resolved_use_kinds
                        .insert(declaration.syntax.id, UseKind::Namespace);
                }
                (None, Some(item_module), _, _) => {
                    self.imported_modules
                        .insert(declaration.syntax.id, item_module);
                    self.resolved_use_kinds.insert(
                        declaration.syntax.id,
                        UseKind::Selected(vec![SelectedImport::new(item.clone())]),
                    );
                }
                (Some(namespace), None, _, _) => {
                    self.imported_modules
                        .insert(declaration.syntax.id, namespace);
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
            &self.module_packages,
            &package_dependency_order(self.package_graph.as_ref()),
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

    /// The namespace an inline submodule declaration introduces. A
    /// `companion` block introduces none: its members are reached through
    /// its target.
    pub fn namespace_child(&self, submodule_syntax: SyntaxId) -> Option<ModuleId> {
        self.child_module(submodule_syntax)
            .filter(|child| !self.modules[child.0].companion)
    }

    /// The type namespaces `module` offers by name: one for each nominal
    /// type it declares that has companion items, and for each alias of
    /// such a type, with the declaration's visibility.
    pub fn type_namespaces(&self, module: ModuleId) -> Vec<(String, ModuleId, Visibility)> {
        let mut namespaces = self.children[module.0]
            .iter()
            .filter(|(_, child)| self.modules[child.0].namespace)
            .filter_map(|(name, child)| {
                let declaration = declared_type(&self.modules[module.0].syntax.items, name)?;
                Some((name.clone(), *child, declaration.visibility))
            })
            .collect::<Vec<_>>();
        namespaces.sort_by(|left, right| left.0.cmp(&right.0));
        namespaces
    }

    /// The module that stands for `module`'s namespace: for a companion
    /// block of a nominal type, the type's namespace; any other module is
    /// its own namespace.
    pub fn companion_primary(&self, module: ModuleId) -> ModuleId {
        self.modules[module.0]
            .companion_namespace
            .unwrap_or(module)
    }

    /// Every `companion` block contributing to the same type as `module`,
    /// with the type's namespace, in module order; just `module` for any
    /// other module.
    pub fn companion_blocks(&self, module: ModuleId) -> Vec<ModuleId> {
        let primary = self.companion_primary(module);
        if !self.modules[primary.0].namespace {
            return vec![module];
        }
        self.modules
            .iter()
            .filter(|candidate| self.companion_primary(candidate.id) == primary)
            .map(|candidate| candidate.id)
            .collect()
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
    package_root_path: Option<PathBuf>,
    package_root: Option<ModuleId>,
    package_name: String,
    package_entry: Option<ModuleId>,
    executable_entry: Option<ModuleId>,
    package_graph: Option<staple_project::PackageGraph>,
    feature_selection: staple_project::FeatureSelection,
    active_features: staple_project::ActiveFeatures,
    module_packages: Vec<Option<staple_project::PackageId>>,
    package_roots: HashMap<staple_project::PackageId, ModuleId>,
    standard_library_root: Option<PathBuf>,
    standard_library_core: Option<ModuleId>,
    standard_library_prelude: Option<ModuleId>,
    standard_library_syntax: Option<ModuleId>,
    standard_library_cinterop: Option<ModuleId>,
    standard_library_io: Option<ModuleId>,
    /// Collects unresolvable imports instead of failing on the first one.
    import_errors: Option<Vec<LoadDiagnostic>>,
}

impl ProgramLoader {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_standard_library_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.standard_library_root = Some(root.into());
        self
    }

    /// Loads past imports that do not resolve, leaving them unbound, for
    /// tooling that analyzes code being edited. The loaded program lists them
    /// in [`Program::import_errors`].
    pub fn recovering_imports(mut self) -> Self {
        self.import_errors = Some(Vec::new());
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

    pub fn with_package_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.package_root_path = Some(root.into());
        self
    }

    pub fn with_package_name(mut self, name: impl Into<String>) -> Self {
        self.package_name = name.into();
        self
    }

    pub fn with_package_graph(mut self, graph: staple_project::PackageGraph) -> Self {
        self.package_graph = Some(graph);
        self
    }
    pub fn with_feature_selection(mut self, selection: staple_project::FeatureSelection) -> Self {
        self.feature_selection = selection;
        self
    }

    pub fn load_package_graph(mut self) -> Result<Program, String> {
        self.load_package_graph_diagnostic(None)
            .map_err(|error| error.to_string())
    }

    pub fn load_package_graph_source_at(
        mut self,
        path: &Path,
        source: &str,
    ) -> Result<Program, LoadDiagnostic> {
        self.load_package_graph_diagnostic(Some((path, source)))
    }

    fn load_package_graph_diagnostic(
        &mut self,
        overlay: Option<(&Path, &str)>,
    ) -> Result<Program, LoadDiagnostic> {
        if self.package_graph.is_none() {
            return Err(LoadDiagnostic::compiler("package graph is not configured"));
        }
        // The standard library is an implicit dependency of every package. Add
        // it to the graph before the package loop below indexes each root, so
        // `use std....` resolves through the same alias machinery as any local
        // dependency.
        self.attach_std_package()?;
        self.active_features = staple_project::resolve_features(
            self.package_graph.as_ref().unwrap(),
            &self.feature_selection,
        )
        .map_err(LoadDiagnostic::compiler)?;
        let graph = self
            .package_graph
            .clone()
            .expect("package graph is configured");
        let root_package = graph.root_package().clone();
        self.module_root = Some(root_package.source_root().to_owned());
        self.package_root_path = Some(root_package.root.clone());
        self.package_name = root_package.name.clone();

        for (index, package) in graph.packages.iter().enumerate() {
            let package_id = staple_project::PackageId(index);
            let (source, exists) = if package.root.is_file() {
                (
                    if overlay.is_some_and(|(path, _)| same_source_path(path, &package.root)) {
                        overlay.unwrap().1.to_owned()
                    } else {
                        std::fs::read_to_string(&package.root).map_err(|error| {
                            LoadDiagnostic::source(&package.root, None, error.to_string())
                        })?
                    },
                    true,
                )
            } else {
                (String::new(), false)
            };
            let root = self.insert_source_for(package.root.clone(), &source, Some(package_id))?;
            // A missing root is an in-memory namespace anchor, not a private
            // source module. It is intentionally never written to disk.
            if !exists {
                self.modules[root.0].visibility = Visibility::Public;
            }
            self.package_roots.insert(package_id, root);
            if package_id == graph.root && exists {
                self.package_root = Some(root);
            }
        }

        let root_module = self.package_roots[&graph.root];
        let entry = if let Some(path) = &root_package.entry {
            let entry = if let Some(entry) = self.paths.get(path).copied() {
                entry
            } else if let Some((overlay_path, source)) = overlay
                && same_source_path(overlay_path, path)
            {
                self.insert_source_for(path.clone(), source, Some(graph.root))?
            } else {
                self.load_file(path)?
            };
            self.package_entry = Some(entry);
            self.executable_entry = Some(entry);
            entry
        } else {
            self.executable_entry = None;
            root_module
        };

        self.load_imports(root_module, root_package.source_root())?;
        if root_package.kind == staple_project::PackageKind::Library {
            let mut files = Vec::new();
            collect_staple_files(root_package.source_root(), &mut files);
            files.sort();
            for path in files {
                let path = std::fs::canonicalize(&path).unwrap_or(path);
                if self.paths.contains_key(&path) {
                    continue;
                }
                let source = if overlay
                    .is_some_and(|(overlay_path, _)| same_source_path(overlay_path, &path))
                {
                    overlay.unwrap().1.to_owned()
                } else {
                    std::fs::read_to_string(&path)
                        .map_err(|error| LoadDiagnostic::source(&path, None, error.to_string()))?
                };
                let syntax = staple_syntax::parse(&source).map_err(|error| {
                    LoadDiagnostic::source(&path, Some(error.offset..error.offset), error.message)
                })?;
                if syntax.visibility == Visibility::Public {
                    let module = self.insert_source_for(path, &source, Some(graph.root))?;
                    let module_root = self.module_root_for(module);
                    self.load_imports(module, &module_root)?;
                }
            }
        }
        if entry != root_module {
            self.load_imports(entry, root_package.source_root())?;
        }
        if let Some((path, source)) = overlay
            && !self
                .paths
                .keys()
                .any(|loaded| same_source_path(loaded, path))
        {
            let owner = self.infer_package(path).or(Some(graph.root));
            let edited = self.insert_source_for(path.to_owned(), source, owner)?;
            let edited_root = self.module_root_for(edited);
            self.load_imports(edited, &edited_root)?;
        }
        self.load_standard_library()?;
        self.discover_companions()?;
        Ok(self.finish_ref(entry))
    }

    pub fn load_path(mut self, entry: &Path) -> Result<Program, String> {
        self.load_path_diagnostic(entry)
            .map_err(|error| error.to_string())
    }

    fn load_path_diagnostic(&mut self, entry: &Path) -> Result<Program, LoadDiagnostic> {
        let entry = canonical_file(entry).map_err(LoadDiagnostic::compiler)?;
        // A configured module root is an explicit package boundary. For the
        // the emitter implicit-root mode, `main.sta` likewise denotes a package;
        // arbitrary standalone files must not cause their entire containing
        // directory (notably the system temp directory) to be indexed.
        let discover_companions = self.module_root.is_some()
            || self.package_root_path.is_some()
            || entry.file_name().is_some_and(|name| name == "main.sta");
        let configured_package_root = self.package_root_path.clone();
        let root = match (&self.module_root, &configured_package_root) {
            (Some(root), _) => {
                canonical_directory(root, "module root").map_err(LoadDiagnostic::compiler)?
            }
            (None, Some(package_root)) => canonical_directory(
                package_root.parent().unwrap_or_else(|| Path::new(".")),
                "package root directory",
            )
            .map_err(LoadDiagnostic::compiler)?,
            (None, None) => entry.parent().unwrap_or_else(|| Path::new(".")).to_owned(),
        };
        if !entry.starts_with(&root) {
            return Err(LoadDiagnostic::compiler(format!(
                "entry module `{}` is outside module root `{}`",
                entry.display(),
                root.display()
            )));
        }
        self.module_root = Some(root.clone());
        // `use std....` in the entry or package-root module is resolved below,
        // before `load_standard_library` runs, so the stdlib package must be in
        // the graph already.
        self.ensure_standard_library_dependency()?;
        if let Some(package_root) = configured_package_root {
            let file_name = package_root
                .file_name()
                .ok_or_else(|| LoadDiagnostic::compiler("package root path has no file name"))?;
            let package_root = root.join(file_name);
            self.package_root_path = Some(package_root.clone());
            if package_root.is_file() {
                let source = std::fs::read_to_string(&package_root).map_err(|error| {
                    LoadDiagnostic::source(
                        &package_root,
                        None,
                        format!("could not read `{}`: {error}", package_root.display()),
                    )
                })?;
                self.package_root = Some(self.insert_source(package_root, &source)?);
            }
        }
        let source = std::fs::read_to_string(&entry).map_err(|error| {
            LoadDiagnostic::source(
                &entry,
                None,
                format!("could not read `{}`: {error}", entry.display()),
            )
        })?;
        let entry_id = self.insert_source(entry, &source)?;
        self.package_entry = Some(entry_id);
        self.executable_entry = Some(entry_id);
        if let Some(package_root) = self.package_root {
            self.load_imports(package_root, &root)?;
        }
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
        self.ensure_standard_library_dependency()?;
        let path = root.join("<stdin>.sta");
        let entry = self.insert_source(path, source)?;
        self.package_entry = Some(entry);
        self.executable_entry = Some(entry);
        self.load_imports(entry, &root)?;
        self.load_standard_library()?;
        Ok(self.finish_ref(entry))
    }

    /// Loads an entry module from in-memory text while retaining its real path.
    /// Imported modules are loaded from the configured module root, or from the
    /// entry module's directory when no root was configured.
    pub fn load_source_at(self, path: &Path, source: &str) -> Result<Program, LoadDiagnostic> {
        self.load_package_source_at(path, source, path)
    }

    /// Loads a package entry while overlaying one editor-owned source file.
    pub fn load_package_source_at(
        mut self,
        path: &Path,
        source: &str,
        entry_path: &Path,
    ) -> Result<Program, LoadDiagnostic> {
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        let source_directory = std::fs::canonicalize(parent).map_err(|error| {
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
            None => source_directory.clone(),
        };
        let file_name = path
            .file_name()
            .ok_or_else(|| LoadDiagnostic::source(path, None, "source path has no file name"))?;
        let source_path = source_directory.join(file_name);
        if !source_path.starts_with(&module_root) {
            return Err(LoadDiagnostic::source(
                path,
                None,
                format!(
                    "source module `{}` is outside module root `{}`",
                    source_path.display(),
                    module_root.display()
                ),
            ));
        }
        self.module_root = Some(module_root.clone());
        self.ensure_standard_library_dependency()?;
        let edited = self.insert_source(source_path.clone(), source)?;
        if let Some(configured_root) = self.package_root_path.clone() {
            let root_name = configured_root.file_name().ok_or_else(|| {
                LoadDiagnostic::source(path, None, "package root path has no file name")
            })?;
            let root_path = module_root.join(root_name);
            self.package_root_path = Some(root_path.clone());
            if root_path == source_path {
                self.package_root = Some(edited);
            } else if root_path.is_file() {
                let root_source = std::fs::read_to_string(&root_path)
                    .map_err(|error| LoadDiagnostic::source(&root_path, None, error.to_string()))?;
                self.package_root = Some(self.insert_source(root_path, &root_source)?);
            }
        }
        let entry = if entry_path == path || source_path == entry_path {
            edited
        } else {
            let entry_path = canonical_file(entry_path)
                .map_err(|error| LoadDiagnostic::source(entry_path, None, error))?;
            if !entry_path.starts_with(&module_root) {
                return Err(LoadDiagnostic::source(
                    entry_path,
                    None,
                    "entry module is outside module root",
                ));
            }
            let entry_source = std::fs::read_to_string(&entry_path)
                .map_err(|error| LoadDiagnostic::source(&entry_path, None, error.to_string()))?;
            self.insert_source(entry_path, &entry_source)?
        };
        self.package_entry = Some(entry);
        self.executable_entry = Some(entry);
        if let Some(package_root) = self.package_root {
            self.load_imports(package_root, &module_root)?;
        }
        self.load_imports(edited, &module_root)?;
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
        if let Some(graph) = &self.package_graph {
            roots.extend(
                graph
                    .packages
                    .iter()
                    .map(|package| package.source_root().to_owned()),
            );
        }
        roots.sort();
        roots.dedup();
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
                    LoadDiagnostic::source(
                        &canonical,
                        None,
                        format!("could not read `{}`: {error}", canonical.display()),
                    )
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
                // A companion applies wherever its target is used, so a file
                // with a companion for a loaded type is part of the program
                // even when nothing imports it.
                let extends_loaded_type = syntax.items.iter().any(|item| match item {
                    Item::Submodule(companion)
                        if companion.companion && companion.blanket_parameter().is_none() =>
                    {
                        self.modules.iter().any(|module| {
                            !module.companion
                                && module.path.starts_with(&root)
                                && declared_type(&module.syntax.items, &companion.name).is_some()
                        })
                    }
                    _ => false,
                });
                if extends_loaded_type {
                    self.load_file(&canonical)?;
                }
            }
        }
        Ok(())
    }

    fn infer_package(&self, path: &Path) -> Option<staple_project::PackageId> {
        self.package_graph.as_ref().and_then(|graph| {
            graph
                .packages
                .iter()
                .enumerate()
                .filter(|(_, package)| path.starts_with(package.source_root()))
                .max_by_key(|(_, package)| package.source_root().components().count())
                .map(|(index, _)| staple_project::PackageId(index))
        })
    }

    /// The stdlib source root (`<stdlib>/std`), where `std/core.sta` and its
    /// siblings live. `resolve_standard_library_root` points one level above it.
    fn standard_library_package_root(&mut self) -> Result<PathBuf, LoadDiagnostic> {
        let root = self.resolve_standard_library_root()?;
        let package_root = root.join("std");
        Ok(std::fs::canonicalize(&package_root).unwrap_or(package_root))
    }

    /// Ensures the current graph carries the standard library as a library
    /// package and records `std` as a dependency alias on every package,
    /// including the stdlib itself. When the stdlib is already the graph's own
    /// package (`staple check` run against `stdlib/staple.kdl`), that package is
    /// reused rather than duplicated. Idempotent; returns the stdlib's
    /// `PackageId`.
    fn attach_std_package(&mut self) -> Result<staple_project::PackageId, LoadDiagnostic> {
        let package_root = self.standard_library_package_root()?;
        let graph = self
            .package_graph
            .as_mut()
            .expect("attach_std_package requires a graph");
        let std_id = if let Some(index) = graph
            .packages
            .iter()
            .position(|package| package.source_root() == package_root)
        {
            staple_project::PackageId(index)
        } else {
            let std_id = staple_project::PackageId(graph.packages.len());
            let manifest = package_root
                .parent()
                .unwrap_or(&package_root)
                .join("staple.kdl");
            // The stdlib's manifest decides its prelude, exactly as when the
            // stdlib is built as the root package, so both builds compile
            // its modules the same way and share one cached object.
            let prelude = staple_project::load_package_graph(&manifest)
                .map(|graph| graph.root_package().prelude)
                .unwrap_or(true);
            graph.packages.push(staple_project::Package {
                name: "std".to_owned(),
                kind: staple_project::PackageKind::Library,
                manifest,
                directory: package_root.clone(),
                root: package_root.join("root.sta"),
                entry: None,
                dependencies: Vec::new(),
                features: HashMap::new(),
                default_features: Vec::new(),
                prelude,
            });
            std_id
        };
        for package in &mut graph.packages {
            if !package
                .dependencies
                .iter()
                .any(|dependency| dependency.alias == "std")
            {
                package.dependencies.push(staple_project::Dependency {
                    alias: "std".to_owned(),
                    package: std_id,
                    default_features: true,
                    features: Vec::new(),
                });
            }
        }
        Ok(std_id)
    }

    /// Ensures a package graph exists and carries the standard library as an
    /// implicit dependency of every package. Safe to call from every entry
    /// point; a no-op once satisfied. Does not materialize any module, so it
    /// leaves module numbering untouched.
    fn ensure_standard_library_dependency(&mut self) -> Result<(), LoadDiagnostic> {
        if self.package_graph.is_none() {
            self.package_graph = Some(staple_project::PackageGraph {
                root: staple_project::PackageId(0),
                packages: Vec::new(),
            });
        }
        self.attach_std_package()?;
        Ok(())
    }

    /// Loads a dependency's declared root module, inserting a public
    /// namespace-anchor module when the package has no root file on disk (the
    /// standard library in a manifest-free build). Used only for bare
    /// `use <alias>`; dotted imports resolve a file under the package directly.
    fn materialize_package_root(
        &mut self,
        package: staple_project::PackageId,
    ) -> Result<ModuleId, LoadDiagnostic> {
        if let Some(id) = self.package_roots.get(&package).copied() {
            return Ok(id);
        }
        let root_path = self
            .package_graph
            .as_ref()
            .expect("package graph is configured")
            .package(package)
            .root
            .clone();
        let (source, exists) = match std::fs::read_to_string(&root_path) {
            Ok(source) => (source, true),
            Err(_) => (String::new(), false),
        };
        let module = self.insert_source_for(root_path, &source, Some(package))?;
        // A missing root is an in-memory namespace anchor, never written to disk.
        if !exists {
            self.modules[module.0].visibility = Visibility::Public;
        }
        self.package_roots.insert(package, module);
        Ok(module)
    }

    fn dependency_target(
        &self,
        module: ModuleId,
        alias: &str,
    ) -> Option<(staple_project::PackageId, PathBuf)> {
        let graph = self.package_graph.as_ref()?;
        // A module with no manifest package still belongs to the graph's root
        // for the purpose of resolving dependency aliases. This keeps `use
        // std....` working in single-file and stdin builds (whose root package
        // is synthetic) without granting those modules a real `PackageId` --
        // package-scoped visibility still requires an explicit manifest.
        let owner = self
            .module_packages
            .get(module.0)
            .copied()
            .flatten()
            .unwrap_or(graph.root);
        let dependency = graph
            .package(owner)
            .dependencies
            .iter()
            .find(|dependency| dependency.alias == alias)?;
        Some((
            dependency.package,
            graph.package(dependency.package).source_root().to_owned(),
        ))
    }

    fn module_root_for(&self, module: ModuleId) -> PathBuf {
        self.module_packages
            .get(module.0)
            .copied()
            .flatten()
            .and_then(|package| {
                self.package_graph
                    .as_ref()
                    .map(|graph| graph.package(package).source_root().to_owned())
            })
            .or_else(|| self.module_root.clone())
            .expect("module root is set")
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
        let root = self.module_root_for(id);
        self.load_imports(id, &root)?;
        Ok(id)
    }

    fn insert_source(&mut self, path: PathBuf, source: &str) -> Result<ModuleId, LoadDiagnostic> {
        let package = self.infer_package(&path);
        self.insert_source_for(path, source, package)
    }

    fn insert_source_for(
        &mut self,
        path: PathBuf,
        source: &str,
        package: Option<staple_project::PackageId>,
    ) -> Result<ModuleId, LoadDiagnostic> {
        let mut syntax = parse_with_syntax_ids(
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
        if let Some(package) = package {
            filter_feature_items(
                &mut syntax.items,
                package,
                self.package_graph.as_ref().unwrap(),
                &self.active_features,
            )?;
        } else if contains_feature_modifier(&syntax.items) {
            return Err(LoadDiagnostic::source(
                path.clone(),
                None,
                "`@feature` requires a package manifest",
            ));
        }
        let id = ModuleId(self.modules.len());
        let visibility = syntax.visibility;
        self.paths.insert(path.clone(), id);
        let qualified_name = path.display().to_string();
        self.modules.push(SourceModule {
            id,
            path: path.clone(),
            syntax,
            parent: None,
            name: None,
            visibility,
            qualified_name: qualified_name.clone(),
            companion: false,
            blanket: false,
            detached: false,
            namespace: false,
            companion_namespace: None,
        });
        self.module_packages.push(package);
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
        for mut declaration in declarations {
            attach_companion_generics(&mut declaration, &mut self.next_syntax_id);
            let blanket = declaration.blanket_parameter().is_some();
            if !declaration.companion && self.children[parent.0].contains_key(&declaration.name) {
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
                blanket,
                detached: false,
                namespace: false,
                companion_namespace: None,
            });
            self.module_packages.push(self.module_packages[parent.0]);
            self.children.push(HashMap::new());
            if !declaration.companion {
                self.children[parent.0].insert(declaration.name, id);
            }
            self.child_modules.insert(declaration.syntax.id, id);
            self.insert_submodules(id, path.clone(), qualified_name)?;
        }

        let mut block_declarations = Vec::new();
        find_block_submodules(
            &self.modules[parent.0].syntax.items,
            &mut block_declarations,
        );
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
                blanket: false,
                detached: false,
                namespace: false,
                companion_namespace: None,
            });
            self.module_packages.push(self.module_packages[parent.0]);
            self.children.push(HashMap::new());
            self.child_modules.insert(declaration.syntax.id, id);
            self.insert_submodules(id, path.clone(), qualified_name)?;
        }
        Ok(())
    }

    fn load_imports(&mut self, module: ModuleId, _root: &Path) -> Result<(), LoadDiagnostic> {
        if !self.loaded_imports.insert(module) {
            return Ok(());
        }
        let root = self.module_root_for(module);
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
                self.resolve_dotted_import(module, &declaration, &root)
            } else {
                self.resolve_import(module, &declaration, &root).map(Some)
            };
            match resolution {
                Ok(imported) => {
                    if let Some(imported) = imported {
                        self.imported_modules
                            .insert(declaration.syntax.id, imported);
                    }
                }
                Err(_) if self.can_defer_inline_import(module, &declaration) => {}
                Err(error) => self.import_error(error)?,
            }
        }
        if let Err(error) = self.load_root_qualified_references(module, &root) {
            self.import_error(error)?;
        }
        let mut children = self.children[module.0]
            .values()
            .copied()
            .collect::<Vec<_>>();
        // Companion blocks are not reachable by name.
        children.extend(
            self.modules
                .iter()
                .filter(|child| child.companion && child.parent == Some(module))
                .map(|child| child.id)
                .filter(|child| !self.children[module.0].values().any(|named| named == child)),
        );
        for child in children {
            self.load_imports(child, &root)?;
        }
        Ok(())
    }

    /// Records an unresolvable import when recovering, or fails with it.
    fn import_error(&mut self, error: LoadDiagnostic) -> Result<(), LoadDiagnostic> {
        match &mut self.import_errors {
            Some(errors) => {
                errors.push(error);
                Ok(())
            }
            None => Err(error),
        }
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
            if !matches!(token.kind, TokenKind::Identifier | TokenKind::Package)
                || (token.text != "package"
                    && self.dependency_target(module, &token.text).is_none())
            {
                index += 1;
                continue;
            }
            let mut parts = vec![token.text.clone()];
            let mut end = index + 1;
            while tokens
                .get(end)
                .is_some_and(|token| token.kind == TokenKind::Dot)
                && tokens.get(end + 1).is_some_and(|token| {
                    matches!(token.kind, TokenKind::Identifier | TokenKind::Package)
                })
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
                            | TokenKind::Percent
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
        // `std` has only a namespace-anchor root, so a bare `std` reference
        // resolves to nothing useful; require at least `std.<item>`. This is a
        // policy choice now, not a mechanism constraint.
        let minimum = if parts.first().is_some_and(|part| part == "std") {
            2
        } else {
            1
        };
        let mut last_error = None;
        let mut longest = None;
        // Record every resolvable prefix, not just the longest one. A chain
        // like `package.outer.inner.value` needs `package.outer` and
        // `package.outer.inner` registered as namespaces too, so tooling can
        // resolve each intermediate segment; resolving the shorter prefixes
        // here also loads those ancestor modules as a side effect.
        for prefix_len in minimum..parts.len() {
            let declaration = UseDeclaration {
                syntax: Syntax::synthetic(SyntaxId::COMPILER, span.clone()),
                visibility: Visibility::Private,
                path: parts[..prefix_len].to_vec(),
                kind: UseKind::Namespace,
            };
            match self.resolve_import(module, &declaration, root) {
                Ok(target) => {
                    let namespace = parts[..prefix_len].join(".");
                    self.root_qualified_modules
                        .insert((module, namespace.clone()), target);
                    longest = Some((namespace, target));
                }
                Err(error) => last_error = Some(error),
            }
        }
        longest.ok_or_else(|| {
            last_error.unwrap_or_else(|| {
                load_diagnostic_at(
                    &span,
                    format!(
                        "could not resolve root-qualified item `{}`",
                        parts.join(".")
                    ),
                )
            })
        })
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
                    .any(|token| token.kind == staple_syntax::TokenKind::Identifier && token.text == *name)
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
            return self.traverse_children(module, target, &parts[index..], true, declaration);
        }

        if parts.first().is_some_and(|part| part == "package") {
            if let Some(package) = self.module_packages.get(module.0).copied().flatten()
                && let Some(root_module) = self.package_roots.get(&package).copied()
            {
                if parts.len() == 1 {
                    return Ok(root_module);
                }
                let source_root = self
                    .package_graph
                    .as_ref()
                    .expect("module package requires a graph")
                    .package(package)
                    .source_root()
                    .to_owned();
                return self.resolve_file_import(module, &parts[1..], &source_root, declaration);
            }
            let entry = self.package_entry.ok_or_else(|| {
                load_diagnostic_at(&declaration.syntax.span, "package entry module is not set")
            })?;
            if parts.len() == 1 {
                return Ok(self.package_root.unwrap_or(entry));
            }
            return self.resolve_file_import(module, &parts[1..], root, declaration);
        }

        if let Some(alias) = parts.first()
            && let Some((package, source_root)) = self.dependency_target(module, alias)
        {
            if parts.len() == 1 {
                let root_module = self.materialize_package_root(package)?;
                self.load_imports(root_module, &source_root)?;
                return Ok(root_module);
            }
            // Pull in the dependency's root module only when it exists on disk
            // (it may re-export names); a namespace-anchor package -- notably the
            // standard library in a manifest-free build -- has nothing to load,
            // and materializing it here would perturb module numbering.
            if let Some(root_module) = self.package_roots.get(&package).copied() {
                self.load_imports(root_module, &source_root)?;
            }
            return self.resolve_file_import(module, &parts[1..], &source_root, declaration);
        }

        if let Some(first) = parts.first()
            && (parts.len() > 1 || declaration.kind != UseKind::Dotted)
            && let Some(child) = self.child_or_type_namespace(module, first)
        {
            return self.traverse_children(module, child, &parts[1..], true, declaration);
        }
        if let Some(first) = parts.first()
            && let Some(child) = self.children[module.0].get(first).copied()
        {
            return self.traverse_children(module, child, &parts[1..], true, declaration);
        }

        // `std` is not special here: it is an implicit dependency alias handled
        // by the `dependency_target` branch above, like any local dependency.
        self.resolve_file_import(module, parts, root, declaration)
    }

    fn resolve_file_import(
        &mut self,
        importing_module: ModuleId,
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
            if file != importing_module && !self.module_visible_from(file, importing_module) {
                return Err(load_diagnostic_at(
                    &declaration.syntax.span,
                    format!("module `{}` is private", parts[..prefix_len].join(".")),
                ));
            }
            return self.traverse_children(
                importing_module,
                file,
                &parts[prefix_len..],
                false,
                declaration,
            );
        }
        Err(load_diagnostic_at(
            &declaration.syntax.span,
            format!("could not resolve module `{}`", declaration.path.join(".")),
        ))
    }

    fn traverse_children(
        &mut self,
        importing_module: ModuleId,
        mut module: ModuleId,
        parts: &[String],
        allow_private: bool,
        declaration: &UseDeclaration,
    ) -> Result<ModuleId, LoadDiagnostic> {
        for (index, part) in parts.iter().enumerate() {
            // A bare dotted import ending at a type imports the type itself,
            // which brings its namespace along.
            let item = declaration.kind == UseKind::Dotted && index + 1 == parts.len();
            let child = if item {
                self.children[module.0].get(part).copied()
            } else {
                self.child_or_type_namespace(module, part)
            };
            let child = child.ok_or_else(|| {
                load_diagnostic_at(
                    &declaration.syntax.span,
                    format!("module has no submodule named `{part}`"),
                )
            })?;
            if !allow_private && !self.module_visible_from(child, importing_module) {
                return Err(load_diagnostic_at(
                    &declaration.syntax.span,
                    format!("submodule `{part}` is private"),
                ));
            }
            module = child;
        }
        Ok(module)
    }

    /// The child of `module` named `part`, or the namespace of the nominal
    /// type of that name `module` declares, made when an import path goes
    /// through it.
    fn child_or_type_namespace(&mut self, module: ModuleId, part: &str) -> Option<ModuleId> {
        if let Some(child) = self.children[module.0].get(part) {
            return Some(*child);
        }
        let declaration = declared_type(&self.modules[module.0].syntax.items, part)?;
        if declaration.kind() == staple_syntax::TypeDeclarationKind::Alias {
            return None;
        }
        Some(type_namespace(
            &mut self.modules,
            &mut self.children,
            &mut self.module_packages,
            &mut self.next_syntax_id,
            module,
            part,
        ))
    }

    fn module_visible_from(&self, target: ModuleId, importing: ModuleId) -> bool {
        match self.modules[target.0].visibility {
            Visibility::Public => true,
            Visibility::Package => self.module_packages[target.0]
                .zip(self.module_packages[importing.0])
                .is_some_and(|(target, importing)| target == importing),
            Visibility::Private => false,
        }
    }

    fn load_standard_library(&mut self) -> Result<(), LoadDiagnostic> {
        self.ensure_standard_library_dependency()?;
        let root = self.resolve_standard_library_root()?;
        let syntax =
            canonical_file(&root.join("std/syntax.sta")).map_err(LoadDiagnostic::compiler)?;
        self.standard_library_syntax = Some(self.load_file(&syntax)?);
        let core = canonical_file(&root.join("std/core.sta")).map_err(LoadDiagnostic::compiler)?;
        self.standard_library_core = Some(self.load_file(&core)?);
        let prelude =
            canonical_file(&root.join("std/prelude.sta")).map_err(LoadDiagnostic::compiler)?;
        self.standard_library_prelude = Some(self.load_file(&prelude)?);
        let cinterop =
            canonical_file(&root.join("std/cinterop.sta")).map_err(LoadDiagnostic::compiler)?;
        self.standard_library_cinterop = Some(self.load_file(&cinterop)?);
        let ptr = canonical_file(&root.join("std/ptr.sta")).map_err(LoadDiagnostic::compiler)?;
        self.load_file(&ptr)?;
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
            &self.module_packages,
            &package_dependency_order(self.package_graph.as_ref()),
        );
        let module_prelude = self
            .modules
            .iter()
            .enumerate()
            .map(|(index, module)| {
                let package_default = self.module_packages[index]
                    .and_then(|id| {
                        self.package_graph
                            .as_ref()
                            .map(|graph| graph.package(id).prelude)
                    })
                    .unwrap_or(true);
                package_default
                    && !module.syntax.modifiers.iter().any(|modifier| {
                        modifier.namespace.is_none() && modifier.name == "no_prelude"
                    })
            })
            .collect();
        let mut program = Program {
            entry,
            executable_entry: self.executable_entry,
            package_graph: self.package_graph,
            active_features: self.active_features,
            module_packages: self.module_packages,
            module_prelude,
            package_name: if self.package_name.is_empty() {
                "package".to_owned()
            } else {
                self.package_name
            },
            package_root: self.package_root,
            package_root_path: self.package_root_path,
            module_root: self.module_root,
            standard_library_core: self.standard_library_core,
            standard_library_prelude: self.standard_library_prelude,
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
            next_syntax_id: self.next_syntax_id,
            import_errors: self.import_errors.unwrap_or_default(),
        };
        let mut next_syntax_id = program.next_syntax_id;
        program.group_companions(&mut next_syntax_id);
        program.next_syntax_id = next_syntax_id;
        program
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

/// Renders one dotted-path segment into a form safe to embed in a linker
/// symbol. Ordinary identifier segments pass through untouched; any other
/// byte (a `-` in a package name, a space in a file stem, …) is hex-escaped
/// as `_xx`, mirroring the operator encoding in `resolve::mangle_function_name`.
fn escape_symbol_segment(segment: &str) -> String {
    let ordinary = !segment.is_empty()
        && segment
            .bytes()
            .all(|byte| byte == b'_' || byte.is_ascii_alphanumeric());
    if ordinary {
        return segment.to_owned();
    }
    let mut escaped = String::with_capacity(segment.len());
    for byte in segment.bytes() {
        if byte == b'_' || byte.is_ascii_alphanumeric() {
            escaped.push(byte as char);
        } else {
            escaped.push_str(&format!("_{byte:02x}"));
        }
    }
    escaped
}

fn canonical_directory(path: &Path, description: &str) -> Result<PathBuf, String> {
    std::fs::canonicalize(path).map_err(|error| {
        format!(
            "could not resolve {description} `{}`: {error}",
            path.display()
        )
    })
}

fn contains_feature_modifier(items: &[Item]) -> bool {
    items.iter().any(|item| match item {
        Item::Modified(modified) => {
            modified
                .modifiers
                .iter()
                .any(|modifier| modifier.namespace.is_none() && modifier.name == "feature")
                || contains_feature_modifier(std::slice::from_ref(&modified.item))
        }
        Item::Submodule(module) => contains_feature_modifier(&module.module.items),
        _ => false,
    })
}

fn filter_feature_items(
    items: &mut Vec<Item>,
    package: staple_project::PackageId,
    graph: &staple_project::PackageGraph,
    active: &staple_project::ActiveFeatures,
) -> Result<(), LoadDiagnostic> {
    let mut filtered = Vec::new();
    for item in std::mem::take(items) {
        if let Some(mut item) =
            filter_feature_item(item, &graph.package(package).features, active.get(&package))?
        {
            if let Item::Submodule(module) = &mut item {
                filter_feature_items(&mut module.module.items, package, graph, active)?;
            }
            filtered.push(item);
        }
    }
    *items = filtered;
    Ok(())
}

fn filter_feature_item(
    item: Item,
    declared: &HashMap<String, Vec<staple_project::FeatureMember>>,
    enabled: Option<&HashSet<String>>,
) -> Result<Option<Item>, LoadDiagnostic> {
    let Item::Modified(mut modified) = item else {
        return Ok(Some(item));
    };
    let mut include = true;
    let mut remaining = Vec::new();
    for modifier in modified.modifiers {
        if modifier.namespace.is_some() || modifier.name != "feature" {
            remaining.push(modifier);
            continue;
        }
        let argument = modifier.argument.as_ref().ok_or_else(|| {
            load_diagnostic_at(
                &modifier.syntax.span,
                "`@feature` requires a parenthesized string literal",
            )
        })?;
        let Some(Expression::String(literal)) = argument.expression.as_ref() else {
            return Err(load_diagnostic_at(
                &modifier.syntax.span,
                "`@feature` requires a string literal argument",
            ));
        };
        let feature = staple_syntax::string_literal::decode(&literal.literal)
            .map_err(|message| load_diagnostic_at(&modifier.syntax.span, message))?;
        if !declared.contains_key(&feature) {
            return Err(load_diagnostic_at(
                &modifier.syntax.span,
                format!("package does not declare feature `{feature}`"),
            ));
        }
        include &= enabled.is_some_and(|features| features.contains(&feature));
    }
    if !include {
        return Ok(None);
    }
    modified.modifiers = remaining;
    Ok(Some(if modified.modifiers.is_empty() {
        *modified.item
    } else {
        Item::Modified(modified)
    }))
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

fn same_source_path(left: &Path, right: &Path) -> bool {
    if left == right {
        return true;
    }
    std::fs::canonicalize(left)
        .ok()
        .zip(std::fs::canonicalize(right).ok())
        .is_some_and(|(left, right)| left == right)
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

fn reattach_inline_items(items: &mut [Item], expanded: &HashMap<SyntaxId, Module>) {
    for item in items {
        match item {
            Item::Submodule(submodule) => {
                if let Some(module) = expanded.get(&submodule.syntax.id) {
                    submodule.module = module.clone();
                }
                reattach_inline_items(&mut submodule.module.items, expanded);
            }
            Item::Modified(modified) => {
                reattach_inline_items(std::slice::from_mut(&mut modified.item), expanded)
            }
            Item::VisibilitySplice(splice) => {
                reattach_inline_items(std::slice::from_mut(&mut splice.item), expanded)
            }
            _ => {}
        }
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
        | Item::Expression(_)) => find_block_submodules_in_block_item(item, out),
    }
}

fn find_block_submodules_in_block_item(item: &Item, out: &mut Vec<Submodule>) {
    match item {
        Item::Binding(binding) => {
            if let Some(value) = &binding.value {
                find_block_submodules_in_expression(value, out);
            }
        }
        Item::PatternBinding(binding) => find_block_submodules_in_expression(&binding.value, out),
        Item::Assignment(assignment) => {
            find_block_submodules_in_expression(&assignment.target, out);
            find_block_submodules_in_expression(&assignment.value, out);
        }
        Item::Return(item) => find_block_submodules_in_expression(&item.value, out),
        Item::Break(item) => {
            if let Some(value) = &item.value {
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
        Expression::Function(function) => find_block_submodules_in_expression(&function.body, out),
        Expression::TypeApplication(application) => {
            find_block_submodules_in_expression(&application.value, out)
        }
        Expression::TypeAscription(ascription) => {
            find_block_submodules_in_expression(&ascription.value, out)
        }
        Expression::Match(match_) => {
            find_block_submodules_in_expression(&match_.subject, out);
            for arm in &match_.arms {
                find_block_submodules_in_expression(&arm.body, out);
            }
        }
        Expression::Loop(loop_) => find_block_submodules_in_block(&loop_.body, out),
        Expression::Coro(coro) => find_block_submodules_in_block(&coro.body, out),
        Expression::Await(await_) => find_block_submodules_in_expression(&await_.operand, out),
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
        Expression::RepeatedProduct(repeated) => {
            find_block_submodules_in_expression(&repeated.value, out);
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
        Expression::Binary(binary) => {
            find_block_submodules_in_expression(&binary.left, out);
            find_block_submodules_in_expression(&binary.right, out);
        }
        Expression::Pipe(binary) => {
            find_block_submodules_in_expression(&binary.receiver, out);
            find_block_submodules_in_expression(&binary.stage, out);
        }
        Expression::Unary(unary) => {
            find_block_submodules_in_expression(&unary.operand, out);
        }
        Expression::Try(try_) => {
            find_block_submodules_in_expression(&try_.operand, out);
        }
        Expression::Conversion(conversion) => {
            find_block_submodules_in_expression(&conversion.value, out);
        }
        Expression::Logical(logical) => {
            find_block_submodules_in_expression(&logical.left, out);
            find_block_submodules_in_expression(&logical.right, out);
        }
        Expression::StringTemplate(template) => {
            for part in &template.parts {
                if let staple_syntax::StringTemplatePart::Interpolation(interpolation) = part {
                    find_block_submodules_in_expression(&interpolation.expression, out);
                }
            }
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
    for item in &block.items {
        find_block_submodules_in_block_item(item, out);
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
        | Item::Expression(_)) => find_block_use_declarations_in_block_item(item, out),
    }
}

fn find_block_use_declarations_in_block_item(item: &Item, out: &mut Vec<UseDeclaration>) {
    match item {
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
        Item::Return(item) => find_block_use_declarations_in_expression(&item.value, out),
        Item::Break(item) => {
            if let Some(value) = &item.value {
                find_block_use_declarations_in_expression(value, out);
            }
        }
        Item::Continue(_) => {}
        Item::Expression(expression) => find_block_use_declarations_in_expression(expression, out),
        Item::Submodule(_) => {}
        Item::TypeDeclaration(_) => {}
        Item::UseDeclaration(declaration) => out.push(declaration.clone()),
        _ => {}
    }
}

fn find_block_use_declarations_in_expression(
    expression: &Expression,
    out: &mut Vec<UseDeclaration>,
) {
    match expression {
        Expression::Function(function) => {
            find_block_use_declarations_in_expression(&function.body, out)
        }
        Expression::TypeApplication(application) => {
            find_block_use_declarations_in_expression(&application.value, out)
        }
        Expression::TypeAscription(ascription) => {
            find_block_use_declarations_in_expression(&ascription.value, out)
        }
        Expression::Match(match_) => {
            find_block_use_declarations_in_expression(&match_.subject, out);
            for arm in &match_.arms {
                find_block_use_declarations_in_expression(&arm.body, out);
            }
        }
        Expression::Loop(loop_) => find_block_use_declarations_in_block(&loop_.body, out),
        Expression::Coro(coro) => find_block_use_declarations_in_block(&coro.body, out),
        Expression::Await(await_) => {
            find_block_use_declarations_in_expression(&await_.operand, out)
        }
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
        Expression::RepeatedProduct(repeated) => {
            find_block_use_declarations_in_expression(&repeated.value, out);
        }
        Expression::Call(call) => {
            find_block_use_declarations_in_expression(&call.callee, out);
            find_block_use_declarations_in_expression(&call.argument, out);
        }
        Expression::Access(access) => find_block_use_declarations_in_expression(&access.value, out),
        Expression::Index(index) => {
            find_block_use_declarations_in_expression(&index.value, out);
            find_block_use_declarations_in_expression(&index.index, out);
        }
        Expression::Binary(binary) => {
            find_block_use_declarations_in_expression(&binary.left, out);
            find_block_use_declarations_in_expression(&binary.right, out);
        }
        Expression::Pipe(binary) => {
            find_block_use_declarations_in_expression(&binary.receiver, out);
            find_block_use_declarations_in_expression(&binary.stage, out);
        }
        Expression::Unary(unary) => {
            find_block_use_declarations_in_expression(&unary.operand, out);
        }
        Expression::Try(try_) => {
            find_block_use_declarations_in_expression(&try_.operand, out);
        }
        Expression::Conversion(conversion) => {
            find_block_use_declarations_in_expression(&conversion.value, out);
        }
        Expression::Logical(logical) => {
            find_block_use_declarations_in_expression(&logical.left, out);
            find_block_use_declarations_in_expression(&logical.right, out);
        }
        Expression::StringTemplate(template) => {
            for part in &template.parts {
                if let staple_syntax::StringTemplatePart::Interpolation(interpolation) = part {
                    find_block_use_declarations_in_expression(&interpolation.expression, out);
                }
            }
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
    for item in &block.items {
        find_block_use_declarations_in_block_item(item, out);
    }
}

/// The packages of a graph, each after the packages it depends on. Ties keep
/// the graph's own order.
pub(crate) fn package_dependency_order(
    graph: Option<&staple_project::PackageGraph>,
) -> Vec<staple_project::PackageId> {
    let Some(graph) = graph else {
        return Vec::new();
    };
    fn visit(
        graph: &staple_project::PackageGraph,
        id: staple_project::PackageId,
        visited: &mut [bool],
        order: &mut Vec<staple_project::PackageId>,
    ) {
        if std::mem::replace(&mut visited[id.0], true) {
            return;
        }
        for dependency in &graph.package(id).dependencies {
            visit(graph, dependency.package, visited, order);
        }
        order.push(id);
    }
    let mut visited = vec![false; graph.packages.len()];
    let mut order = Vec::new();
    for index in 0..graph.packages.len() {
        visit(
            graph,
            staple_project::PackageId(index),
            &mut visited,
            &mut order,
        );
    }
    order
}

/// The order modules initialize in: every module after the modules it imports,
/// except within a cycle.
///
/// The order is built one package at a time, dependencies first, and a
/// package's own order depends only on its own modules and their paths. The
/// standard library therefore initializes in the same order whatever program
/// it is part of, which matters because initialization order decides which
/// module-level reads need a run-time initialization check. That check is
/// part of the symbols a separately compiled package exports. Modules outside
/// every package come last.
fn initialization_order(
    modules: &[SourceModule],
    imports: &HashMap<SyntaxId, ModuleId>,
    additional_imports: &HashMap<SyntaxId, ModuleId>,
    root_qualified_modules: &HashMap<(ModuleId, String), ModuleId>,
    module_packages: &[Option<staple_project::PackageId>],
    package_order: &[staple_project::PackageId],
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
    }
    // Dependency aliases loaded by staple-cli can feed this same logical-root
    // map in the future; initialization should depend on resolved package
    // identities, not on how their source directories happen to be laid out.
    for ((source, _), target) in root_qualified_modules {
        edges[source.0].push(*target);
    }

    let package_of = |module: ModuleId| module_packages.get(module.0).copied().flatten();
    let mut groups = package_order
        .iter()
        .map(|package| Some(*package))
        .chain([None])
        .map(|package| {
            modules
                .iter()
                .map(|module| module.id)
                .filter(|module| package_of(*module) == package)
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    // A package missing from the order (no graph) still initializes.
    let ordered = groups.iter().map(Vec::len).sum::<usize>();
    if ordered < modules.len() {
        groups.push(
            modules
                .iter()
                .map(|module| module.id)
                .filter(|module| {
                    package_of(*module).is_some_and(|package| !package_order.contains(&package))
                })
                .collect(),
        );
    }
    groups
        .into_iter()
        .flat_map(|group| group_initialization_order(modules, &edges, group))
        .collect()
}

/// Orders one package's modules, considering only the imports between them.
/// Modules are numbered by path rather than by load order (companion modules,
/// which the loader discovers last, after the rest), so the result does not
/// change with the order the loader happened to read files in.
fn group_initialization_order(
    modules: &[SourceModule],
    edges: &[Vec<ModuleId>],
    mut group: Vec<ModuleId>,
) -> Vec<ModuleId> {
    group.sort_by(|left, right| {
        modules[left.0]
            .companion
            .cmp(&modules[right.0].companion)
            .then_with(|| modules[left.0].path.cmp(&modules[right.0].path))
            .then_with(|| {
                modules[left.0]
                    .qualified_name
                    .cmp(&modules[right.0].qualified_name)
            })
    });
    let local = group
        .iter()
        .enumerate()
        .map(|(index, module)| (*module, index))
        .collect::<HashMap<_, _>>();
    let edges = group
        .iter()
        .map(|module| {
            let mut targets = edges[module.0]
                .iter()
                .filter_map(|target| local.get(target).copied())
                .collect::<Vec<_>>();
            targets.sort_unstable();
            targets.dedup();
            targets
        })
        .collect::<Vec<_>>();
    let count = group.len();

    fn visit(node: usize, edges: &[Vec<usize>], seen: &mut [bool], order: &mut Vec<usize>) {
        if std::mem::replace(&mut seen[node], true) {
            return;
        }
        for dependency in &edges[node] {
            visit(*dependency, edges, seen, order);
        }
        order.push(node);
    }
    let mut finish = Vec::new();
    let mut seen = vec![false; count];
    for node in 0..count {
        visit(node, &edges, &mut seen, &mut finish);
    }

    let mut reverse = vec![Vec::new(); count];
    for (node, dependencies) in edges.iter().enumerate() {
        for dependency in dependencies {
            reverse[*dependency].push(node);
        }
    }
    let mut component_of = vec![usize::MAX; count];
    let mut components: Vec<Vec<usize>> = Vec::new();
    fn collect(
        node: usize,
        reverse: &[Vec<usize>],
        component: usize,
        component_of: &mut [usize],
        members: &mut Vec<usize>,
    ) {
        if component_of[node] != usize::MAX {
            return;
        }
        component_of[node] = component;
        members.push(node);
        for next in &reverse[node] {
            collect(*next, reverse, component, component_of, members);
        }
    }
    for node in finish.into_iter().rev() {
        if component_of[node] == usize::MAX {
            let component = components.len();
            let mut members = Vec::new();
            collect(node, &reverse, component, &mut component_of, &mut members);
            // Local numbers follow the path order.
            members.sort_unstable();
            components.push(members);
        }
    }

    let mut component_edges = vec![HashSet::new(); components.len()];
    for (node, dependencies) in edges.iter().enumerate() {
        for dependency in dependencies {
            let from = component_of[node];
            let to = component_of[*dependency];
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
        .flat_map(|component| components[component].iter().map(|member| group[*member]))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn companion_members(source: &str) -> Vec<staple_syntax::Binding> {
        let program = Program::single(staple_syntax::parse(source).unwrap());
        let companion = program
            .modules()
            .iter()
            .find(|module| module.companion)
            .expect("companion module");
        let mut items = companion.syntax.items.clone();
        items
            .iter_mut()
            .flat_map(companion_member_bindings)
            .map(|binding| binding.clone())
            .collect()
    }

    #[test]
    fn companion_members_get_separate_fresh_copies_of_the_header() {
        let members = companion_members(
            "companion<T where Clone T> Box T { def first: <U> (Box T, U) -> T = x => x; def second: Box T -> T = x => x }\n",
        );
        let [first, second] = members.as_slice() else {
            panic!("two members")
        };
        let header = first.companion_generics.as_ref().unwrap();
        assert_eq!(header.parameters[0].names(), ["T"]);
        assert_eq!(
            first
                .all_type_parameters()
                .iter()
                .flat_map(|p| p.names())
                .collect::<Vec<_>>(),
            ["T", "U"]
        );
        let other = second.companion_generics.as_ref().unwrap();
        assert_ne!(header.parameters[0].syntax().id, other.parameters[0].syntax().id);
        assert_ne!(header.trait_bounds[0].syntax.id, other.trait_bounds[0].syntax.id);
        assert_ne!(
            first.companion_target.as_ref().unwrap().syntax().id,
            second.companion_target.as_ref().unwrap().syntax().id
        );
    }

    #[test]
    fn companion_members_inside_modifiers_and_extern_blocks_get_the_header() {
        let members = companion_members(concat!(
            "companion<T> Box T {\n",
            "    /// Documented.\n",
            "    @track_caller\n",
            "    pub def first = (b: Box T) => b\n",
            "    pub extern \"staple-intrinsic\" { second: Box T -> T; third: <U> (Box T, U) -> T }\n",
            "}\n",
        ));
        assert_eq!(members.len(), 3);
        assert!(members.iter().all(|binding| binding.companion_target.is_some()
            && binding.companion_generics.is_some()));
        assert_eq!(members[2].all_type_parameters().len(), 2);
    }

    #[test]
    fn source_at_reports_a_structured_missing_import() {
        let entry = std::env::temp_dir().join("staple-loader-structured-test.sta");
        let error = ProgramLoader::new()
            .with_standard_library_root(
                PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .parent()
                    .unwrap()
                    .join("stdlib"),
            )
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

    fn module_by_suffix(program: &Program, suffix: &str) -> ModuleId {
        program
            .modules()
            .iter()
            .find(|module| module.parent.is_none() && module.path.ends_with(suffix))
            .unwrap_or_else(|| panic!("no module for `{suffix}`"))
            .id
    }

    #[test]
    fn module_dotted_name_uses_dotted_use_paths_for_the_standard_library() {
        let entry = std::env::temp_dir().join("staple-module-dotted-name-std.sta");
        let program = ProgramLoader::new()
            .with_standard_library_root(
                PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .parent()
                    .unwrap()
                    .join("stdlib"),
            )
            .load_source_at(&entry, "use std.io.println\nprintln \"hi\"\n")
            .unwrap();

        assert_eq!(
            program.module_dotted_name(program.standard_library_core().unwrap()),
            Some("std.core".to_owned())
        );
        assert_eq!(
            program.module_dotted_name(program.standard_library_io().unwrap()),
            Some("std.io".to_owned())
        );
        assert_eq!(
            program.module_dotted_name(module_by_suffix(&program, "std/ops.sta")),
            Some("std.ops".to_owned())
        );
        assert_eq!(
            program.module_dotted_name(module_by_suffix(&program, "std/number/types.sta")),
            Some("std.number.types".to_owned())
        );
    }

    #[test]
    fn module_dotted_name_is_a_relative_path_without_a_manifest() {
        // A directory opened with no `staple.kdl`: there is no package identity,
        // so the module is named by its path relative to the module root, with
        // no leading package segment.
        let dir = std::env::temp_dir().join(format!(
            "staple-module-dotted-name-lone-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(dir.join("tools")).unwrap();
        std::fs::write(dir.join("tools/text.sta"), "pub mod\npub let shout = 1\n").unwrap();
        let entry = dir.join("main.sta");
        let program = ProgramLoader::new()
            .with_standard_library_root(
                PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .parent()
                    .unwrap()
                    .join("stdlib"),
            )
            .load_source_at(&entry, "use tools.text.shout\nshout\n")
            .unwrap();
        assert_eq!(
            program.module_dotted_name(program.entry()),
            Some("main".to_owned())
        );
        assert_eq!(
            program.module_dotted_name(module_by_suffix(&program, "tools/text.sta")),
            Some("tools.text".to_owned())
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn module_dotted_name_is_none_for_an_in_memory_program() {
        let module = staple_syntax::parse("let answer = 42\n").unwrap();
        let program = Program::single(module);
        assert_eq!(program.module_dotted_name(program.entry()), None);
    }

    #[test]
    fn module_dotted_name_covers_a_configured_package_root_and_its_files() {
        let root = std::env::temp_dir().join(format!(
            "staple-module-dotted-name-package-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(root.join("tools")).unwrap();
        let root_path = root.join("root.sta");
        std::fs::write(&root_path, "pub mod\n").unwrap();
        std::fs::write(root.join("tools/text.sta"), "pub mod\npub let shout = 1\n").unwrap();
        let path = root.join("main.sta");
        let program = ProgramLoader::new()
            .with_module_root(&root)
            .with_package_root(&root_path)
            .with_package_name("example")
            .with_standard_library_root(
                PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .parent()
                    .unwrap()
                    .join("stdlib"),
            )
            .load_source_at(&path, "use tools.text.shout\nshout\n")
            .unwrap();

        assert_eq!(
            program.module_dotted_name(program.package_root().unwrap()),
            Some("example".to_owned())
        );
        assert_eq!(
            program.module_dotted_name(module_by_suffix(&program, "tools/text.sta")),
            Some("example.tools.text".to_owned())
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}

/// The top-level type declaration of `name` in `items`, looking through
/// modifiers.
fn declared_type<'a>(items: &'a [Item], name: &str) -> Option<&'a staple_syntax::TypeDeclaration> {
    items.iter().find_map(|item| {
        let mut item = item;
        while let Item::Modified(modified) = item {
            item = &modified.item;
        }
        match item {
            Item::TypeDeclaration(declaration) if declaration.name == name => Some(declaration),
            _ => None,
        }
    })
}

/// The nominal type a companion target names, as the module declaring it and
/// its name.
type TypeKey = (ModuleId, String);

/// Groups `companion` blocks by the nominal types they extend. A block's
/// target is found from the block's own module: its declarations, its
/// imports, and the prelude, following aliases to the type they name. Each
/// such type gets one namespace module holding the members of all its
/// blocks, wherever they are written; no block is the namespace itself. A
/// block whose target is structural, or not found here, is detached, and
/// name resolution attaches it to its target by identity.
struct CompanionGrouping<'a> {
    modules: &'a mut Vec<SourceModule>,
    children: &'a mut Vec<HashMap<String, ModuleId>>,
    module_packages: &'a mut Vec<Option<staple_project::PackageId>>,
    next_syntax_id: &'a mut usize,
    child_modules: &'a HashMap<SyntaxId, ModuleId>,
    imported_modules: &'a HashMap<SyntaxId, ModuleId>,
    dotted_imports: &'a HashMap<SyntaxId, DottedImport>,
    prelude: Option<ModuleId>,
    uses_prelude: &'a dyn Fn(ModuleId) -> bool,
}

impl CompanionGrouping<'_> {
    fn group(self) {
        let blocks = self
            .modules
            .iter()
            .filter(|module| module.companion && !module.blanket && !module.namespace)
            .map(|module| module.id)
            .collect::<Vec<_>>();
        for block in blocks {
            let target = self.block_target(block);
            let namespace = target.as_ref().map(|(module, name)| {
                type_namespace(
                    self.modules,
                    self.children,
                    self.module_packages,
                    self.next_syntax_id,
                    *module,
                    name,
                )
            });
            let module = &mut self.modules[block.0];
            module.companion_namespace = namespace;
            module.detached = namespace.is_none();
        }
        // An alias offers the namespace of the type it names.
        for module in 0..self.modules.len() {
            let aliases = self.modules[module]
                .syntax
                .items
                .iter()
                .filter_map(|item| {
                    let mut item = item;
                    while let Item::Modified(modified) = item {
                        item = &modified.item;
                    }
                    match item {
                        Item::TypeDeclaration(declaration)
                            if declaration.kind() == staple_syntax::TypeDeclarationKind::Alias =>
                        {
                            Some(declaration.name.clone())
                        }
                        _ => None,
                    }
                })
                .collect::<Vec<_>>();
            for alias in aliases {
                if self.children[module].contains_key(&alias) {
                    continue;
                }
                let Some((target_module, target)) =
                    self.follow_aliases((ModuleId(module), alias.clone()), &mut HashSet::new())
                else {
                    continue;
                };
                if let Some(namespace) = self.children[target_module.0].get(&target).copied()
                    && self.modules[namespace.0].namespace
                {
                    self.children[module].insert(alias, namespace);
                }
            }
        }
    }

    /// The nominal type companion `block`'s header names.
    fn block_target(&self, block: ModuleId) -> Option<TypeKey> {
        let parent = self.modules[block.0].parent?;
        let header = self.modules[parent.0].syntax.items.iter().find_map(|item| {
            let mut item = item;
            while let Item::Modified(modified) = item {
                item = &modified.item;
            }
            match item {
                Item::Submodule(submodule)
                    if self.child_modules.get(&submodule.syntax.id) == Some(&block) =>
                {
                    Some(submodule)
                }
                _ => None,
            }
        })?;
        let named = type_head(header.companion_target.as_ref()?)?;
        let key = self.lookup_qualified(parent, named.namespace.as_deref(), &named.name)?;
        self.follow_aliases(key, &mut HashSet::new())
    }

    /// The nominal type `key` names: itself, or for an alias, the type at
    /// the head of its underlying type. `None` for an alias of a structural
    /// type or of a type parameter.
    fn follow_aliases(&self, key: TypeKey, seen: &mut HashSet<TypeKey>) -> Option<TypeKey> {
        if !seen.insert(key.clone()) {
            return None;
        }
        let (module, name) = &key;
        let declaration = declared_type(&self.modules[module.0].syntax.items, name)?;
        if declaration.kind() != staple_syntax::TypeDeclarationKind::Alias {
            return Some(key);
        }
        let named = type_head(declaration.underlying()?)?;
        if named.namespace.is_none()
            && declaration
                .type_parameters
                .iter()
                .any(|parameter| parameter.names().contains(&named.name.as_str()))
        {
            return None;
        }
        let next = self.lookup_qualified(*module, named.namespace.as_deref(), &named.name)?;
        self.follow_aliases(next, seen)
    }

    /// Where the type `name`, qualified by `namespace` when written so, seen
    /// from `module`, is declared.
    fn lookup_qualified(
        &self,
        module: ModuleId,
        namespace: Option<&str>,
        name: &str,
    ) -> Option<TypeKey> {
        let Some(namespace) = namespace else {
            return self.lookup(module, name, false, &mut HashSet::new());
        };
        let mut segments = namespace.split('.');
        let first = segments.next()?;
        let mut target = self.children[module.0].get(first).copied().or_else(|| {
            self.uses(module).find_map(|declaration| {
                (declaration.path.last().map(String::as_str) == Some(first)
                    && matches!(declaration.kind, UseKind::Namespace | UseKind::Dotted))
                .then(|| self.imported_modules.get(&declaration.syntax.id).copied())
                .flatten()
            })
        })?;
        for segment in segments {
            target = self.children[target.0].get(segment).copied()?;
        }
        self.lookup(target, name, true, &mut HashSet::new())
    }

    /// Where the type `name` seen from `module` is declared: in `module`,
    /// or through its imports and the prelude. `exported` limits the search
    /// to what `module` offers to its importers.
    fn lookup(
        &self,
        module: ModuleId,
        name: &str,
        exported: bool,
        seen: &mut HashSet<(ModuleId, String)>,
    ) -> Option<TypeKey> {
        if !seen.insert((module, name.to_owned())) {
            return None;
        }
        if let Some(declaration) = declared_type(&self.modules[module.0].syntax.items, name)
            && (!exported || declaration.visibility != Visibility::Private)
        {
            return Some((module, name.to_owned()));
        }
        let uses = self
            .uses(module)
            .filter(|declaration| !exported || declaration.visibility != Visibility::Private)
            .cloned()
            .collect::<Vec<_>>();
        for declaration in uses {
            let imported = self.imported_modules.get(&declaration.syntax.id).copied();
            let found = match &declaration.kind {
                UseKind::Selected(entries) => entries.iter().find_map(|entry| {
                    (entry.alias.as_deref().unwrap_or(&entry.name) == name)
                        .then(|| self.lookup(imported?, &entry.name, true, seen))
                        .flatten()
                }),
                UseKind::Renamed { item, alias } if alias == name => {
                    imported.and_then(|source| self.lookup(source, item, true, seen))
                }
                UseKind::Dotted if declaration.path.last().map(String::as_str) == Some(name) => self
                    .dotted_imports
                    .get(&declaration.syntax.id)
                    .and_then(|import| import.item_module)
                    .and_then(|source| self.lookup(source, name, true, seen)),
                UseKind::Glob => imported.and_then(|source| self.lookup(source, name, true, seen)),
                _ => None,
            };
            if found.is_some() {
                return found;
            }
        }
        if !exported
            && (self.uses_prelude)(module)
            && let Some(prelude) = self.prelude
            && prelude != module
        {
            return self.lookup(prelude, name, true, seen);
        }
        None
    }

    /// The top-level `use` declarations of `module`.
    fn uses(&self, module: ModuleId) -> impl Iterator<Item = &UseDeclaration> {
        self.modules[module.0]
            .syntax
            .items
            .iter()
            .filter_map(|item| match item {
                Item::UseDeclaration(declaration) => Some(declaration),
                _ => None,
            })
    }
}

/// The named type at the head of `ty`, through type applications.
fn type_head(ty: &staple_syntax::Type) -> Option<&staple_syntax::NamedType> {
    match ty {
        staple_syntax::Type::Named(named) => Some(named),
        staple_syntax::Type::Application(application) => type_head(&application.callee),
        staple_syntax::Type::EffectApplication(application) => type_head(&application.callee),
        _ => None,
    }
}

/// The namespace module of the nominal type `name` declared in `module`,
/// made on first use. It is a companion module without source, named after
/// the type, so the type's name reaches it.
fn type_namespace(
    modules: &mut Vec<SourceModule>,
    children: &mut Vec<HashMap<String, ModuleId>>,
    module_packages: &mut Vec<Option<staple_project::PackageId>>,
    next_syntax_id: &mut usize,
    module: ModuleId,
    name: &str,
) -> ModuleId {
    if let Some(existing) = children[module.0].get(name).copied() {
        return existing;
    }
    let id = ModuleId(modules.len());
    let parent = &modules[module.0];
    let mut module_syntax = Syntax::compiler();
    module_syntax.id = SyntaxId(*next_syntax_id);
    *next_syntax_id += 1;
    let syntax = Module {
        syntax: module_syntax,
        declaration_syntax: None,
        docs: Vec::new(),
        visibility: Visibility::Public,
        modifiers: Vec::new(),
        items: Vec::new(),
    };
    let namespace = SourceModule {
        id,
        path: parent.path.clone(),
        syntax,
        parent: Some(module),
        name: Some(name.to_owned()),
        visibility: Visibility::Public,
        qualified_name: format!("{}.{name}", parent.qualified_name),
        companion: true,
        blanket: false,
        detached: false,
        namespace: true,
        companion_namespace: None,
    };
    modules.push(namespace);
    module_packages.push(module_packages[module.0]);
    children.push(HashMap::new());
    children[module.0].insert(name.to_owned(), id);
    id
}

/// Gives each member binding of a `companion` block its own copy of the
/// block's compile-time parameters and target. Blocks for the same type merge
/// into one module, so this must run per block, before merging. Each copy gets
/// fresh syntax identities: resolution caches a parameter's identity per
/// syntax node, so members sharing one copy would overwrite each other's.
fn attach_companion_generics(declaration: &mut Submodule, next_syntax_id: &mut usize) {
    if !declaration.companion {
        return;
    }
    let Some(target) = declaration.companion_target.clone() else {
        return;
    };
    let generics = staple_syntax::GenericParameters {
        syntax: declaration
            .generics_syntax
            .clone()
            .unwrap_or_else(Syntax::compiler),
        parameters: declaration.type_parameters.clone(),
        trait_bounds: declaration.trait_bounds.clone(),
        subtype_bounds: declaration.subtype_bounds.clone(),
        equality_bounds: declaration.equality_bounds.clone(),
        sized_relaxations: declaration.sized_relaxations.clone(),
    };
    for item in &mut declaration.module.items {
        for binding in companion_member_bindings(item) {
            if binding.companion_target.is_some() {
                continue;
            }
            let mut member_generics = generics.clone();
            refresh_generics_ids(&mut member_generics, next_syntax_id);
            let mut member_target = target.clone();
            refresh_type_ids(&mut member_target, next_syntax_id);
            binding.companion_generics = Some(member_generics);
            binding.companion_target = Some(member_target);
        }
    }
}

/// The bindings declared by a companion member, looking through modifiers
/// (`///` docs, `@modifier`) and visibility splices that wrap it.
fn companion_member_bindings(item: &mut Item) -> Vec<&mut staple_syntax::Binding> {
    match item {
        Item::Binding(binding) => vec![binding],
        Item::ExternBlock(block) => block.bindings.iter_mut().collect(),
        Item::Modified(modified) => companion_member_bindings(&mut modified.item),
        Item::VisibilitySplice(splice) => companion_member_bindings(&mut splice.item),
        _ => Vec::new(),
    }
}

fn refresh_syntax_id(syntax: &mut Syntax, next_syntax_id: &mut usize) {
    syntax.id = SyntaxId(*next_syntax_id);
    *next_syntax_id += 1;
}

fn refresh_generics_ids(
    generics: &mut staple_syntax::GenericParameters,
    next_syntax_id: &mut usize,
) {
    refresh_syntax_id(&mut generics.syntax, next_syntax_id);
    for parameter in &mut generics.parameters {
        refresh_type_parameter_ids(parameter, next_syntax_id);
    }
    for bound in &mut generics.trait_bounds {
        refresh_syntax_id(&mut bound.syntax, next_syntax_id);
        refresh_syntax_id(&mut bound.trait_name.syntax, next_syntax_id);
        for argument in &mut bound.arguments {
            refresh_type_ids(argument, next_syntax_id);
        }
    }
    for bound in &mut generics.subtype_bounds {
        refresh_syntax_id(&mut bound.syntax, next_syntax_id);
        refresh_syntax_id(&mut bound.parameter.syntax, next_syntax_id);
        refresh_type_ids(&mut bound.supertype, next_syntax_id);
    }
    for bound in &mut generics.equality_bounds {
        refresh_syntax_id(&mut bound.syntax, next_syntax_id);
        refresh_type_ids(&mut bound.left, next_syntax_id);
        refresh_type_ids(&mut bound.right, next_syntax_id);
    }
    for relaxation in &mut generics.sized_relaxations {
        refresh_syntax_id(&mut relaxation.syntax, next_syntax_id);
        refresh_syntax_id(&mut relaxation.parameter.syntax, next_syntax_id);
    }
}

fn refresh_type_parameter_ids(
    parameter: &mut staple_syntax::TypeParameterPattern,
    next_syntax_id: &mut usize,
) {
    match parameter {
        staple_syntax::TypeParameterPattern::Binding(binding) => {
            refresh_syntax_id(&mut binding.syntax, next_syntax_id)
        }
        staple_syntax::TypeParameterPattern::Effect(binding) => {
            refresh_syntax_id(&mut binding.syntax, next_syntax_id)
        }
        staple_syntax::TypeParameterPattern::Product(product) => {
            refresh_syntax_id(&mut product.syntax, next_syntax_id);
            for element in &mut product.elements {
                refresh_type_parameter_ids(element, next_syntax_id);
            }
        }
        staple_syntax::TypeParameterPattern::Splice(splice) => {
            refresh_syntax_id(&mut splice.syntax, next_syntax_id)
        }
    }
}

fn refresh_effect_ids(effects: &mut staple_syntax::EffectSet, next_syntax_id: &mut usize) {
    refresh_syntax_id(&mut effects.syntax, next_syntax_id);
    if let Some(variable) = &mut effects.variable {
        refresh_syntax_id(&mut variable.syntax, next_syntax_id);
    }
    for resource in &mut effects.resources {
        refresh_syntax_id(&mut resource.syntax, next_syntax_id);
        refresh_type_ids(&mut resource.value_type, next_syntax_id);
    }
}

/// Refreshes every syntax identity in a type. Default values of product
/// elements are expressions and keep their identities; companion headers do
/// not contain them in practice.
fn refresh_type_ids(ty: &mut staple_syntax::Type, next_syntax_id: &mut usize) {
    use staple_syntax::Type;
    match ty {
        Type::Inferred(ty) => refresh_syntax_id(&mut ty.syntax, next_syntax_id),
        Type::NumberLiteral(ty) => refresh_syntax_id(&mut ty.syntax, next_syntax_id),
        Type::StringLiteral(ty) => refresh_syntax_id(&mut ty.syntax, next_syntax_id),
        Type::Named(ty) => refresh_syntax_id(&mut ty.syntax, next_syntax_id),
        Type::Splice(ty) => refresh_syntax_id(&mut ty.syntax, next_syntax_id),
        Type::Product(product) => {
            refresh_syntax_id(&mut product.syntax, next_syntax_id);
            for element in &mut product.elements {
                refresh_syntax_id(&mut element.syntax, next_syntax_id);
                refresh_type_ids(&mut element.ty, next_syntax_id);
            }
        }
        Type::Sum(sum) => {
            refresh_syntax_id(&mut sum.syntax, next_syntax_id);
            for alternative in &mut sum.alternatives {
                refresh_type_ids(alternative, next_syntax_id);
            }
        }
        Type::Function(function) => {
            refresh_syntax_id(&mut function.syntax, next_syntax_id);
            refresh_type_ids(&mut function.parameter, next_syntax_id);
            for marker in [&mut function.mutable_marker, &mut function.moved_marker]
                .into_iter()
                .flatten()
            {
                refresh_syntax_id(marker, next_syntax_id);
            }
            refresh_effect_ids(&mut function.effects, next_syntax_id);
            refresh_type_ids(&mut function.result, next_syntax_id);
        }
        Type::Forall(forall) => {
            refresh_syntax_id(&mut forall.syntax, next_syntax_id);
            refresh_generics_ids(&mut forall.generics, next_syntax_id);
            refresh_type_ids(&mut forall.body, next_syntax_id);
        }
        Type::Application(application) => {
            refresh_syntax_id(&mut application.syntax, next_syntax_id);
            refresh_type_ids(&mut application.callee, next_syntax_id);
            refresh_type_ids(&mut application.argument, next_syntax_id);
        }
        Type::EffectApplication(application) => {
            refresh_syntax_id(&mut application.syntax, next_syntax_id);
            refresh_type_ids(&mut application.callee, next_syntax_id);
            refresh_effect_ids(&mut application.effects, next_syntax_id);
        }
        Type::Array(array) => {
            refresh_syntax_id(&mut array.syntax, next_syntax_id);
            refresh_type_ids(&mut array.element, next_syntax_id);
            refresh_type_ids(&mut array.count, next_syntax_id);
        }
    }
}
