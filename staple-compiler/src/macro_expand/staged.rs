//! Stage-ordered expansion.
//!
//! A macro runs only after everything it needs has been expanded: every item
//! in its compile-time closure, and the macros those items call. Items are
//! expanded on demand and remembered by their original syntax id, so the module
//! walk finds them done and puts them back where they were written.
use super::closure::{ItemId, ItemKind, Lookup, MacroClosure};
use super::*;

/// The interpreter's transient state, set aside while an item is expanded
/// ahead of its turn from the middle of another expansion.
pub(super) struct SavedState {
    expansion_stack: Vec<MacroKey>,
    modifier_frames: Vec<(MacroKey, Vec<SyntaxId>)>,
    emitted_items: Option<Vec<Item>>,
    quote_context: Option<MetaType>,
    invocation_spans: Vec<Span>,
    root_invocation_span: Option<Span>,
    type_env: TypeEnv,
    call_target: Option<MetaType>,
    pending_return: Option<Value>,
    pending_break: Option<Value>,
    pending_continue: bool,
    argument_place: Option<EnvironmentBinding>,
    helper_eval_depth: usize,
    loop_types: Vec<Vec<CompileType>>,
    steps: usize,
    compile_time_depth: usize,
    running_macros: Vec<ItemId>,
}

/// A step of the staged expansion in progress: a macro being made ready or an
/// item being expanded.
#[derive(Clone, PartialEq)]
pub(super) enum StageNode {
    Macro(MacroKey),
    Item(ItemId),
}

/// What a top-level item expanded to: the items that go in its place in the
/// module, and the form the interpreter and closure scans see.
pub(super) struct Expanded {
    pub(super) output: Rc<Vec<Item>>,
    pub(super) group: Rc<Vec<Item>>,
}

impl MacroExpander {
    pub(super) fn save_state(&mut self) -> SavedState {
        SavedState {
            expansion_stack: std::mem::take(&mut self.expansion_stack),
            modifier_frames: std::mem::take(&mut self.modifier_frames),
            emitted_items: self.emitted_items.take(),
            quote_context: self.quote_context.take(),
            invocation_spans: std::mem::take(&mut self.invocation_spans),
            root_invocation_span: self.root_invocation_span.take(),
            type_env: std::mem::take(&mut self.type_env),
            call_target: self.call_target.take(),
            pending_return: self.pending_return.take(),
            pending_break: self.pending_break.take(),
            pending_continue: std::mem::take(&mut self.pending_continue),
            argument_place: self.argument_place.take(),
            helper_eval_depth: std::mem::take(&mut self.helper_eval_depth),
            loop_types: std::mem::take(&mut self.loop_types),
            steps: std::mem::take(&mut self.steps),
            compile_time_depth: std::mem::take(&mut self.compile_time_depth),
            running_macros: std::mem::take(&mut self.running_macros),
        }
    }

    pub(super) fn restore_state(&mut self, saved: SavedState) {
        self.expansion_stack = saved.expansion_stack;
        self.modifier_frames = saved.modifier_frames;
        self.emitted_items = saved.emitted_items;
        self.quote_context = saved.quote_context;
        self.invocation_spans = saved.invocation_spans;
        self.root_invocation_span = saved.root_invocation_span;
        self.type_env = saved.type_env;
        self.call_target = saved.call_target;
        self.pending_return = saved.pending_return;
        self.pending_break = saved.pending_break;
        self.pending_continue = saved.pending_continue;
        self.argument_place = saved.argument_place;
        self.helper_eval_depth = saved.helper_eval_depth;
        self.loop_types = saved.loop_types;
        self.steps = saved.steps;
        self.compile_time_depth = saved.compile_time_depth;
        self.running_macros = saved.running_macros;
    }

