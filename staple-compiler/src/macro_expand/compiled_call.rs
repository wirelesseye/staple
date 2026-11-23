//! Macro calls as compiled macro code takes them, and its results as the
//! expander reads them back.
//!
//! The expander still matches every call. Asked to, it records each call of a
//! macro with a body as an argument tree, from the values the interpreter
//! binds to the parameters, so the same call can be made through the macro's
//! compiled entry function. A compiled result comes back as a syntax fragment,
//! which [`compiled_result`] reads by the macro's declared result category.

use std::cell::RefCell;
use std::collections::HashSet;

use staple_syntax::{Diagnostic, Item, Span, Syntax, SyntaxId, TokenKind};

use super::lower_quote::TemplateTable;
use super::{
    DelimitedValueContents, MacroArgument, MacroDefinition, MacroExpander, MetaType, SyntaxValue,
    Value, match_syntax_fragment, syntax_category,
};
use crate::macro_runtime::args::ArgTree;
use crate::{ModuleId, Program};

/// One call of a macro with a body.
#[derive(Clone)]
#[allow(dead_code)] // Only tests read recorded calls so far.
pub(crate) struct RecordedCall {
    /// The overload's declaration, which names its entry function.
    pub(crate) declaration: staple_syntax::MacroDeclaration,
    /// The module that defines the macro.
    pub(crate) definition_module: ModuleId,
    pub(crate) call_site: Span,
    /// One tree per parameter.
    pub(crate) arguments: Vec<ArgTree>,
    /// The declared result category.
    pub(crate) result: MetaType,
    /// What the interpreter's call returned, when it succeeded.
    pub(crate) interpreted: Option<ExpandedSyntax>,
    /// How long the interpreter's call took, when it succeeded.
    pub(crate) elapsed: Option<std::time::Duration>,
    started: std::time::Instant,
}

thread_local! {
    static RECORDED: RefCell<Option<Vec<RecordedCall>>> = const { RefCell::new(None) };
}

/// Runs `f` with call recording on, and returns what it recorded on this thread.
#[allow(dead_code)] // Only the test harness records calls so far.
pub(crate) fn record_calls<T>(f: impl FnOnce() -> T) -> (T, Vec<RecordedCall>) {
    let previous = RECORDED.with(|recorded| recorded.borrow_mut().replace(Vec::new()));
    let value = f();
    let calls = RECORDED.with(|recorded| std::mem::replace(&mut *recorded.borrow_mut(), previous));
    (value, calls.unwrap_or_default())
}

fn recording() -> bool {
    RECORDED.with(|recorded| recorded.borrow().is_some())
}

/// A call the expander asks compiled code to make.
pub struct CompiledCall<'a> {
    pub declaration: &'a staple_syntax::MacroDeclaration,
    pub definition_module: ModuleId,
    pub call_site: Span,
    /// One tree per parameter.
    pub arguments: Vec<ArgTree>,
    /// The first expansion mark the call may use.
    pub next_mark: u64,
}

/// A closure-only check program ready to be lowered, emitted, linked, and loaded
/// into a macro library.
pub struct ClosureLibraryRequest<'a> {
    /// The expanded-so-far program, for resolving manifest keys and templates.
    pub program: &'a Program,
    /// The closure-only check program for the requested macros.
    pub check_program: Program,
    /// Quotation templates collected when building `check_program`.
    pub templates: TemplateTable,
    /// `(module, declaration_id, entry_function_name)` for each macro in this request.
    pub entries: Vec<(ModuleId, SyntaxId, String)>,
    /// The next syntax id after `check_program` was built, for `NameResolver::pre_expanded`.
    pub next_syntax_id: usize,
}

/// Backend that compiles/loads macro libraries and executes compiled macro calls.
pub trait MacroBackend: Send {
    /// Makes the compiled library for the macros in `request` available,
    /// returning the `SyntaxId`s of the macro declarations that are now ready,
    /// or the diagnostics that stopped it. A diagnostic with no source span is
    /// reported at the call that needed the library.
    fn prepare(
        &mut self,
        request: ClosureLibraryRequest<'_>,
    ) -> Result<Vec<SyntaxId>, Vec<Diagnostic>>;

