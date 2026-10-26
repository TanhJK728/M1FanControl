//! Manual background workers and foreground launchd services with bounded IPC.
//!
//! The control loop owns the hardware lock and recovery guard. Call `ready` only
//! after verified engagement, and `finish` only after automatic recovery finishes.
//! No PID file is used for control. Service installation is an explicit deploy step.

use crate::platform;
use crate::smc::{Error, Result};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const RUNTIME: &str = "/var/run/m1fan";
const STARTUP_FD: &str = "M1FAN_STARTUP_FD";
const START_TIMEOUT: Duration = Duration::from_secs(30);
const STOP_TIMEOUT: Duration = Duration::from_secs(30);
const CLIENT_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_CLIENTS: usize = 16;
const MAX_REQUEST: usize = 128;
const MAX_RESPONSE: usize = 32 * 1024;
const LOG_LIMIT: u64 = 2 * 1024 * 1024;
static STARTUP_CLAIMED: AtomicBool = AtomicBool::new(false);

fn io_error(context: &str, error: io::Error) -> Error {
    Error::Io(format!("{context}: {error}"))
}

fn root() -> Result<()> {
    if unsafe { libc::geteuid() } == 0 {
        Ok(())
    } else {
        Err(Error::Permission(
            "start/stop/daemon-status require sudo".into(),
        ))
    }
}

#[derive(Clone)]
struct Runtime {
    path: PathBuf,
    uid: u32,
}

impl Runtime {
    fn system() -> Self {
        Self {
            path: PathBuf::from(RUNTIME),
            uid: 0,
        }
    }

    fn check(&self, create: bool) -> Result<()> {
        if create {
            match fs::DirBuilder::new().mode(0o700).create(&self.path) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(io_error("create daemon runtime", e)),
            }
        }
        let meta =
            fs::symlink_metadata(&self.path).map_err(|e| io_error("inspect daemon runtime", e))?;
        if !meta.is_dir() || meta.uid() != self.uid || meta.mode() & 0o777 != 0o700 {
            return Err(Error::Permission(
                "daemon runtime must be a real owner-only directory (0700)".into(),
            ));
        }
        Ok(())
    }

    fn socket(&self) -> PathBuf {
        self.path.join("control.sock")
    }

    fn check_socket(&self) -> Result<()> {
        let meta = fs::symlink_metadata(self.socket())
            .map_err(|e| io_error("inspect daemon socket", e))?;
        if !meta.file_type().is_socket() || meta.uid() != self.uid || meta.mode() & 0o777 != 0o600 {
            return Err(Error::Permission(
                "unsafe daemon socket ownership/type/permissions".into(),
            ));
        }
        Ok(())
    }

    fn file(&self, name: &str) -> Result<File> {
        self.check(false)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(self.path.join(name))
            .map_err(|e| io_error("open daemon file", e))?;
        let meta = file
            .metadata()
            .map_err(|e| io_error("inspect daemon file", e))?;
        if !meta.is_file()
            || meta.uid() != self.uid
            || meta.nlink() != 1
            || meta.mode() & 0o777 != 0o600
        {
            return Err(Error::Permission(
                "unsafe daemon file ownership/type/permissions".into(),
            ));
        }
        Ok(file)
    }

    fn read_final(&self) -> Result<String> {
        self.check(false)?;
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(self.path.join("last-status"))
            .map_err(|e| io_error("read last daemon status", e))?;
        let meta = file
            .metadata()
            .map_err(|e| io_error("inspect last daemon status", e))?;
        if !meta.is_file()
            || meta.uid() != self.uid
            || meta.nlink() != 1
            || meta.mode() & 0o777 != 0o600
        {
            return Err(Error::Permission("unsafe last daemon status file".into()));
        }
        let mut message = String::new();
        file.take(MAX_RESPONSE as u64)
            .read_to_string(&mut message)
            .map_err(|e| io_error("read last daemon status", e))?;
        Ok(message)
    }

    fn store_final(&self, message: &str) -> Result<()> {
        let mut file = self.file("last-status")?;
        file.set_len(0)
            .map_err(|e| io_error("truncate daemon final status", e))?;
        file.write_all(message.as_bytes())
            .map_err(|e| io_error("save daemon final status", e))?;
        file.flush()
            .map_err(|e| io_error("flush daemon final status", e))
    }
}

#[cfg(target_os = "macos")]
fn peer_uid(stream: &UnixStream) -> io::Result<u32> {
    let mut uid = 0;
    let mut gid = 0;
    if unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) } != 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(uid)
    }
}

#[cfg(not(target_os = "macos"))]
fn peer_uid(stream: &UnixStream) -> io::Result<u32> {
    let mut credential: libc::ucred = unsafe { std::mem::zeroed() };
    let mut length = std::mem::size_of_val(&credential) as libc::socklen_t;
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut credential as *mut libc::ucred).cast(),
            &mut length,
        )
    };
    if result != 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(credential.uid)
    }
}

