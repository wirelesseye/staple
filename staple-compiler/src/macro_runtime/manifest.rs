//! Sidecar manifests for compiled macro libraries.
//!
//! A cached macro library outlives the compilation that built it, so its
//! manifest names modules by package and qualified path, and names macros by
//! module, macro name, and overload index rather than per-compilation
//! [`ModuleId`]s or [`SyntaxId`]s. Quotation templates are stored as
//! wire-encoded fragments alongside their defining module key.

// Only tests write and read manifests until library builds and the macro
// server use them.
#![cfg_attr(not(test), allow(dead_code))]

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use staple_syntax::{Item, SyntaxId};

use super::wire::{self, WIRE_VERSION};
use crate::macro_expand::lower_quote::{Template, TemplateTable};
use crate::{ModuleId, Program};

const MANIFEST_MAGIC: &[u8; 8] = b"STAPLEMM";
pub(crate) const MANIFEST_FORMAT_VERSION: u32 = 1;

/// Stable identifier of a module across compilations, independent of `ModuleId`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct ModuleKey {
    /// Package name, or `None` for modules outside any package.
    pub(crate) package: Option<String>,
    /// Qualified module path (for example `std.syntax` or `<memory>`).
    pub(crate) path: String,
}

impl ModuleKey {
    fn display(&self) -> String {
        match &self.package {
            Some(package) => format!("{package}:{}", self.path),
            None => self.path.clone(),
        }
    }
}

/// Stable identifier of one macro overload within a compilation.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct EntryKey {
    pub(crate) module: ModuleKey,
    pub(crate) macro_name: String,
    pub(crate) overload_index: usize,
}

/// One entry in a library manifest, pairing its stable key with its shim symbol.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ManifestEntry {
    pub(crate) key: EntryKey,
    pub(crate) shim_symbol: String,
}

/// One quotation template in a library manifest, with its module named by
/// package and path and its tokens encoded as a wire fragment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ManifestTemplate {
    pub(crate) module: ModuleKey,
    pub(crate) fragment: Vec<u8>,
}

/// Sidecar manifest written beside a compiled macro shared library.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryManifest {
    pub(crate) format_version: u32,
    pub(crate) wire_version: u32,
    pub(crate) entries: Vec<ManifestEntry>,
    pub(crate) templates: Vec<ManifestTemplate>,
}

/// A manifest resolved against the current compilation's [`Program`].
#[derive(Clone)]
pub struct LoadedManifest {
    /// Maps each macro declaration's `SyntaxId` in the current compilation to
    /// its `__staple_entry.<n>` shim symbol in the library.
    pub entries: HashMap<SyntaxId, String>,
    /// The library's quotation templates, with `ModuleId`s mapped to the
    /// current compilation.
    pub templates: TemplateTable,
}

impl LoadedManifest {
    /// Returns `(module_id, template_syntax)` pairs formatted for
    /// [`crate::macro_runtime::client::MacroClient::load`].
    pub fn client_templates(&self) -> Vec<(usize, staple_syntax::Syntax)> {
        self.templates
            .templates
            .iter()
            .map(|template| (template.module.0, template.contents.clone()))
            .collect()
    }
}

/// Returns the manifest path beside `library_path`.
pub(crate) fn manifest_path(library_path: &Path) -> PathBuf {
    library_path.with_extension("manifest")
}

/// Computes the stable [`ModuleKey`] of `module` in `program`.
pub(crate) fn module_key(program: &Program, module: ModuleId) -> ModuleKey {
    let package = program
        .package_of(module)
        .and_then(|id| program.package_graph().map(|graph| graph.package(id).name.clone()));
    let path = program.module(module).qualified_name.clone();
    ModuleKey { package, path }
}

/// Returns the macro name and 0-based overload index of `declaration_id` within
/// `module`, if that declaration exists in `module`.
pub(crate) fn macro_overload_key(
    program: &Program,
    module: ModuleId,
    declaration_id: SyntaxId,
) -> Option<EntryKey> {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for item in &program.module(module).syntax.items {
        if let Item::MacroDeclaration(declaration) = item {
            let index = counts.entry(declaration.name.as_str()).or_insert(0);
            let current = *index;
            *index += 1;
            if declaration.syntax.id == declaration_id {
                return Some(EntryKey {
                    module: module_key(program, module),
                    macro_name: declaration.name.clone(),
                    overload_index: current,
                });
            }
        }
    }
    None
}