    /// Makes `definition` ready to run: its closure is expanded, and so are
    /// the macros that closure calls. Compiler-provided macros and templates
    /// need nothing. `Ok` says whether anything was done now; `Err` means the
    /// macro depends on itself, which is reported at `call_span`.
    pub(super) fn ensure_ready(
        &mut self,
        definition: &MacroDefinition,
        call_span: &Span,
    ) -> Result<bool, ()> {
        if !matches!(definition.kind, MacroKind::User(_)) || definition.is_template {
            return Ok(false);
        }
        let key = definition.key.clone();
        if self.macros_ready.contains(&key) {
            return Ok(false);
        }
        if self.macros_failed.contains(&key) {
            return Err(());
        }
        let node = StageNode::Macro(key.clone());
        if self.soft_stage > 0 && self.record_cycle(&node) {
            return Ok(false);
        }
        if self.cycle_at(&node, call_span) {
            return Err(());
        }
        self.stage_stack.push(StageNode::Macro(key.clone()));
        let saved = self.save_state();
        let outcome = self.make_ready(definition, call_span);
        self.restore_state(saved);
        self.stage_stack.pop();
        match outcome {
            Ok(()) => {
                self.macro_ready_order.push(key.name.clone());
                self.macros_ready.insert(key);
                Ok(true)
            }
            Err(()) => Err(()),
        }
    }

    fn make_ready(&mut self, definition: &MacroDefinition, call_span: &Span) -> Result<(), ()> {
        let start = definition.declaration.syntax.id;
        let closure = self.settle(call_span, |expander| expander.macro_closure(start))?;
        self.macro_closures.insert(start, closure);
        Ok(())
    }

    /// Makes an expression that compile-time code evaluates in `module`, such
    /// as a `const` initializer, ready to run: what it reaches is expanded.
    pub(super) fn ensure_expression_ready(
        &mut self,
        module: ModuleId,
        expression: &Expression,
        call_span: &Span,
    ) -> Result<(), ()> {
        let saved = self.save_state();
        // A `const` whose initializer reaches itself is the evaluator's to
        // reject, so loops are only recorded here.
        self.soft_stage += 1;
        let outcome = self.settle(call_span, |expander| {
            let templates = &expander.template_items;
            let lookup = Lookup {
                scopes: &expander.scopes,
                parents: &expander.parent_modules,
                companions: &expander.companion_modules,
            };
            expander
                .closure
                .closure_of_expression(&lookup, module, expression, &|item| {
                    templates.contains(&item)
                })
        });
        self.soft_stage -= 1;
        self.restore_state(saved);
        outcome.map(|_| ())
    }

    /// Expands everything `compute` says is needed, and the macros it calls,
    /// until a rescan finds nothing new.
    fn settle(
        &mut self,
        call_span: &Span,
        mut compute: impl FnMut(&mut Self) -> MacroClosure,
    ) -> Result<MacroClosure, ()> {
        loop {
            let closure = compute(self);
            let mut progressed = false;
            for used in &closure.macros_used {
                if let Some(used_key) = self.macro_by_item.get(used).cloned()
                    && let Some(used_definition) = self.definitions.get(&used_key).cloned()
                    && !self.macros_ready.contains(&used_key)
                {
                    // A macro already under way is a cycle through whichever
                    // item of this closure calls it.
                    if self
                        .stage_stack
                        .contains(&StageNode::Macro(used_key.clone()))
                    {
                        let lookup = Lookup {
                            scopes: &self.scopes,
                            parents: &self.parent_modules,
                            companions: &self.companion_modules,
                        };
                        let caller = self.closure.caller_of(&lookup, &closure.items, *used);
                        if let Some(caller) = caller {
                            // An item that makes names may only be reached
                            // because the closure over-approximates which names
                            // are needed, so that is recorded, not an error.
                            let generator = self.closure.items[&caller].generates;
                            self.cycle_through(
                                StageNode::Item(caller),
                                &used_key,
                                call_span,
                                !generator,
                            );
                            if generator {
                                continue;
                            }
                            return Err(());
                        }
                    }
                    self.ensure_ready(&used_definition, call_span)?;
                }
            }
            for item in &closure.items {
                if self
                    .closure
                    .items
                    .get(item)
                    .is_some_and(|indexed| !indexed.expanded)
                {
                    progressed |= self.ensure_expanded(*item, Some(call_span))?;
                }
            }
            // A name that resolves to nothing may be made by an item-position
            // macro or a modifier in the module it is written in, so expand
            // those first and look again.
            let lookup = Lookup {
                scopes: &self.scopes,
                parents: &self.parent_modules,
                companions: &self.companion_modules,
            };
            let unresolved = self.closure.unresolved_in(&lookup, &closure.items);
            let mut modules = Vec::new();
            for (module, _) in unresolved {
                if !modules.contains(&module) {
                    modules.push(module);
                }
            }
            for module in modules {
                for generator in self.closure.generators_in(module) {
                    // A name that nothing defines may or may not be made by a
                    // generator, so one that is already under way is not
                    // counted as a cycle; the name just stays unresolved.
                    self.soft_stage += 1;
                    let expanded = self.ensure_expanded(generator, None);
                    self.soft_stage -= 1;
                    progressed |= expanded?;
                }
            }
            if !progressed {
                return Ok(closure);
            }
        }
    }

