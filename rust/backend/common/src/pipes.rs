//! Per-connection named-pipe transport between the client CGI program and
//! the running service program. Each connection gets its own pair of
//! pipes, named with the connection uuid (underscores replacing dashes):
//!
//!   - client-to-server: carries the client's messages to the engine. One
//!     send = one episode: the CGI opens the pipe, writes one frame,
//!     closes.
//!   - server-to-client: carries queued engine messages back. One poll =
//!     one episode: the CGI opens the pipe, the service writes one frame
//!     (a JSON array of the connection's queued messages), both close.
//!
//! The service is the pipe server on both; the CGI program is always the
//! client. The service is SINGLE-THREADED: the server types here never
//! block - each is ticked from the service's main loop
//! ([`InboundServerPipe::drain_frames`], [`OutboundServerPipe::try_serve_poll`])
//! and returns immediately when there's nothing to do. The clients block
//! (with deadlines): a CGI request is its own short-lived process.
//!
//! The recorded names live in the connection's profile file (see
//! `crate::state::ConnectionProfile`) - the service serves whatever names
//! the profile records.
//!
//! Platform mapping: Windows pipes are `\\.\pipe\stadhouder_{c2s,s2c}_<uuid>`
//! (a uuid is globally unique, so no per-instance tag is needed); on
//! Linux/Unix they are FIFOs `{c2s,s2c}_<uuid>` in `<home>/stadhouder/pipes/`,
//! created by the service when it picks up the connection.
//!
//! Framing (both directions): a 4-byte little-endian length, then that
//! many bytes of UTF-8 payload.
use std::io::{Read, Write};
use std::path::Path;

const MAX_FRAME_BYTES: u32 = 16 * 1024 * 1024;

/// The pipe pair serving one connection.
#[derive(Clone)]
pub struct PipeNames {
    pub client_to_server: String,
    pub server_to_client: String,
}

/// The platform-appropriate pipe names for a connection. `stadhouder_dir`
/// only matters on Unix (the FIFOs live under it); Windows pipe names are
/// global.
pub fn pipe_names_for(stadhouder_dir: &Path, connection_id: &str) -> PipeNames {
    imp::pipe_names_for(stadhouder_dir, connection_id)
}

/// Creates whatever the platform needs before the pipes can be served
/// (FIFO files on Unix; nothing on Windows). Server side only.
pub fn create_serving_pipes(names: &PipeNames) -> Result<(), String> {
    imp::create_serving_pipes(names)
}

/// Removes what `create_serving_pipes` made. Best-effort; server side only.
pub fn remove_serving_pipes(names: &PipeNames) {
    imp::remove_serving_pipes(names);
}

/// The server end of a connection's client-to-server pipe. Non-blocking:
/// tick [`drain_frames`](Self::drain_frames) from the main loop.
pub struct InboundServerPipe(imp::InboundServerPipe);

impl InboundServerPipe {
    pub fn create(pipe_name: &str) -> Result<InboundServerPipe, String> {
        Ok(InboundServerPipe(imp::InboundServerPipe::create(pipe_name)?))
    }

    /// Every complete frame that has arrived since the last call - empty
    /// (and immediate) when nothing has. Partial frames stay buffered
    /// until their remainder arrives.
    pub fn drain_frames(&mut self) -> Result<Vec<String>, String> {
        self.0.drain_frames()
    }
}

/// The server end of a connection's server-to-client pipe. Non-blocking:
/// tick [`try_serve_poll`](Self::try_serve_poll) from the main loop.
pub struct OutboundServerPipe(imp::OutboundServerPipe);

impl OutboundServerPipe {
    pub fn create(pipe_name: &str) -> Result<OutboundServerPipe, String> {
        Ok(OutboundServerPipe(imp::OutboundServerPipe::create(pipe_name)?))
    }

    /// If a poll client is waiting, answers it with the frame `provide`
    /// produces and returns true; returns false (immediately) when no
    /// poller is there. `provide` is only called once a client is attached.
    pub fn try_serve_poll(&mut self, provide: impl FnOnce() -> String) -> Result<bool, String> {
        self.0.try_serve_poll(provide)
    }
}