/// Builds a [`LibraryManifest`] from `(module, declaration_id, shim_symbol)`
/// entries and `templates`.
///
/// `program` is the expanded program, the same kind of program
/// [`LibraryManifest::resolve`] maps against, not a check program. A
/// closure-only check program declares only the macros in the closure, so the
/// overload indexes counted there would not match the ones counted at load time.
pub fn build_manifest(
    program: &Program,
    entries: &[(ModuleId, SyntaxId, String)],
    templates: &TemplateTable,
) -> Result<LibraryManifest, String> {
    let mut manifest_entries = Vec::with_capacity(entries.len());
    for (module, declaration_id, shim_symbol) in entries {
        let key = macro_overload_key(program, *module, *declaration_id).ok_or_else(|| {
            format!(
                "internal error: macro declaration {:?} not found in module {:?}",
                declaration_id, module
            )
        })?;
        manifest_entries.push(ManifestEntry {
            key,
            shim_symbol: shim_symbol.clone(),
        });
    }
    let manifest_templates = templates
        .templates
        .iter()
        .map(|template| ManifestTemplate {
            module: module_key(program, template.module),
            fragment: wire::encode_fragment(&without_compilation_numbers(&template.contents)),
        })
        .collect();
    Ok(LibraryManifest {
        format_version: MANIFEST_FORMAT_VERSION,
        wire_version: WIRE_VERSION,
        entries: manifest_entries,
        templates: manifest_templates,
    })
}

