# Staged macro compilation, Phase 4: building and running phase-1 libraries

Breakdown of Phase 4 in [`staged-macro-compilation.md`](staged-macro-compilation.md).
Line references are against `docs/staged-macro-plan` at `bba16df7`
(2026-11-22), the end of Phase 3.

Phase 3 made compiled macro code run, but only in-process, under a JIT test
harness, from a check program built by the interpreter's expansion. Phase 4 makes
it run the way a build will:

- Macro code is compiled into shared libraries for the host.
- The libraries are cached, and the standard library's are prebuilt.
- They run in `staple __macro-server`, a separate process that turns panics,
  crashes and timeouts into diagnostics.
- The standard library's own macros build in stage order, with no macro body run
  by the interpreter.

The interpreter stays the default. The compiled engine is reachable through a
hidden `--macro-engine=compiled` flag. Comparing the engines at scale, making
compiled the default and the language server are Phase 5.

## Rules for the implementor

- **Keep the status section of this file up to date.** Each time you finish a
  task, find a blocker or measure something, update
  [Implementation status](#implementation-status) in the same commit. Mark the
  task done, partly done or blocked, add the commit, and write down what you
  found. Anything that changes the plan goes in [Findings](#findings). When the
  phase ends, update the Phase 4 section of the umbrella plan with a short
  status paragraph, and change later phases where the findings disagree with
  them.
- Commit everything to `docs/staged-macro-plan`, with no pull requests. A task
  can take several commits, but the tests pass after each one.
- Don't refer to the plan files or phase numbers in the code and
  documentation, except from the plan files themselves.
- Don't run `staple fmt` (see `AGENTS.md`), and don't use its output in tests.
- Interpreter expansion must not change. Run `scripts/compare-expansion.py`
  against the start of the phase after every task that touches the expander or
  the standard library, and explain every difference in Findings.
- `scripts/check-macro-closures.py --mode all` stays clean after every commit.
- Don't add `if`, `when`, `for` or `while` to code in `std.syntax`, or to code the
  syntax runtime reaches. Write it with `match` and `loop` (task 6).
- Work on macOS and Linux, as Phase 0 did (its Docker environment is on
  `spike/macro-server`, under `spike/macro-server/linux/`). Other hosts keep the
  interpreter, and `--macro-engine=compiled` reports that it isn't supported.
- Library builds take seconds. Put the slow tests behind one shared fixture or
  mark them `#[ignore]` with a script that runs them, and say which in Findings.
  Phase 3's `target` directory grew to 226 GB, so clean old test binaries when
  disk runs low.

## What already exists

All of these were read from the source at `bba16df7`, unless they name the spike.

- **The spike built and loaded libraries, on an unmerged branch.**
  `spike/macro-server` has:
  - `--emit dylib` with `--library-entry`, `--library-host-symbol` and
    `--library-trap-exit` (`staple-cli/src/compile.rs`).
  - `EmissionUnit::with_library(LibrarySpec)`, which emits
    `__staple_library_init` and one `__staple_entry.<symbol>` shim per entry
    (`staple-compiler/src/codegen/lowered/mod.rs`).
  - `staple-cli/src/macro_server.rs`, a line-protocol server over
    stdin/stdout.

  Its findings still apply (Phase 0, Findings 1, 3, 4, 7 and 9):
  - Host thunks and the `exit` trap must be *hidden external* definitions.
  - Use `_setjmp` and `_longjmp`.
  - A stack overflow kills the process with SIGABRT.
  - Symbols must not depend on the user's module layout.

  None of this code is on the branch. Port it, don't merge the branch.
- **The host table.** `macro_runtime/intrinsics.rs` lists every host function
  in `INTRINSICS` (`:65`) as a name, a symbol `__staple_syntax_<name>` and an
  address. The JIT harness maps the same list. The arena and other runtime state
  are thread-local.
- **Entry functions.** The check program gives each macro overload with typed
  parameters an entry function `entry#<name>#<syntax id>: MatchNode -> Syntax`
  (`check_program.rs:464`, built at `:651`). It decodes the parameters through
  `FromMatch`, calls the macro and splices the result. Its name holds a
  per-compilation syntax id, and it is declared as `operator.<hex>`.
- **Check programs per package.** `check_programs` (`check_program.rs:907`) builds
  one program per package from the interpreter's expansion. The program's own
  package is closure-only, and every other package is whole, with all of its
  macros converted. The syntax types are ordinary types only in this mode
  (`ordinary_syntax_types`), and compile-time items are kept.
- **Templates.** `lower_quote::TemplateTable` holds each quotation's tokens and
  the `ModuleId` of the code that holds it, in lowering order. The harness
  installs it in a thread-local (`harness.rs:247`).
- **The call path in the parent.** `compiled_call::with_compiled_engine`
  (`compiled_call.rs:83`) installs a `CompiledEngine`. While one is installed,
  `call_compiled` (`:207`) builds the call's `ArgTree`s from the values that
  argument matching bound, makes the call, and reads the result by the declared
  category. It also renames the binders a quotation introduced and gives the
  result the parent's hygiene. Argument matching runs no user code: user `Parse`
  types are not macro parameter types (`Staple.md`, "Parsing syntax with `Parse`").
- **The wire format.** `macro_runtime/wire.rs` encodes `Message::Call`,
  `Message::Result` and `Message::Error` (`:152`) as one length-prefixed frame,
  `WIRE_VERSION` 1 (`:48`). Nothing has persisted it yet.
- **Stage order.** `macro_expand/staged.rs` makes a macro ready on demand:
  `ensure_ready` (`:94`) expands the macro's closure and the macros it calls,
  then `make_ready` (`:131`) lets it run. Cycles are reported there.
- **The object cache.** `staple-cli/src/cache.rs` fingerprints packages
  (`package_fingerprints`, `:183`) from the compiler identity, the target,
  sources, features and dependency fingerprints. It writes objects atomically
  (`ensure_object`, `:300`). The standard library's objects are prebuilt in
  `<stdlib>/target/<triple>` by `scripts/install-stdlib.sh`, and incremental
  builds link them (`package_objects`, `compile.rs:332`).
- **Effects.** `Staple.md` ("Compile-time effects") says macro code may not read
  files, read standard input, spawn processes or seed `std.rand` from the OS.
  Writing to standard error is allowed. Nothing enforces it.
- **The quotation cycle.** The closure scan doesn't count the calls a quotation
  lowers to (`__quote`, `Splice.splice`, `Parse.parse_quote`). Counting them gives
  `when` → `Splice` impl for `Expr` → `CharExpr` → `Char` → `len_utf8` →
  `when` (Phase 2, group 8). Two `typegroup ConversionError` chains in
  `std.convert` are recorded as generated-item loops but don't fail.
- **Measurements to beat.** The interpreter: B1 (load and expand the standard
  library) 0.28 s, B2 (20 × 50-field `@derive_eq`) 0.29 s, B3 (1,000 `if`) 0.71 s.
  A library build with cached std objects: 1.6–2.0 s, of which the front end is
  about 1.1 s. A pipelined server request: about 7 µs. An unoptimized compiled
  `if` call: about 0.5 ms.

## Decisions for this phase

1. **Library builds live in `staple-cli`, behind a trait the expander calls.**
   The expander runs inside `staple-compiler`, but the cache and the linker are in
   `staple-cli`. The compiler defines a `MacroBackend` trait with two methods.
   One makes the library for a set of macros available and returns their entry
   keys. The other makes a call. The trait replaces the test-only
   `CompiledEngine`, and the harness implements it too. The CLI implements it
   with the cache, the linker and a server client. The language server will
   implement the same trait in Phase 5.
2. **There are two kinds of library.**
   - A **closure library** holds the macros of the package being expanded. It
     is built when `make_ready` makes a macro ready, from that macro's closure
     as expanded so far. This is the stage tower: a later macro's closure may
     already contain items generated by an earlier closure library.
   - A **package library** holds every macro of a dependency. It is built once
     the dependency has been fully expanded, and only once per dependency
     fingerprint, not once per dependent.

   The standard library is a dependency of every other package, so dependents
   always load its package library. Its own expansion uses closure libraries,
   since `String` needs compiled `if` before `std` can be built at all.
3. **A package built for macro code is a different object.** In a check
   program, the syntax types are ordinary types, compile-time items are kept and
   macros become functions, so its code differs from the package's normal object.
   - Each package therefore gets a *macro-mode object*, cached beside its normal
     object under its own fingerprint.
   - A package library is the package's macro-mode object plus its
     dependencies' macro-mode objects, linked as a shared library.
   - A closure library compiles only its own package's closure items and links
     its dependencies' macro-mode objects. This is the cached-object path Phase
     0 measured at 1.6 s.
4. **Nothing per compilation goes into a library.** A cached library outlives
   the compilation that built it, but `ModuleId`s, syntax ids and marks are
   numbered per compilation.
   - Each library has a manifest beside it. The manifest lists:
     - each entry by a stable key: package name, module path, macro name and
       overload index;
     - the entry's shim symbol;
     - the templates, with modules named by package and path.
   - The parent maps names to its own `ModuleId`s when it loads the library.
   - Shim symbols are generated by the compiler (`__staple_entry.<n>`), not
     derived from `entry#name#id`.
   - Marks already come from the parent with each call (`CallContext`).
5. **One request at a time, with no batching.** The umbrella plan proposed
   batching requests by expansion round. That would mean restructuring the
   on-demand expander, which makes one call at a time. A request costs about
   7–10 µs (Phase 0), against about 0.5 ms for the cheapest compiled call. Revisit
   only if the measurements in task 12 say otherwise.
6. **The protocol doesn't use the server's standard streams.** Macro code may
   write to standard error, and anything it writes to standard output would
   corrupt a protocol on stdout.
   - The parent passes a socket pair (or two pipes) as file descriptor 3.
   - During each call, the server sends file descriptors 1 and 2 to a capture
     pipe.
   - A result or error frame carries what the call wrote. The parent copies
     it to its own standard error, labeled with the invocation.
   - A panic's message is that captured output, so no runtime change is needed
     to return it as data.
7. **One worker thread runs everything in a server.** That thread has a 256 MiB
   stack, as expansion has today (`macro_expand.rs:1786`). It runs each library's
   init and every call. The collector's stack bottom and the arena are per
   thread, so init and calls must not be split across threads. Deep but finite
   recursion then works as it does in the interpreter.
8. **Limits and their diagnostics.** The parent enforces the limits and reports
   each one at the invocation, naming the macro.

   | Case | Default | Diagnostic |
   |---|---|---|
   | Timeout | 10 s per call | ``macro `m` did not finish within 10 s`` |
   | `panic` | – | ``macro `m` panicked: <message>`` |
   | `compile_error` | – | the macro's message, as the interpreter reports it |
   | Stack overflow (SIGABRT or SIGSEGV with Rust's message) | 256 MiB | ``macro `m` overflowed its stack`` |
   | Other crash | – | ``macro `m` crashed (<signal>)`` |
   | Memory | none beyond the OS | – |

   - After a timeout or a crash, the parent kills the server and starts a new
     one. Libraries are reloaded when a call next needs them.
   - A trapped `exit` skips the abandoned frames' `Drop`s, which leaks anything
     held outside the collector. This is accepted: the effects that need such
     resources are forbidden (Decision 9).
   - A hidden environment variable, `STAPLE_MACRO_TIMEOUT`, overrides the
     timeout for tests and slow machines.
9. **Effects are enforced by reachability, not by a restricted runtime.** The
   umbrella plan proposed linking against a runtime without the forbidden
   modules. Package libraries contain all of a package's code, and lowering seeds
   every non-generic function, so the forbidden functions are present anyway.
   What matters is whether an entry function can reach them.
   - After lowering, walk the call graph from the entry functions. Count any
     function whose address is taken in reachable code.
   - Report each forbidden function reached, with the chain from the macro:
     ``macro `m` reaches `std.fs.read_file`, which macro code can't use``.
   - The forbidden set is a list of `std` functions and extern bindings, kept
     next to the effects rule in `Staple.md`.
   - A list of names can't cover foreign functions a package declares itself,
     so any reachable foreign function the standard library doesn't declare is
     also rejected (changed in the review of task 5; see Findings).
   - Report it when the library is built, not when a call reaches it.
10. **Optimize by measurement.** Phase 0 found that codegen runs no IR pass
    pipeline. Add one for libraries, and choose the level from task 11's
    measurements. Package libraries are built once per fingerprint, so they
    should be optimized unless that is unreasonably slow. Closure libraries are
    rebuilt after each edit, so a cheaper level may win there.
11. **Fingerprints.**
    - A macro-mode object is fingerprinted like a normal object, plus a
      macro-mode tag.
    - A closure library is fingerprinted from:
      - the expanded closure items' tokens, which excludes syntax ids and
        includes the results of earlier compiled calls;
      - the compiler identity, the host triple and the active features;
      - its dependencies' macro-mode fingerprints;
      - the optimization level.
    - Libraries and manifests live in the user cache root (`user_cache_root`,
      `cache.rs:153`), under `macros/`. The standard library's are prebuilt
      in `<stdlib>/target/<host>/macros`.
12. **The engine is chosen with a hidden flag.** `--macro-engine=interp|compiled`
    is added here, defaulting to `interp`, because this phase's own end-to-end
    tests need it. With `compiled`:
    - templates still expand in the parent;
    - every other macro call goes to the server;
    - no macro body or helper runs in the interpreter;
    - the interpreter's argument matching still builds the `ArgTree`s.

    Phase 5 compares the engines and changes the default.

## Tasks

Tasks 1 to 4 build the path from library to diagnostic. Task 5 enforces effects,
task 6 breaks the cycle the stage tower would hit, and tasks 7 to 9 build and
cache real libraries. Tasks 10 and 11 make them cheaper, and task 12 runs it all
end to end.

### 1. Library emission

- Port `LibrarySpec`, `__staple_library_init`, the entry shims, the host binding
  (`__staple_library_bind`, filling one slot per `INTRINSICS` entry) and the
  `exit` trap from `spike/macro-server`. The thunks and the trap are hidden
  external definitions.
- A shim calls `_setjmp`, calls its entry function with the closure environment
  the entry function expects, and returns the result handle or the trapped exit
  code.
- Shim symbols are `__staple_entry.<n>`, numbered by the build (Decision 4).
- Emit the template table and the entry list as the library's manifest
  (task 2), not as data inside the library.
- Link with `-shared` on Linux and `-dynamiclib -undefined dynamic_lookup` on
  macOS, as the spike did. A bound library must have no undefined
  `__staple_syntax_*` or `exit` symbols. Test that with `nm`, as Phase 0 did.
- Tests: build a library from a check program with one macro, `dlopen` it in a
  test, bind it to `INTRINSICS`, call the shim, and compare the result with the
  harness's. A `panic` and a `compile_error` both come back as an exit code.

### 2. Manifests and load-time mapping

- Write the manifest (Decision 4) next to each library. It holds a format
  version, the wire version, each entry's stable key and shim symbol, and the
  templates encoded as wire fragments, with modules named by package and path.
- In the parent, map the manifest's modules to `ModuleId`s and its keys to
  macro declarations when the library loads. A key that matches nothing is an
  internal error naming the library.
- The parent keeps the template table, since quotation filling runs in the
  server: the server gets the templates with the load request.
- Persisting the wire format makes `WIRE_VERSION` a real compatibility check.
  Add the load and handshake messages (task 3) as version 2.
- Tests: a library built in one compilation is loaded and called from a second
  compilation of the same source with different `ModuleId`s and syntax ids, and
  its quotations get the second compilation's hygiene.

### 3. The server

- `staple __macro-server` (hidden), in `staple-cli/src/macro_server.rs`. It calls
  into `staple_compiler::macro_runtime::server`, so the host table and the arena
  are the compiler's.
- Messages: `Hello` (protocol and wire version, and a checksum of the
  `INTRINSICS` names), `Load` (library path, manifest), `Call`, `Result`, `Error`
  and `Shutdown`. A version or checksum mismatch ends the handshake with an error
  the parent reports as an internal error.
- Use the protocol channel and output capture from Decision 6, and the worker
  thread from Decision 7.
- Load with `RTLD_NOW | RTLD_LOCAL`. Each library has its own copy of the runtime.
  Check that two loaded libraries keep separate collector state, with calls
  alternating between them. If they don't, record it and run one server per
  library instead. That only changes the client.
- `compile_error` records its message in the host and then exits (Phase 3,
  Decision 12). The error frame carries the recorded message.
- Tests (integration tests in `staple-cli`, using `CARGO_BIN_EXE_staple`):
  handshake, load, call, a version mismatch, two libraries in one server, a
  panic followed by a working call, output capture, and `compile_error`.

### 4. The client and failure handling

- `macro_runtime/client.rs`: start the server lazily on the first compiled call,
  keep it for the compiler invocation, and shut it down at the end.
- The server executable is the running `staple` (`std::env::current_exe`). Tests
  pass the binary explicitly.
- Enforce the limits in Decision 8. Recognize SIGABRT and SIGSEGV with Rust's
  overflow message as a stack overflow. Restart after a timeout or crash, and
  reload libraries lazily.
- Report every failure at the invocation, with the macro's name, as the
  interpreter's call failures are reported now. Forward captured output to
  standard error.
- Tests: each row of Decision 8's table, a restart followed by a working call,
  and debug output reaching standard error.

### 5. Effects

- Implement Decision 9's reachability walk on the lowered module of a library,
  from its entry functions.
- Write the forbidden list:
  - `std.fs`;
  - reading from `std.io` standard input;
  - process spawning in `std.process`;
  - OS seeding in `std.rand`.

  Read the modules for the exact functions and extern bindings, and write the
  list in Findings.
- Report each reached function once per macro, with the shortest chain.
- Tests: a macro that reads a file is rejected with its chain, a macro that only
  mentions such a function in dead code is accepted, and one that writes to
  standard error is accepted.

### 6. The quotation cycle and generated-item loops

- Make the closure scan count the calls a quotation lowers to, so closure
  libraries contain the syntax runtime their quotations need.
- Break the cycle this exposes (`when` → `Splice` impl for `Expr` → `CharExpr` →
  `Char` → `len_utf8` → `when`) in the source. Rewrite the `Char` functions the
  syntax runtime reaches with `match` and `loop`. Then follow every other cycle
  the scan now reports, the same way. If a cycle can only be broken by changing
  the language, stop and raise it.
- Decide whether the two `typegroup ConversionError` chains in `std.convert` need
  breaking: run the standard library's staged expansion with the compiled
  engine (task 7) and see whether they stop it. Record the answer in Findings.
- Tests: the closure of a quoting macro contains `__quote` and its `Splice` impls,
  and the standard library reports no cycles.

### 7. Closure libraries and the stage tower

- Implement `MacroBackend` in the CLI and install it when
  `--macro-engine=compiled` is given (Decision 12). Add the flag to `build`,
  `check`, `run`, `compile` and `expand`.
- When `make_ready` makes a macro of the package being expanded ready, build or
  find its closure library (Decision 2). Build the closure-only check program
  from the closure as expanded so far, with no interpreter expansion behind it.
  Then lower, emit, link and load it.
- Build the standard library from scratch with `--macro-engine=compiled`. Record
  how many closure libraries its own expansion needs, which macros they hold, and
  the total time.
  - If the count makes the bootstrap too slow, put every macro whose closure is
    already expanded into the same library, and record the change.
  - "Too slow" means more than twice the cost of the libraries actually needed.
- No macro body or helper may run in the interpreter on this path. Add an
  assertion that fails if one does.
- Tests: a user package defines a macro and uses it in the same module. A macro
  whose closure uses another macro of the same package expands after it. The
  standard library builds with the compiled engine.

### 8. Package libraries and macro-mode objects

- Build each dependency's macro-mode object (Decision 3) and its package library
  once the dependency is expanded, and cache both (task 9). The library contains
  every macro of the package, as `check_programs` converts them today.
- A dependent loads its dependencies' package libraries before it expands
  anything. Its closure libraries link the dependencies' macro-mode objects.
- `scripts/install-stdlib.sh` prebuilds the standard library's macro-mode
  object, package library and manifest into `<stdlib>/target/<host>/macros`,
  after its normal objects.
- Keep the compiled `Parse` impls ahead of the shared generic ones in every
  build, as the check module does. Appending them made a standard-library check
  take 104 s instead of 7 s. If a whole-package build loses that order, fix the
  ordering sensitivity in trait obligation resolution, as its own change, and
  record it.
- Tests: two dependents of one package share its library, a change to the
  dependency rebuilds it once, and a fresh install's prebuilt library is used
  without a build.

### 9. Caching

- Fingerprint objects and libraries as Decision 11 says, with `cache.rs`'s
  hasher, and write them atomically, as `ensure_object` does.
- A library whose manifest fails to read, or whose versions don't match, is
  rebuilt, not trusted.
- Concurrent compilers may build the same library. The atomic rename settles
  that, as it does for objects. Test it with two threads.
- Prune stale libraries as `prune_stale` prunes objects.
- Tests: a second build loads every library from the cache without building. A
  change to a macro's closure rebuilds that closure's library only. A change to
  the compiler identity, the triple, a feature or the optimization level gives a
  new fingerprint.

### 10. Build cost

- With cached macro-mode objects, a closure library should cost about what Phase
  0 measured (1.6–2.0 s), of which about 1.1 s is the front end checking the
  standard library. Measure it on the B-series inputs and on a user macro.
- Then try the two options Phase 0 named, time-boxed to two days in total, and
  keep whichever helps:
  - check only the closure's modules against the dependencies' checked
    interfaces;
  - cache the checked standard library.

  Record what each saved and why one was kept or neither was.

### 11. Optimization

- Add an IR pass pipeline for library builds (Decision 10).
- Measure build time and call time at `O0`, `O1` and `O2` for the standard
  library's package library and for a closure library, with the macros from
  Phase 3's call-cost table.
- Choose the levels and write them in Findings and in the code.

### 12. End to end

- Run the standard library, `examples/macros.sta`, the macro fixtures and the
  macro tests under `staple-compiler/tests` with `--macro-engine=compiled`. Compare
  each expansion with the interpreter's, using Phase 3's debug-tree comparison
  (`macro_runtime/compare.rs`), adapted to read from two real expansions.
- The four normalized differences Phase 3 found, and the differences listed
  under Phase 5 in the umbrella plan, are expected. Fix anything else, or list it
  in Findings with a reason. The full differential run over every test is Phase 5.
- Measure B1, B2 and B3 with the compiled engine, both with a cold cache and with
  a warm one. Also measure per-call cost for the macros in Phase 3's table, server
  start-up, and the time to restart after a crash. Compare with the interpreter.

### 13. Language reference

- In "Compile-time effects": the rule is now enforced, with what counts as
  reaching a function and the forbidden list.
- Document the limits and their diagnostics (Decision 8), and that macro output
  to standard error is shown.
- Document that macro code sees the host's layouts and pointer width, not the
  target's (umbrella Decision 8).
- Document where compiled macros are cached and that a reinstalled compiler
  rebuilds them.
- Don't document `--macro-engine`. It is hidden until Phase 5.

### 14. Close out

- Run the expansion comparison, the closure check and the workspace tests, and
  record the results.
- Update the umbrella plan: the Phase 4 status paragraph, and what Phase 5 and
  Phase 6 inherit.
- Record in Findings anything the language server will need: library rebuilds
  in the background, the lifetime of a server per session, and how a failed
  build leaves invocations unexpanded.

## Out of scope

- Making `compiled` the default, the full differential run and the language
  server (Phase 5).
- Enforcing syntax ownership in normal builds with the interpreter engine.
  It comes with the compiled engine as the default (Phase 5).
- Producing `ArgTree`s directly from argument matching, deleting
  `@interpreted_only` items, and removing the `ordinary_syntax_types` gates
  (Phase 6).
- Hosts other than macOS and Linux.
- An in-process mode for trusted libraries (umbrella Decision 3).
- Compile-time file reads such as `include_str` (umbrella open question 3).

## Done when

- `--macro-engine=compiled` expands the standard library from scratch with no
  macro body or helper run by the interpreter. Template macros expand in the
  parent, and every other call goes through a library in the server.
- The standard library, the macro example, the fixtures and the macro tests
  expand the same with both engines, apart from differences listed in Findings
  with a reason.
- A panic, `compile_error`, a timeout, a stack overflow and a crash each give a
  diagnostic at the invocation, and expansion continues with a restarted server
  where it can.
- A macro that reaches a forbidden function is rejected with its chain.
- A second build loads every library from the cache. A fresh standard-library
  install includes its prebuilt package library, and a dependency's library is
  built once per fingerprint.
- Library builds are optimized at the levels task 11 chose, and B1–B3 are
  measured with cold and warm caches.
- Interpreter output on the corpus is unchanged, apart from standard-library files
  edited for this phase, and the closure check is clean.

## Implementation status

Update this section as you work (see "Rules for the implementor").

| Task | Status | Commit | Notes |
|---|---|---|---|
| 1. Library emission | done | `bac82ed7` | `LibrarySpec`, `__staple_library_init`, `__staple_library_bind`, `exit` trap, `__staple_entry.<n>` shims, `--emit dylib`, `@compiled_only compile_error`, and `dlopen`/`nm` tests. Internal generic trapping stubs (e.g. `__parse_primitive`) are skipped when binding host symbols. Review fixes: `exit` is always defined when trapped, bind returns the first unresolved name, shared host symbols bind to their implemented entry. See Findings. |
| 2. Manifests and load-time mapping | done | `c10c1253` | `WIRE_VERSION` bumped to 2 with `encode_fragment`/`decode_fragment`; sidecar `.manifest` files (`manifest.rs`) store format/wire versions, `(package, path, macro, overload)` keys, shim symbols, and wire-encoded templates; `resolve` maps modules and macro declarations in the parent and reports missing keys as internal errors naming the library; cross-compilation hygiene test passes. Review fixes: keys come from the expanded program, templates drop compilation numbers. See Findings. |
| 3. The server | done | `5152a998` | `staple __macro-server` (`staple-cli/src/macro_server.rs`) + `macro_runtime::server` over fd 3 (`socketpair`), 256 MiB worker thread, `Hello`/`Load`/`Loaded`/`Call`/`Result`/`Error`/`Shutdown` wire messages, `INTRINSICS` FNV-1a checksum, fd 1/2 capture pipe, `RTLD_NOW | RTLD_LOCAL`, and `staple-cli/tests/macro_server.rs` integration tests. Review fixes: initialization traps `exit` and runs under output capture, loading a loaded image again doesn't rerun its initialization, one capture file per server. See Findings. |
| 4. The client and failure handling | done | `04eb738e` | `macro_runtime::client::MacroClient` with lazy server spawn/shutdown, lazy library reload, `STAPLE_MACRO_TIMEOUT` (default 10 s), stderr forwarding, and Decision 8 failure diagnostics (`panic`, `compile_error`, timeout, stack overflow, crash); unit and integration tests in `client.rs` and `staple-cli/tests/macro_server.rs`. Review fixes: libraries are registered with `MacroClient::load`, load failures and timeouts are reported at the macro, crash output is forwarded, the server's standard output is closed, `exit n` reads as an exit. See Findings. |
| 5. Effects | done | `c103ada8` | `LoweredModule::check_macro_effects` (`lower/macro_effects.rs`) walks lowered call graph (instances, taken callable values, artifacts, and extern targets) with BFS from entry functions; forbidden list documented in `Staple.md` and Findings; unit tests cover reachable file read, dead code, and stderr. Review fixes: foreign functions `std` doesn't declare are rejected, the list drops names `std` doesn't have, chains keep the function that calls a foreign function, `Staple.md` no longer claims enforcement. Not called yet: libraries are first built in tasks 7 and 8. See Findings. |
| 6. The quotation cycle and generated-item loops | done | `1d473565` | `Expression::Quote` in `macro_expand/scan.rs` now records `__quote`/`Parse.parse_quote`, `Sequence`, and `Splice`; `StdModules` carries `syntax`. Rewrote `if`/`when`/`for`/`while` to `match`/`loop` across `std.syntax` and all stdlib modules reached by `if`/syntax runtime; `check-macro-closures.py --mode all --jobs 2` passes (232 inputs, 0 diagnostics). `compare-expansion.py` against `88fdea78` matches 218/232 (only the 14 edited stdlib modules differ). Unit tests added in `closure.rs`. See Findings. |
| 7. Closure libraries and the stage tower | done | `caf7d865` | `CliMacroBackend` (`staple-cli/src/macro_backend.rs`) behind `--macro-engine=compiled` on `build`, `check`, `run`, `compile` and `expand`; `make_ready` builds one closure library per macro from a closure-only check program. The standard library bootstraps with 3 closure libraries (`if`, `when`, `typegroup`) in about 33 s. Review fixes: the effects check runs (it was passed its arguments swapped), closure programs register generated `companion` blocks, libraries are built for the host, no compiler panics on strict-mode fallbacks, doc comments before a modifier are read in streams, item sequences break lines before modifiers and `let`, the interpreter's static check no longer gates macros with libraries. See Findings. |
| 8. Package libraries and macro-mode objects | done | `18d6147a`, `5e712235`, `62cdacbb` | `MacroBackend::begin`/`available`/`prebuild`; `prepare_package_library` lowers a whole-package macro-mode check program, emitted as a plain package object; package libraries link it with their dependencies' macro-mode objects, a shim-only library object and the runtime. Closure libraries now emit only the expanding package and link dependency objects. Macro caches live under `<package>/target/staple/<host>/macros` and `$XDG_CACHE_HOME/staple/macros`; `install-stdlib.sh` prebuilds into `<stdlib>/target/<host>/macros`. Manifest module keys are package-relative paths (format 2). Tests are `#[ignore]`d and run by `scripts/test-macro-libraries.sh` in release. See Findings. |
| 9. Caching | not started | | |
| 10. Build cost | not started | | |
| 11. Optimization | not started | | |
| 12. End to end | not started | | |
| 13. Language reference | not started | | |
| 14. Close out | not started | | |

## Measurements

Task 8, macOS 27 on an Apple M2, `aarch64-apple-darwin`, release `staple`,
2026-11-24, commit `18d6147a`:

- Cold standard-library package library (from a trivial user package's
  `staple check --macro-engine=compiled`, `STAPLE_MACRO_STATS=1`): 8 closure
  libraries in 76 s, the `std` package library in 142 s; 146 s total. A warm
  cache takes 2.2 s.
- Prebuilding the standard library into a copy's
  `target/<host>/macros` from the warm user cache: 37 s, 0 package libraries
  built (the three std closure libraries for the copy's own expansion).
- A `first`/`second`/`macros` package graph, cold: `macros` in 146 s after
  `std`; changing `macros` rebuilds it once, then `first` and `second`
  (146 s for all three). A warm check takes 1.7 s.
- A user package run from a prebuilt standard library, fresh cache: 0
  package libraries, one closure library (`keep`) in 10 s, 13 s total.

For each one, record what was measured, the result, the machine, the
commit and the command. Include:

- the standard library's bootstrap: library count and total time (task 7);
- closure-library build time before and after task 10;
- build and call time per optimization level (task 11);
- B1–B3 with cold and warm caches, per-call cost, server start-up and restart
  time (task 12).

## Findings

- **Two libraries can share a server (task 3).** Loading two compiled macro
  libraries with `RTLD_NOW | RTLD_LOCAL` into one `staple __macro-server`
  process and alternating calls between them keeps their runtime and collector
  state separate on macOS (verified in `staple-cli/tests/macro_server.rs`).
- **Forbidden compile-time effects list (task 5).**
  - `std.fs` I/O operations (`metadata`, `symlink_metadata`, `exists`, `read`,
    `read_to_string`, `write`, `write_string`, `append_string`, `copy`,
    `create_dir`, `create_dir_all`, `remove_file`, `remove_dir`,
    `remove_dir_all`, `rename`, `canonicalize`, `current_dir`, `read_dir`,
    `File.*`, `DirEntry.metadata`) and externs (`__staple_fs_errno`,
    `__staple_fs_errno_value`, `__staple_fs_open`, `__staple_fs_stat`,
    `__staple_fs_fstat`, `__staple_fs_dir_open`, `__staple_fs_dir_next`,
    `__staple_fs_dir_close`, `__staple_fs_rename`, `fopen`, `fclose`, `fread`,
    `fwrite`, `fseeko`, `ftello`, `fileno`, `fsync`, `mkdir`, `rmdir`, `unlink`,
    `realpath`, `getcwd`).
  - `std.io` standard input reading (`read_line`, `Stdin.read_byte`,
    `__staple_io_read_byte`).
  - `std.process` has no process spawning, so it adds nothing. Spawning
    needs a foreign function the standard library doesn't declare, and those
    are all rejected. (The first version listed `spawn`, `Command.*` and C
    names, none of which `std` has.)
  - `std.rand` OS seeding (`fill_entropy`, `Rng.from_entropy`,
    `HashKey.from_entropy`, `getentropy`).

Record:

- whether two libraries can share a server (task 3);
- the forbidden list (task 5);
- the cycles broken and the answer on the generated-item loops (task 6);
- whether the bootstrap batched its libraries (task 7);
- whether the `Parse` impl order held (task 8);
- which build-cost option was kept (task 10);
- the chosen optimization levels (task 11);
- every difference between the engines with its reason (task 12);
- anything that changes Phases 5 and 6.

### Review of tasks 1 and 2

- **Manifest keys come from the expanded program.** `build_manifest` counted
  overloads in the program it was given, and the test gave it the check
  program. A closure-only check program, which closure libraries use, declares
  no macros at all, so the key was missing and the build failed. Keys are now
  counted in the expanded program, which `resolve` also reads. `CheckRun` keeps
  that program as `expanded`. A test builds a manifest from a closure-only run
  where only the second overload is invoked.
- **`exit` is defined whenever it is trapped.** The trap only gave `exit` a body
  when the library's own module declared it. A closure library whose code never
  names `exit` would have left `panic` in a linked standard-library object
  calling libc's `exit`, ending the server.
- **Binding reports a missing host function.** `__staple_library_bind` now
  returns the name of the first symbol the resolver has no address for, or null.
  Before, it stored a null pointer and the first call crashed. The test resolver
  returns null instead of panicking across `extern "C"`, and a test checks the
  reported name.
- **Templates drop compilation numbers.** A manifest template no longer keeps
  its fragment's module, mark or token hygiene. Filling a quotation overwrote
  them anyway.
- **Shared host symbols.** `__staple_syntax_compile_error`, `_is_empty` and
  `_error_at` each have an interpreter-only stub entry and a compiled entry. The
  JIT and `resolve_intrinsic` picked the compiled one only because it came
  later in `INTRINSICS`. `binding_for` now picks the implemented entry, and a
  test checks that each symbol has at most one implementation.
- **Slow tests.** The two library tests take about three minutes each and are
  `#[ignore]`d. `scripts/test-macro-libraries.sh` runs them.
- **Warnings.** The manifest code and the fragment encoders have no non-test
  callers yet, so they allow dead code outside tests.

### Review of tasks 3 to 5

- **A `panic` during initialization.** `__staple_library_init` runs the
  library's top-level code, but only the entry shims set the `exit` trap's
  jump buffer, so a `panic` there jumped to an unset buffer and crashed the
  server. Initialization now sets the buffer and returns 1 when it exited. The
  server runs it under output capture and replies with an exit error, which
  the client reports at the macro being called, as it does a call's. A test
  builds a library whose setup panics in a value the interpreter never
  evaluates.
- **Loading is part of the call.** A load that timed out or crashed was
  reported as an internal read error with no macro named. It now gets the
  same timeout and crash diagnostics as a call. Its timeout is at least the
  default 10 s, since mapping a large library can take longer than a short
  test timeout.
- **Libraries are registered per compilation.** The client kept libraries by
  path and sent their templates only on the first load. Templates carry the
  module numbers of one compilation, so a long-lived client, such as the
  language server's, would have filled quotations with stale modules.
  `MacroClient::load` now registers a library with its templates and returns
  a handle. Each compilation registers its libraries again.
- **The same image loaded twice.** Loading a path already loaded gives back
  the image in the process, and the server then bound and initialized it a
  second time. It now shares the image under a new handle without either.
  A library file rewritten in place would still give the old image, so tasks
  7 and 9 must write libraries to new paths named by their fingerprint and
  never rewrite one.
- **Foreign functions.** A macro's code runs with no effects, so it can't call
  the `std` functions that need `IO`, which every file, standard-input and
  entropy function does. Foreign functions need no effect, so they were the
  real way out, and a list of names only covered `std`'s own: any package can
  declare `extern "c" { open: ... }`. Any reachable foreign function `std`
  doesn't declare is now rejected. The forbidden names are still checked, as
  a second line for `std`'s own bindings.
- **Chains through a foreign call.** The chain printer replaced the function
  that calls a foreign function with the foreign function's name, so
  `m -> helper -> open` read as `m -> open`. The old test expected the short
  chain.
- **The forbidden list and `Staple.md`.** The list named `std.process.spawn`,
  `Command.*` and C spawning functions that `std` doesn't have, and
  `Staple.md` documented them. `Staple.md` also said the check runs when a
  library is built, but nothing calls it until tasks 7 and 8. It now
  describes the rule, says the interpreter doesn't check it, and notes that
  `panic` and `compile_error` are how a macro writes to standard error, since
  `eprintln` needs `IO`.
- **Smaller fixes.**
  - `exit 3` read as ``macro `m` panicked: macro exited with status 3``; it is
    now ``macro `m` exited with status 3``.
  - The server's standard error after a crash, which holds what the macro
    wrote, is forwarded without the runtime's overflow message.
  - The server's standard output is closed, since all macro output is
    captured.
  - One unlinked capture file per server, truncated per call, instead of
    one created and deleted per call.
  - The client writes forwarded output to a sink, standard error by default,
    so tests can check it.
- **Tests added.** Output from a call that succeeds reaches the sink with
  the macro's label, `exit 3`, a panic during initialization, and two
  handles on one library. Macros can't write to standard error without
  `panic`, so the successful-output test uses a C binding to `putchar`.

### Task 6: The quotation cycle and generated-item loops

- **Quotation scanning (`scan.rs`, `macro_expand.rs`, `resolve.rs`).**
  `StdModules` now holds `syntax: Option<ModuleId>` for `std.syntax`.
  `Scanner::expression`'s `Expression::Quote` arm now calls `self.standard`
  for `__quote` (`QuoteKind::Quote`) or `Parse` (`QuoteKind::ParseQuote`),
  `Sequence`, and (when the quotation has splices) `Splice`.
- **Cycles broken in the standard library.** Rewriting `if`/`when`/`for`/`while`
  to `match` and `loop` in `std.syntax` and every item reached by `if` or the
  syntax runtime eliminated all macro-closure cycles across:
  - `stdlib/std/syntax.sta` (`Parse.parse_all`, `Parenthesized`, `Bracketed`,
    `Angled`, `Braced`, `(H, ...T)`, `Sequence<T>`, `Separated<T, Comma>`)
  - `stdlib/std/convert.sta` (`checked_conversion`)
  - `stdlib/std/string.sta` (`from_utf8`, `impl Ord String`,
    `impl TryConvert String Bool`)
  - `stdlib/std/char.sta` (`from_u32`, `len_utf8`, `is_whitespace`,
    `is_ascii_*`, `to_ascii_*`, `encode_utf8`, `write_hex`, `impl Debug Char`)
  - `stdlib/std/number.sta` (`Divide`, `Remainder`, `ShiftLeft`, `ShiftRight`,
    `PartialOrd`, `Ord`, `TryConvert <Int> Bool`)
  - `stdlib/std/boolean.sta` (`Display Bool`, `Not Bool`, `Convert Bool <Int>`)
  - `stdlib/std/cmp.sta` (`Eq.ne`)
  - `stdlib/std/shared.sta` (`Shared.new`)
  - `stdlib/std/alloc.sta` (`MemLayout.array`, `MemLayout.extend`)
  - `stdlib/std/shared_buffer.sta` (`SharedBuffer`, `SharedBufferIter`,
    `Eq SharedBuffer<T>`, `Hash SharedBuffer<T>`)
  - `stdlib/std/buffer.sta` (`push`, `into_shared_buffer`, `transfer`,
    `get_unchecked`, `clone_at`, `Eq Buffer<T>`, `Hash Buffer<T>`,
    `append_raw_bytes`)
  - `stdlib/std/list.sta` (`grow`, `reserve`, `push`)
  - `stdlib/std/cinterop.sta` (`from_string_unchecked`, `has_interior_nul`,
    `TryConvert String CString`, `TryConvert CString String`)
  - `stdlib/std/string_builder.sta` (`StringBuilder.push`)
- **Deferred to Task 7 (`typegroup ConversionError` in `std.convert`).**
  Whether the two `typegroup ConversionError` chains in `std.convert` need
  breaking waits for Task 7, when the standard library is expanded using the
  compiled engine.
- **Verification.**
  - `scripts/check-macro-closures.py --mode all --jobs 2`: `232 inputs, 0 with
    diagnostics, 0 distinct messages`.
  - `scripts/compare-expansion.py` against `88fdea78`: `218/232 matched`. The
    only 14 differing captures are the 14 edited `stdlib/*.sta` files listed
    above, where `if`/`when`/`while` were rewritten to `match` and `loop`; all
    corpus programs, examples, compiler test fixtures, and unedited stdlib
    modules expanded identically.
  - Added `quoting_macro_closure_includes_quote_runtime_and_splice_impls` and
    `standard_library_has_no_macro_closure_cycles` in
    `staple-compiler/src/macro_expand/closure.rs` (`11 passed; 0 failed; 1 ignored`).

### Task 7: Closure libraries and the stage tower

- **Standard-library compiled bootstrap.** Building `stdlib` from scratch with
  `--macro-engine=compiled` (`STAPLE_MACRO_STATS=1 /usr/bin/time -p`) built
  **3 closure libraries** in `32.854s` (`lib #0: if`, `lib #1: when`,
  `lib #2: typegroup`) and completed in `43.60s` real (`37.30s` user, `1.65s`
  sys). Because stdlib's own expansion only invokes those 3 non-template macros,
  no extra batching of already-expanded macros was needed.
- **`std.convert` `ConversionError` cycle.** `std.string`/`std.number` in `if`'s
  closure depend on `ConversionError`, which previously used `typegroup`. Rewrote
  `ConversionError` in `stdlib/std/convert.sta` by hand as `companion ConversionError`
  + `pub type ConversionError from ...` and removed `use package.typegroup.(typegroup)`.
- **Closure-only check programs (`whole_dependencies = false`).** Running the
  real stage tower in `MacroExpander::make_ready` exposed several gaps in
  closure-only check programs and the compiled macro runtime:
  - `CheckSnapshot.modules` must be `Vec::new()` so `Submodule` and
    `UseDeclaration` fall back to `shell`; `ClosureIndex::build` must index
    companion blocks on `namespace == true` modules; `keep_import` must check
    `defined[target]` for `UseKind::Dotted` single-item imports; and
    `expand_in_walk` must record per-binding expansions for multi-binding
    `Item::ExternBlock`s.
  - `prepare_compiled_macro` settles `compiled_only_items` closures before
    building `groups`, expands `Item::Modified` (`@syntax_node`), filters
    `@interpreted_only`, and splices `@compiled_only` items (`Parse` and
    `Splice` impls first); `resolve.rs` and `typecheck.rs` tolerate partial
    stdlib in `ordinary_syntax_types` programs.
  - `NameResolver::resolve_on_thread` transfers the `ENGINE` `MacroBackend` into
    the 256 MiB worker thread and restores it on `.join()`.
  - `codegen/lowered/mod.rs` uses the argument pattern's `test.subject` when
    unboxing `@syntax_node` wrappers (instead of the outer `Wrapper.representation`,
    which can be `Opaque` in recursive syntax types); `macro_expand.rs`
    flattens substituted `Type::Sum` alternatives in `substitute_type` so
    `typegroup_sum` matches between interpreted and compiled.
  - `wire.rs` (`Message::Load` template `ModuleId` decoding), `intrinsics.rs`
    (`sequence_syntax` newline separator for item sequences and doc-comment
    modifier fallback), `compiled_call.rs` (`AliasBody`/`WrapperBody`/`FromBody`/`OpaqueBody`
    in `arg_tree`), and `staple-syntax/src/parser.rs` (`parse_modifier_or_doc`
    nested `if` avoiding an `opt-level = 3` let-chain miscompilation on
    `rustc 1.96.0`).
- **Verification.**
  - `cargo test -p staple --test compiled_macros -- --test-threads=1`: `3 passed; 0 failed`
    (`user_package_macro_used_in_same_module`, `user_package_macro_closure_uses_earlier_macro`,
    `stdlib_builds_with_compiled_engine`).
  - `cargo test -p staple-compiler --lib macro_expand::closure::tests:: -- --test-threads=2`:
    `11 passed; 0 failed; 1 ignored`.
  - `cargo test -p staple-compiler --lib macro_runtime::compare::tests:: -- --test-threads=1`:
    `3 passed; 0 failed`.
  - `scripts/check-macro-closures.py --staple target/release/staple --mode all --jobs 2`:
    `232 inputs, 0 with diagnostics, 0 distinct messages`.

### Review of tasks 6 and 7

- **The effects check never ran.** `CliMacroBackend::prepare` passed
  `(entry_symbol, macro_name, span)` where `check_macro_effects` takes
  `(macro_name, entry_symbol, span)`. It looked up each macro's name as a
  symbol, found nothing and skipped the macro silently, so every library
  passed. Entries are now a `MacroEffectEntry` struct with named fields, and an
  entry symbol the lowered program lacks is an internal error. Chains also no
  longer show the macro's own mangled function (`operator.6d61…`). A test
  checks that a macro reaching its own `extern "c"` binding is rejected under
  the compiled engine.
- **Closure programs dropped generated modules.** A closure check program
  takes its module structure from the program as it was before expansion, so
  the `companion` block `typegroup` writes was only an item and its types
  (`Shape.Circle`) didn't resolve. This, not an ordering cycle alone, is what
  any user `typegroup` and `std.fs`'s and `std.rand`'s typegroups hit.
  `Program::register_generated_inline_modules` now registers such blocks and
  groups only them; grouping every block again detached the blocks whose types
  the closure left out. The hand-written `ConversionError` stays: `if`'s
  closure needs it before `typegroup` can run. A test builds a macro whose
  closure uses a user `typegroup`.
- **Libraries are built for the host.** The backend passed `--target`,
  `--linker`, `-L` and `-l` to codegen and the linker, so cross builds made
  libraries the compiler couldn't load. It now uses the host triple and `CC`
  (or `cc`). Foreign libraries a package links aren't needed: macro code can't
  reach a foreign function the standard library doesn't declare.
- **Diagnostics from library builds.** `MacroBackend::prepare` returns
  `Vec<Diagnostic>` instead of one string, so resolution and type errors in a
  closure keep their own spans; one without a span is reported at the call.
  A macro whose library failed isn't checked again by the interpreter.
- **No panics in strict mode.** The `panic!`s for `call_trees`, `call_compiled`
  and `arg_tree` returning `None`, and the `assert!`s on falling back to the
  interpreter, were debugging aids reachable from user code. A call compiled
  code can't take is now reported as ``internal error: macro `m` could not be
  run as compiled code``, and no body runs in the interpreter.
- **The interpreter's static check.** `check_before_running` checked every
  macro body with the interpreter's checker before each call, under the
  compiled engine too, so the compiled engine rejected what the interpreter
  can't model (`typegroup` values, foreign calls), after building the library.
  A macro with a library is now taken as checked by its library's type check.
  A macro that is never called is still checked by the interpreter's checker,
  until task 8's package libraries check every macro.
- **Doc comments in modifier streams.** `parse_leaf_intrinsic` re-parsed the
  whole fragment from its start when a modifier failed at a later offset. The
  cause was in the parser: reading one modifier from a stream skipped a doc
  comment before it as trivia, so `/// doc` then `@a` gave only `@a`, and the
  trailing newline then failed to parse. `parse_modifier_prefix` now reads a
  doc comment before the next modifier as its own `doc` modifier, and the
  fallback is gone. Still open, under both engines: a doc comment after the
  last modifier, or a stream holding only doc comments, reads as empty,
  because `is_empty` counts comments as trivia.
- **Item separators.** `sequence_syntax` starts a new line before a doc
  comment, a modifier (`@`), `let`, `pub`, `def` or a declaration keyword, and
  a space otherwise. An item written as a bare macro call by name still gets a
  space; deciding by the sequence's element type would need a new intrinsic
  argument.
- **Compiled-only items.** Every closure library re-expanded every
  `@compiled_only` item. They are now expanded once per compilation and
  reused. Every library still carries all of them, since nearly every closure
  needs the syntax runtime; task 11 measures whether trimming them is worth it.
- **Smaller fixes.** The `MIN / -1` and `% -1` comments deleted in task 6 are
  back in `number.sta`. A wire test covers a template module number larger
  than the bytes left in the frame. The stage-tower test checks that
  `double_plus_two` gets a library and that `add_one`, a quotation-only
  template the parent expands, doesn't.
- **Left as is.**
  - `resolve.rs` skips the "standard library does not declare" errors for
    every check program, not only closure-only ones. A normal compile still
    reports a missing declaration, so nothing goes unchecked.
  - Task 6's `match` rewrites of functions that aren't on a cycle stay.
    Finding which can go back to `if` needs a closure check per function.

### Task 8: Package libraries and macro-mode objects

- **What a package library is.** `prepare_package_library` expands a package
  with `CheckMode::All` and builds its whole-package macro-mode check program:
  every item of the package is kept, every macro becomes a function (and an
  entry when its parameters are typed), and dependencies are included whole
  for resolution and checking. The object is emitted with
  `EmissionUnit::packages([package])`, like a normal package object, and
  cached as `<name>-macro-<fingerprint>.o`. The package library links that
  object, its dependencies' macro-mode objects, one **shim-only** object
  emitted with `EmissionUnit::packages([]).with_library(spec)`, and the runtime
  object. The shim-only object carries `__staple_library_init`, the entry
  shims, host binding, the `exit` trap, and calls every module initializer by
  symbol; that keeps macro-mode objects shareable without duplicate library
  symbols. A dependency's package library is built once, before any dependent
  expands (`MacroBackend::begin`), and its entries are mapped to the current
  program's declarations; `MacroBackend::available` then tells the expander to
  skip a closure for those macros.
- **Closure libraries changed shape.** A closure library now emits only the
  expanding package (`EmissionUnit::packages([root])`) from its closure-only
  check program and links its dependencies' macro-mode objects and the runtime.
  The closure check still uses `whole_dependencies = false`, so it has every
  closure item it needs; code generation declares the dependencies' exported
  functions and emits internal copies of their generic and local ones. This is
  what makes the "closure libraries link the dependencies' macro-mode objects"
  decision work without duplicate definitions.
- **Manifest module keys are package-relative paths.** A standard-library
  library built with the standard library as its root named modules by
  absolute file path, because its root modules were loaded by path; the same
  package loaded as a dependency named them by dotted name. A dependent then
  failed with ``references unknown module `std:/.../std/cmp.sta` ``. `module_key`
  now uses the path relative to the package's source root (package name plus
  `std/cmp.sta`), plus a generated module's qualified-name suffix so
  `companion` blocks that share their file's path stay distinct. This is
  stable in both builds; `MANIFEST_FORMAT_VERSION` is 2 so older manifests are
  rebuilt.
- **Check programs expand helper and macro bodies.** A macro provided by a
  package library never computes a closure, so its package's compile-time
  helpers are left as written by normal expansion; the whole-package check
  program then resolved raw `if` calls inside `std.cmp`/`std.hash` helpers and
  failed with ``macro `if` must be invoked``. A package-library expansion
  (`CheckMode::PackageLibrary`, distinct from the closure check's `All`)
  expands every compile-time helper body and macro declaration body before
  assembling the check snapshot (`expand_compile_time_helper`,
  `expand_macro_declaration_for_check`), and those items replace the raw
  `CheckSnapshot::compile_time` entries. This is also what lets a package
  library check a package whose macros were never invoked. The closure check's
  `All` mode is unchanged, so `check-macro-closures.py` does not pay for it.
- **`Parse` implementation order held.** Whole-package check programs are built
  from `CheckSnapshot::modules`, which `expand_program_checked` fills with the
  compiled `Parse` implementations before the shared items, as the check module
  already did. No ordering regression appeared; the cost is the whole
  dependency type check, not obligation order (see below).
- **Cache layout and prebuild.** Macro-mode objects, libraries and manifests
  are read from `<package>/target/staple/<host>/macros`, the installed
  standard library's `<stdlib>/target/<host>/macros`, and
  `$XDG_CACHE_HOME/staple/macros`, in that order; non-std dependencies write
  the user cache. `staple build --macro-engine=compiled` on a library root
  emits and writes its own package library, and copies it into its own macro
  directory when it came from the user cache. `scripts/install-stdlib.sh` runs
  a second `staple build --macro-engine=compiled` after the normal objects, so
  a fresh install contains the macro-mode object, package library and manifest.
  `ensure_library` renames the library and its sidecar together; a manifest
  that does not read or resolve is removed and rebuilt.
- **Cost.** A cold standard library package library costs about 142 s in
  release, of which 76 s is the eight closure libraries for the standard
  library's own expansion. Each user package library re-expands and
  type-checks the whole standard library dependency; a three-package graph
  with one macro costs about 146 s after the standard library. This is the
  task 10 target; the measurements above are the baseline.
- **Fingerprints are provisional.** A macro-mode object is named by the normal
  package fingerprint plus a `macro-mode` tag, and its dependencies' normal
  package fingerprints. Task 9 replaces those with macro-mode dependency
  fingerprints, adds the optimization level, and covers pruning, version
  checks and concurrent builds; the atomic object and library renames here are
  the base it builds on.
- **Slow tests.** The three task 8 tests build package libraries and are
  `#[ignore]`d. `scripts/test-macro-libraries.sh` runs them with
  `cargo test --release`, because the debug `staple` type-checks the whole
  standard library for each package library (about 50 minutes in an
  unoptimized build, against about 2.5 minutes optimized). The compiled-macro
  integration tests share one standard-library cache per test binary through
  `shared_cache`, so the standard library is built once there rather than once
  per test.
- **Prebuilt standard library check.** Running a user package with a fresh
  cache against a copy whose `target/<host>/macros` came from the shared cache
  builds no package libraries; the `if` calls go to the installed `std`
  library and only the user macro gets a closure library.

