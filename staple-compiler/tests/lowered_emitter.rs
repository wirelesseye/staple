use std::path::Path;

use inkwell::context::Context;
use staple_compiler::{CodeGenerator, Emitter, Lowerer, NameResolver, ProgramLoader, TypeChecker};

fn compile(source: &str, emitter: Emitter) -> Result<String, Vec<staple_syntax::Diagnostic>> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source(source, root)
        .unwrap();
    let resolved = NameResolver::new().resolve_program(program).unwrap();
    let typed = TypeChecker::new().check(resolved).unwrap();
    let lowered = Lowerer::new().lower(&typed).unwrap();
    let context = Context::create();
    CodeGenerator::with_emitter(&context, emitter).compile_module(&lowered)
}

#[test]
fn selector_preserves_legacy_and_reports_unported_lowered_body() {
    let llvm = compile("", Emitter::Legacy).unwrap();
    assert!(llvm.contains("define i32 @main()"));

    let diagnostics = compile("", Emitter::Lowered).unwrap_err();
    assert!(
        diagnostics[0].message.starts_with("lowered emitter:"),
        "{}",
        diagnostics[0].message
    );
}