/// Client side: one send-episode - deliver one frame to the service.
/// Retries briefly while the pipe isn't there yet (the service picks new
/// connections up on its scan cadence). Retry periods and deadlines are
/// engine time: they're divided by `time_factor` (see
/// `crate::time::wait_factor`) to get real waits.
pub fn client_send(pipe_name: &str, frame: &str, time_factor: f64) -> Result<(), String> {
    imp::client_send(pipe_name, frame, time_factor)
}

/// Client side: one poll-episode - returns the frame the service writes.
/// Waits scale by `time_factor`, as for [`client_send`].
pub fn client_receive(pipe_name: &str, time_factor: f64) -> Result<String, String> {
    imp::client_receive(pipe_name, time_factor)
}

/// The client retry cadence/deadlines, expressed in engine time and scaled
/// to real durations by the time factor. The not-found deadline exceeds
/// the service's connection-scan interval (4s), so a client arriving right
/// after its own connect outlives the pickup latency.
struct ClientWaits {
    retry: std::time::Duration,
    not_found_deadline: std::time::Instant,
    busy_deadline: std::time::Instant,
}

fn client_waits(time_factor: f64) -> ClientWaits {
    use crate::time::scale_wait_ms;
    use std::time::{Duration, Instant};
    let now = Instant::now();
    ClientWaits {
        retry: Duration::from_millis(scale_wait_ms(50, time_factor)),
        not_found_deadline: now + Duration::from_millis(scale_wait_ms(6_000, time_factor)),
        busy_deadline: now + Duration::from_millis(scale_wait_ms(10_000, time_factor)),
    }
}

fn write_frame(w: &mut impl Write, message: &str) -> Result<(), String> {
    let bytes = message.as_bytes();
    if bytes.len() > MAX_FRAME_BYTES as usize {
        return Err(format!("message of {} bytes exceeds the frame limit", bytes.len()));
    }
    let len = (bytes.len() as u32).to_le_bytes();
    w.write_all(&len)
        .and_then(|()| w.write_all(bytes))
        .and_then(|()| w.flush())
        .map_err(|e| format!("pipe write failed: {e}"))
}

fn read_frame(r: &mut impl Read) -> Result<String, String> {
    let mut len_bytes = [0u8; 4];
    r.read_exact(&mut len_bytes)
        .map_err(|e| format!("pipe read failed: {e}"))?;
    let len = u32::from_le_bytes(len_bytes);
    if len > MAX_FRAME_BYTES {
        return Err(format!("frame of {len} bytes exceeds the frame limit"));
    }
    let mut buf = vec![0u8; len as usize];
    r.read_exact(&mut buf)
        .map_err(|e| format!("pipe read failed: {e}"))?;
    String::from_utf8(buf).map_err(|_| "pipe frame is not UTF-8".to_string())
}

/// Pops every complete length-prefixed frame off the front of `buffer`,
/// leaving any trailing partial frame in place for the next tick.
fn extract_frames(buffer: &mut Vec<u8>) -> Result<Vec<String>, String> {
    let mut frames = Vec::new();
    loop {
        if buffer.len() < 4 {
            return Ok(frames);
        }
        let len = u32::from_le_bytes([buffer[0], buffer[1], buffer[2], buffer[3]]);
        if len > MAX_FRAME_BYTES {
            return Err(format!("frame of {len} bytes exceeds the frame limit"));
        }
        let end = 4 + len as usize;
        if buffer.len() < end {
            return Ok(frames);
        }
        let frame = String::from_utf8(buffer[4..end].to_vec())
            .map_err(|_| "pipe frame is not UTF-8".to_string())?;
        buffer.drain(..end);
        frames.push(frame);
    }
}

#[cfg(windows)]
mod imp {
    use std::fs::{File, OpenOptions};
    use std::io::{ErrorKind, Read};
    use std::os::windows::io::{AsRawHandle, FromRawHandle};
    use std::path::Path;
    use std::time::Instant;

