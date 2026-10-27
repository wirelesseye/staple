//! `Debug` output of the pipeline's stage results stays printable.
//!
//! Every program loads the standard library, so a derived `Debug` that
//! repeats shared data per node grows past what memory can hold. A test that
//! calls `expect_err` on an unexpected `Ok` then exhausts memory instead of
//! failing. These tests bound each stage's `Debug` output for a tiny program.

use staple_compiler::{Lowerer, NameResolver, ProgramLoader, TypeChecker};
use std::{
    fmt::{self, Debug, Write},
    path::Path,
};

/// Counts formatted bytes without storing them, and stops formatting once
/// the count passes `limit` so a regression fails fast.
struct ByteCounter {
    bytes: usize,
    limit: usize,
}

impl Write for ByteCounter {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        self.bytes += text.len();
        if self.bytes > self.limit {
            Err(fmt::Error)
        } else {
            Ok(())
        }
    }
}

fn assert_debug_within(name: &str, value: &impl Debug, limit: usize) {
    let mut counter = ByteCounter { bytes: 0, limit };
    let within = write!(counter, "{value:?}").is_ok();
    assert!(within, "`{name}` Debug output exceeds {limit} bytes");
}

const MIB: usize = 1 << 20;

#[test]
fn stage_results_have_bounded_debug_output() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source("let x = 1\n", root)
        .unwrap_or_else(|error| panic!("load: {error}"));
    assert_debug_within("Program", &program, 128 * MIB);

    let resolved = NameResolver::new()
        .resolve_program(program)
        .unwrap_or_else(|errors| panic!("resolve: {} diagnostics", errors.len()));
    assert_debug_within("ResolvedModule", &resolved, 256 * MIB);

    let typed = TypeChecker::new()
        .check(resolved)
        .unwrap_or_else(|errors| panic!("check: {} diagnostics", errors.len()));
    assert_debug_within("TypedModule", &typed, 4096);

    let lowered = Lowerer::new()
        .lower(&typed)
        .unwrap_or_else(|errors| panic!("lower: {} diagnostics", errors.len()));
    assert_debug_within("LoweredModule", &lowered, 384 * MIB);
}