    /// Executes one macro call through compiled code. Returning `None` leaves
    /// the call to the interpreter (allowed only when strict compiled mode is
    /// off, as in the comparison harness).
    fn call(&mut self, call: &CompiledCall<'_>) -> Option<Result<(Syntax, u64), String>>;
}

pub(crate) struct ActiveBackend {
    backend: Box<dyn MacroBackend>,
    strict: bool,
}

thread_local! {
    static ENGINE: RefCell<Option<ActiveBackend>> = const { RefCell::new(None) };
}

pub(crate) fn take_backend() -> Option<ActiveBackend> {
    ENGINE.with(|engine| engine.borrow_mut().take())
}

pub(crate) fn restore_backend(active: Option<ActiveBackend>) {
    ENGINE.with(|engine| *engine.borrow_mut() = active);
}

/// Runs `f` with `backend` making the macro calls it takes on this thread.
///
/// When `strict` is `true`, no non-template macro body or helper may fall back
/// to the interpreter.
pub fn with_macro_backend<T>(
    backend: Box<dyn MacroBackend>,
    strict: bool,
    f: impl FnOnce() -> T,
) -> T {
    let previous = ENGINE.with(|current| {
        current
            .borrow_mut()
            .replace(ActiveBackend { backend, strict })
    });
    let value = f();
    ENGINE.with(|current| *current.borrow_mut() = previous);
    value
}

pub(super) fn compiling() -> bool {
    ENGINE.with(|engine| engine.borrow().is_some())
}

pub(super) fn strict_compiled_mode() -> bool {
    ENGINE.with(|engine| engine.borrow().as_ref().is_some_and(|active| active.strict))
}

pub(super) fn prepare_compiled_library(
    request: ClosureLibraryRequest<'_>,
) -> Option<Result<Vec<SyntaxId>, Vec<Diagnostic>>> {
    let mut active = ENGINE.with(|engine| engine.borrow_mut().take())?;
    let result = active.backend.prepare(request);
    ENGINE.with(|current| *current.borrow_mut() = Some(active));
    Some(result)
}

/// What a macro produced, as the expander places it.
#[derive(Clone)]
pub(crate) struct ExpandedSyntax(SyntaxValue);

impl ExpandedSyntax {
    /// The kind of syntax, as diagnostics name it.
    #[allow(dead_code)] // Only tests read the category so far.
    pub(crate) fn category(&self) -> &'static str {
        syntax_category(&self.0)
    }

    /// The syntax of the root, or of each item of an item list.
    #[allow(dead_code)] // Only tests read the syntax so far.
    pub(crate) fn syntaxes(&self) -> Vec<Syntax> {
        match &self.0 {
            SyntaxValue::Items(items) => items
                .iter()
                .map(|item| super::syntax_ops::item_syntax(item).clone())
                .collect(),
            value => value.syntax().cloned().into_iter().collect(),
        }
    }

    /// The syntax's tokens, without trivia.
    #[allow(dead_code)] // Only tests read the tokens so far.
    pub(crate) fn tokens(&self) -> Vec<staple_syntax::SyntaxToken> {
        self.syntaxes()
            .iter()
            .flat_map(|syntax| syntax.tokens().iter().filter(|token| !token.kind.is_trivia()).cloned())
            .collect()
    }

    /// The id of the syntax's root, or of its first item.
    #[allow(dead_code)] // Only tests read the id so far.
    pub(crate) fn id(&self) -> Option<staple_syntax::SyntaxId> {
        match &self.0 {
            SyntaxValue::Items(items) => items.first().map(|item| super::syntax_ops::item_syntax(item).id),
            value => value.syntax().map(|syntax| syntax.id),
        }
    }

    /// The source text of the syntax, without the trivia around it, one line
    /// per item of an item list. A node the macro built without tokens has
    /// none.
    #[allow(dead_code)] // Only tests read the text so far.
    pub(crate) fn text(&self) -> String {
        let text = match &self.0 {
            SyntaxValue::Items(items) => items
                .iter()
                .map(|item| super::syntax_ops::item_syntax(item).text().trim().to_owned())
                .collect::<Vec<_>>()
                .join("\n"),
            value => value.syntax().map(Syntax::text).unwrap_or_default(),
        };
        text.trim().to_owned()
    }
}