    use windows_sys::Win32::Foundation::{
        GetLastError, ERROR_BROKEN_PIPE, ERROR_NO_DATA, ERROR_PIPE_CONNECTED,
        ERROR_PIPE_LISTENING, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Storage::FileSystem::{FlushFileBuffers, PIPE_ACCESS_DUPLEX};
    use windows_sys::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PeekNamedPipe,
        SetNamedPipeHandleState, PIPE_NOWAIT, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_WAIT,
    };

    use super::PipeNames;
    use crate::layout;

    const ERROR_PIPE_BUSY: i32 = 231;

    /// Everything after `\\.\pipe\` is one opaque name to the Npfs driver,
    /// but that name can itself contain backslashes - `stadhouder\` here
    /// isn't a real directory (nothing needs creating first), just a
    /// namespacing prefix, mirroring the Unix side's `stadhouder/pipes/`.
    pub fn pipe_names_for(_stadhouder_dir: &Path, connection_id: &str) -> PipeNames {
        let tag = layout::underscore_uuid(connection_id);
        PipeNames {
            client_to_server: format!(r"\\.\pipe\stadhouder\c2s_{tag}"),
            server_to_client: format!(r"\\.\pipe\stadhouder\s2c_{tag}"),
        }
    }

    pub fn create_serving_pipes(_names: &PipeNames) -> Result<(), String> {
        // Windows pipe instances come into being with the server types'
        // create() - nothing to pre-create on disk.
        Ok(())
    }

    pub fn remove_serving_pipes(_names: &PipeNames) {
        // The name disappears with the last server handle.
    }

    /// One NOWAIT pipe instance, created once and reused across episodes
    /// via non-blocking connect-checks + disconnect. The File owns the
    /// handle; the name exists as long as this does.
    struct Instance {
        file: File,
        connected: bool,
    }

    impl Instance {
        fn create(pipe_name: &str) -> Result<Instance, String> {
            let mut path_w: Vec<u16> = pipe_name.encode_utf16().collect();
            path_w.push(0);

            let handle = unsafe {
                CreateNamedPipeW(
                    path_w.as_ptr(),
                    PIPE_ACCESS_DUPLEX,
                    // NOWAIT so the single-threaded service's connect-checks
                    // (and reads) never block.
                    PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_NOWAIT,
                    1,
                    64 * 1024,
                    64 * 1024,
                    0,
                    std::ptr::null(),
                )
            };
            if handle == INVALID_HANDLE_VALUE {
                return Err(format!("CreateNamedPipeW failed (error {})", unsafe {
                    GetLastError()
                }));
            }
            Ok(Instance {
                file: unsafe { File::from_raw_handle(handle as _) },
                connected: false,
            })
        }

        /// Non-blocking: whether a client is attached (checking for a new
        /// one if none was). ERROR_NO_DATA means a client connected AND
        /// closed since the last check - that still counts as attached, so
        /// its buffered data (a fast send episode) gets drained before the
        /// disconnect.
        fn check_connected(&mut self) -> Result<bool, String> {
            if self.connected {
                return Ok(true);
            }
            let ok = unsafe {
                ConnectNamedPipe(self.file.as_raw_handle() as _, std::ptr::null_mut())
            };
            let error = unsafe { GetLastError() };
            if ok != 0 || error == ERROR_PIPE_CONNECTED || error == ERROR_NO_DATA {
                self.connected = true;
                Ok(true)
            } else if error == ERROR_PIPE_LISTENING {
                Ok(false)
            } else {
                Err(format!("ConnectNamedPipe failed (error {error})"))
            }
        }

        /// Bytes waiting to be read, or `None` when the client end has
        /// closed (and everything it wrote has been drained).
        fn available(&self) -> Result<Option<u32>, String> {
            let mut avail: u32 = 0;
            let ok = unsafe {
                PeekNamedPipe(
                    self.file.as_raw_handle() as _,
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null_mut(),
                    &mut avail,
                    std::ptr::null_mut(),
                )
            };
            if ok != 0 {
                return Ok(Some(avail));
            }
            let error = unsafe { GetLastError() };
            if error == ERROR_BROKEN_PIPE || error == ERROR_NO_DATA {
                Ok(None)
            } else {
                Err(format!("PeekNamedPipe failed (error {error})"))
            }
        }