    /// Expands the top-level item `id` ahead of the module walk, unless that is
    /// already done. `Err` means the item is already being expanded and needs
    /// what its own expansion is making ready, which is reported at `call_span`.
    pub(super) fn ensure_expanded(
        &mut self,
        id: ItemId,
        call_span: Option<&Span>,
    ) -> Result<bool, ()> {
        let Some(indexed) = self.closure.items.get(&id) else {
            return Ok(false);
        };
        if indexed.expanded {
            return Ok(false);
        }
        let module = indexed.module;
        // An item that makes names is only in a closure because names are
        // matched loosely, so a cycle through one is recorded, not reported.
        let report = indexed.generates;
        let node = StageNode::Item(id);
        let in_progress = match call_span {
            Some(span) if !report && self.soft_stage == 0 => self.cycle_at(&node, span),
            _ => self.record_cycle(&node),
        };
        if in_progress {
            return if call_span.is_some() && !report && self.soft_stage == 0 {
                Err(())
            } else {
                Ok(false)
            };
        }
        let Some(item) = self.closure.group_of(id).first().cloned() else {
            return Ok(false);
        };
        self.stage_stack.push(node);
        let saved = self.save_state();
        let expanded = self.expand_unit(module, item, true);
        self.restore_state(saved);
        self.stage_stack.pop();
        self.finish_unit(id, expanded, true);
        Ok(true)
    }

    /// Expands one top-level item. With `for_compile_time`, compile-time
    /// helpers and macro bodies are expanded too, since the interpreter
    /// evaluates them; the output keeps a macro declaration as written.
    pub(super) fn expand_unit(
        &mut self,
        module: ModuleId,
        mut item: Item,
        for_compile_time: bool,
    ) -> Expanded {
        if for_compile_time {
            if let Item::Binding(binding) = &item
                && binding_is_compile_time_helper(binding)
            {
                self.compile_time_depth += 1;
                self.expand_block_item_contents(module, &mut item, 0);
                self.compile_time_depth -= 1;
                let both = Rc::new(vec![item]);
                return Expanded {
                    output: both.clone(),
                    group: both,
                };
            }
            if let Item::MacroDeclaration(declaration) = &item {
                let key = self.macro_by_item.get(&declaration.syntax.id).cloned();
                let value = key.as_ref().and_then(|key| {
                    self.definitions
                        .get(key)
                        .and_then(|definition| definition.declaration.value.clone())
                });
                if let (Some(key), Some(value)) = (key, value) {
                    self.compile_time_depth += 1;
                    let value = self.expand_expression(module, value, 0);
                    self.compile_time_depth -= 1;
                    if let Some(definition) = self.definitions.get_mut(&key) {
                        definition.declaration.value = Some(value.clone());
                        if matches!(definition.kind, MacroKind::User(_)) {
                            definition.kind = MacroKind::User(value);
                        }
                        let group =
                            Rc::new(vec![Item::MacroDeclaration(definition.declaration.clone())]);
                        return Expanded {
                            output: Rc::new(vec![item]),
                            group,
                        };
                    }
                }
                let both = Rc::new(vec![item]);
                return Expanded {
                    output: both.clone(),
                    group: both,
                };
            }
        }
        self.expand_item(module, &mut item, 0);
        let output = match self.emitted_items.take() {
            Some(generated) => self.expand_generated_items(module, generated, 1),
            None => vec![item],
        };
        let output = Rc::new(output);
        Expanded {
            group: output.clone(),
            output,
        }
    }