impl MacroExpander {
    /// Records a function-style call before the interpreter runs it, and
    /// returns its index for [`Self::record_result`].
    pub(super) fn record_call(
        &mut self,
        definition: &MacroDefinition,
        arguments: &[MacroArgument],
        call_span: &Span,
    ) -> Option<usize> {
        if !recording() {
            return None;
        }
        let arguments = self.argument_trees(definition, arguments)?;
        self.record_trees(definition, arguments, call_span)
    }

    fn argument_trees(
        &mut self,
        definition: &MacroDefinition,
        arguments: &[MacroArgument],
    ) -> Option<Vec<ArgTree>> {
        let mut trees = Vec::with_capacity(arguments.len());
        for (expected, argument) in definition.parameters.iter().zip(arguments) {
            trees.push(arg_tree(&self.macro_argument_value(expected, argument.clone())?)?);
        }
        Some(trees)
    }

    /// A function-style call's argument trees, when compiled code makes calls.
    pub(super) fn call_trees(
        &mut self,
        definition: &MacroDefinition,
        arguments: &[MacroArgument],
    ) -> Option<Vec<ArgTree>> {
        if !compiling() {
            return None;
        }
        self.argument_trees(definition, arguments)
    }

    /// A modifier call's argument trees, when compiled code makes calls.
    pub(super) fn modifier_trees(
        &self,
        argument: Option<(&Value, &Syntax)>,
        item: &Item,
    ) -> Option<Vec<ArgTree>> {
        if !compiling() {
            return None;
        }
        modifier_arguments(argument, item)
    }

    /// Makes a call through compiled code, when it can take it. The outer
    /// `None` leaves the call to the interpreter; the inner one is a call that
    /// failed, with its diagnostic reported.
    pub(super) fn call_compiled(
        &mut self,
        definition: &MacroDefinition,
        arguments: Vec<ArgTree>,
        call_span: &Span,
    ) -> Option<Option<SyntaxValue>> {
        let call = CompiledCall {
            declaration: &definition.declaration,
            definition_module: definition.key.module,
            call_site: call_span.clone(),
            arguments,
            next_mark: self.next_mark,
        };
        let mut active = ENGINE.with(|engine| engine.borrow_mut().take())?;
        let outcome = active.backend.call(&call);
        ENGINE.with(|current| *current.borrow_mut() = Some(active));
        let first_mark = self.next_mark;
        let result = match outcome? {
            Ok((syntax, next_mark)) => {
                self.next_mark = next_mark;
                compiled_result(&definition.result, syntax, &mut self.next_syntax_id)
            }
            Err(message) => Err(message),
        };
        Some(match result {
            Ok(ExpandedSyntax(mut value)) => {
                // The names a quotation binds take its mark, as the
                // interpreter renames them, and each node resolves where its
                // tokens were written: quoted ones in the macro's module,
                // spliced ones at the caller.
                let marks = super::Marks::Quoted(quotation_marks(&value, first_mark..self.next_mark));
                let mark = self.next_mark;
                super::alpha_rename_value(&mut value, &marks);
                self.freshen_by_tokens(&mut value, definition.key.module, mark);
                Some(value)
            }
            Err(message) => {
                self.diagnostics.push(Diagnostic::new(
                    call_span.clone(),
                    message,
                ));
                None
            }
        })
    }

    /// Records a modifier call: its argument, when it has one, and the item.
    ///
    /// The interpreter binds a modifier's `Parenthesized<…>` argument to what
    /// is inside the parentheses. Its declared type is the group, so the tree
    /// is the group, around the argument's parenthesized syntax.
    pub(super) fn record_modifier_call(
        &mut self,
        definition: &MacroDefinition,
        argument: Option<(&Value, &Syntax)>,
        item: &Item,
        call_span: &Span,
    ) -> Option<usize> {
        if !recording() {
            return None;
        }
        let arguments = modifier_arguments(argument, item)?;
        self.record_trees(definition, arguments, call_span)
    }

