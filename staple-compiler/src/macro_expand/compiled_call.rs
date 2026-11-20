//! Macro calls as compiled macro code takes them, and its results as the
//! expander reads them back.
//!
//! The expander still matches every call. Asked to, it records each call of a
//! macro with a body as an argument tree, from the values the interpreter
//! binds to the parameters, so the same call can be made through the macro's
//! compiled entry function. A compiled result comes back as a syntax fragment,
//! which [`compiled_result`] reads by the macro's declared result category.

use std::cell::RefCell;

use staple_syntax::{Item, Span, Syntax};

use super::{
    DelimitedValueContents, MacroArgument, MacroDefinition, MacroExpander, MetaType, SyntaxValue,
    Value, match_syntax_fragment, syntax_category,
};
use crate::ModuleId;
use crate::macro_runtime::args::ArgTree;

/// One call of a macro with a body.
#[derive(Clone)]
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

/// What a macro produced, as the expander places it.
#[derive(Clone)]
pub(crate) struct ExpandedSyntax(SyntaxValue);

impl ExpandedSyntax {
    /// The kind of syntax, as diagnostics name it.
    pub(crate) fn category(&self) -> &'static str {
        syntax_category(&self.0)
    }

    /// The syntax's tokens, without trivia.
    #[allow(dead_code)] // Only tests read the tokens so far.
    pub(crate) fn tokens(&self) -> Vec<staple_syntax::SyntaxToken> {
        let syntaxes = match &self.0 {
            SyntaxValue::Items(items) => items
                .iter()
                .map(|item| super::syntax_ops::item_syntax(item).clone())
                .collect(),
            value => value.syntax().cloned().into_iter().collect::<Vec<_>>(),
        };
        syntaxes
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
        let mut values = Vec::with_capacity(arguments.len());
        for (expected, argument) in definition.parameters.iter().zip(arguments) {
            values.push(self.macro_argument_value(expected, argument.clone())?);
        }
        self.record_values(definition, &values, call_span)
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
        let mut arguments = Vec::new();
        if let Some((value, syntax)) = argument {
            arguments.push(ArgTree::Group(trimmed(syntax), Box::new(arg_tree(value)?)));
        }
        arguments.push(ArgTree::Node(trimmed(super::syntax_ops::item_syntax(item))));
        self.record_trees(definition, arguments, call_span)
    }

    fn record_values(
        &mut self,
        definition: &MacroDefinition,
        values: &[Value],
        call_span: &Span,
    ) -> Option<usize> {
        let arguments = values.iter().map(arg_tree).collect::<Option<Vec<_>>>()?;
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
                call.interpreted = Some(ExpandedSyntax(result));
            }
        });
    }
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
        Value::Syntax(value) => ArgTree::Node(trimmed(value.syntax()?)),
        Value::Nominal(name, _) if name == "None" => ArgTree::Absent,
        Value::Nominal(name, value) if name == "Some" => arg_tree(value)?,
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