impl LibraryManifest {
    /// Encodes the manifest into bytes.
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(MANIFEST_MAGIC);
        bytes.extend_from_slice(&self.format_version.to_le_bytes());
        bytes.extend_from_slice(&self.wire_version.to_le_bytes());
        write_u32(&mut bytes, self.entries.len());
        for entry in &self.entries {
            write_module_key(&mut bytes, &entry.key.module);
            write_str(&mut bytes, &entry.key.macro_name);
            write_u32(&mut bytes, entry.key.overload_index);
            write_str(&mut bytes, &entry.shim_symbol);
        }
        write_u32(&mut bytes, self.templates.len());
        for template in &self.templates {
            write_module_key(&mut bytes, &template.module);
            write_bytes(&mut bytes, &template.fragment);
        }
        bytes
    }

    /// Decodes a manifest read from `library_path`'s sidecar file, checking its
    /// format and wire versions.
    pub(crate) fn decode(bytes: &[u8], library_path: &Path) -> Result<Self, String> {
        let mut cursor = Cursor { bytes };
        let magic = cursor
            .take(MANIFEST_MAGIC.len())
            .map_err(|error| manifest_error(library_path, &error))?;
        if magic != MANIFEST_MAGIC {
            return Err(manifest_error(library_path, "invalid manifest header"));
        }
        let format_version = cursor
            .read_u32()
            .map_err(|error| manifest_error(library_path, &error))?;
        if format_version != MANIFEST_FORMAT_VERSION {
            return Err(manifest_error(
                library_path,
                &format!(
                    "manifest format version {format_version} does not match {MANIFEST_FORMAT_VERSION}"
                ),
            ));
        }
        let wire_version = cursor
            .read_u32()
            .map_err(|error| manifest_error(library_path, &error))?;
        if wire_version != WIRE_VERSION {
            return Err(manifest_error(
                library_path,
                &format!("manifest wire version {wire_version} does not match {WIRE_VERSION}"),
            ));
        }
        let entry_count = cursor
            .read_u32()
            .map_err(|error| manifest_error(library_path, &error))? as usize;
        let mut entries = Vec::with_capacity(entry_count.min(bytes.len()));
        for _ in 0..entry_count {
            let module = read_module_key(&mut cursor)
                .map_err(|error| manifest_error(library_path, &error))?;
            let macro_name = read_string(&mut cursor)
                .map_err(|error| manifest_error(library_path, &error))?;
            let overload_index = cursor
                .read_u32()
                .map_err(|error| manifest_error(library_path, &error))?
                as usize;
            let shim_symbol = read_string(&mut cursor)
                .map_err(|error| manifest_error(library_path, &error))?;
            entries.push(ManifestEntry {
                key: EntryKey {
                    module,
                    macro_name,
                    overload_index,
                },
                shim_symbol,
            });
        }
        let template_count = cursor
            .read_u32()
            .map_err(|error| manifest_error(library_path, &error))? as usize;
        let mut templates = Vec::with_capacity(template_count.min(bytes.len()));
        for _ in 0..template_count {
            let module = read_module_key(&mut cursor)
                .map_err(|error| manifest_error(library_path, &error))?;
            let fragment = read_bytes(&mut cursor)
                .map_err(|error| manifest_error(library_path, &error))?;
            templates.push(ManifestTemplate { module, fragment });
        }
        if !cursor.bytes.is_empty() {
            return Err(manifest_error(library_path, "trailing manifest bytes"));
        }
        Ok(Self {
            format_version,
            wire_version,
            entries,
            templates,
        })
    }

    /// Writes this manifest next to `library_path`.
    pub fn write_beside(&self, library_path: &Path) -> Result<(), String> {
        let path = manifest_path(library_path);
        std::fs::write(&path, self.encode())
            .map_err(|error| format!("could not write `{}`: {error}", path.display()))
    }

    /// Reads and decodes the manifest next to `library_path`.
    pub fn read_beside(library_path: &Path) -> Result<Self, String> {
        let path = manifest_path(library_path);
        let bytes = std::fs::read(&path)
            .map_err(|error| format!("could not read `{}`: {error}", path.display()))?;
        Self::decode(&bytes, library_path)
    }

    /// Maps this manifest's module keys, macro keys, and templates to `program`.
    pub fn resolve(
        &self,
        program: &Program,
        library_path: &Path,
    ) -> Result<LoadedManifest, String> {
        let mut modules: HashMap<ModuleKey, ModuleId> = HashMap::new();
        for module in program.modules() {
            let key = module_key(program, module.id);
            if !module.companion && !module.namespace {
                modules.insert(key, module.id);
            } else {
                modules.entry(key).or_insert(module.id);
            }
        }

        let mut entries = HashMap::with_capacity(self.entries.len());
        for entry in &self.entries {
            let module_id = modules.get(&entry.key.module).copied().ok_or_else(|| {
                format!(
                    "internal error: macro library `{}` references unknown module `{}`",
                    library_path.display(),
                    entry.key.module.display()
                )
            })?;
            let declaration_id = find_macro_declaration(
                program,
                module_id,
                &entry.key.macro_name,
                entry.key.overload_index,
            )
            .ok_or_else(|| {
                format!(
                    "internal error: macro `{}` (overload {}) in `{}` matched no declaration in `{}`",
                    entry.key.macro_name,
                    entry.key.overload_index,
                    library_path.display(),
                    entry.key.module.display()
                )
            })?;
            entries.insert(declaration_id, entry.shim_symbol.clone());
        }

        let mut templates = TemplateTable::default();
        for template in &self.templates {
            let module_id = modules.get(&template.module).copied().ok_or_else(|| {
                format!(
                    "internal error: macro library `{}` template references unknown module `{}`",
                    library_path.display(),
                    template.module.display()
                )
            })?;
            let contents = wire::decode_fragment(&template.fragment).map_err(|error| {
                format!(
                    "internal error: could not decode template fragment in `{}`: {error}",
                    library_path.display()
                )
            })?;
            templates.templates.push(Template {
                contents,
                module: module_id,
            });
        }

        Ok(LoadedManifest { entries, templates })
    }
}