        /// Ends the episode: back to listening for the next client.
        fn disconnect(&mut self) {
            unsafe { DisconnectNamedPipe(self.file.as_raw_handle() as _) };
            self.connected = false;
        }

        /// Switches the handle between blocking and non-blocking mode. The
        /// connect-checks need NOWAIT, but an outbound episode needs WAIT:
        /// FlushFileBuffers on a NOWAIT handle doesn't wait for the client
        /// to read, so a disconnect right after would discard the frame.
        fn set_blocking(&self, blocking: bool) {
            let mode: u32 =
                PIPE_READMODE_BYTE | if blocking { PIPE_WAIT } else { PIPE_NOWAIT };
            unsafe {
                SetNamedPipeHandleState(
                    self.file.as_raw_handle() as _,
                    &mode,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                );
            }
        }
    }

    pub struct InboundServerPipe {
        instance: Instance,
        buffer: Vec<u8>,
    }

    impl InboundServerPipe {
        pub fn create(pipe_name: &str) -> Result<InboundServerPipe, String> {
            Ok(InboundServerPipe {
                instance: Instance::create(pipe_name)?,
                buffer: Vec::new(),
            })
        }

        pub fn drain_frames(&mut self) -> Result<Vec<String>, String> {
            if !self.instance.check_connected()? {
                return Ok(Vec::new());
            }
            let mut episode_over = false;
            loop {
                match self.instance.available()? {
                    Some(0) => break,
                    Some(avail) => {
                        let mut chunk = vec![0u8; avail as usize];
                        let n = self
                            .instance
                            .file
                            .read(&mut chunk)
                            .map_err(|e| format!("pipe read failed: {e}"))?;
                        self.buffer.extend_from_slice(&chunk[..n]);
                    }
                    None => {
                        // Client closed its end and everything is drained -
                        // the episode is over; back to listening.
                        self.instance.disconnect();
                        episode_over = true;
                        break;
                    }
                }
            }
            let frames = super::extract_frames(&mut self.buffer);
            if episode_over {
                // Anything left now is a partial frame whose sender is
                // gone - it can never complete.
                self.buffer.clear();
            }
            frames
        }
    }

    pub struct OutboundServerPipe {
        instance: Instance,
    }

    impl OutboundServerPipe {
        pub fn create(pipe_name: &str) -> Result<OutboundServerPipe, String> {
            Ok(OutboundServerPipe {
                instance: Instance::create(pipe_name)?,
            })
        }

        pub fn try_serve_poll(&mut self, provide: impl FnOnce() -> String) -> Result<bool, String> {
            if !self.instance.check_connected()? {
                return Ok(false);
            }
            // A dead poller (opened, then vanished) must be detected BEFORE
            // `provide` drains the connection's outbox into the void. The
            // inbound-direction peek doubles as a liveness probe: a live
            // poller reads Ok (it just writes nothing), a departed one
            // reads "broken".
            if self.instance.available()?.is_none() {
                self.instance.disconnect();
                return Ok(false);
            }

            self.instance.set_blocking(true);
            let result = super::write_frame(&mut self.instance.file, &provide());
            if result.is_ok() {
                unsafe { FlushFileBuffers(self.instance.file.as_raw_handle() as _) };
                // Belt and braces: flush's blocking behaviour has proven
                // unreliable on a mode-switched handle, and disconnecting
                // discards anything unread. A well-behaved poll client
                // closes as soon as it has read the frame - observable as
                // the probe turning "broken" - so wait for that (bounded,
                // in case the client hangs).
                for _ in 0..400 {
                    match self.instance.available() {
                        Ok(None) | Err(_) => break,
                        Ok(Some(_)) => std::thread::sleep(std::time::Duration::from_millis(5)),
                    }
                }
            }
            // Either way, free the pipe for the next poller.
            self.instance.disconnect();
            self.instance.set_blocking(false);
            Ok(true)
        }
    }