    fn record_trees(
        &mut self,
        definition: &MacroDefinition,
        arguments: Vec<ArgTree>,
        call_span: &Span,
    ) -> Option<usize> {
        RECORDED.with(|recorded| {
            let mut recorded = recorded.borrow_mut();
            let calls = recorded.as_mut()?;
            calls.push(RecordedCall {
                declaration: definition.declaration.clone(),
                definition_module: definition.key.module,
                call_site: call_span.clone(),
                arguments,
                result: definition.result.clone(),
                interpreted: None,
                elapsed: None,
                started: std::time::Instant::now(),
            });
            Some(calls.len() - 1)
        })
    }

    /// Records what the interpreter's call `index` returned.
    pub(super) fn record_result(&self, index: Option<usize>, result: Option<SyntaxValue>) {
        let (Some(index), Some(result)) = (index, result) else {
            return;
        };
        RECORDED.with(|recorded| {
            if let Some(call) = recorded.borrow_mut().as_mut().and_then(|calls| calls.get_mut(index)) {
                call.elapsed = Some(call.started.elapsed());
                call.interpreted = Some(ExpandedSyntax(result));
            }
        });
    }
}

/// A modifier call's trees: its argument, when it has one, and the item.
///
/// The interpreter binds a modifier's `Parenthesized<…>` argument to what is
/// inside the parentheses. Its declared type is the group, so the tree is the
/// group, around the argument's parenthesized syntax.
fn modifier_arguments(argument: Option<(&Value, &Syntax)>, item: &Item) -> Option<Vec<ArgTree>> {
    let mut arguments = Vec::new();
    if let Some((value, syntax)) = argument {
        arguments.push(ArgTree::Group(trimmed(syntax), Box::new(arg_tree(value)?)));
    }
    arguments.push(ArgTree::Node(trimmed(&modified_syntax(item)?)));
    Some(arguments)
}

/// The fragment of the item a modifier is applied to. A modified item keeps
/// the syntax it was read with after its outer modifiers are taken off, so
/// the fragment starts at the first modifier still on it.
fn modified_syntax(item: &Item) -> Option<Syntax> {
    let Item::Modified(modified) = item else {
        return Some(super::syntax_ops::item_syntax(item).clone());
    };
    let first = &modified.modifiers.first()?.syntax;
    let whole = &modified.syntax;
    if !std::ptr::eq(first.token_stream().as_ptr(), whole.token_stream().as_ptr())
        || !whole.token_range.contains(&first.token_range.start)
    {
        return None;
    }
    let mut syntax = whole.clone();
    syntax.token_range = first.token_range.start..whole.token_range.end;
    Some(syntax)
}