// Nonblocking connect also bounds a full listener backlog. UnixStream::connect
// alone can wait for accept indefinitely, even when reads are nonblocking later.
fn connect_bounded(path: &Path, timeout: Duration) -> io::Result<UnixStream> {
    let path = path.as_os_str().as_bytes();
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    if path.contains(&0) || path.len() >= address.sun_path.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid Unix socket path",
        ));
    }
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (destination, source) in address.sun_path.iter_mut().zip(path) {
        *destination = *source as libc::c_char;
    }
    let length =
        (std::mem::offset_of!(libc::sockaddr_un, sun_path) + path.len() + 1) as libc::socklen_t;
    #[cfg(target_os = "macos")]
    {
        address.sun_len = length as u8;
    }
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
    if fd == -1 {
        return Err(io::Error::last_os_error());
    }
    let stream = unsafe { UnixStream::from_raw_fd(fd) };
    stream.set_nonblocking(true)?;
    if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } == -1 {
        return Err(io::Error::last_os_error());
    }
    let connected =
        unsafe { libc::connect(fd, (&address as *const libc::sockaddr_un).cast(), length) };
    if connected == 0 {
        return Ok(stream);
    }
    let error = io::Error::last_os_error();
    if !matches!(
        error.raw_os_error(),
        Some(libc::EINPROGRESS | libc::EINTR | libc::EAGAIN)
    ) {
        return Err(error);
    }
    let deadline = Instant::now() + timeout;
    loop {
        if platform::stopped() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "startup interrupted",
            ));
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "daemon connect deadline expired",
            ));
        }
        let mut poll = libc::pollfd {
            fd,
            events: libc::POLLOUT,
            revents: 0,
        };
        let result = unsafe { libc::poll(&mut poll, 1, 10) };
        if result < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if result == 0 {
            continue;
        }
        let mut status: libc::c_int = 0;
        let mut size = std::mem::size_of_val(&status) as libc::socklen_t;
        if unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_ERROR,
                (&mut status as *mut libc::c_int).cast(),
                &mut size,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        if status != 0 {
            return Err(io::Error::from_raw_os_error(status));
        }
        // Some systems report a full Unix backlog as EAGAIN rather than a
        // pending connect. Confirm the peer instead of treating SO_ERROR=0 as success.
        stream.peer_addr()?;
        return Ok(stream);
    }
}

fn authenticate(stream: &UnixStream, uid: u32) -> Result<()> {
    if peer_uid(stream).map_err(|e| io_error("verify IPC peer", e))? == uid {
        Ok(())
    } else {
        Err(Error::Permission(
            "IPC peer has an unexpected user ID".into(),
        ))
    }
}

fn pause(deadline: Instant, interruptible: bool) -> Result<()> {
    if interruptible && platform::stopped() {
        return Err(Error::Interrupted);
    }
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(Error::Control("daemon IPC deadline expired".into()));
    }
    std::thread::sleep(remaining.min(Duration::from_millis(10)));
    Ok(())
}

fn send_bounded(
    stream: &mut UnixStream,
    bytes: &[u8],
    deadline: Instant,
    interruptible: bool,
) -> Result<()> {
    let mut offset = 0;
    while offset < bytes.len() {
        if interruptible && platform::stopped() {
            return Err(Error::Interrupted);
        }
        if Instant::now() >= deadline {
            return Err(Error::Control("daemon IPC write deadline expired".into()));
        }
        match stream.write(&bytes[offset..]) {
            Ok(0) => {
                return Err(Error::Control(
                    "daemon IPC peer closed while writing".into(),
                ));
            }
            Ok(n) => offset += n,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => pause(deadline, interruptible)?,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(io_error("write daemon IPC", e)),
        }
    }
    Ok(())
}

fn read_line(stream: &mut UnixStream, deadline: Instant, interruptible: bool) -> Result<String> {
    let mut bytes = Vec::new();
    let mut byte = [0];
    loop {
        if interruptible && platform::stopped() {
            return Err(Error::Interrupted);
        }
        if Instant::now() >= deadline {
            return Err(Error::Control("daemon IPC read deadline expired".into()));
        }
        match stream.read(&mut byte) {
            Ok(0) => return Err(Error::Control("daemon startup peer disconnected".into())),
            Ok(_) if byte[0] == b'\n' => {
                return String::from_utf8(bytes)
                    .map_err(|_| Error::Control("non-UTF8 daemon handshake".into()));
            }
            Ok(_) => {
                bytes.push(byte[0]);
                if bytes.len() > MAX_RESPONSE {
                    return Err(Error::Control("daemon handshake too large".into()));
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => pause(deadline, interruptible)?,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(io_error("read daemon handshake", e)),
        }
    }
}

fn startup_channel() -> Result<Option<UnixStream>> {
    if STARTUP_CLAIMED.swap(true, Ordering::SeqCst) {
        return Ok(None);
    }
    let Some(value) = std::env::var_os(STARTUP_FD) else {
        return Ok(None);
    };
    let fd: RawFd = value
        .to_str()
        .and_then(|s| s.parse().ok())
        .filter(|fd| *fd > 2)
        .ok_or_else(|| Error::Control("invalid inherited startup socket".into()))?;
    // Validate before taking ownership: an arbitrary inherited descriptor is not
    // accepted as a handshake transport. It is never used as an SMC connection.
    let mut meta: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd, &mut meta) } != 0 || meta.st_mode & libc::S_IFMT != libc::S_IFSOCK {
        return Err(Error::Control("startup descriptor is not a socket".into()));
    }
    let stream = unsafe { UnixStream::from_raw_fd(fd) };
    authenticate(&stream, unsafe { libc::geteuid() })?;
    stream
        .set_nonblocking(true)
        .map_err(|e| io_error("configure startup socket", e))?;
    if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } == -1 {
        return Err(io_error(
            "protect startup socket",
            io::Error::last_os_error(),
        ));
    }
    Ok(Some(stream))
}

