//! Machine-readable runnable corpus for scripts/compare-run-output.py.
use staple_compiler::{CorpusExpectation, CorpusSource, codegen_corpus};
fn main() {
    for program in codegen_corpus() {
        if program.expectation != CorpusExpectation::MustRun {
            continue;
        }
        let (kind, source) = match program.source {
            CorpusSource::Inline(source) => ("inline", source),
            CorpusSource::File(path) => ("file", path),
        };
        println!(
            "{{\"name\":{:?},\"kind\":{:?},\"source\":{:?}}}",
            program.name, kind, source
        );
    }
}
