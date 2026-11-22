//! Out-of-process macro execution server (`staple __macro-server`).
//!
//! Reads framed [`wire::Message`] requests from file descriptor 3, runs all
//! library initialization and entry calls on one 256 MiB-stack worker thread
//! (so the collector's stack bottom and the thread-local arena stay on one
//! thread), and captures standard output and standard error around each call.

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::io::{Read, Write};
use std::sync::{Arc, mpsc};

use super::arena;
use super::args;
use super::intrinsics::{self, clear_compile_error, resolve_intrinsic, take_compile_error};
use super::quote::{self, CallContext};
use super::wire::{
    ErrorKind, FrameReadError, Message, PROTOCOL_VERSION, WIRE_VERSION, WireError, read_frame,
    write_frame,
};
use crate::ModuleId;
use crate::macro_expand::lower_quote::{Template, TemplateTable};
use staple_syntax::Syntax;

const WORKER_STACK_SIZE: usize = 256 * 1024 * 1024;

/// Runs the macro server over file descriptor 3 until `Shutdown` or EOF.
pub fn run() -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::fd::FromRawFd;
        use std::os::unix::net::UnixStream;

        // Fd 3 is inherited from the parent as a socketpair end (or pipe).
        let mut stream = unsafe { UnixStream::from_raw_fd(3) };
        serve_stream(&mut stream)
    }
    #[cfg(not(unix))]
    {
        Err("macro server is only supported on Unix".to_owned())
    }
}

/// Serves one protocol stream until handshake failure, `Shutdown`, or EOF.
pub(crate) fn serve_stream(stream: &mut (impl Read + Write)) -> Result<(), String> {
    if !handshake(stream)? {
        return Ok(());
    }

    let (request_tx, request_rx) = mpsc::sync_channel::<WorkerRequest>(0);
    let worker = std::thread::Builder::new()
        .name("macro-server-worker".to_owned())
        .stack_size(WORKER_STACK_SIZE)
        .spawn(move || worker_loop(request_rx))
        .map_err(|error| format!("could not spawn macro worker thread: {error}"))?;

    let loop_result = (|| -> Result<(), String> {
        loop {
            let message = match read_frame(stream) {
                Ok(Some(message)) => message,
                Ok(None) => return Ok(()),
                Err(error) => {
                    let _ = write_frame(
                        stream,
                        &Message::Error {
                            kind: ErrorKind::Internal,
                            message: format!("protocol read error: {error}"),
                            output: Vec::new(),
                        },
                    );
                    return Err(format!("protocol read error: {error}"));
                }
            };
            match message {
                Message::Shutdown => return Ok(()),
                Message::Load { path, templates } => {
                    let (reply_tx, reply_rx) = mpsc::sync_channel(1);
                    request_tx
                        .send(WorkerRequest::Load {
                            path,
                            templates,
                            reply: reply_tx,
                        })
                        .map_err(|_| "macro worker thread exited unexpectedly".to_owned())?;
                    let reply = reply_rx
                        .recv()
                        .map_err(|_| "macro worker thread exited unexpectedly".to_owned())?;
                    write_frame(stream, &reply)
                        .map_err(|error| format!("protocol write error: {error}"))?;
                }
                Message::Call {
                    library,
                    entry,
                    context,
                    arguments,
                } => {
                    let (reply_tx, reply_rx) = mpsc::sync_channel(1);
                    request_tx
                        .send(WorkerRequest::Call {
                            library,
                            entry,
                            context,
                            arguments,
                            reply: reply_tx,
                        })
                        .map_err(|_| "macro worker thread exited unexpectedly".to_owned())?;
                    let reply = reply_rx
                        .recv()
                        .map_err(|_| "macro worker thread exited unexpectedly".to_owned())?;
                    write_frame(stream, &reply)
                        .map_err(|error| format!("protocol write error: {error}"))?;
                }
                other => {
                    let message = format!("unexpected message in macro server: {other:?}");
                    let _ = write_frame(
                        stream,
                        &Message::Error {
                            kind: ErrorKind::Internal,
                            message: message.clone(),
                            output: Vec::new(),
                        },
                    );
                    return Err(message);
                }
            }
        }
    })();

    drop(request_tx);
    let _ = worker.join();
    loop_result
}

