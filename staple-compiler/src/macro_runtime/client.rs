//! Parent-side client for `staple __macro-server`.
//!
//! Starts the server lazily on the first compiled macro call, keeps it alive
//! for the compiler invocation, enforces per-call timeouts, forwards captured
//! output to standard error, and restarts the server after a timeout or crash
//! (reloading libraries lazily).

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use staple_syntax::Syntax;

use super::args::ArgTree;
use super::intrinsics::intrinsics_checksum;
use super::quote::CallContext;
use super::wire::{
    ErrorKind, FrameReadError, Message, PROTOCOL_VERSION, WIRE_VERSION, read_frame, write_frame,
};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

/// Client for a lazily started `staple __macro-server` child process.
pub struct MacroClient {
    server_binary: PathBuf,
    timeout: Duration,
    #[cfg(unix)]
    process: Option<unix::RunningServer>,
    /// Server-assigned library handles for paths already loaded into the
    /// current server process. Cleared whenever the server restarts.
    loaded: HashMap<PathBuf, u32>,
}

impl MacroClient {
    /// Creates a client that spawns the currently running `staple` executable
    /// (`std::env::current_exe`) and reads `STAPLE_MACRO_TIMEOUT` if set.
    pub fn new() -> Result<Self, String> {
        let binary = std::env::current_exe()
            .map_err(|error| format!("internal error: could not locate `staple` binary: {error}"))?;
        Ok(Self::with_binary(binary))
    }

    /// Creates a client that spawns `server_binary` for `staple __macro-server`.
    pub fn with_binary(server_binary: PathBuf) -> Self {
        let timeout = timeout_from_env().unwrap_or(DEFAULT_TIMEOUT);
        Self {
            server_binary,
            timeout,
            #[cfg(unix)]
            process: None,
            loaded: HashMap::new(),
        }
    }

    /// Overrides the per-call timeout.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Runs one macro call in the server, starting the server and loading
    /// `library_path` lazily if needed.
    #[allow(clippy::too_many_arguments)]
    pub fn call(
        &mut self,
        library_path: &Path,
        templates: &[(usize, Syntax)],
        entry: &str,
        macro_name: &str,
        context: CallContext,
        arguments: Vec<ArgTree>,
    ) -> Result<(Syntax, u64), String> {
        #[cfg(unix)]
        {
            self.call_unix(
                library_path,
                templates,
                entry,
                macro_name,
                context,
                arguments,
            )
        }
        #[cfg(not(unix))]
        {
            let _ = (
                library_path,
                templates,
                entry,
                macro_name,
                context,
                arguments,
            );
            Err("compiled macro server is only supported on Unix".to_owned())
        }
    }

    /// Shuts down the running server process, if any.
    pub fn shutdown(&mut self) {
        #[cfg(unix)]
        if let Some(process) = self.process.take() {
            self.loaded.clear();
            process.shutdown();
        }
    }
}

impl Drop for MacroClient {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Parses `STAPLE_MACRO_TIMEOUT` (e.g. `"10"`, `"0.5"`, `"5s"`, `"200ms"`).
fn timeout_from_env() -> Option<Duration> {
    let raw = std::env::var("STAPLE_MACRO_TIMEOUT").ok()?;
    parse_timeout(&raw)
}

fn parse_timeout(raw: &str) -> Option<Duration> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some(ms) = trimmed.strip_suffix("ms") {
        let value = ms.trim().parse::<u64>().ok()?;
        return (value > 0).then(|| Duration::from_millis(value));
    }
    let seconds_text = trimmed.strip_suffix('s').unwrap_or(trimmed).trim();
    let seconds = seconds_text.parse::<f64>().ok()?;
    if !seconds.is_finite() || seconds <= 0.0 {
        return None;
    }
    Some(Duration::from_secs_f64(seconds))
}

fn format_timeout(timeout: Duration) -> String {
    if timeout.subsec_nanos() == 0 {
        format!("{} s", timeout.as_secs())
    } else {
        format!("{} ms", timeout.as_millis().max(1))
    }
}