/// Report failures before WorkerIpc could bind. Already claimed/closed startup
/// channels are ignored, so callers may also use this in a top-level error path.
pub fn worker_failure(error: &Error) -> Result<()> {
    if let Some(mut stream) = startup_channel()? {
        let message = format!("ERROR {}\n", one_line(&error.to_string()));
        send_bounded(
            &mut stream,
            message.as_bytes(),
            Instant::now() + Duration::from_secs(1),
            false,
        )?;
    }
    Ok(())
}

fn one_line(value: &str) -> String {
    let mut message = String::new();
    for character in value.chars() {
        let character = if character == '\n' || character == '\r' {
            ' '
        } else {
            character
        };
        if message.len() + character.len_utf8() > MAX_RESPONSE - 128 {
            break;
        }
        message.push(character);
    }
    message
}

fn await_startup(stream: &mut UnixStream, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    let first = read_line(stream, deadline, true)?;
    if first != "READY" {
        return Err(Error::Control(format!("worker startup failed: {first}")));
    }
    send_bounded(stream, b"ACK\n", deadline, true)?;
    let last = read_line(stream, deadline, true)?;
    if last != "RUNNING" {
        return Err(Error::Control(format!(
            "worker did not confirm running: {last}"
        )));
    }
    Ok(())
}