fn handshake(stream: &mut (impl Read + Write)) -> Result<bool, String> {
    let expected_checksum = intrinsics::intrinsics_checksum();
    let first = match read_frame(stream) {
        Ok(Some(message)) => message,
        Ok(None) => return Ok(false),
        Err(FrameReadError::Wire(WireError::Version(version))) => {
            let message = format!(
                "wire version mismatch: client sent {version}, server expects {WIRE_VERSION}"
            );
            let _ = write_frame(
                stream,
                &Message::Error {
                    kind: ErrorKind::Internal,
                    message: message.clone(),
                    output: Vec::new(),
                },
            );
            return Err(message);
        }
        Err(error) => {
            let message = format!("handshake read failed: {error}");
            let _ = write_frame(
                stream,
                &Message::Error {
                    kind: ErrorKind::Internal,
                    message: message.clone(),
                    output: Vec::new(),
                },
            );
            return Err(message);
        }
    };

    let Message::Hello {
        protocol_version,
        wire_version,
        intrinsics_checksum,
    } = first
    else {
        let message = "expected `Hello` as the first message".to_owned();
        let _ = write_frame(
            stream,
            &Message::Error {
                kind: ErrorKind::Internal,
                message: message.clone(),
                output: Vec::new(),
            },
        );
        return Err(message);
    };

    if protocol_version != PROTOCOL_VERSION {
        let message = format!(
            "protocol version mismatch: client sent {protocol_version}, server expects {PROTOCOL_VERSION}"
        );
        let _ = write_frame(
            stream,
            &Message::Error {
                kind: ErrorKind::Internal,
                message: message.clone(),
                output: Vec::new(),
            },
        );
        return Err(message);
    }
    if wire_version != WIRE_VERSION {
        let message = format!(
            "wire version mismatch: client sent {wire_version}, server expects {WIRE_VERSION}"
        );
        let _ = write_frame(
            stream,
            &Message::Error {
                kind: ErrorKind::Internal,
                message: message.clone(),
                output: Vec::new(),
            },
        );
        return Err(message);
    }
    if intrinsics_checksum != expected_checksum {
        let message = format!(
            "intrinsics checksum mismatch: client sent {intrinsics_checksum:#x}, server expects {expected_checksum:#x}"
        );
        let _ = write_frame(
            stream,
            &Message::Error {
                kind: ErrorKind::Internal,
                message: message.clone(),
                output: Vec::new(),
            },
        );
        return Err(message);
    }

    write_frame(
        stream,
        &Message::Hello {
            protocol_version: PROTOCOL_VERSION,
            wire_version: WIRE_VERSION,
            intrinsics_checksum: expected_checksum,
        },
    )
    .map_err(|error| format!("handshake write failed: {error}"))?;
    Ok(true)
}

enum WorkerRequest {
    Load {
        path: String,
        templates: Vec<(usize, Syntax)>,
        reply: mpsc::SyncSender<Message>,
    },
    Call {
        library: u32,
        entry: String,
        context: CallContext,
        arguments: Vec<super::args::ArgTree>,
        reply: mpsc::SyncSender<Message>,
    },
}

fn worker_loop(requests: mpsc::Receiver<WorkerRequest>) {
    let mut libraries: Vec<LoadedLibrary> = Vec::new();
    while let Ok(request) = requests.recv() {
        match request {
            WorkerRequest::Load {
                path,
                templates,
                reply,
            } => {
                let response = match LoadedLibrary::load(&path, templates) {
                    Ok(library) => {
                        let id = u32::try_from(libraries.len())
                            .expect("loaded library count fits in u32");
                        libraries.push(library);
                        Message::Loaded { library: id }
                    }
                    Err(message) => Message::Error {
                        kind: ErrorKind::Internal,
                        message,
                        output: Vec::new(),
                    },
                };
                let _ = reply.send(response);
            }
            WorkerRequest::Call {
                library,
                entry,
                context,
                arguments,
                reply,
            } => {
                let response = match libraries.get(library as usize) {
                    Some(loaded) => loaded.call(&entry, context, &arguments),
                    None => Message::Error {
                        kind: ErrorKind::Internal,
                        message: format!("unknown library handle {library}"),
                        output: Vec::new(),
                    },
                };
                let _ = reply.send(response);
            }
        }
    }
}

