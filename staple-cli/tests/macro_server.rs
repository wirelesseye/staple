#[cfg(unix)]
mod unix_tests {
    use std::ffi::c_int;
    use std::os::fd::{FromRawFd, OwnedFd};
    use std::os::unix::net::UnixStream;
    use std::os::unix::process::CommandExt;
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command};
    use std::sync::OnceLock;

    use staple_compiler::macro_runtime::intrinsics::intrinsics_checksum;
    use staple_compiler::macro_runtime::quote::CallContext;
    use staple_compiler::macro_runtime::wire::{
        ErrorKind, Message, PROTOCOL_VERSION, WIRE_VERSION, encode, read_frame, write_frame,
    };
    use staple_compiler::macro_runtime::{TestMacroCall, prepare_macro_library_object};
    use staple_syntax::Syntax;

    const AF_UNIX: c_int = 1;
    const SOCK_STREAM: c_int = 1;
    const F_SETFD: c_int = 2;

    unsafe extern "C" {
        fn socketpair(domain: c_int, ty: c_int, protocol: c_int, sv: *mut c_int) -> c_int;
        fn dup2(oldfd: c_int, newfd: c_int) -> c_int;
        fn close(fd: c_int) -> c_int;
        fn fcntl(fd: c_int, cmd: c_int, ...) -> c_int;
    }

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "staple-server-test-{label}-{}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("tempdir should be created");
            Self(path)
        }
    }

    struct BuiltLibrary {
        path: PathBuf,
        templates: Vec<(usize, Syntax)>,
        calls: Vec<TestMacroCall>,
    }

    struct Fixture {
        _dir: TempDir,
        lib_a: BuiltLibrary,
        lib_b: BuiltLibrary,
    }

    fn workspace_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("staple-cli is in workspace")
            .to_path_buf()
    }

    fn link_shared_library(object: &Path, output: &Path) {
        let linker = std::env::var_os("CC").unwrap_or_else(|| "cc".into());
        let mut command = Command::new(&linker);
        command.arg(object).arg("-o").arg(output);
        if cfg!(target_os = "macos") {
            command.args(["-dynamiclib", "-undefined", "dynamic_lookup"]);
        } else {
            command.args(["-shared", "-lm"]);
        }
        let result = command.output().expect("linker should run");
        assert!(
            result.status.success(),
            "linker failed:\n{}",
            String::from_utf8_lossy(&result.stderr)
        );
    }

    fn build_library(dir: &Path, name: &str, source: &str, macro_names: &[&str]) -> BuiltLibrary {
        let root = workspace_root();
        let stdlib = root.join("stdlib");
        let object = dir.join(format!("{name}.o"));
        let ext = if cfg!(target_os = "macos") { "dylib" } else { "so" };
        let lib = dir.join(format!("lib{name}.{ext}"));
        let (templates, calls) =
            prepare_macro_library_object(source, &stdlib, &root, macro_names, &object)
                .expect("test macro library should build");
        link_shared_library(&object, &lib);
        BuiltLibrary {
            path: lib,
            templates,
            calls,
        }
    }

    fn fixture() -> &'static Fixture {
        static FIXTURE: OnceLock<Fixture> = OnceLock::new();
        FIXTURE.get_or_init(|| {
            let dir = TempDir::new("fixture");
            let source_a = concat!(
                "use std.process.panic\n",
                "use std.syntax.*\n",
                "macro twice: Expr -> Expr = value => parse_quote { $value + $value }\n",
                "macro boom: Expr -> Expr = _value => panic \"macro exploded\"\n",
                "macro bad: Expr -> Expr = _value => compile_error \"bad macro input\"\n",
                "let a = twice (1 + 2)\n",
            );
            let source_b = concat!(
                "use std.syntax.*\n",
                "macro wrap: Expr -> Expr = value => {\n",
                "    let name = Ident \"wrapped\"\n",
                "    parse_quote { { let $name = $value; $name } }\n",
                "}\n",
                "let b = wrap (3 + 4)\n",
            );
            let lib_a = build_library(
                &dir.0,
                "a",
                source_a,
                &["twice", "boom", "bad"],
            );
            let lib_b = build_library(&dir.0, "b", source_b, &["wrap"]);
            Fixture {
                _dir: dir,
                lib_a,
                lib_b,
            }
        })
    }

    struct ServerProcess {
        child: Child,
        stream: UnixStream,
    }

    impl ServerProcess {
        fn spawn() -> Self {
            let mut fds = [0 as c_int; 2];
            assert_eq!(
                unsafe { socketpair(AF_UNIX, SOCK_STREAM, 0, fds.as_mut_ptr()) },
                0,
                "socketpair should succeed"
            );
            let parent_fd = unsafe { OwnedFd::from_raw_fd(fds[0]) };
            let child_fd = fds[1];

            let mut command = Command::new(env!("CARGO_BIN_EXE_staple"));
            command.arg("__macro-server");
            unsafe {
                command.pre_exec(move || {
                    if child_fd != 3 {
                        if dup2(child_fd, 3) < 0 {
                            return Err(std::io::Error::last_os_error());
                        }
                        close(child_fd);
                    }
                    if fcntl(3, F_SETFD, 0) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            let child = command.spawn().expect("macro server should spawn");
            unsafe {
                close(child_fd);
            }
            let stream = UnixStream::from(parent_fd);
            Self { child, stream }
        }

        fn handshake(&mut self) {
            write_frame(
                &mut self.stream,
                &Message::Hello {
                    protocol_version: PROTOCOL_VERSION,
                    wire_version: WIRE_VERSION,
                    intrinsics_checksum: intrinsics_checksum(),
                },
            )
            .expect("hello should write");
            let reply = read_frame(&mut self.stream)
                .expect("hello reply should decode")
                .expect("hello reply should arrive");
            match reply {
                Message::Hello {
                    protocol_version,
                    wire_version,
                    intrinsics_checksum: checksum,
                } => {
                    assert_eq!(protocol_version, PROTOCOL_VERSION);
                    assert_eq!(wire_version, WIRE_VERSION);
                    assert_eq!(checksum, intrinsics_checksum());
                }
                other => panic!("expected Hello reply, got {other:?}"),
            }
        }

        fn load(&mut self, lib: &BuiltLibrary) -> u32 {
            write_frame(
                &mut self.stream,
                &Message::Load {
                    path: lib.path.to_string_lossy().into_owned(),
                    templates: lib.templates.clone(),
                },
            )
            .expect("load should write");
            match read_frame(&mut self.stream)
                .expect("load reply should decode")
                .expect("load reply should arrive")
            {
                Message::Loaded { library } => library,
                other => panic!("expected Loaded, got {other:?}"),
            }
        }

        fn call(&mut self, library: u32, info: &TestMacroCall, mark: u64) -> Message {
            write_frame(
                &mut self.stream,
                &Message::Call {
                    library,
                    entry: info.entry_shim.clone(),
                    context: CallContext {
                        definition_module: info.definition_module,
                        next_mark: mark,
                        call_site: info.call_site.clone(),
                        identifier_mark: None,
                    },
                    arguments: info.arguments.clone(),
                },
            )
            .expect("call should write");
            read_frame(&mut self.stream)
                .expect("call reply should decode")
                .expect("call reply should arrive")
        }

        fn shutdown(mut self) {
            let _ = write_frame(&mut self.stream, &Message::Shutdown);
            let status = self.child.wait().expect("server should exit");
            assert!(status.success(), "server exited with {status}");
        }
    }

    #[test]
    fn server_handshake_load_call_output_panic_compile_error_and_two_libraries() {
        let fix = fixture();
        let mut server = ServerProcess::spawn();
        server.handshake();

        let lib_a = server.load(&fix.lib_a);
        let lib_b = server.load(&fix.lib_b);

        // 1. Normal call on library A.
        let Message::Result {
            syntax,
            next_mark,
            output,
        } = server.call(lib_a, &fix.lib_a.calls[0], 1_000_000)
        else {
            panic!("expected Result from twice");
        };
        assert_eq!(syntax.text().trim(), "(1 + 2) + (1 + 2)");
        assert!(next_mark > 1_000_000);
        assert!(output.is_empty(), "unexpected output: {output:?}");

        // 2. Panic captures stderr in `output` and returns ErrorKind::Exit, and server stays usable.
        let Message::Error {
            kind,
            output,
            ..
        } = server.call(lib_a, &fix.lib_a.calls[1], 1_000_020)
        else {
            panic!("expected Error from boom");
        };
        assert_eq!(kind, ErrorKind::Exit);
        let panic_output = String::from_utf8_lossy(&output);
        assert!(
            panic_output.contains("panic: macro exploded"),
            "missing panic output: {panic_output:?}"
        );

        // Working call right after panic.
        let Message::Result { syntax, .. } = server.call(lib_a, &fix.lib_a.calls[0], 1_000_030)
        else {
            panic!("expected Result after panic");
        };
        assert_eq!(syntax.text().trim(), "(1 + 2) + (1 + 2)");

        // 3. compile_error returns ErrorKind::CompileError with its recorded message.
        let Message::Error {
            kind,
            message,
            ..
        } = server.call(lib_a, &fix.lib_a.calls[2], 1_000_040)
        else {
            panic!("expected Error from bad");
        };
        assert_eq!(kind, ErrorKind::CompileError);
        assert_eq!(message, "bad macro input");

        // 4. Alternating calls between two libraries in one server keep separate state.
        for round in 0..4 {
            let mark_a = 2_000_000 + round * 10;
            let Message::Result { syntax: a_syn, .. } =
                server.call(lib_a, &fix.lib_a.calls[0], mark_a)
            else {
                panic!("expected Result from lib_a round {round}");
            };
            assert_eq!(a_syn.text().trim(), "(1 + 2) + (1 + 2)");

            let mark_b = 3_000_000 + round * 10;
            let Message::Result { syntax: b_syn, .. } =
                server.call(lib_b, &fix.lib_b.calls[0], mark_b)
            else {
                panic!("expected Result from lib_b round {round}");
            };
            assert_eq!(b_syn.text().trim(), "{ let wrapped = (3 + 4); wrapped }");
        }

        server.shutdown();
    }

    #[test]
    fn server_rejects_version_and_checksum_mismatches() {
        // 1. Wire version mismatch (patched frame header).
        {
            let mut server = ServerProcess::spawn();
            let mut frame = encode(&Message::Hello {
                protocol_version: PROTOCOL_VERSION,
                wire_version: WIRE_VERSION,
                intrinsics_checksum: intrinsics_checksum(),
            });
            frame[4..8].copy_from_slice(&(WIRE_VERSION + 1).to_le_bytes());
            use std::io::Write;
            server.stream.write_all(&frame).expect("frame writes");
            server.stream.flush().expect("frame flushes");
            let reply = read_frame(&mut server.stream)
                .expect("error frame decodes")
                .expect("error frame arrives");
            assert!(
                matches!(
                    reply,
                    Message::Error {
                        kind: ErrorKind::Internal,
                        ref message,
                        ..
                    } if message.contains("wire version")
                ),
                "unexpected reply: {reply:?}"
            );
            let status = server.child.wait().expect("server exits");
            assert!(!status.success());
        }

        // 2. Protocol version mismatch.
        {
            let mut server = ServerProcess::spawn();
            write_frame(
                &mut server.stream,
                &Message::Hello {
                    protocol_version: PROTOCOL_VERSION + 1,
                    wire_version: WIRE_VERSION,
                    intrinsics_checksum: intrinsics_checksum(),
                },
            )
            .expect("hello writes");
            let reply = read_frame(&mut server.stream)
                .expect("error frame decodes")
                .expect("error frame arrives");
            assert!(
                matches!(
                    reply,
                    Message::Error {
                        kind: ErrorKind::Internal,
                        ref message,
                        ..
                    } if message.contains("protocol version")
                ),
                "unexpected reply: {reply:?}"
            );
            let status = server.child.wait().expect("server exits");
            assert!(!status.success());
        }

        // 3. Intrinsics checksum mismatch.
        {
            let mut server = ServerProcess::spawn();
            write_frame(
                &mut server.stream,
                &Message::Hello {
                    protocol_version: PROTOCOL_VERSION,
                    wire_version: WIRE_VERSION,
                    intrinsics_checksum: intrinsics_checksum() ^ 1,
                },
            )
            .expect("hello writes");
            let reply = read_frame(&mut server.stream)
                .expect("error frame decodes")
                .expect("error frame arrives");
            assert!(
                matches!(
                    reply,
                    Message::Error {
                        kind: ErrorKind::Internal,
                        ref message,
                        ..
                    } if message.contains("checksum")
                ),
                "unexpected reply: {reply:?}"
            );
            let status = server.child.wait().expect("server exits");
            assert!(!status.success());
        }
    }
}