/// A copy of a template with the module ids and marks of the compilation that
/// built it removed. Filling a quotation stamps every token with the loading
/// compilation's module and mark, so these numbers are never read; dropping
/// them keeps a cached manifest free of numbers that mean something else in
/// the next compilation.
fn without_compilation_numbers(contents: &staple_syntax::Syntax) -> staple_syntax::Syntax {
    let mut contents = contents.clone();
    contents.definition_module = None;
    contents.expansion_mark = None;
    let tokens = contents
        .token_stream()
        .iter()
        .cloned()
        .map(|mut token| {
            token.hygiene = None;
            token
        })
        .collect::<Vec<_>>();
    contents.tokens = std::sync::Arc::from(tokens);
    contents
}

fn find_macro_declaration(
    program: &Program,
    module: ModuleId,
    name: &str,
    overload_index: usize,
) -> Option<SyntaxId> {
    let mut seen = 0usize;
    for item in &program.module(module).syntax.items {
        if let Item::MacroDeclaration(declaration) = item
            && declaration.name == name
        {
            if seen == overload_index {
                return Some(declaration.syntax.id);
            }
            seen += 1;
        }
    }
    None
}

fn manifest_error(library_path: &Path, detail: &str) -> String {
    format!(
        "internal error: invalid macro manifest for `{}`: {detail}",
        library_path.display()
    )
}

fn write_u32(out: &mut Vec<u8>, value: usize) {
    let value = u32::try_from(value).expect("manifest count fits in u32");
    out.extend_from_slice(&value.to_le_bytes());
}

fn write_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    write_u32(out, bytes.len());
    out.extend_from_slice(bytes);
}

fn write_str(out: &mut Vec<u8>, text: &str) {
    write_bytes(out, text.as_bytes());
}

fn write_module_key(out: &mut Vec<u8>, key: &ModuleKey) {
    match &key.package {
        Some(package) => {
            out.push(1);
            write_str(out, package);
        }
        None => out.push(0),
    }
    write_str(out, &key.path);
}

struct Cursor<'a> {
    bytes: &'a [u8],
}

impl<'a> Cursor<'a> {
    fn take(&mut self, len: usize) -> Result<&'a [u8], String> {
        if self.bytes.len() < len {
            return Err("unexpected end of manifest".to_owned());
        }
        let (head, tail) = self.bytes.split_at(len);
        self.bytes = tail;
        Ok(head)
    }

    fn read_u8(&mut self) -> Result<u8, String> {
        Ok(self.take(1)?[0])
    }

    fn read_u32(&mut self) -> Result<u32, String> {
        let bytes = self.take(4)?;
        Ok(u32::from_le_bytes(bytes.try_into().expect("four bytes")))
    }
}

fn read_bytes(cursor: &mut Cursor<'_>) -> Result<Vec<u8>, String> {
    let len = cursor.read_u32()? as usize;
    Ok(cursor.take(len)?.to_vec())
}

fn read_string(cursor: &mut Cursor<'_>) -> Result<String, String> {
    let bytes = read_bytes(cursor)?;
    String::from_utf8(bytes).map_err(|_| "manifest string is not valid UTF-8".to_owned())
}