#[cfg(unix)]
mod dynamic {
    use super::*;

    unsafe extern "C" {
        fn dlopen(path: *const c_char, mode: c_int) -> *mut c_void;
        fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
        fn dlclose(handle: *mut c_void) -> c_int;
        fn dlerror() -> *const c_char;
        fn pipe(fds: *mut c_int) -> c_int;
        fn dup(fd: c_int) -> c_int;
        fn dup2(oldfd: c_int, newfd: c_int) -> c_int;
        fn close(fd: c_int) -> c_int;
        fn fflush(stream: *mut c_void) -> c_int;
    }

    const RTLD_NOW: c_int = 2;
    #[cfg(target_os = "macos")]
    const RTLD_LOCAL: c_int = 4;
    #[cfg(not(target_os = "macos"))]
    const RTLD_LOCAL: c_int = 0;

    extern "C" fn resolve_host_symbol(name: *const c_char) -> *const c_void {
        let name = unsafe { CStr::from_ptr(name) };
        let Ok(name) = name.to_str() else {
            return std::ptr::null();
        };
        resolve_intrinsic(name).unwrap_or(std::ptr::null())
    }

    pub(super) struct LoadedLibrary {
        path: String,
        handle: *mut c_void,
        exit_code: Option<*const i32>,
        templates: Arc<TemplateTable>,
    }

    impl LoadedLibrary {
        pub(super) fn load(path: &str, templates: Vec<(usize, Syntax)>) -> Result<Self, String> {
            let spelled = CString::new(path).map_err(|error| error.to_string())?;
            let handle = unsafe { dlopen(spelled.as_ptr(), RTLD_NOW | RTLD_LOCAL) };
            if handle.is_null() {
                return Err(format!("could not load `{path}`: {}", last_dlerror()));
            }
            let table = TemplateTable {
                templates: templates
                    .into_iter()
                    .map(|(module, contents)| Template {
                        contents,
                        module: ModuleId(module),
                    })
                    .collect(),
            };
            let mut library = Self {
                path: path.to_owned(),
                handle,
                exit_code: None,
                templates: Arc::new(table),
            };
            library.exit_code = library
                .symbol("__staple_library_exit_code")
                .map(|ptr| ptr as *const i32);
            if let Some(bind) = library.symbol("__staple_library_bind") {
                let bind: unsafe extern "C" fn(
                    extern "C" fn(*const c_char) -> *const c_void,
                ) -> *const c_char = unsafe { std::mem::transmute(bind) };
                let missing = unsafe { bind(resolve_host_symbol) };
                if !missing.is_null() {
                    let name = unsafe { CStr::from_ptr(missing) }.to_string_lossy();
                    return Err(format!(
                        "macro library `{path}` requires unknown host symbol `{name}`"
                    ));
                }
            }
            let init = library
                .symbol("__staple_library_init")
                .ok_or_else(|| format!("macro library `{path}` has no `__staple_library_init`"))?;
            let init: unsafe extern "C" fn() = unsafe { std::mem::transmute(init) };
            unsafe { init() };
            Ok(library)
        }

        fn symbol(&self, name: &str) -> Option<*mut c_void> {
            let spelled = CString::new(name).ok()?;
            let symbol = unsafe { dlsym(self.handle, spelled.as_ptr()) };
            (!symbol.is_null()).then_some(symbol)
        }

        pub(super) fn call(
            &self,
            entry: &str,
            context: CallContext,
            arguments: &[args::ArgTree],
        ) -> Message {
            let Some(symbol) = self.symbol(entry) else {
                return Message::Error {
                    kind: ErrorKind::Internal,
                    message: format!("macro library `{}` has no entry `{entry}`", self.path),
                    output: Vec::new(),
                };
            };
            let shim: unsafe extern "C" fn(u64) -> u64 = unsafe { std::mem::transmute(symbol) };

            quote::install_templates(self.templates.clone());
            clear_compile_error();
            quote::begin_call(context);
            let arg_handle = args::insert_arguments(arguments);

            let (result_handle, output) = capture_output(|| unsafe { shim(arg_handle) });

            if result_handle == u64::MAX {
                arena::reset();
                if let Some(message) = take_compile_error() {
                    return Message::Error {
                        kind: ErrorKind::CompileError,
                        message,
                        output,
                    };
                }
                let code = self
                    .exit_code
                    .map(|ptr| unsafe { *ptr })
                    .unwrap_or(1);
                return Message::Error {
                    kind: ErrorKind::Exit,
                    message: format!("macro exited with status {code}"),
                    output,
                };
            }

            let syntax = arena::with_arena(|arena| arena.syntax(result_handle.into()).cloned());
            arena::reset();
            let Some(syntax) = syntax else {
                return Message::Error {
                    kind: ErrorKind::Internal,
                    message: format!(
                        "macro entry `{entry}` in `{}` did not return a syntax handle",
                        self.path
                    ),
                    output,
                };
            };
            Message::Result {
                syntax,
                next_mark: quote::next_mark(),
                output,
            }
        }
    }