fn forward_output(macro_name: &str, output: &[u8]) {
    if output.is_empty() {
        return;
    }
    let text = String::from_utf8_lossy(output);
    let mut stderr = std::io::stderr().lock();
    for line in text.lines() {
        let _ = writeln!(stderr, "macro `{macro_name}`: {line}");
    }
    let _ = stderr.flush();
}

/// Splits captured output into an extracted panic message (if a `"panic: ..."`
/// line is present) and any remaining output lines to forward to stderr.
fn split_panic_output(output: &[u8]) -> (Option<String>, Vec<u8>) {
    if output.is_empty() {
        return (None, Vec::new());
    }
    let text = String::from_utf8_lossy(output);
    let mut panic_message = None;
    let mut other_lines = Vec::new();
    for line in text.lines() {
        if panic_message.is_none()
            && let Some(rest) = line.strip_prefix("panic: ")
        {
            panic_message = Some(rest.to_owned());
        } else {
            other_lines.push(line);
        }
    }
    let remaining = if other_lines.is_empty() {
        Vec::new()
    } else {
        let mut joined = other_lines.join("\n");
        joined.push('\n');
        joined.into_bytes()
    };
    (panic_message, remaining)
}

#[cfg(unix)]
mod unix {
    use std::ffi::c_int;
    use std::io::Read;
    use std::os::fd::{FromRawFd, OwnedFd};
    use std::os::unix::net::UnixStream;
    use std::os::unix::process::{CommandExt, ExitStatusExt};
    use std::process::{Child, Command, Stdio};
    use std::sync::{Arc, Mutex};
    use std::thread::JoinHandle;

    use super::*;

    const AF_UNIX: c_int = 1;
    const SOCK_STREAM: c_int = 1;
    const F_SETFD: c_int = 2;

    const SIGABRT: c_int = 6;
    const SIGSEGV: c_int = 11;

    unsafe extern "C" {
        fn socketpair(domain: c_int, ty: c_int, protocol: c_int, sv: *mut c_int) -> c_int;
        fn dup2(oldfd: c_int, newfd: c_int) -> c_int;
        fn close(fd: c_int) -> c_int;
        fn fcntl(fd: c_int, cmd: c_int, ...) -> c_int;
    }

    pub(super) struct RunningServer {
        child: Child,
        stream: UnixStream,
        stderr: Arc<Mutex<Vec<u8>>>,
        stderr_thread: Option<JoinHandle<()>>,
    }

    impl RunningServer {
        fn spawn(binary: &Path, timeout: Duration) -> Result<Self, String> {
            let mut fds = [0 as c_int; 2];
            if unsafe { socketpair(AF_UNIX, SOCK_STREAM, 0, fds.as_mut_ptr()) } != 0 {
                return Err(format!(
                    "internal error: could not create macro server socket pair: {}",
                    std::io::Error::last_os_error()
                ));
            }
            let parent_fd = unsafe { OwnedFd::from_raw_fd(fds[0]) };
            let child_fd = fds[1];

            let mut command = Command::new(binary);
            command.arg("__macro-server").stderr(Stdio::piped());
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
            let mut child = command.spawn().map_err(|error| {
                unsafe {
                    close(child_fd);
                }
                format!(
                    "internal error: could not start `{}` __macro-server: {error}",
                    binary.display()
                )
            })?;
            unsafe {
                close(child_fd);
            }

            let stderr_buffer = Arc::new(Mutex::new(Vec::new()));
            let stderr_thread = child.stderr.take().map(|mut pipe| {
                let buffer = stderr_buffer.clone();
                std::thread::spawn(move || {
                    let mut chunk = [0u8; 4096];
                    loop {
                        match pipe.read(&mut chunk) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                if let Ok(mut guard) = buffer.lock() {
                                    guard.extend_from_slice(&chunk[..n]);
                                }
                            }
                        }
                    }
                })
            });

            let stream = UnixStream::from(parent_fd);
            let _ = stream.set_read_timeout(Some(DEFAULT_TIMEOUT.max(timeout)));
            let _ = stream.set_write_timeout(Some(DEFAULT_TIMEOUT.max(timeout)));