    /// Opens the pipe as a client, retrying while it's busy (another CGI
    /// process mid-episode) or not there yet (the service hasn't picked the
    /// connection up).
    fn open_client(pipe_name: &str, time_factor: f64) -> Result<File, String> {
        let waits = super::client_waits(time_factor);
        loop {
            match OpenOptions::new().read(true).write(true).open(pipe_name) {
                Ok(f) => return Ok(f),
                Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
                    if Instant::now() >= waits.busy_deadline {
                        return Err("stadhouder service is busy (pipe timeout)".to_string());
                    }
                }
                Err(e) if e.kind() == ErrorKind::NotFound => {
                    if Instant::now() >= waits.not_found_deadline {
                        return Err(
                            "stadhouder service is not running (pipe not found)".to_string()
                        );
                    }
                }
                Err(e) => return Err(format!("failed to open service pipe: {e}")),
            }
            std::thread::sleep(waits.retry);
        }
    }

    pub fn client_send(pipe_name: &str, frame: &str, time_factor: f64) -> Result<(), String> {
        let mut file = open_client(pipe_name, time_factor)?;
        super::write_frame(&mut file, frame)
    }

    pub fn client_receive(pipe_name: &str, time_factor: f64) -> Result<String, String> {
        let mut file = open_client(pipe_name, time_factor)?;
        super::read_frame(&mut file)
    }
}

#[cfg(unix)]
mod imp {
    use std::ffi::CString;
    use std::fs::{self, File, OpenOptions};
    use std::io::{ErrorKind, Read};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::io::AsRawFd;
    use std::path::Path;
    use std::time::Instant;

    use super::PipeNames;
    use crate::layout;

    pub fn pipe_names_for(stadhouder_dir: &Path, connection_id: &str) -> PipeNames {
        let tag = layout::underscore_uuid(connection_id);
        let pipes_dir = stadhouder_dir.join("pipes");
        PipeNames {
            client_to_server: pipes_dir.join(format!("c2s_{tag}")).to_string_lossy().into_owned(),
            server_to_client: pipes_dir.join(format!("s2c_{tag}")).to_string_lossy().into_owned(),
        }
    }