    impl Drop for LoadedLibrary {
        fn drop(&mut self) {
            unsafe {
                dlclose(self.handle);
            }
        }
    }

    fn last_dlerror() -> String {
        let message = unsafe { dlerror() };
        if message.is_null() {
            "unknown dlerror".to_owned()
        } else {
            unsafe { CStr::from_ptr(message) }
                .to_string_lossy()
                .into_owned()
        }
    }

    pub(super) fn capture_output<T>(f: impl FnOnce() -> T) -> (T, Vec<u8>) {
        use std::os::fd::FromRawFd;

        let mut pipe_fds = [0 as c_int; 2];
        if unsafe { pipe(pipe_fds.as_mut_ptr()) } != 0 {
            return (f(), Vec::new());
        }
        let read_end = pipe_fds[0];
        let write_end = pipe_fds[1];
        let saved_stdout = unsafe { dup(1) };
        let saved_stderr = unsafe { dup(2) };
        if saved_stdout < 0 || saved_stderr < 0 {
            unsafe {
                close(read_end);
                close(write_end);
                if saved_stdout >= 0 {
                    close(saved_stdout);
                }
                if saved_stderr >= 0 {
                    close(saved_stderr);
                }
            }
            return (f(), Vec::new());
        }

        unsafe {
            dup2(write_end, 1);
            dup2(write_end, 2);
            close(write_end);
        }

        let reader = std::thread::spawn(move || {
            let mut file = unsafe { std::fs::File::from_raw_fd(read_end) };
            let mut buffer = Vec::new();
            let _ = file.read_to_end(&mut buffer);
            buffer
        });

        let result = f();

        let _ = std::io::stdout().flush();
        let _ = std::io::stderr().flush();
        unsafe {
            fflush(std::ptr::null_mut());
            dup2(saved_stdout, 1);
            dup2(saved_stderr, 2);
            close(saved_stdout);
            close(saved_stderr);
        }

        let output = reader.join().unwrap_or_default();
        (result, output)
    }
}

#[cfg(not(unix))]
mod dynamic {
    use super::*;

    pub(super) struct LoadedLibrary;

    impl LoadedLibrary {
        pub(super) fn load(_path: &str, _templates: Vec<(usize, Syntax)>) -> Result<Self, String> {
            Err("macro server is only supported on Unix".to_owned())
        }

        pub(super) fn call(
            &self,
            _entry: &str,
            _context: CallContext,
            _arguments: &[args::ArgTree],
        ) -> Message {
            Message::Error {
                kind: ErrorKind::Internal,
                message: "macro server is only supported on Unix".to_owned(),
                output: Vec::new(),
            }
        }
    }
}

use dynamic::LoadedLibrary;

#[cfg(all(test, unix))]
mod tests {
    use std::ffi::{c_int, c_void};

    unsafe extern "C" {
        fn write(fd: c_int, buf: *const c_void, count: usize) -> isize;
    }

    #[test]
    fn capture_output_collects_stdout_and_stderr() {
        let (value, output) = super::dynamic::capture_output(|| {
            let out = b"captured stdout\n";
            let err = b"captured stderr\n";
            unsafe {
                write(1, out.as_ptr().cast(), out.len());
                write(2, err.as_ptr().cast(), err.len());
            }
            42
        });
        assert_eq!(value, 42);
        let text = String::from_utf8_lossy(&output);
        assert!(text.contains("captured stdout"), "{text:?}");
        assert!(text.contains("captured stderr"), "{text:?}");
    }
}