            let mut server = Self {
                child,
                stream,
                stderr: stderr_buffer,
                stderr_thread,
            };
            server.handshake()?;
            Ok(server)
        }

        fn handshake(&mut self) -> Result<(), String> {
            let expected_checksum = intrinsics_checksum();
            write_frame(
                &mut self.stream,
                &Message::Hello {
                    protocol_version: PROTOCOL_VERSION,
                    wire_version: WIRE_VERSION,
                    intrinsics_checksum: expected_checksum,
                },
            )
            .map_err(|error| format!("internal error: macro server handshake write failed: {error}"))?;

            let reply = read_frame(&mut self.stream).map_err(|error| {
                format!("internal error: macro server handshake read failed: {error}")
            })?;
            match reply {
                Some(Message::Hello {
                    protocol_version,
                    wire_version,
                    intrinsics_checksum,
                }) => {
                    if protocol_version != PROTOCOL_VERSION {
                        return Err(format!(
                            "internal error: macro server protocol version {protocol_version} does not match {PROTOCOL_VERSION}"
                        ));
                    }
                    if wire_version != WIRE_VERSION {
                        return Err(format!(
                            "internal error: macro server wire version {wire_version} does not match {WIRE_VERSION}"
                        ));
                    }
                    if intrinsics_checksum != expected_checksum {
                        return Err(format!(
                            "internal error: macro server intrinsics checksum {intrinsics_checksum:#x} does not match {expected_checksum:#x}"
                        ));
                    }
                    Ok(())
                }
                Some(Message::Error { message, .. }) => {
                    Err(format!("internal error: macro server handshake failed: {message}"))
                }
                Some(other) => Err(format!(
                    "internal error: unexpected macro server handshake reply: {other:?}"
                )),
                None => Err("internal error: macro server closed during handshake".to_owned()),
            }
        }

        pub(super) fn shutdown(mut self) {
            let _ = write_frame(&mut self.stream, &Message::Shutdown);
            drop(self.stream);
            if self.child.wait().is_err() {
                let _ = self.child.kill();
                let _ = self.child.wait();
            }
            if let Some(thread) = self.stderr_thread.take() {
                let _ = thread.join();
            }
        }

        fn kill_and_collect_stderr(mut self) -> (Option<std::process::ExitStatus>, Vec<u8>) {
            let _ = self.child.kill();
            let status = self.child.wait().ok();
            drop(self.stream);
            if let Some(thread) = self.stderr_thread.take() {
                let _ = thread.join();
            }
            let stderr = self
                .stderr
                .lock()
                .map(|guard| guard.clone())
                .unwrap_or_default();
            (status, stderr)
        }

        fn wait_and_collect_stderr(mut self) -> (Option<std::process::ExitStatus>, Vec<u8>) {
            drop(self.stream);
            let status = self.child.wait().ok();
            if let Some(thread) = self.stderr_thread.take() {
                let _ = thread.join();
            }
            let stderr = self
                .stderr
                .lock()
                .map(|guard| guard.clone())
                .unwrap_or_default();
            (status, stderr)
        }
    }

    impl MacroClient {
        fn ensure_server(&mut self) -> Result<&mut RunningServer, String> {
            if self.process.is_none() {
                let server = RunningServer::spawn(&self.server_binary, self.timeout)?;
                self.process = Some(server);
                self.loaded.clear();
            }
            Ok(self.process.as_mut().expect("server just started"))
        }

        fn load_library(
            &mut self,
            library_path: &Path,
            templates: &[(usize, Syntax)],
        ) -> Result<u32, String> {
            if let Some(&handle) = self.loaded.get(library_path)
                && self.process.is_some()
            {
                return Ok(handle);
            }
            let path = library_path.to_string_lossy().into_owned();
            let load_timeout = DEFAULT_TIMEOUT.max(self.timeout);
            let server = self.ensure_server()?;
            let _ = server.stream.set_read_timeout(Some(load_timeout));
            write_frame(
                &mut server.stream,
                &Message::Load {
                    path,
                    templates: templates.to_vec(),
                },
            )
            .map_err(|error| format!("internal error: could not send Load to macro server: {error}"))?;

            let reply = match read_frame(&mut server.stream) {
                Ok(reply) => reply,
                Err(error) => {
                    let server = self.process.take().expect("server active");
                    self.loaded.clear();
                    let _ = server.kill_and_collect_stderr();
                    return Err(format!(
                        "internal error: could not read Load reply from macro server: {error}"
                    ));
                }
            };
            match reply {
                Some(Message::Loaded { library }) => {
                    self.loaded.insert(library_path.to_path_buf(), library);
                    Ok(library)
                }
                Some(Message::Error { message, .. }) => Err(format!("internal error: {message}")),
                Some(other) => Err(format!(
                    "internal error: unexpected reply to Load from macro server: {other:?}"
                )),
                None => {
                    let server = self.process.take().expect("server active");
                    self.loaded.clear();
                    let _ = server.wait_and_collect_stderr();
                    Err("internal error: macro server exited while loading library".to_owned())
                }
            }
        }

        pub(super) fn call_unix(
            &mut self,
            library_path: &Path,
            templates: &[(usize, Syntax)],
            entry: &str,
            macro_name: &str,
            context: CallContext,
            arguments: Vec<ArgTree>,
        ) -> Result<(Syntax, u64), String> {
            let library = self.load_library(library_path, templates)?;
            let timeout = self.timeout;
            let server = self.process.as_mut().expect("server loaded library");
            let _ = server.stream.set_read_timeout(Some(timeout));

            if let Err(error) = write_frame(
                &mut server.stream,
                &Message::Call {
                    library,
                    entry: entry.to_owned(),
                    context,
                    arguments,
                },
            ) {
                let server = self.process.take().expect("server active");
                self.loaded.clear();
                let (status, stderr) = server.wait_and_collect_stderr();
                return Err(classify_disconnect(
                    macro_name,
                    status,
                    &stderr,
                    Some(&error.to_string()),
                ));
            }

            match read_frame(&mut server.stream) {
                Ok(Some(Message::Result {
                    syntax,
                    next_mark,
                    output,
                })) => {
                    forward_output(macro_name, &output);
                    Ok((syntax, next_mark))
                }
                Ok(Some(Message::Error {
                    kind,
                    message,
                    output,
                })) => match kind {
                    ErrorKind::CompileError => {
                        forward_output(macro_name, &output);
                        Err(format!("macro `{macro_name}` panicked: {message}"))
                    }
                    ErrorKind::Exit => {
                        let (panic_msg, rest) = split_panic_output(&output);
                        forward_output(macro_name, &rest);
                        let detail = panic_msg.unwrap_or(message);
                        Err(format!("macro `{macro_name}` panicked: {detail}"))
                    }
                    ErrorKind::Internal => {
                        forward_output(macro_name, &output);
                        Err(format!("internal error in macro `{macro_name}`: {message}"))
                    }
                },
                Ok(Some(other)) => {
                    let server = self.process.take().expect("server active");
                    self.loaded.clear();
                    let _ = server.kill_and_collect_stderr();
                    Err(format!(
                        "internal error: unexpected reply for macro `{macro_name}`: {other:?}"
                    ))
                }
                Ok(None) => {
                    let server = self.process.take().expect("server active");
                    self.loaded.clear();
                    let (status, stderr) = server.wait_and_collect_stderr();
                    Err(classify_disconnect(macro_name, status, &stderr, None))
                }
                Err(FrameReadError::Io(error))
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    let server = self.process.take().expect("server active");
                    self.loaded.clear();
                    let _ = server.kill_and_collect_stderr();
                    Err(format!(
                        "macro `{macro_name}` did not finish within {}",
                        format_timeout(timeout)
                    ))
                }
                Err(error) => {
                    let server = self.process.take().expect("server active");
                    self.loaded.clear();
                    let (status, stderr) = server.wait_and_collect_stderr();
                    Err(classify_disconnect(
                        macro_name,
                        status,
                        &stderr,
                        Some(&error.to_string()),
                    ))
                }
            }
        }
    }

    pub(super) fn classify_crash(
        macro_name: &str,
        signal: Option<c_int>,
        stderr: &[u8],
    ) -> String {
        let stderr_text = String::from_utf8_lossy(stderr);
        if matches!(signal, Some(SIGABRT | SIGSEGV))
            && (stderr_text.contains("overflowed its stack")
                || stderr_text.contains("stack overflow"))
        {
            return format!("macro `{macro_name}` overflowed its stack");
        }
        match signal {
            Some(sig) => format!("macro `{macro_name}` crashed ({})", signal_name(sig)),
            None => format!("macro `{macro_name}` crashed"),
        }
    }

    fn classify_disconnect(
        macro_name: &str,
        status: Option<std::process::ExitStatus>,
        stderr: &[u8],
        io_detail: Option<&str>,
    ) -> String {
        if let Some(status) = status {
            if let Some(signal) = status.signal() {
                return classify_crash(macro_name, Some(signal), stderr);
            }
            if !status.success() {
                let stderr_text = String::from_utf8_lossy(stderr);
                if stderr_text.contains("overflowed its stack")
                    || stderr_text.contains("stack overflow")
                {
                    return format!("macro `{macro_name}` overflowed its stack");
                }
                return format!("macro `{macro_name}` crashed (exit status {status})");
            }
        }
        match io_detail {
            Some(detail) => format!("macro `{macro_name}` crashed ({detail})"),
            None => format!("macro `{macro_name}` crashed"),
        }
    }

    fn signal_name(signal: c_int) -> String {
        match signal {
            4 => "SIGILL".to_owned(),
            6 => "SIGABRT".to_owned(),
            7 if cfg!(target_os = "linux") => "SIGBUS".to_owned(),
            8 => "SIGFPE".to_owned(),
            9 => "SIGKILL".to_owned(),
            10 if cfg!(target_os = "macos") => "SIGBUS".to_owned(),
            11 => "SIGSEGV".to_owned(),
            15 => "SIGTERM".to_owned(),
            other => format!("signal {other}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeout_parsing_handles_seconds_and_millis() {
        assert_eq!(parse_timeout("10"), Some(Duration::from_secs(10)));
        assert_eq!(parse_timeout("5s"), Some(Duration::from_secs(5)));
        assert_eq!(parse_timeout("250ms"), Some(Duration::from_millis(250)));
        assert_eq!(parse_timeout("0.25"), Some(Duration::from_millis(250)));
        assert_eq!(parse_timeout("0"), None);
        assert_eq!(parse_timeout("-1"), None);
        assert_eq!(parse_timeout("abc"), None);

        assert_eq!(format_timeout(Duration::from_secs(10)), "10 s");
        assert_eq!(format_timeout(Duration::from_millis(250)), "250 ms");
    }

    #[test]
    fn panic_output_splits_panic_line_and_other_output() {
        let (msg, rest) = split_panic_output(b"debug line\npanic: boom at a.sta:1:2\n");
        assert_eq!(msg.as_deref(), Some("boom at a.sta:1:2"));
        assert_eq!(rest, b"debug line\n");
    }

    #[cfg(unix)]
    #[test]
    fn crash_classification_recognizes_stack_overflow_and_other_signals() {
        assert_eq!(
            unix::classify_crash(
                "m",
                Some(6),
                b"thread 'macro-server-worker' has overflowed its stack\nfatal runtime error: stack overflow\n"
            ),
            "macro `m` overflowed its stack"
        );
        assert_eq!(
            unix::classify_crash(
                "m",
                Some(11),
                b"fatal runtime error: stack overflow\n"
            ),
            "macro `m` overflowed its stack"
        );
        assert_eq!(
            unix::classify_crash("m", Some(11), b""),
            "macro `m` crashed (SIGSEGV)"
        );
        assert_eq!(
            unix::classify_crash("m", Some(9), b""),
            "macro `m` crashed (SIGKILL)"
        );
    }
}
