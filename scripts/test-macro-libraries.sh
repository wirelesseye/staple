#!/bin/sh
# Runs the tests that build, link and load compiled macro libraries. They
# compile whole check programs, so they are ignored in the default test run.
# The package-library tests expand and check the standard library in macro
# mode, which is only practical with an optimized compiler.
#
# usage: test-macro-libraries.sh [extra cargo test arguments]
set -eu

script_directory=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
cd "$script_directory/.."

cargo test -p staple-compiler --lib macro_runtime::harness::library_tests "$@" -- --ignored
cargo test --release -p staple --test compiled_macros package_library "$@" -- --ignored
cargo test --release -p staple --test compiled_macros prebuilt_standard_library "$@" -- --ignored