    fn mkfifo(path: &str) -> Result<(), String> {
        if let Some(parent) = Path::new(path).parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("failed to create {}: {e}", parent.display()))?;
        }
        let c_path =
            CString::new(path.as_bytes()).map_err(|_| format!("invalid pipe path {path}"))?;
        if unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) } != 0 {
            return Err(format!(
                "mkfifo {path} failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(())
    }

    pub fn create_serving_pipes(names: &PipeNames) -> Result<(), String> {
        for name in [&names.client_to_server, &names.server_to_client] {
            let _ = fs::remove_file(name); // stale from a previous run
            mkfifo(name)?;
        }
        Ok(())
    }

    pub fn remove_serving_pipes(names: &PipeNames) {
        let _ = fs::remove_file(&names.client_to_server);
        let _ = fs::remove_file(&names.server_to_client);
    }

    /// Holds the FIFO's read end open (non-blocking) for the connection's
    /// whole life: client sends always find a reader, and their bytes
    /// queue in the FIFO until the next tick drains them.
    pub struct InboundServerPipe {
        file: File,
        buffer: Vec<u8>,
    }

    impl InboundServerPipe {
        pub fn create(pipe_name: &str) -> Result<InboundServerPipe, String> {
            let file = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(pipe_name)
                .map_err(|e| format!("failed to open {pipe_name}: {e}"))?;
            Ok(InboundServerPipe {
                file,
                buffer: Vec::new(),
            })
        }

        pub fn drain_frames(&mut self) -> Result<Vec<String>, String> {
            let mut chunk = [0u8; 4096];
            loop {
                match self.file.read(&mut chunk) {
                    // 0 = no writers right now (not EOF in any final sense -
                    // our read end persists and the next sender reopens).
                    Ok(0) => break,
                    Ok(n) => self.buffer.extend_from_slice(&chunk[..n]),
                    Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                    Err(e) => return Err(format!("pipe read failed: {e}")),
                }
            }
            super::extract_frames(&mut self.buffer)
        }
    }

    /// The write side of the server-to-client FIFO, opened per episode: a
    /// non-blocking write-open only succeeds while a reader (a blocked
    /// poll client) is attached - that's the poll detection.
    pub struct OutboundServerPipe {
        path: String,
    }

    impl OutboundServerPipe {
        pub fn create(pipe_name: &str) -> Result<OutboundServerPipe, String> {
            Ok(OutboundServerPipe {
                path: pipe_name.to_string(),
            })
        }

        pub fn try_serve_poll(&mut self, provide: impl FnOnce() -> String) -> Result<bool, String> {
            let file = match OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(&self.path)
            {
                Ok(f) => f,
                Err(e) if e.raw_os_error() == Some(libc::ENXIO) => return Ok(false),
                Err(e) => return Err(format!("failed to open {}: {e}", self.path)),
            };
            // Back to blocking writes; frames can exceed the FIFO buffer.
            unsafe {
                let fd = file.as_raw_fd();
                let flags = libc::fcntl(fd, libc::F_GETFL);
                libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK);
            }
            let mut file = file;
            // A poller that vanished mid-write (EPIPE) isn't the service's
            // problem.
            let _ = super::write_frame(&mut file, &provide());
            Ok(true)
        }
    }

    pub fn client_send(pipe_name: &str, frame: &str, time_factor: f64) -> Result<(), String> {
        // Non-blocking write-open: ENXIO = no reader, i.e. the service
        // isn't serving this pipe (yet) - retry briefly, since the service
        // picks new connections up on its scan cadence.
        let waits = super::client_waits(time_factor);
        let file = loop {
            match OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(pipe_name)
            {
                Ok(f) => break f,
                Err(e)
                    if e.raw_os_error() == Some(libc::ENXIO)
                        || e.kind() == ErrorKind::NotFound =>
                {
                    if Instant::now() >= waits.not_found_deadline {
                        return Err(
                            "stadhouder service is not running (no reader on the pipe)".to_string()
                        );
                    }
                }
                Err(e) => return Err(format!("failed to open {pipe_name}: {e}")),
            }
            std::thread::sleep(waits.retry);
        };
        // Back to blocking writes; frames can exceed the FIFO buffer.
        unsafe {
            let fd = file.as_raw_fd();
            let flags = libc::fcntl(fd, libc::F_GETFL);
            libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK);
        }
        let mut file = file;
        super::write_frame(&mut file, frame)
    }

    pub fn client_receive(pipe_name: &str, time_factor: f64) -> Result<String, String> {
        // Wait for the FIFO to exist (service pick-up latency), then a
        // blocking read-open waits for the service's next poll-serving
        // tick to open the write end.
        let waits = super::client_waits(time_factor);
        while !Path::new(pipe_name).exists() {
            if Instant::now() >= waits.not_found_deadline {
                return Err("stadhouder service is not running (pipe not found)".to_string());
            }
            std::thread::sleep(waits.retry);
        }
        let mut file =
            File::open(pipe_name).map_err(|e| format!("failed to open {pipe_name}: {e}"))?;
        super::read_frame(&mut file)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn frame_roundtrip() {
        let mut buf: Vec<u8> = Vec::new();
        write_frame(&mut buf, "hello there").unwrap();
        let mut cursor = std::io::Cursor::new(buf);
        assert_eq!(read_frame(&mut cursor).unwrap(), "hello there");
    }

    #[test]
    fn empty_frame_roundtrip() {
        let mut buf: Vec<u8> = Vec::new();
        write_frame(&mut buf, "").unwrap();
        let mut cursor = std::io::Cursor::new(buf);
        assert_eq!(read_frame(&mut cursor).unwrap(), "");
    }

    #[test]
    fn oversized_frame_is_rejected() {
        let mut buf: Vec<u8> = Vec::new();
        buf.extend_from_slice(&u32::MAX.to_le_bytes());
        let mut cursor = std::io::Cursor::new(buf);
        assert!(read_frame(&mut cursor).is_err());
    }

    #[test]
    fn truncated_frame_is_an_error() {
        let mut buf: Vec<u8> = Vec::new();
        write_frame(&mut buf, "hello").unwrap();
        buf.truncate(buf.len() - 2);
        let mut cursor = std::io::Cursor::new(buf);
        assert!(read_frame(&mut cursor).is_err());
    }

    #[test]
    fn extract_frames_handles_partials_and_multiples() {
        let mut three: Vec<u8> = Vec::new();
        write_frame(&mut three, "three").unwrap();

        let mut buf: Vec<u8> = Vec::new();
        write_frame(&mut buf, "one").unwrap();
        write_frame(&mut buf, "two").unwrap();
        buf.extend_from_slice(&three[..5]); // "three" only partially arrived

        let frames = extract_frames(&mut buf).unwrap();
        assert_eq!(frames, vec!["one".to_string(), "two".to_string()]);
        assert_eq!(buf.len(), 5); // the partial stays buffered

        // The rest arrives.
        buf.extend_from_slice(&three[5..]);
        assert_eq!(extract_frames(&mut buf).unwrap(), vec!["three".to_string()]);
        assert!(buf.is_empty());
    }

    #[test]
    fn pipe_names_use_underscored_uuid() {
        let names = pipe_names_for(
            std::path::Path::new("/tmp/x/stadhouder"),
            "123e4567-e89b-42d3-a456-426614174000",
        );
        assert!(names.client_to_server.contains("c2s_123e4567_e89b_42d3_a456_426614174000"));
        assert!(names.server_to_client.contains("s2c_123e4567_e89b_42d3_a456_426614174000"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_pipe_names_live_under_a_stadhouder_subfolder() {
        let names = pipe_names_for(
            std::path::Path::new("C:\\irrelevant"),
            "123e4567-e89b-42d3-a456-426614174000",
        );
        assert_eq!(
            names.client_to_server,
            r"\\.\pipe\stadhouder\c2s_123e4567_e89b_42d3_a456_426614174000"
        );
        assert_eq!(
            names.server_to_client,
            r"\\.\pipe\stadhouder\s2c_123e4567_e89b_42d3_a456_426614174000"
        );
    }

    /// Both directions over the real platform transport, driven the way
    /// the single-threaded service drives them: a ticking loop on the
    /// server side, blocking clients on the other.
    #[test]
    fn episodes_over_real_pipes() {
        let dir = std::env::temp_dir()
            .join(format!("stadhouder-pipes-test-{}", std::process::id()))
            .join("stadhouder");
        std::fs::create_dir_all(&dir).unwrap();
        let names = pipe_names_for(&dir, "00000000-0000-4000-8000-000000000001");
        create_serving_pipes(&names).unwrap();

        let server_names = names.clone();
        let server = std::thread::spawn(move || {
            let mut inbound = InboundServerPipe::create(&server_names.client_to_server)?;
            let mut outbound = OutboundServerPipe::create(&server_names.server_to_client)?;

            let mut received = None;
            for _ in 0..200 {
                if let Some(frame) = inbound.drain_frames()?.into_iter().next() {
                    received = Some(frame);
                    break;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            let received = received.ok_or("no frame arrived")?;

            for _ in 0..200 {
                if outbound.try_serve_poll(|| format!("echo:{received}"))? {
                    return Ok::<String, String>(received);
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err("no poller arrived".to_string())
        });

        std::thread::sleep(Duration::from_millis(100));
        client_send(&names.client_to_server, "hello", 1.0).unwrap();
        let reply = client_receive(&names.server_to_client, 1.0).unwrap();
        assert_eq!(reply, "echo:hello");
        assert_eq!(server.join().unwrap().unwrap(), "hello");

        remove_serving_pipes(&names);
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }
}