/// The tree of a value the interpreter binds to a parameter.
///
/// A syntax value is a fragment without the trivia around it, except a
/// delimited one, which is a group around its contents. An option is its value or absent. Sequences, products
/// and items are lists, and a separated list is a list of its elements, its
/// separator and its trailing separator, absent when there is none.
fn arg_tree(value: &Value) -> Option<ArgTree> {
    Some(match value {
        Value::Syntax(SyntaxValue::Delimited(delimited)) => {
            let contents = match &delimited.contents {
                // One slot is the contents themselves.
                DelimitedValueContents::Slotted(values) if values.len() == 1 => {
                    arg_tree(&values[0].1)?
                }
                DelimitedValueContents::Slotted(values) | DelimitedValueContents::Product(values) => {
                    ArgTree::List(values.iter().map(|(_, value)| arg_tree(value)).collect::<Option<_>>()?)
                }
                DelimitedValueContents::Sequence(values) => {
                    ArgTree::List(values.iter().map(arg_tree).collect::<Option<_>>()?)
                }
                DelimitedValueContents::Separated {
                    elements,
                    separator,
                    trailing,
                } => separated_tree(elements, separator, *trailing)?,
            };
            ArgTree::Group(trimmed(&delimited.syntax), Box::new(contents))
        }
        Value::Syntax(SyntaxValue::Items(items)) => ArgTree::List(
            items
                .iter()
                .map(|item| ArgTree::Node(trimmed(super::syntax_ops::item_syntax(item))))
                .collect(),
        ),
        // A doc comment is a modifier with no tokens of its own, and a
        // visibility's syntax need not spell it, so both are written out.
        Value::Syntax(SyntaxValue::Modifier(modifier)) if modifier.invocation.doc.is_some() => {
            let doc = modifier.invocation.doc.as_deref().unwrap_or_default();
            ArgTree::Node(written(&format!("///{doc}\n")))
        }
        Value::Syntax(SyntaxValue::Visibility(visibility)) => ArgTree::Node(written(match visibility.kind {
            staple_syntax::VisibilityKind::Public => "pub",
            staple_syntax::VisibilityKind::Package => "pub(package)",
            staple_syntax::VisibilityKind::Private => "",
        })),
        Value::Syntax(value) => ArgTree::Node(trimmed(value.syntax()?)),
        Value::Nominal(name, _) if name == "None" => ArgTree::Absent,
        Value::Nominal(name, value) if name == "Some" => arg_tree(value)?,
        Value::Nominal(name, value) if name == "AliasBody" => {
            let Value::Syntax(ty) = value.as_ref() else {
                return None;
            };
            ArgTree::Node(written(&format!("= {}", ty.syntax()?.text())))
        }
        Value::Nominal(name, value) if name == "WrapperBody" || name == "FromBody" => {
            let keyword = if name == "WrapperBody" { "wrap" } else { "from" };
            let Value::Product(fields, _) = value.as_ref() else {
                return None;
            };
            let mut elements = fields.iter();
            let (_, Value::Syntax(SyntaxValue::Visibility(vis))) = elements.next()? else {
                return None;
            };
            let (_, Value::Syntax(ty)) = elements.next()? else {
                return None;
            };
            let vis_text = match vis.kind {
                staple_syntax::VisibilityKind::Public => "pub ",
                staple_syntax::VisibilityKind::Package => "pub(package) ",
                staple_syntax::VisibilityKind::Private => "",
            };
            let ty_text = ty.syntax()?.text();
            ArgTree::Node(written(&format!("{keyword} {vis_text}{ty_text}")))
        }
        Value::Nominal(name, _) if name == "OpaqueBody" => ArgTree::Node(written("opaque")),
        Value::Sequence(values) => {
            ArgTree::List(values.iter().map(arg_tree).collect::<Option<_>>()?)
        }
        Value::Product(values, _) => {
            ArgTree::List(values.iter().map(|(_, value)| arg_tree(value)).collect::<Option<_>>()?)
        }
        Value::Separated {
            elements,
            separator,
            trailing,
        } => separated_tree(elements, separator, *trailing)?,
        _ => return None,
    })
}

/// The marks of the quotations one call made, from the result's tokens.
///
/// A quotation stamps every token it writes, and the identifiers and
/// literals the call constructs share one more mark of the call. Only a
/// quotation writes keywords and punctuation, so a mark one of those carries
/// is a quotation's. The interpreter splices a constructed identifier without
/// renaming it.
fn quotation_marks(value: &SyntaxValue, marks: std::ops::Range<u64>) -> HashSet<u64> {
    let mut quoted = HashSet::new();
    let mut add = |syntax: &Syntax| {
        quoted.extend(
            syntax
                .tokens()
                .iter()
                .filter(|token| {
                    !token.kind.is_trivia()
                        && !matches!(
                            token.kind,
                            TokenKind::Identifier
                                | TokenKind::String
                                | TokenKind::Char
                                | TokenKind::Byte
                                | TokenKind::Integer
                                | TokenKind::Float
                        )
                })
                .filter_map(|token| token.hygiene)
                .map(|(_, mark)| mark)
                .filter(|mark| marks.contains(mark)),
        );
    };
    match value {
        SyntaxValue::Items(items) => items
            .iter()
            .for_each(|item| add(super::syntax_ops::item_syntax(item))),
        value => {
            if let Some(syntax) = value.syntax() {
                add(syntax);
            }
        }
    }
    quoted
}

/// A fragment of new tokens, for a value no source spells.
fn written(text: &str) -> Syntax {
    super::syntax_ops::syntax_from_text(text, &mut 0)
}