fn cancel_child(child: &mut Child, timeout: Duration) -> String {
    match child.try_wait() {
        Ok(Some(status)) => return format!("worker exited ({status})"),
        Err(error) => return format!("worker wait failed: {error}"),
        Ok(None) => {}
    }
    // This Child is still unreaped, so its PID cannot have been recycled.
    let signal = unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
    if signal != 0 {
        return format!(
            "could not request worker recovery: {}",
            io::Error::last_os_error()
        );
    }
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return format!("worker exited after SIGTERM ({status})"),
            Err(error) => return format!("worker wait failed: {error}"),
            Ok(None) => {}
        }
        if Instant::now() >= deadline {
            return format!(
                "SIGTERM requested recovery; worker {} has not exited within {}s; inspect sudo m1fan daemon-status and /var/run/m1fan/daemon.log",
                child.id(),
                timeout.as_secs()
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn spawn_detached(command: &mut Command) -> Result<(Child, UnixStream)> {
    let (parent, child_stream) =
        UnixStream::pair().map_err(|e| io_error("create startup channel", e))?;
    parent
        .set_nonblocking(true)
        .map_err(|e| io_error("configure parent startup socket", e))?;
    let inherited_fd = child_stream.as_raw_fd();
    command
        .env(STARTUP_FD, inherited_fd.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // pre_exec runs after fork: use only async-signal-safe libc calls here.
    unsafe {
        command.pre_exec(move || {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            if libc::fcntl(inherited_fd, libc::F_SETFD, 0) == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = command
        .spawn()
        .map_err(|e| io_error("spawn m1fan worker", e))?;
    drop(child_stream);
    Ok((child, parent))
}

pub fn start(trace: Option<&Path>) -> Result<String> {
    root()?;
    Runtime::system().check(true)?;
    let executable = std::env::current_exe().map_err(|e| io_error("find m1fan executable", e))?;
    let mut command = Command::new(executable);
    command.arg("_worker");
    if let Some(path) = trace {
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .map_err(|e| io_error("resolve trace path", e))?
                .join(path)
        };
        command.arg("--trace").arg(absolute);
    }
    let (mut child, mut parent) = spawn_detached(&mut command)?;
    match await_startup(&mut parent, START_TIMEOUT) {
        Ok(()) => Ok(format!(
            "background temperature control running (PID {}); log: /var/run/m1fan/daemon.log",
            child.id()
        )),
        Err(error) => {
            drop(parent);
            let cancellation = cancel_child(&mut child, STOP_TIMEOUT);
            Err(Error::Control(format!("{error}; {cancellation}")))
        }
    }
}

fn request(runtime: &Runtime, command: &str, timeout: Duration) -> Result<String> {
    runtime.check(false)?;
    runtime.check_socket()?;
    let mut stream = connect_bounded(&runtime.socket(), timeout)
        .map_err(|e| io_error("connect daemon (is it running?)", e))?;
    authenticate(&stream, runtime.uid)?;
    stream
        .set_nonblocking(true)
        .map_err(|e| io_error("configure daemon client", e))?;
    let deadline = Instant::now() + timeout;
    send_bounded(&mut stream, command.as_bytes(), deadline, true)?;
    let mut response = Vec::new();
    let mut buffer = [0; 1024];
    loop {
        if platform::stopped() {
            return Err(Error::Interrupted);
        }
        if Instant::now() >= deadline {
            return Err(Error::Control(
                "daemon reply timed out; stop/recovery may still be in progress".into(),
            ));
        }
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => {
                response.extend_from_slice(&buffer[..n]);
                if response.len() > MAX_RESPONSE {
                    return Err(Error::Control("daemon response too large".into()));
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => pause(deadline, true)?,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(io_error("read daemon reply", e)),
        }
    }
    let response =
        String::from_utf8(response).map_err(|_| Error::Control("non-UTF8 daemon reply".into()))?;
    if let Some(message) = response.strip_prefix("OK\n") {
        Ok(message.trim_end().to_owned())
    } else if let Some(message) = response.strip_prefix("ERR\n") {
        Err(Error::Control(message.trim_end().to_owned()))
    } else {
        Err(Error::Control(
            "daemon disconnected without a verified reply".into(),
        ))
    }
}

pub fn stop() -> Result<String> {
    root()?;
    request(&Runtime::system(), "stop\n", STOP_TIMEOUT)
}

pub fn status() -> Result<String> {
    root()?;
    let runtime = Runtime::system();
    match request(&runtime, "status\n", CLIENT_TIMEOUT) {
        Ok(message) => Ok(message),
        Err(error) => {
            // A last-status file is historical; never present it as a live reply.
            if let Ok(message) = runtime.read_final()
                && !message.is_empty()
            {
                return Ok(format!(
                    "worker unavailable: {error}\nLast recorded exit: {message}"
                ));
            }
            Err(error)
        }
    }
}

struct Client {
    stream: UnixStream,
    input: Vec<u8>,
    output: Vec<u8>,
    sent: usize,
    deadline: Instant,
    stopping: bool,
}

impl Client {
    fn respond(&mut self, message: &str) {
        self.output = message.as_bytes().to_vec();
        self.deadline = Instant::now() + CLIENT_TIMEOUT;
    }

    fn flush(&mut self) -> bool {
        if self.output.is_empty() {
            return true;
        }
        match self.stream.write(&self.output[self.sent..]) {
            Ok(0) => false,
            Ok(n) => {
                self.sent += n;
                self.sent < self.output.len()
            }
            Err(e) => {
                e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::Interrupted
            }
        }
    }
}

pub struct WorkerIpc {
    runtime: Runtime,
    listener: UnixListener,
    clients: Vec<Client>,
    startup: Option<UnixStream>,
    socket_identity: (u64, u64),
    stop_requested: bool,
    service: bool,
    ready: bool,
    healthy: bool,
    instance: String,
}

impl WorkerIpc {
    /// Call after acquiring the shared control lock, before hardware writes.
    pub fn bind() -> Result<Self> {
        root()?;
        let mut ipc = Self::bind_at(Runtime::system())?;
        ipc.startup = startup_channel()?;
        if ipc.startup.is_none() {
            return Err(Error::Control(
                "_worker must be launched with m1fan start".into(),
            ));
        }
        Ok(ipc)
    }

    /// Bind a foreground service after acquiring the shared control lock. This
    /// mode is supervised directly by launchd and has no startup parent/socket.
    pub fn bind_service() -> Result<Self> {
        root()?;
        let mut ipc = Self::bind_at(Runtime::system())?;
        ipc.service = true;
        Ok(ipc)
    }

    fn bind_at(runtime: Runtime) -> Result<Self> {
        runtime.check(true)?;
        if let Ok(meta) = fs::symlink_metadata(runtime.socket()) {
            runtime.check_socket()?;
            match connect_bounded(&runtime.socket(), CLIENT_TIMEOUT) {
                Ok(_) => {
                    return Err(Error::Control(
                        "a daemon IPC socket is already listening".into(),
                    ));
                }
                Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => {
                    // The caller holds ControlLock. Check identity before removing
                    // the stale socket, and never remove an arbitrary path type.
                    let current = fs::symlink_metadata(runtime.socket())
                        .map_err(|e| io_error("reinspect stale socket", e))?;
                    if current.ino() != meta.ino() || current.dev() != meta.dev() {
                        return Err(Error::Control(
                            "daemon socket changed during inspection".into(),
                        ));
                    }
                    fs::remove_file(runtime.socket())
                        .map_err(|e| io_error("remove stale daemon socket", e))?;
                }
                Err(e) => return Err(io_error("probe existing daemon socket", e)),
            }
        }
        let listener =
            UnixListener::bind(runtime.socket()).map_err(|e| io_error("bind daemon socket", e))?;
        fs::set_permissions(runtime.socket(), fs::Permissions::from_mode(0o600))
            .map_err(|e| io_error("protect daemon socket", e))?;
        listener
            .set_nonblocking(true)
            .map_err(|e| io_error("configure daemon listener", e))?;
        let meta = fs::symlink_metadata(runtime.socket())
            .map_err(|e| io_error("inspect bound socket", e))?;
        Ok(Self {
            runtime,
            listener,
            clients: Vec::new(),
            startup: None,
            socket_identity: (meta.dev(), meta.ino()),
            stop_requested: false,
            service: false,
            ready: false,
            healthy: true,
            instance: format!(
                "{:x}-{:x}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos()
            ),
        })
    }

    pub fn set_instance(&mut self, instance: &str) -> Result<()> {
        if instance.is_empty()
            || instance.len() > 128
            || !instance
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-')
        {
            return Err(Error::Invalid("invalid service instance identifier".into()));
        }
        self.instance = instance.to_owned();
        Ok(())
    }

    pub fn ready(&mut self) -> Result<()> {
        if self.service {
            self.ready = true;
            return Ok(());
        }
        let Some(mut stream) = self.startup.take() else {
            return Err(Error::Control("startup channel unavailable".into()));
        };
        let deadline = Instant::now() + Duration::from_secs(3);
        send_bounded(&mut stream, b"READY\n", deadline, true)?;
        if read_line(&mut stream, deadline, true)? != "ACK" {
            return Err(Error::Control(
                "startup parent did not acknowledge readiness".into(),
            ));
        }
        send_bounded(&mut stream, b"RUNNING\n", deadline, true)?;
        self.ready = true;
        Ok(())
    }

    /// Readiness in identity describes current health, independently of the
    /// one-shot background-worker startup handshake.
    pub fn set_healthy(&mut self, healthy: bool) {
        self.healthy = healthy;
    }

    /// Poll once per control iteration. All clients are nonblocking and bounded;
    /// abandoned status connections never cause a control failure.
    pub fn poll(&mut self, status: &str) -> Result<bool> {
        if status.len() + 4 > MAX_RESPONSE {
            return Err(Error::Diagnostic("daemon status exceeds IPC limit".into()));
        }
        for _ in 0..MAX_CLIENTS {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    if self.clients.len() >= MAX_CLIENTS
                        || authenticate(&stream, self.runtime.uid).is_err()
                    {
                        continue;
                    }
                    if stream.set_nonblocking(true).is_err() {
                        continue;
                    }
                    self.clients.push(Client {
                        stream,
                        input: Vec::new(),
                        output: Vec::new(),
                        sent: 0,
                        deadline: Instant::now() + CLIENT_TIMEOUT,
                        stopping: false,
                    });
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(io_error("accept daemon client", e)),
            }
        }
        for client in &mut self.clients {
            if client.stopping || !client.output.is_empty() {
                continue;
            }
            let mut buffer = [0; MAX_REQUEST + 1];
            match client.stream.read(&mut buffer) {
                Ok(0) => {
                    client.deadline = Instant::now();
                }
                Ok(n) => {
                    client.input.extend_from_slice(&buffer[..n]);
                    if client.input.len() > MAX_REQUEST {
                        client.respond("ERR\nrequest too large\n");
                    } else if client.input.contains(&b'\n') {
                        match client.input.as_slice() {
                            b"status\n" => client.respond(&format!("OK\n{status}\n")),
                            b"identity\n" => client.respond(&format!(
                                "OK\nprotocol=2\nversion={}\npid={}\nservice={}\nready={}\ninstance={}\n",
                                env!("CARGO_PKG_VERSION"),std::process::id(),
                                self.service,
                                self.ready && self.healthy,self.instance
                            )),
                            b"stop\n" => {
                                client.stopping = true;
                                self.stop_requested = true;
                            }
                            _ => client.respond("ERR\nunknown request\n"),
                        }
                    }
                }
                Err(e)
                    if e.kind() == io::ErrorKind::WouldBlock
                        || e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => {
                    client.deadline = Instant::now();
                }
            }
        }
        self.clients.retain_mut(|client| {
            if client.stopping {
                return true;
            }
            Instant::now() < client.deadline && client.flush()
        });
        Ok(self.stop_requested)
    }

    /// `Ok(())` means the caller has completed and verified automatic restoration.
    /// IPC failure must never prevent the caller from performing that restoration.
    pub fn finish(&mut self, result: &Result<()>) -> Result<()> {
        if let Some(mut stream) = self.startup.take() {
            let detail = match result {
                Ok(()) => "worker stopped before becoming ready".into(),
                Err(e) => e.to_string(),
            };
            let _ = send_bounded(
                &mut stream,
                format!("ERROR {}\n", one_line(&detail)).as_bytes(),
                Instant::now() + Duration::from_secs(1),
                false,
            );
        }
        let final_message = match result {
            Ok(()) => "OK\nautomatic restoration verified; background worker stopped\n".to_owned(),
            Err(error) => format!("ERR\nworker stopped: {}\n", one_line(&error.to_string())),
        };
        let saved = self.runtime.store_final(&final_message);
        let deadline = Instant::now() + Duration::from_secs(1);
        for client in &mut self.clients {
            if client.stopping {
                let _ = send_bounded(
                    &mut client.stream,
                    final_message.as_bytes(),
                    deadline,
                    false,
                );
            }
        }
        self.clients.clear();
        saved
    }
}

impl Drop for WorkerIpc {
    fn drop(&mut self) {
        if let Ok(meta) = fs::symlink_metadata(self.runtime.socket())
            && meta.file_type().is_socket()
            && (meta.dev(), meta.ino()) == self.socket_identity
        {
            let _ = fs::remove_file(self.runtime.socket());
        }
    }
}

/// Two files, each at most 2 MiB: daemon.log and daemon.log.1. SMC traces use
/// the separate explicit --trace destination and are not sent through this log.
pub struct DaemonLog {
    runtime: Runtime,
    file: File,
    size: u64,
    limit: u64,
}

impl DaemonLog {
    pub fn open() -> Result<Self> {
        root()?;
        Self::open_at(Runtime::system(), LOG_LIMIT)
    }

    fn open_at(runtime: Runtime, limit: u64) -> Result<Self> {
        runtime.check(true)?;
        let mut file = runtime.file("daemon.log")?;
        let size = file
            .metadata()
            .map_err(|e| io_error("inspect log size", e))?
            .len();
        use std::io::Seek;
        file.seek(io::SeekFrom::End(0))
            .map_err(|e| io_error("seek daemon log", e))?;
        Ok(Self {
            runtime,
            file,
            size,
            limit,
        })
    }

    fn rotate(&mut self) -> io::Result<()> {
        self.file.flush()?;
        let previous = self.runtime.path.join("daemon.log.1");
        match fs::symlink_metadata(&previous) {
            Ok(meta) => {
                if !meta.is_file()
                    || meta.uid() != self.runtime.uid
                    || meta.nlink() != 1
                    || meta.mode() & 0o777 != 0o600
                {
                    return Err(io::Error::other("unsafe previous daemon log"));
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        fs::rename(self.runtime.path.join("daemon.log"), previous)?;
        self.file = self.runtime.file("daemon.log").map_err(io::Error::other)?;
        self.size = 0;
        Ok(())
    }

    pub fn line(&mut self, message: &str) -> Result<()> {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        writeln!(self, "{timestamp} {}", one_line(message))
            .map_err(|e| io_error("write daemon log", e))?;
        self.flush().map_err(|e| io_error("flush daemon log", e))
    }
}

impl Write for DaemonLog {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        let length = bytes.len().min(self.limit as usize);
        if self.size + length as u64 > self.limit {
            self.rotate()?;
        }
        let written = self.file.write(&bytes[..length])?;
        self.size += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;
    static NEXT: AtomicU64 = AtomicU64::new(0);

    struct Fixture {
        runtime: Runtime,
    }
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "m1fan-daemon-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::SeqCst)
            ));
            let runtime = Runtime {
                path,
                uid: unsafe { libc::geteuid() },
            };
            runtime.check(true).unwrap();
            Self { runtime }
        }
        fn bind(&self) -> WorkerIpc {
            WorkerIpc::bind_at(self.runtime.clone()).unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.runtime.path);
        }
    }

    fn drive(ipc: &mut WorkerIpc, until: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !until() {
            ipc.poll("fan 0 manual; target 7000; actual 7100").unwrap();
            assert!(Instant::now() < deadline, "IPC test deadline");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn runtime_rejects_symlinks_and_group_access() {
        let fixture = Fixture::new();
        fs::set_permissions(&fixture.runtime.path, fs::Permissions::from_mode(0o750)).unwrap();
        assert!(fixture.runtime.check(false).is_err());
        fs::set_permissions(&fixture.runtime.path, fs::Permissions::from_mode(0o700)).unwrap();
        let link = fixture.runtime.path.join("alias");
        std::os::unix::fs::symlink(&fixture.runtime.path, &link).unwrap();
        assert!(
            Runtime {
                path: link,
                uid: fixture.runtime.uid
            }
            .check(false)
            .is_err()
        );
    }

    #[test]
    fn runtime_rejects_hardlinked_state_before_truncation() {
        let fixture = Fixture::new();
        let mut file = fixture.runtime.file("last-status").unwrap();
        file.write_all(b"original evidence").unwrap();
        fs::hard_link(
            fixture.runtime.path.join("last-status"),
            fixture.runtime.path.join("second"),
        )
        .unwrap();
        assert!(fixture.runtime.store_final("replacement").is_err());
        assert_eq!(
            fs::read_to_string(fixture.runtime.path.join("second")).unwrap(),
            "original evidence"
        );
    }

    #[test]
    fn duplicate_worker_keeps_original_listener() {
        let fixture = Fixture::new();
        let _ipc = fixture.bind();
        assert!(WorkerIpc::bind_at(fixture.runtime.clone()).is_err());
        assert!(fixture.runtime.socket().exists());
    }

    #[test]
    fn stale_socket_is_replaced_and_removed_on_drop() {
        let fixture = Fixture::new();
        let stale = UnixListener::bind(fixture.runtime.socket()).unwrap();
        fs::set_permissions(fixture.runtime.socket(), fs::Permissions::from_mode(0o600)).unwrap();
        drop(stale);
        let ipc = fixture.bind();
        assert!(fixture.runtime.socket().exists());
        drop(ipc);
        assert!(!fixture.runtime.socket().exists());
    }

    #[test]
    fn socket_symlink_is_rejected() {
        let fixture = Fixture::new();
        let target = fixture.runtime.path.join("untouched");
        fs::write(&target, b"keep").unwrap();
        std::os::unix::fs::symlink(&target, fixture.runtime.socket()).unwrap();
        assert!(WorkerIpc::bind_at(fixture.runtime.clone()).is_err());
        assert_eq!(fs::read(target).unwrap(), b"keep");
    }

    #[test]
    fn status_returns_last_frame_and_abandoned_client_is_ignored() {
        let fixture = Fixture::new();
        let mut ipc = fixture.bind();
        drop(UnixStream::connect(fixture.runtime.socket()).unwrap());
        let runtime = fixture.runtime.clone();
        let task =
            std::thread::spawn(move || request(&runtime, "status\n", Duration::from_secs(2)));
        drive(&mut ipc, || task.is_finished());
        assert_eq!(
            task.join().unwrap().unwrap(),
            "fan 0 manual; target 7000; actual 7100"
        );
    }

    #[test]
    fn stop_waits_for_verified_recovery_result() {
        let fixture = Fixture::new();
        let mut ipc = fixture.bind();
        let runtime = fixture.runtime.clone();
        let task = std::thread::spawn(move || request(&runtime, "stop\n", Duration::from_secs(2)));
        let deadline = Instant::now() + Duration::from_secs(1);
        while !ipc.poll("still manual").unwrap() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(
            !task.is_finished(),
            "stop cannot acknowledge before cleanup"
        );
        ipc.finish(&Ok(())).unwrap();
        assert!(
            task.join()
                .unwrap()
                .unwrap()
                .contains("automatic restoration verified")
        );
        assert!(
            fs::read_to_string(fixture.runtime.path.join("last-status"))
                .unwrap()
                .starts_with("OK\n")
        );
    }

    #[test]
    fn failed_recovery_never_replies_success() {
        let fixture = Fixture::new();
        let mut ipc = fixture.bind();
        let runtime = fixture.runtime.clone();
        let task = std::thread::spawn(move || request(&runtime, "stop\n", Duration::from_secs(2)));
        let deadline = Instant::now() + Duration::from_secs(1);
        while !ipc.poll("manual").unwrap() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(2));
        }
        ipc.finish(&Err(Error::Control("restoration mismatch".into())))
            .unwrap();
        assert!(
            task.join()
                .unwrap()
                .unwrap_err()
                .to_string()
                .contains("restoration mismatch")
        );
    }

    #[test]
    fn oversized_and_slow_clients_do_not_block_poll() {
        let fixture = Fixture::new();
        let mut ipc = fixture.bind();
        let mut large = UnixStream::connect(fixture.runtime.socket()).unwrap();
        large.write_all(&[b'x'; MAX_REQUEST + 1]).unwrap();
        let _slow = UnixStream::connect(fixture.runtime.socket()).unwrap();
        let started = Instant::now();
        assert!(!ipc.poll("healthy").unwrap());
        assert!(started.elapsed() < Duration::from_millis(200));
        let mut response = String::new();
        large.read_to_string(&mut response).unwrap();
        assert!(response.contains("request too large"));
        for client in &mut ipc.clients {
            client.deadline = Instant::now();
        }
        ipc.poll("healthy").unwrap();
        assert!(ipc.clients.is_empty());
    }

    #[test]
    fn startup_requires_parent_acknowledgement() {
        let fixture = Fixture::new();
        let mut ipc = fixture.bind();
        let (mut parent, worker) = UnixStream::pair().unwrap();
        parent.set_nonblocking(true).unwrap();
        worker.set_nonblocking(true).unwrap();
        ipc.startup = Some(worker);
        let task = std::thread::spawn(move || await_startup(&mut parent, Duration::from_secs(1)));
        ipc.ready().unwrap();
        task.join().unwrap().unwrap();
    }

    #[test]
    fn startup_disconnect_and_expiry_return_failure() {
        let fixture = Fixture::new();
        let mut ipc = fixture.bind();
        let (parent, worker) = UnixStream::pair().unwrap();
        worker.set_nonblocking(true).unwrap();
        ipc.startup = Some(worker);
        drop(parent);
        assert!(ipc.ready().is_err());
        let (mut parent, _worker) = UnixStream::pair().unwrap();
        parent.set_nonblocking(true).unwrap();
        let started = Instant::now();
        assert!(await_startup(&mut parent, Duration::from_millis(20)).is_err());
        assert!(started.elapsed() < Duration::from_millis(200));
    }

    #[test]
    fn startup_failure_is_propagated_without_ready() {
        let (mut parent, mut worker) = UnixStream::pair().unwrap();
        parent.set_nonblocking(true).unwrap();
        worker.write_all(b"ERROR mock engage failed\n").unwrap();
        assert!(
            await_startup(&mut parent, Duration::from_secs(1))
                .unwrap_err()
                .to_string()
                .contains("mock engage failed")
        );
    }

    #[test]
    fn cancel_child_uses_bounded_sigterm_reap() {
        let mut child = Command::new("/bin/sleep")
            .arg("10")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let result = cancel_child(&mut child, Duration::from_secs(1));
        assert!(result.contains("exited"));
        assert!(child.try_wait().unwrap().is_some());
    }

    #[test]
    fn log_rotation_keeps_two_bounded_files() {
        let fixture = Fixture::new();
        let mut log = DaemonLog::open_at(fixture.runtime.clone(), 64).unwrap();
        for _ in 0..20 {
            log.write_all(b"a bounded line\n").unwrap();
        }
        log.flush().unwrap();
        assert!(
            fs::metadata(fixture.runtime.path.join("daemon.log"))
                .unwrap()
                .len()
                <= 64
        );
        assert!(
            fs::metadata(fixture.runtime.path.join("daemon.log.1"))
                .unwrap()
                .len()
                <= 64
        );
        assert_eq!(fs::read_dir(&fixture.runtime.path).unwrap().count(), 2);
    }
    #[test]
    fn last_status_read_is_readonly_and_rejects_links() {
        let fixture = Fixture::new();
        assert!(fixture.runtime.read_final().is_err());
        assert_eq!(fs::read_dir(&fixture.runtime.path).unwrap().count(), 0);
        fixture.runtime.store_final("previous exit").unwrap();
        assert_eq!(fixture.runtime.read_final().unwrap(), "previous exit");
        let target = fixture.runtime.path.join("target");
        fs::rename(fixture.runtime.path.join("last-status"), &target).unwrap();
        std::os::unix::fs::symlink(target, fixture.runtime.path.join("last-status")).unwrap();
        assert!(fixture.runtime.read_final().is_err());
    }

    #[test]
    fn peer_with_different_uid_is_rejected() {
        let (stream, _other) = UnixStream::pair().unwrap();
        let uid = unsafe { libc::geteuid() };
        assert!(authenticate(&stream, uid).is_ok());
        assert!(matches!(
            authenticate(&stream, uid + 1),
            Err(Error::Permission(_))
        ));
    }

    // Run only by the spawn test below. It exercises the actual inherited-FD
    // and setsid path while doing no SMC access, file mutation or root work.
    #[test]
    fn mock_worker_entry() {
        if std::env::var("M1FAN_DAEMON_TEST_WORKER").as_deref() != Ok("1") {
            return;
        }
        assert_eq!(unsafe { libc::getsid(0) }, unsafe { libc::getpid() });
        let mut bytes = [0];
        assert_eq!(io::stdin().read(&mut bytes).unwrap(), 0);
        let mut stream = startup_channel().unwrap().unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        send_bounded(&mut stream, b"READY\n", deadline, false).unwrap();
        assert_eq!(read_line(&mut stream, deadline, false).unwrap(), "ACK");
        send_bounded(&mut stream, b"RUNNING\n", deadline, false).unwrap();
    }

    #[test]
    fn detached_mock_process_inherits_handshake_without_terminal() {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", "daemon::tests::mock_worker_entry", "--nocapture"])
            .env("M1FAN_DAEMON_TEST_WORKER", "1");
        let (mut child, mut parent) = spawn_detached(&mut command).unwrap();
        let result = await_startup(&mut parent, Duration::from_secs(2));
        if result.is_err() {
            let _ = cancel_child(&mut child, Duration::from_secs(1));
        }
        result.unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success());
                break;
            }
            if Instant::now() >= deadline {
                let _ = cancel_child(&mut child, Duration::from_secs(1));
                panic!("mock worker did not exit");
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    #[test]
    fn diagnostic_messages_are_bounded_by_utf8_bytes() {
        let message = one_line(&"温".repeat(MAX_RESPONSE));
        assert!(message.len() <= MAX_RESPONSE - 128);
        assert!(!one_line("line1\nline2\r").contains(['\n', '\r']));
    }
    #[test]
    fn foreground_service_ready_needs_no_parent_and_is_repeatable() {
        let fixture = Fixture::new();
        let mut ipc = fixture.bind();
        ipc.service = true;
        assert!(ipc.startup.is_none());
        ipc.ready().unwrap();
        ipc.ready().unwrap();
        let runtime = fixture.runtime.clone();
        let task =
            std::thread::spawn(move || request(&runtime, "identity\n", Duration::from_secs(2)));
        drive(&mut ipc, || task.is_finished());
        let identity = task.join().unwrap().unwrap();
        assert!(identity.contains("service=true\nready=true"));
        assert!(identity.contains(&format!("pid={}", std::process::id())));
        for healthy in [false, true] {
            ipc.set_healthy(healthy);
            let runtime = fixture.runtime.clone();
            let task =
                std::thread::spawn(move || request(&runtime, "identity\n", Duration::from_secs(2)));
            drive(&mut ipc, || task.is_finished());
            let identity = task.join().unwrap().unwrap();
            assert!(identity.contains(&format!("service=true\nready={healthy}")));
        }
    }

    #[test]
    fn service_stop_waits_for_final_restoration_even_while_automatic() {
        let fixture = Fixture::new();
        let mut ipc = fixture.bind();
        ipc.service = true;
        ipc.ready().unwrap();
        let runtime = fixture.runtime.clone();
        let task = std::thread::spawn(move || request(&runtime, "stop\n", Duration::from_secs(2)));
        let deadline = Instant::now() + Duration::from_secs(1);
        while !ipc.poll("automatic monitoring; no writes needed").unwrap() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(!task.is_finished());
        ipc.finish(&Ok(())).unwrap();
        assert!(
            task.join()
                .unwrap()
                .unwrap()
                .contains("automatic restoration verified")
        );
    }
}
