//! Filling quotations in compiled macro code.
//!
//! A quotation is lowered to `__quote (id, splices)`. Its fragment, `id`, is a
//! template the compiler kept when it lowered the quotation, and `splices`
//! holds one syntax value per distinct name the fragment splices, in order of
//! first use. Compiled `__quote` hands the values to the host one at a time,
//! and the host fills the template the same way the interpreter fills a raw
//! quotation.
//!
//! The parent owns hygiene. A macro call carries a [`CallContext`] that says
//! which expansion marks the call may use; each quotation takes the next one,
//! as each interpreted quotation does. Quoted tokens are stamped with the
//! module that wrote the quotation and that mark, and spliced tokens keep the
//! hygiene they bring.

use std::cell::RefCell;
use std::convert::Infallible;
use std::sync::Arc;

use staple_syntax::{Span, Syntax, SyntaxId};

use super::arena::with_arena;
use crate::ModuleId;
use crate::macro_expand::lower_quote::{TemplateTable, distinct_splice_names};
use crate::macro_expand::syntax_ops::{append_syntax_tokens, fill_quote, root_hygiene};

/// What the parent says about one macro call.
#[derive(Clone, Debug)]
pub(crate) struct CallContext {
    /// The module that defines the macro being called. Identifiers the macro
    /// constructs resolve there.
    pub(crate) definition_module: ModuleId,
    /// The first expansion mark the call may use. Every quotation takes the
    /// next unused one.
    pub(crate) next_mark: u64,
    /// Where the macro was called. Constructed identifiers have this span.
    pub(crate) call_site: Span,
    /// The mark every identifier the call constructs shares, taken when the
    /// first one is spliced.
    pub(crate) identifier_mark: Option<u64>,
}

impl Default for CallContext {
    fn default() -> Self {
        Self {
            definition_module: ModuleId(0),
            next_mark: 1,
            call_site: Span::Compiler,
            identifier_mark: None,
        }
    }
}

thread_local! {
    static TEMPLATES: RefCell<Arc<TemplateTable>> = RefCell::new(Arc::default());
    static CONTEXT: RefCell<CallContext> = RefCell::new(CallContext::default());
}

/// Makes `templates` the fragments quotations on this thread fill. They belong
/// to one check program, and stay until another program's replace them.
#[allow(dead_code)] // Only the test harness makes calls so far.
pub(crate) fn install_templates(templates: Arc<TemplateTable>) {
    TEMPLATES.with(|current| *current.borrow_mut() = templates);
}

/// Starts a macro call on this thread.
#[allow(dead_code)] // Only the test harness makes calls so far.
pub(crate) fn begin_call(context: CallContext) {
    CONTEXT.with(|current| *current.borrow_mut() = context);
}

/// The first expansion mark no quotation of the current call has used, which
/// the parent continues from.
#[allow(dead_code)] // Only the test harness makes calls so far.
pub(crate) fn next_mark() -> u64 {
    CONTEXT.with(|current| current.borrow().next_mark)
}

/// The module, mark and span of an identifier the current call constructs.
///
/// The interpreter gives each constructed identifier a mark of its own when
/// it is built. Compiled code learns of one only when it is spliced, and may
/// splice it more than once, so every identifier one call constructs shares a
/// mark. That keeps `let $name = ...; $name` one binding.
pub(crate) fn constructed_identifier() -> (ModuleId, u64, Span) {
    let mark = CONTEXT.with(|current| current.borrow().identifier_mark);
    let mark = mark.unwrap_or_else(|| {
        let mark = take_mark();
        CONTEXT.with(|current| current.borrow_mut().identifier_mark = Some(mark));
        mark
    });
    CONTEXT.with(|current| {
        let context = current.borrow();
        (context.definition_module, mark, context.call_site.clone())
    })
}

fn take_mark() -> u64 {
    CONTEXT.with(|current| {
        let mut context = current.borrow_mut();
        let mark = context.next_mark;
        context.next_mark += 1;
        mark
    })
}

/// Starts filling template `template`.
pub(crate) extern "C" fn quote_begin(template: u64) -> u64 {
    with_arena(|arena| arena.insert_quote(template)).into()
}

/// Gives a quotation the value of its next distinct splice name.
pub(crate) extern "C" fn quote_splice(quotation: u64, splice: u64) -> u64 {
    let added = with_arena(|arena| arena.push_quote_splice(quotation.into(), splice));
    assert!(added, "quotation handle {quotation:#x} is not a quotation being filled");
    quotation
}

/// Fills a quotation's template with the values it was given.
pub(crate) extern "C" fn quote_end(quotation: u64) -> u64 {
    let (template, splices) = with_arena(|arena| {
        arena
            .quote(quotation.into())
            .map(|(template, splices)| (template, splices.to_vec()))
    })
    .unwrap_or_else(|| panic!("quotation handle {quotation:#x} is not a quotation being filled"));
    let template = TEMPLATES
        .with(|templates| templates.borrow().templates.get(template as usize).cloned())
        .unwrap_or_else(|| panic!("there is no quotation template {template}"));
    let names = distinct_splice_names(template.contents.tokens());
    assert_eq!(
        names.len(),
        splices.len(),
        "a quotation is given one value per distinct splice name"
    );
    let values = splices
        .iter()
        .map(|&handle| {
            with_arena(|arena| arena.syntax(handle.into()).cloned()).unwrap_or_else(|| {
                panic!("syntax handle {handle:#x} is not a live syntax fragment")
            })
        })
        .collect::<Vec<Syntax>>();
    let mark = take_mark();
    let id = SyntaxId(with_arena(|arena| arena.next_syntax_id()));
    let Ok(filled) = fill_quote(
        &template.contents,
        template.module.0,
        mark,
        id,
        |name, output| {
            let index = names
                .iter()
                .position(|known| known == name)
                .expect("every splice name was collected");
            let value = &values[index];
            append_syntax_tokens(output, value, root_hygiene(value));
            Ok::<(), Infallible>(())
        },
    );
    with_arena(|arena| arena.insert_syntax(filled)).into()
}