    /// The items a module-walk step puts in place of `item`, reusing an earlier
    /// expansion of it.
    pub(super) fn expand_in_walk(&mut self, module: ModuleId, item: Item) -> Rc<Vec<Item>> {
        let id = item.syntax().id;
        if let Some(done) = self.item_expansions.remove(&id) {
            return done;
        }
        let tracked = self.closure.items.contains_key(&id);
        if tracked {
            self.stage_stack.push(StageNode::Item(id));
            // An implementation is found by what it names, so keep it
            // readable while its own expansion runs.
            if self.closure.items[&id].kind == ItemKind::Impl {
                self.closure.keep_written(id, &item);
            }
        }
        let expanded = self.expand_unit(module, item, false);
        if tracked {
            self.stage_stack.pop();
        }
        let output = expanded.output.clone();
        if tracked {
            // Compile-time helpers and macro bodies are only expanded when
            // something needs them, so they are not yet complete.
            let complete = !matches!(expanded.group.first(), Some(Item::MacroDeclaration(_)))
                && !matches!(
                    expanded.group.first(),
                    Some(Item::Binding(binding)) if binding_is_compile_time_helper(binding)
                );
            self.finish_unit_output(id, expanded, complete, false);
        }
        output
    }

    /// Records the result of expanding `id` and refreshes what depends on it.
    fn finish_unit(&mut self, id: ItemId, expanded: Expanded, complete: bool) {
        self.finish_unit_output(id, expanded, complete, true);
    }

    fn finish_unit_output(
        &mut self,
        id: ItemId,
        expanded: Expanded,
        complete: bool,
        keep_output: bool,
    ) {
        let introduced = self
            .closure
            .record_expansion(id, expanded.group.clone(), complete);
        if introduced {
            self.closure
                .rebuild(&self.use_kinds, &self.imported_modules);
        }
        if keep_output {
            self.item_expansions.insert(id, expanded.output);
        }
    }

    fn node_label(&self, node: &StageNode) -> String {
        match node {
            StageNode::Macro(key) => format!(
                "{} `{}` in {}",
                if key.modifier { "modifier" } else { "macro" },
                key.name,
                self.module_names[key.module.0]
            ),
            StageNode::Item(id) => {
                let indexed = &self.closure.items[id];
                format!(
                    "`{}` in {}",
                    indexed.label, self.module_names[indexed.module.0]
                )
            }
        }
    }

    /// Like [`Self::record_cycle`], and reports the cycle as an error at the
    /// macro invocation `call_span` that closed it: the macro that needs itself,
    /// then one note per link, each at the item it names.
    fn cycle_at(&mut self, node: &StageNode, call_span: &Span) -> bool {
        let Some(start) = self.stage_stack.iter().position(|entry| entry == node) else {
            return false;
        };
        self.record_cycle(node);
        let mut chain = self.stage_stack[start..].to_vec();
        chain.push(node.clone());
        self.report_cycle(chain, call_span);
        if let StageNode::Macro(key) = node {
            self.macros_failed.insert(key.clone());
        }
        true
    }

    /// Reports a chain of nodes that leads back to its first: the macro that
    /// needs itself, then one note per link, each at the node it names.
    fn report_cycle(&mut self, chain: Vec<StageNode>, call_span: &Span) {
        let name = chain
            .iter()
            .find_map(|entry| match entry {
                StageNode::Macro(key) => Some(key.name.clone()),
                StageNode::Item(_) => None,
            })
            .unwrap_or_default();
        self.diagnostics.push(Diagnostic::new(
            call_span.clone(),
            format!("macro `{name}` is needed to expand code that `{name}` itself depends on"),
        ));
        for link in chain.windows(2) {
            self.diagnostics.push(Diagnostic::new(
                self.node_span(&link[1]),
                format!(
                    "note: {} is needed by {}",
                    self.node_label(&link[1]),
                    self.node_label(&link[0])
                ),
            ));
        }
    }