/// A fragment without the trivia around it, which separates an argument from
/// what is next to it rather than belonging to it.
fn trimmed(syntax: &Syntax) -> Syntax {
    let tokens = syntax.tokens();
    let Some(first) = tokens.iter().position(|token| !token.kind.is_trivia()) else {
        return syntax.clone();
    };
    let last = tokens
        .iter()
        .rposition(|token| !token.kind.is_trivia())
        .unwrap_or(first);
    if first == 0 && last + 1 == tokens.len() {
        return syntax.clone();
    }
    super::syntax_ops::syntax_slice(syntax, first, last + 1)
}

fn separated_tree(elements: &[Value], separator: &Value, trailing: bool) -> Option<ArgTree> {
    let separator = arg_tree(separator)?;
    let trailing = if trailing {
        separator.clone()
    } else {
        ArgTree::Absent
    };
    Some(ArgTree::List(vec![
        ArgTree::List(elements.iter().map(arg_tree).collect::<Option<_>>()?),
        separator,
        trailing,
    ]))
}

/// Reads a compiled macro's result by its declared category, with fresh ids.
///
/// Raw `Syntax` stays a fragment, which the expander parses where it places
/// it, as it does an interpreted one. A `Sequence<Item>` is a list of items.
/// Every other category is parsed as the interpreter parses that category's
/// arguments.
pub(crate) fn compiled_result(
    result: &MetaType,
    syntax: Syntax,
    next_syntax_id: &mut usize,
) -> Result<ExpandedSyntax, String> {
    let value = match result {
        MetaType::Syntax => SyntaxValue::Raw(syntax),
        MetaType::Sequence(element) if **element == MetaType::Item => SyntaxValue::Items(
            staple_syntax::parse_item_list_fragment(&syntax, next_syntax_id)
                .map_err(|error| error.message)?,
        ),
        result => match match_syntax_fragment(result, &syntax, next_syntax_id) {
            Some(Value::Syntax(value)) => value,
            _ => {
                return Err(format!(
                    "the macro produced `{}`, which is not `{}` syntax",
                    syntax.text().trim(),
                    super::format_meta_signature(std::slice::from_ref(result))
                ));
            }
        },
    };
    Ok(ExpandedSyntax(value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::macro_expand::{DelimitedMetaContents, DelimiterKind};

    fn read(result: MetaType, text: &str) -> Result<ExpandedSyntax, String> {
        let mut next = 1;
        let syntax = super::super::syntax_ops::syntax_from_text(text, &mut next);
        compiled_result(&result, syntax, &mut next)
    }

    fn category(result: MetaType, text: &str) -> &'static str {
        read(result, text)
            .unwrap_or_else(|message| panic!("`{text}` reads: {message}"))
            .category()
    }

    #[test]
    fn a_result_is_read_by_its_declared_category() {
        assert_eq!(category(MetaType::Syntax, "f )"), "syntax fragment");
        assert_eq!(category(MetaType::Expr, "f 1"), "expression");
        assert_eq!(category(MetaType::Type, "Option<I32>"), "type");
        assert_eq!(category(MetaType::Pattern, "Some x"), "pattern");
        assert_eq!(category(MetaType::Item, "type A = I32"), "item");
        assert_eq!(category(MetaType::Visibility, "pub"), "visibility");
        assert_eq!(
            category(
                MetaType::Delimited(
                    DelimiterKind::Braced,
                    DelimitedMetaContents::Sequence(Box::new(MetaType::SyntaxNode)),
                ),
                "{ a b }",
            ),
            "braced"
        );
        let items = read(MetaType::Sequence(Box::new(MetaType::Item)), "type A = I32\ntype B = I32")
            .expect("an item list reads");
        assert_eq!(items.category(), "item sequence");
        assert_eq!(items.text(), "type A = I32\ntype B = I32");
        // Every node gets a fresh id.
        assert!(items.id().is_some_and(|id| id.0 > 1));
    }

    #[test]
    fn a_result_of_another_category_is_an_error() {
        let error = read(MetaType::Item, "1 +").err().expect("`1 +` is not an item");
        assert_eq!(error, "the macro produced `1 +`, which is not `Item` syntax");
        assert!(read(MetaType::Ident(None), "f 1").is_err());
        assert!(read(MetaType::Sequence(Box::new(MetaType::Item)), "type =").is_err());
    }
}

