#!/bin/sh
# Runs the tests that build, link and load compiled macro libraries. Each one
# compiles a whole check program, so they are ignored in the default test run.
#
# usage: test-macro-libraries.sh [extra cargo test arguments]
set -eu

script_directory=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
cd "$script_directory/.."
exec cargo test -p staple-compiler --lib macro_runtime::harness::library_tests "$@" -- --ignored