    /// Reports the cycle that closes when `via` calls the macro `used`, which
    /// is already being made ready.
    fn cycle_through(&mut self, via: StageNode, used: &MacroKey, call_span: &Span, report: bool) {
        let node = StageNode::Macro(used.clone());
        let Some(start) = self.stage_stack.iter().position(|entry| entry == &node) else {
            return;
        };
        let mut chain = self.stage_stack[start..].to_vec();
        chain.push(via);
        chain.push(node);
        let labels = chain
            .iter()
            .map(|entry| self.node_label(entry))
            .collect::<Vec<_>>();
        if !self.cycles.contains(&labels) {
            self.cycles.push(labels);
        }
        if report {
            self.report_cycle(chain, call_span);
            self.macros_failed.insert(used.clone());
        }
    }

    fn node_span(&self, node: &StageNode) -> Span {
        match node {
            StageNode::Macro(key) => self
                .definitions
                .get(key)
                .map(|definition| definition.declaration.syntax.span.clone())
                .unwrap_or(Span::Compiler),
            StageNode::Item(id) => self.closure.items[id].span.clone(),
        }
    }

    /// When `node` is already in progress, records the chain from it back to
    /// itself. Returns whether it was.
    fn record_cycle(&mut self, node: &StageNode) -> bool {
        let Some(start) = self.stage_stack.iter().position(|entry| entry == node) else {
            return false;
        };
        let mut chain = self.stage_stack[start..]
            .iter()
            .map(|entry| self.node_label(entry))
            .collect::<Vec<_>>();
        chain.push(self.node_label(node));
        if !self.cycles.contains(&chain) {
            self.cycles.push(chain);
        }
        true
    }

    /// `binding` with its macros expanded. Expanding a function the first time
    /// it is called keeps the interpreter's tables cheap to maintain. A
    /// function outside the running macro's closure is expanded here too, so
    /// expansion still works, but it is recorded as a closure miss: the scan
    /// should have found it.
    pub(super) fn expanded_binding(&mut self, binding: Rc<Binding>) -> Rc<Binding> {
        self.check_closure_member(binding.syntax.id);
        // Only functions: a `const` expands to its folded value, which names
        // intrinsics relative to its own hygiene rather than to the module the
        // interpreter evaluates it in.
        if binding.kind != BindingKind::Def {
            return binding;
        }
        if let Some(latest) = self.latest_bindings.get(&binding.syntax.id) {
            return latest.clone();
        }
        let Some(unit) = self.closure.unit_of(binding.syntax.id) else {
            return binding;
        };
        let _ = self.ensure_expanded(unit, None);
        let id = binding.syntax.id;
        let complete = self
            .closure
            .items
            .get(&unit)
            .is_some_and(|indexed| indexed.expanded);
        let found = self
            .closure
            .group_of(unit)
            .iter()
            .find_map(|item| match item {
                Item::Binding(found) if found.syntax.id == id => Some(Rc::new(found.clone())),
                _ => None,
            });
        match found {
            Some(found) if complete => {
                self.latest_bindings.insert(id, found.clone());
                found
            }
            _ => binding,
        }
    }
}