fn read_module_key(cursor: &mut Cursor<'_>) -> Result<ModuleKey, String> {
    let has_package = cursor.read_u8()?;
    let package = match has_package {
        0 => None,
        1 => Some(read_string(cursor)?),
        other => return Err(format!("invalid package flag {other}")),
    };
    let path = read_string(cursor)?;
    Ok(ModuleKey { package, path })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_round_trips_and_checks_versions() {
        let manifest = LibraryManifest {
            format_version: MANIFEST_FORMAT_VERSION,
            wire_version: WIRE_VERSION,
            entries: vec![ManifestEntry {
                key: EntryKey {
                    module: ModuleKey {
                        package: Some("app".to_owned()),
                        path: "app.macros".to_owned(),
                    },
                    macro_name: "twice".to_owned(),
                    overload_index: 0,
                },
                shim_symbol: "__staple_entry.0".to_owned(),
            }],
            templates: vec![],
        };
        let bytes = manifest.encode();
        let lib_path = Path::new("/tmp/libtest.dylib");
        let decoded = LibraryManifest::decode(&bytes, lib_path).expect("manifest decodes");
        assert_eq!(decoded, manifest);

        let mut bad_format = bytes.clone();
        bad_format[8..12].copy_from_slice(&(MANIFEST_FORMAT_VERSION + 1).to_le_bytes());
        let error = LibraryManifest::decode(&bad_format, lib_path).unwrap_err();
        assert!(error.contains("libtest.dylib"), "{error}");
        assert!(error.contains("format version"), "{error}");

        let mut bad_wire = bytes;
        bad_wire[12..16].copy_from_slice(&(WIRE_VERSION + 1).to_le_bytes());
        let error = LibraryManifest::decode(&bad_wire, lib_path).unwrap_err();
        assert!(error.contains("libtest.dylib"), "{error}");
        assert!(error.contains("wire version"), "{error}");
    }

    #[test]
    fn keys_come_from_the_expanded_program_of_a_closure_only_check() {
        // Only the second overload is invoked, so a closure-only check program
        // declares neither; the expanded program still declares both.
        let source = concat!(
            "use std.syntax.*\n",
            "macro pick: Ident -> Expr = name => parse_quote { 1 }\n",
            "macro pick: Expr -> Expr = value => parse_quote { 2 }\n",
            "let a = pick (1 + 2)\n",
        );
        let (run, calls) = crate::macro_expand::compiled_call::record_calls(|| {
            crate::macro_runtime::harness::check_run_for_with(source, false)
        });
        let entry = run.expanded.entry();
        let call = calls
            .iter()
            .find(|call| call.definition_module == entry)
            .expect("the entry module calls `pick`");
        let declaration = call.declaration.syntax.id;
        let manifest = build_manifest(
            &run.expanded,
            &[(entry, declaration, "__staple_entry.0".to_owned())],
            &run.templates,
        )
        .expect("the manifest builds from the expanded program");
        assert_eq!(manifest.entries[0].key.overload_index, 1);
        let loaded = manifest
            .resolve(&run.expanded, Path::new("/tmp/libpick.so"))
            .expect("the manifest resolves");
        assert_eq!(
            loaded.entries.get(&declaration).map(String::as_str),
            Some("__staple_entry.0")
        );
    }

    #[test]
    fn templates_lose_the_numbers_of_the_compilation_that_built_them() {
        let mut contents = crate::macro_expand::syntax_ops::syntax_from_text("a + b", &mut 1);
        contents.definition_module = Some(7);
        contents.expansion_mark = Some(42);
        let tokens = contents
            .token_stream()
            .iter()
            .cloned()
            .map(|mut token| {
                token.hygiene = Some((7, 42));
                token
            })
            .collect::<Vec<_>>();
        contents.tokens = std::sync::Arc::from(tokens);
        let stripped = without_compilation_numbers(&contents);
        assert_eq!(stripped.definition_module, None);
        assert_eq!(stripped.expansion_mark, None);
        assert!(stripped.token_stream().iter().all(|token| token.hygiene.is_none()));
        assert_eq!(stripped.text(), contents.text());
    }

    #[test]
    fn resolve_reports_missing_key_with_library_name() {
        let root = crate::macro_runtime::harness::workspace_root();
        let program = crate::ProgramLoader::new()
            .with_standard_library_root(crate::macro_runtime::harness::standard_library_root())
            .load_source("let x = 1\n", &root)
            .expect("source loads");
        let manifest = LibraryManifest {
            format_version: MANIFEST_FORMAT_VERSION,
            wire_version: WIRE_VERSION,
            entries: vec![ManifestEntry {
                key: EntryKey {
                    module: module_key(&program, program.entry()),
                    macro_name: "missing_macro".to_owned(),
                    overload_index: 0,
                },
                shim_symbol: "__staple_entry.0".to_owned(),
            }],
            templates: vec![],
        };
        let lib_path = Path::new("/tmp/libmissing.so");
        let Err(error) = manifest.resolve(&program, lib_path) else {
            panic!("missing macro key should fail");
        };
        assert!(error.contains("internal error"), "{error}");
        assert!(error.contains("libmissing.so"), "{error}");
        assert!(error.contains("missing_macro"), "{error}");
    }
}
