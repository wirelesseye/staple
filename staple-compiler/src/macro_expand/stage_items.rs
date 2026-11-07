//! `@interpreted_only` and `@compiled_only`: standard-library items with one
//! body for the macro interpreter and another for compiled macro code.
//!
//! The items are separated before anything indexes the program, so the
//! interpreter, its trait registry and the normal build only ever see the
//! interpreted view. The check program swaps in the compiled view afterwards.

use std::collections::HashSet;

use staple_syntax::{Item, SyntaxId};

use crate::{ModuleId, Program};

const INTERPRETED_ONLY: &str = "interpreted_only";
const COMPILED_ONLY: &str = "compiled_only";

/// The items that belong to one engine.
#[derive(Default)]
pub(super) struct StageItems {
    /// The items only compiled code uses, per module and unexpanded.
    pub(super) compiled: Vec<(ModuleId, Item)>,
    /// The items the check program leaves out.
    pub(super) interpreted: HashSet<SyntaxId>,
}

/// Takes the marked items out of the standard library's modules. The modifiers
/// mean nothing elsewhere, so another package's use of one is an unknown
/// modifier.
pub(super) fn separate(program: &mut Program) -> StageItems {
    let mut stages = StageItems::default();
    let Some(library) = program
        .standard_library_syntax()
        .and_then(|module| program.package_of(module))
    else {
        return stages;
    };
    let in_library = program
        .modules()
        .iter()
        .map(|module| program.package_of(module.id) == Some(library))
        .collect::<Vec<_>>();
    for (module, in_library) in program.modules_mut().iter_mut().zip(in_library) {
        if !in_library {
            continue;
        }
        let id = module.id;
        let mut kept = Vec::with_capacity(module.syntax.items.len());
        for item in std::mem::take(&mut module.syntax.items) {
            match take_marker(item) {
                (item, None) => kept.push(item),
                (item, Some(Stage::Interpreted)) => {
                    stages.interpreted.insert(item.syntax().id);
                    kept.push(item);
                }
                (item, Some(Stage::Compiled)) => stages.compiled.push((id, item)),
            }
        }
        module.syntax.items = kept;
    }
    stages
}

enum Stage {
    Interpreted,
    Compiled,
}

/// Removes a stage modifier from `item`, unwrapping it when nothing else
/// modifies the item.
fn take_marker(item: Item) -> (Item, Option<Stage>) {
    let Item::Modified(mut modified) = item else {
        return (item, None);
    };
    let marker = modified.modifiers.iter().position(|modifier| {
        modifier.namespace.is_none()
            && (modifier.name == INTERPRETED_ONLY || modifier.name == COMPILED_ONLY)
    });
    let Some(marker) = marker else {
        return (Item::Modified(modified), None);
    };
    let stage = if modified.modifiers.remove(marker).name == INTERPRETED_ONLY {
        Stage::Interpreted
    } else {
        Stage::Compiled
    };
    if modified.modifiers.is_empty() {
        (*modified.item, Some(stage))
    } else {
        (Item::Modified(modified), Some(stage))
    }
}