impl MacroExpander {
    /// Records a closure miss when compile-time code reaches the declaration
    /// `inner` but the closure of the running macro does not hold its item.
    fn check_closure_member(&mut self, inner: SyntaxId) {
        let Some(running) = self.running_macros.last() else {
            return;
        };
        let Some(closure) = self.macro_closures.get(running) else {
            return;
        };
        let Some(unit) = self.closure.unit_of(inner) else {
            return;
        };
        if closure.items.contains(&unit) {
            return;
        }
        let name = self
            .macro_by_item
            .get(running)
            .map(|key| key.name.clone())
            .unwrap_or_default();
        let item = self.node_label(&StageNode::Item(unit));
        let miss = (name, item);
        if !self.closure_misses.contains(&miss) {
            self.closure_misses.push(miss);
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::ProgramLoader;
    use std::path::PathBuf;

    /// The order macros became ready in, expanding `source` from a scratch
    /// directory next to the standard library.
    fn ready_order(source: &str) -> Vec<String> {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
        let source = format!("use std.syntax.(parse_quote, Expr, Item, Sequence)\n{source}");
        let program = ProgramLoader::new()
            .with_standard_library_root(root.join("stdlib"))
            .load_source(&source, &root)
            .expect("source should load");
        let (_, analysis) = super::super::expand_program(program).expect("source should expand");
        analysis.macro_ready_order
    }

    #[test]
    fn a_macro_is_ready_after_the_macros_its_closure_calls() {
        let order = ready_order(concat!(
            "macro first = value: Expr => helper value\n",
            "def helper = value: Expr => Expr :: second value\n",
            "macro second = value: Expr => { let copy = value; parse_quote { $copy } }\n",
            "let a = first 1\n",
        ));
        let position = |name: &str| order.iter().position(|ready| ready == name);
        let (first, second) = (position("first"), position("second"));
        assert!(first.is_some() && second.is_some(), "{order:?}");
        assert!(second < first, "{order:?}");
    }

    /// The recorded cycles of expanding `source`.
    fn cycles(source: &str) -> Vec<Vec<String>> {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
        let source = format!("use std.syntax.(parse_quote, Expr, Item, Sequence)\n{source}");
        let program = ProgramLoader::new()
            .with_standard_library_root(root.join("stdlib"))
            .load_source(&source, &root)
            .expect("source should load");
        let (_, analysis) = super::super::expand_program(program).expect("source should expand");
        analysis.cycles
    }

    #[test]
    fn a_cycle_through_a_generated_item_is_recorded_but_not_an_error() {
        // `helper` names the type `Made`, which only `make` defines, and the
        // call to `make` contains a call to `again`. Whether `again` really
        // needs `Made` cannot be told from names alone, so it expands.
        let cycles = cycles(concat!(
            "macro make: Expr -> Sequence<Item> = value: Expr => parse_quote {\n",
            "    type Made wrap I32\n",
            "    let used = $value\n",
            "}\n",
            "make (again 1)\n",
            "def helper = value: Expr => Expr :: { let probe = Made 1; value }\n",
            "macro again = value: Expr => helper value\n",
        ));
        assert!(
            cycles
                .iter()
                .any(|chain| chain.iter().any(|link| link.contains("macro `again`"))),
            "{cycles:?}"
        );
    }

    /// The closure misses of expanding `source`.
    fn closure_misses(source: &str) -> Vec<(String, String)> {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
        let source = format!("use std.syntax.(parse_quote, Expr, Item, Sequence)\n{source}");
        let program = ProgramLoader::new()
            .with_standard_library_root(root.join("stdlib"))
            .load_source(&source, &root)
            .expect("source should load");
        let (_, analysis) = super::super::expand_program(program).expect("source should expand");
        analysis.closure_misses
    }

    #[test]
    fn companion_functions_that_compile_time_code_calls_are_in_the_closure() {
        let misses = closure_misses(concat!(
            "type Box wrap I32\n",
            "companion Box {\n",
            "    pub def doubled = (b: Box) => b\n",
            "    pub def tripled = (b: Box) => b\n",
            "}\n",
            "def helper = value: Expr => Expr :: {\n",
            "    let piped = Box 1 |> doubled\n",
            "    let named = Box.tripled piped\n",
            "    value\n",
            "}\n",
            "macro m = value: Expr => helper value\n",
            "let a = m 1\n",
        ));
        assert!(misses.is_empty(), "{misses:?}");
    }

    #[test]
    fn template_macros_are_never_made_ready() {
        let order = ready_order(concat!(
            "macro wrap = value: Expr => parse_quote { ($value) }\n",
            "let a = wrap 1\n",
        ));
        assert!(!order.iter().any(|ready| ready == "wrap"), "{order:?}");
    }
}
