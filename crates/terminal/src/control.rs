pub use so1_term_core::control::{
    ControlCommand, ControlDirection, ControlError, ControlErrorCode, ControlOutcome,
    ControlRequest, ControlResponse, ListPane, MAX_REQUEST_LINE_BYTES, NO_COMMAND_BLOCK, TextScope,
    block_result, list_pane_entry, parse_request, parse_response, serialize_request,
    serialize_response,
};

/// Longest accepted `so1term-server --session` name.
pub const MAX_SESSION_NAME_BYTES: usize = 64;

/// Accepts 1-64 ASCII letters, digits, `-` and `_`, so the name is safe in a socket file name.
pub fn validate_session_name(session: &str) -> Result<(), String> {
    let valid = !session.is_empty()
        && session.len() <= MAX_SESSION_NAME_BYTES
        && session
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_');
    if valid {
        Ok(())
    } else {
        Err(format!(
            "invalid session name {session:?}: use 1-{MAX_SESSION_NAME_BYTES} ASCII letters, digits, '-' or '_'"
        ))
    }
}

/// Socket file name of a persistent server session inside the runtime directory.
pub fn session_socket_name(session: &str) -> String {
    format!("so1term-server-{session}.sock")
}

#[cfg(unix)]
mod transport {
    use super::*;
    use std::{
        ffi::CString,
        fs,
        io::{self, BufRead, BufReader, Read, Write},
        os::unix::{
            ffi::OsStrExt,
            fs::{FileTypeExt, MetadataExt, PermissionsExt},
            net::{UnixListener, UnixStream},
        },
        path::{Path, PathBuf},
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
        thread,
        time::Duration,
    };

    const MAX_CONNECTIONS: usize = 16;
    /// Socket read/write timeout of one control request, on both ends.
    pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
    static TERMINATION_SOCKET_PATH: std::sync::atomic::AtomicPtr<libc::c_char> =
        std::sync::atomic::AtomicPtr::new(std::ptr::null_mut());

    unsafe extern "C" fn terminate_and_unlink(signal: libc::c_int) {
        let path = TERMINATION_SOCKET_PATH.load(Ordering::Relaxed);
        if !path.is_null() {
            unsafe { libc::unlink(path) };
        }
        unsafe { libc::_exit(128 + signal) };
    }

    /// A control connection whose first unanswered request was `attach`.
    ///
    /// The listener hands it to the session's [`AttachHook`] instead of the
    /// request dispatcher. `buffered` holds bytes the client already sent after
    /// the attach request line (for example a pipelined `Hello`); they belong to
    /// the attach stream and must be read before `stream`. The stream has no
    /// read timeout; its 30 s write timeout is left for the hook to adjust.
    pub struct AttachConnection {
        pub id: u64,
        pub stream: UnixStream,
        pub buffered: Vec<u8>,
    }

    /// Takes over an attach connection; runs on the connection's client thread.
    pub type AttachHook = Arc<dyn Fn(AttachConnection) + Send + Sync>;

    pub struct ControlServer {
        socket_path: PathBuf,
        stopping: Arc<AtomicBool>,
        active: Arc<AtomicUsize>,
        thread: Option<thread::JoinHandle<()>>,
    }

    impl ControlServer {
        /// Starts the per-window listener `so1term-<pid>.sock` in `directory`.
        pub fn start_at<F>(directory: &Path, pid: u32, dispatch: F) -> io::Result<Self>
        where
            F: Fn(ControlRequest) -> ControlResponse + Send + Sync + 'static,
        {
            Self::start_named(directory, &format!("so1term-{pid}.sock"), dispatch, None)
        }

        /// Starts the persistent-server listener for `session` in `directory`.
        ///
        /// A live listener on the same session name fails with `AddrInUse`; a stale
        /// socket left by a killed server is removed and replaced.
        pub fn start_session_at<F>(directory: &Path, session: &str, dispatch: F) -> io::Result<Self>
        where
            F: Fn(ControlRequest) -> ControlResponse + Send + Sync + 'static,
        {
            super::validate_session_name(session)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
            Self::start_named(
                directory,
                &super::session_socket_name(session),
                dispatch,
                None,
            )
        }

        /// Like [`Self::start_session_at`], but `attach` requests upgrade the
        /// connection: the client thread passes it to `attach` and stops reading
        /// control requests from it. Every other request is dispatched as usual.
        pub fn start_session_with_attach<F>(
            directory: &Path,
            session: &str,
            dispatch: F,
            attach: AttachHook,
        ) -> io::Result<Self>
        where
            F: Fn(ControlRequest) -> ControlResponse + Send + Sync + 'static,
        {
            super::validate_session_name(session)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
            Self::start_named(
                directory,
                &super::session_socket_name(session),
                dispatch,
                Some(attach),
            )
        }

        fn start_named<F>(
            directory: &Path,
            file_name: &str,
            dispatch: F,
            attach: Option<AttachHook>,
        ) -> io::Result<Self>
        where
            F: Fn(ControlRequest) -> ControlResponse + Send + Sync + 'static,
        {
            match fs::symlink_metadata(directory) {
                Ok(link_metadata) => {
                    if !link_metadata.file_type().is_dir() {
                        return Err(io::Error::new(
                            io::ErrorKind::PermissionDenied,
                            "control runtime path must be a real directory",
                        ));
                    }
                    validate_directory_metadata(
                        link_metadata.uid(),
                        link_metadata.permissions().mode(),
                        current_uid(),
                    )?;
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    fs::create_dir_all(directory)?;
                    fs::set_permissions(directory, fs::Permissions::from_mode(0o700))?;
                    let metadata = fs::symlink_metadata(directory)?;
                    validate_directory_metadata(
                        metadata.uid(),
                        metadata.permissions().mode(),
                        current_uid(),
                    )?;
                }
                Err(error) => return Err(error),
            }
            let socket_path = directory.join(file_name);
            if socket_path.exists() || fs::symlink_metadata(&socket_path).is_ok() {
                let metadata = fs::symlink_metadata(&socket_path)?;
                if !metadata.file_type().is_socket() {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        "control socket path is not a socket",
                    ));
                }
                match UnixStream::connect(&socket_path) {
                    Ok(_) => {
                        return Err(io::Error::new(
                            io::ErrorKind::AddrInUse,
                            "control socket already has a listener",
                        ));
                    }
                    Err(error)
                        if matches!(
                            error.kind(),
                            io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
                        ) =>
                    {
                        fs::remove_file(&socket_path)?
                    }
                    Err(error) => return Err(error),
                }
            }
            let listener = UnixListener::bind(&socket_path)?;
            fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600))?;
            let cleanup_path = CString::new(socket_path.as_os_str().as_bytes())
                .map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "socket path contains a NUL byte",
                    )
                })?
                .into_raw();
            TERMINATION_SOCKET_PATH.store(cleanup_path, Ordering::Relaxed);
            unsafe {
                libc::signal(
                    libc::SIGTERM,
                    terminate_and_unlink as *const () as libc::sighandler_t,
                )
            };
            listener.set_nonblocking(true)?;
            let stopping = Arc::new(AtomicBool::new(false));
            let stop = stopping.clone();
            let dispatch = Arc::new(dispatch);
            let active = Arc::new(AtomicUsize::new(0));
            let active_for_listener = active.clone();
            let thread = thread::Builder::new()
                .name("so1term-control-listener".into())
                .spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        match listener.accept() {
                            Ok((stream, _)) => {
                                // macOS and the BSDs let accepted sockets inherit
                                // O_NONBLOCK from the listener; workers need blocking I/O.
                                if stream.set_nonblocking(false).is_err() {
                                    continue;
                                }
                                if !peer_uid(&stream)
                                    .is_ok_and(|uid| peer_uid_matches(uid, current_uid()))
                                {
                                    continue;
                                }
                                if active.fetch_add(1, Ordering::AcqRel) >= MAX_CONNECTIONS {
                                    active.fetch_sub(1, Ordering::AcqRel);
                                    continue;
                                }
                                let dispatch = dispatch.clone();
                                let attach = attach.clone();
                                let active_for_client = active.clone();
                                if thread::Builder::new()
                                    .name("so1term-control-client".into())
                                    .spawn(move || {
                                        handle_connection(
                                            stream,
                                            move |request| dispatch(request),
                                            attach.as_ref(),
                                        );
                                        active_for_client.fetch_sub(1, Ordering::AcqRel);
                                    })
                                    .is_err()
                                {
                                    active.fetch_sub(1, Ordering::AcqRel);
                                }
                            }
                            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                                thread::sleep(Duration::from_millis(20))
                            }
                            Err(_) => break,
                        }
                    }
                })?;
            Ok(Self {
                socket_path,
                stopping,
                active: active_for_listener,
                thread: Some(thread),
            })
        }
        pub fn socket_path(&self) -> &Path {
            &self.socket_path
        }

        /// Waits up to `timeout` for connected clients to finish, so a final reply
        /// is written before the process exits. Returns true when none remain.
        pub fn wait_for_clients(&self, timeout: Duration) -> bool {
            let deadline = std::time::Instant::now() + timeout;
            while self.active.load(Ordering::Acquire) > 0 {
                if std::time::Instant::now() >= deadline {
                    return false;
                }
                thread::sleep(Duration::from_millis(5));
            }
            true
        }
    }

    impl Drop for ControlServer {
        fn drop(&mut self) {
            self.stopping.store(true, Ordering::Relaxed);
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
            let _ = fs::remove_file(&self.socket_path);
        }
    }

    fn handle_connection<F: Fn(ControlRequest) -> ControlResponse>(
        stream: UnixStream,
        dispatch: F,
        attach: Option<&AttachHook>,
    ) {
        let _ = stream.set_read_timeout(Some(REQUEST_TIMEOUT));
        let _ = stream.set_write_timeout(Some(REQUEST_TIMEOUT));
        let Ok(reader_stream) = stream.try_clone() else {
            return;
        };
        let mut reader = BufReader::new(reader_stream);
        let mut line = Vec::with_capacity(4096);
        loop {
            line.clear();
            let mut too_large = false;
            loop {
                let available = match reader.fill_buf() {
                    Ok(b) => b,
                    Err(_) => return,
                };
                if available.is_empty() {
                    return;
                }
                if let Some(end) = available.iter().position(|byte| *byte == b'\n') {
                    let take = end + 1;
                    if line.len() + take > MAX_REQUEST_LINE_BYTES {
                        too_large = true;
                    } else {
                        line.extend_from_slice(&available[..take]);
                    }
                    reader.consume(take);
                    break;
                }
                let take = available.len();
                if line.len() + take > MAX_REQUEST_LINE_BYTES {
                    too_large = true;
                } else {
                    line.extend_from_slice(available);
                }
                reader.consume(take);
                if too_large {
                    break;
                }
            }
            if too_large {
                let response = ControlResponse {
                    id: None,
                    outcome: ControlOutcome::Failure(ControlError::new(
                        ControlErrorCode::LineTooLarge,
                        "request line exceeds 1 MiB",
                    )),
                };
                let _ = write_response(reader.get_mut(), &response);
                return;
            }
            let request = match parse_request(&line) {
                Ok(request) => request,
                Err(error) => {
                    let _ = write_response(
                        reader.get_mut(),
                        &ControlResponse {
                            id: error.id,
                            outcome: ControlOutcome::Failure(error),
                        },
                    );
                    return;
                }
            };
            if let (ControlCommand::Attach, Some(attach)) = (&request.command, attach) {
                // The connection becomes an attach stream: no more control requests.
                let buffered = reader.buffer().to_vec();
                let stream = reader.into_inner();
                if stream.set_read_timeout(None).is_err() {
                    return;
                }
                attach(AttachConnection {
                    id: request.id,
                    stream,
                    buffered,
                });
                return;
            }
            let response = dispatch(request);
            if write_response(reader.get_mut(), &response).is_err() {
                return;
            }
        }
    }

    fn write_response(stream: &mut UnixStream, response: &ControlResponse) -> io::Result<()> {
        stream.write_all(
            &serialize_response(response).map_err(|e| io::Error::other(e.to_string()))?,
        )?;
        Ok(())
    }

    /// Sends one request and reads its response, waiting at most
    /// [`REQUEST_TIMEOUT`] for the response.
    pub fn send_request(path: &Path, request: &ControlRequest) -> io::Result<ControlResponse> {
        let stream = UnixStream::connect(path)?;
        stream.set_read_timeout(Some(REQUEST_TIMEOUT))?;
        exchange(stream, request)
    }

    /// Like [`send_request`], but both writing the request and reading the
    /// response time out after `timeout`, which must be non-zero. A timeout
    /// fails with [`io::ErrorKind::WouldBlock`] or [`io::ErrorKind::TimedOut`].
    pub fn send_request_with_timeout(
        path: &Path,
        request: &ControlRequest,
        timeout: Duration,
    ) -> io::Result<ControlResponse> {
        let stream = UnixStream::connect(path)?;
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
        exchange(stream, request)
    }

    fn exchange(mut stream: UnixStream, request: &ControlRequest) -> io::Result<ControlResponse> {
        stream
            .write_all(&serialize_request(request).map_err(|e| io::Error::other(e.to_string()))?)?;
        let reader = BufReader::new(stream);
        let mut line = Vec::new();
        reader
            .take((MAX_REQUEST_LINE_BYTES + 1) as u64)
            .read_until(b'\n', &mut line)?;
        parse_response(&line).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }

    pub fn validate_directory_metadata(owner: u32, mode: u32, expected_uid: u32) -> io::Result<()> {
        if owner != expected_uid || mode & 0o077 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "control runtime directory must be owned by this user and mode 0700",
            ));
        }
        Ok(())
    }
    pub fn peer_uid_matches(peer: u32, expected: u32) -> bool {
        peer == expected
    }
    fn current_uid() -> u32 {
        unsafe { libc::geteuid() }
    }

    #[cfg(target_os = "linux")]
    fn peer_uid(stream: &UnixStream) -> io::Result<u32> {
        use std::os::fd::AsRawFd;
        let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
        let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        let rc = unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&mut cred as *mut libc::ucred).cast(),
                &mut len,
            )
        };
        if rc == 0 {
            Ok(cred.uid)
        } else {
            Err(io::Error::last_os_error())
        }
    }
    #[cfg(any(
        target_os = "macos",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly"
    ))]
    fn peer_uid(stream: &UnixStream) -> io::Result<u32> {
        use std::os::fd::AsRawFd;
        let mut uid = 0;
        let mut gid = 0;
        let rc = unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) };
        if rc == 0 {
            Ok(uid)
        } else {
            Err(io::Error::last_os_error())
        }
    }

    #[cfg(not(any(
        target_os = "linux",
        target_os = "macos",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly"
    )))]
    fn peer_uid(_stream: &UnixStream) -> io::Result<u32> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "peer credential checks are unsupported on this Unix platform",
        ))
    }

    pub fn runtime_directory() -> io::Result<PathBuf> {
        if let Some(path) = std::env::var_os("XDG_RUNTIME_DIR").filter(|value| !value.is_empty()) {
            Ok(PathBuf::from(path).join("so1term"))
        } else {
            Ok(std::env::temp_dir().join(format!("so1term-{}", current_uid())))
        }
    }
}

#[cfg(unix)]
pub use transport::{
    AttachConnection, AttachHook, ControlServer, REQUEST_TIMEOUT, peer_uid_matches,
    runtime_directory, send_request, send_request_with_timeout, validate_directory_metadata,
};

#[cfg(not(unix))]
pub struct ControlServer;

#[cfg(all(test, unix))]
mod tests {
    use super::{
        AttachConnection, ControlCommand, ControlOutcome, ControlRequest, ControlResponse,
        ControlServer, MAX_REQUEST_LINE_BYTES, peer_uid_matches, send_request, serialize_request,
    };
    use serde_json::json;
    use std::{
        fs,
        os::unix::{
            fs::{MetadataExt, PermissionsExt},
            net::UnixListener,
        },
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
    };

    /// Private scratch directory removed when the test finishes.
    struct TestRoot(PathBuf);

    impl std::ops::Deref for TestRoot {
        type Target = PathBuf;

        fn deref(&self) -> &PathBuf {
            &self.0
        }
    }

    impl Drop for TestRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn test_root() -> TestRoot {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let root = std::env::temp_dir().join(format!(
            "so1term-control-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        TestRoot(root)
    }

    fn private_dir(path: &PathBuf) {
        fs::create_dir(path).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[test]
    fn runtime_directory_and_socket_permissions_are_private() {
        let root = test_root();
        let runtime = root.join("runtime");
        let server = ControlServer::start_at(&runtime, 1234, |_| ControlResponse {
            id: Some(1),
            outcome: ControlOutcome::Success(json!({"ready": true})),
        })
        .unwrap();
        assert_eq!(
            fs::metadata(&runtime).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(server.socket_path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn insecure_or_wrong_owner_runtime_directories_are_refused() {
        let root = test_root();
        let broad = root.join("broad");
        fs::create_dir(&broad).unwrap();
        fs::set_permissions(&broad, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(ControlServer::start_at(&broad, 1235, |_| unreachable!()).is_err());

        let wrong_owner = root.join("wrong-owner");
        fs::create_dir(&wrong_owner).unwrap();
        let wrong_uid = current_uid().wrapping_add(1);
        assert!(
            super::validate_directory_metadata(
                fs::metadata(&wrong_owner).unwrap().uid(),
                fs::metadata(&wrong_owner).unwrap().permissions().mode(),
                wrong_uid
            )
            .is_err()
        );
    }

    #[test]
    fn stale_socket_is_replaced_but_live_socket_is_never_stolen() {
        let root = test_root();
        let stale_dir = root.join("stale");
        private_dir(&stale_dir);
        let stale_path = stale_dir.join("so1term-1236.sock");
        {
            let listener = UnixListener::bind(&stale_path).unwrap();
            drop(listener);
        }
        let stale = ControlServer::start_at(&stale_dir, 1236, |_| unreachable!()).unwrap();
        assert!(stale.socket_path().exists());
        drop(stale);

        let live_dir = root.join("live");
        private_dir(&live_dir);
        let live_path = live_dir.join("so1term-1237.sock");
        let listener = UnixListener::bind(&live_path).unwrap();
        assert!(ControlServer::start_at(&live_dir, 1237, |_| unreachable!()).is_err());
        assert!(live_path.exists());
        drop(listener);
    }

    #[test]
    fn peer_uid_check_accepts_only_the_expected_user() {
        assert!(peer_uid_matches(1000, 1000));
        assert!(!peer_uid_matches(1001, 1000));
    }

    #[test]
    fn oversized_line_returns_an_error_and_closes_the_connection() {
        let root = test_root();
        let server =
            ControlServer::start_at(&root.join("oversized"), 1238, |_| unreachable!()).unwrap();
        let mut stream = std::os::unix::net::UnixStream::connect(server.socket_path()).unwrap();
        let bytes = vec![b'x'; MAX_REQUEST_LINE_BYTES + 1];
        let _ = std::io::Write::write_all(&mut stream, &bytes);
        let mut response = String::new();
        std::io::Read::read_to_string(&mut stream, &mut response).unwrap();
        assert!(response.contains("LINE_TOO_LARGE"));
        assert_eq!(
            response.lines().count(),
            1,
            "connection must close after overflow"
        );
    }

    #[test]
    fn so1ctl_client_round_trips_through_a_real_listener_and_fake_dispatcher() {
        let root = test_root();
        let server = ControlServer::start_at(&root.join("integration"), 1239, |request| {
            assert_eq!(request.command, ControlCommand::List);
            ControlResponse {
                id: Some(request.id),
                outcome: ControlOutcome::Success(json!({"tabs": []})),
            }
        })
        .unwrap();
        let response = send_request(
            server.socket_path(),
            &ControlRequest {
                id: 42,
                command: ControlCommand::List,
            },
        )
        .unwrap();
        assert_eq!(response.id, Some(42));
        assert_eq!(
            response.outcome,
            ControlOutcome::Success(json!({"tabs": []}))
        );
    }

    #[test]
    fn attach_requests_are_handed_to_the_hook_with_buffered_bytes() {
        use std::io::{Read, Write};
        let root = test_root();
        let (sender, receiver) = std::sync::mpsc::channel();
        let server = ControlServer::start_session_with_attach(
            &root.join("attach"),
            "hook",
            |request| ControlResponse {
                id: Some(request.id),
                outcome: ControlOutcome::Success(json!({"dispatched": true})),
            },
            std::sync::Arc::new(move |connection: AttachConnection| {
                let _ = sender.send(connection);
            }),
        )
        .unwrap();
        // Other requests still go through the dispatcher.
        let response = send_request(
            server.socket_path(),
            &ControlRequest {
                id: 1,
                command: ControlCommand::List,
            },
        )
        .unwrap();
        assert_eq!(
            response.outcome,
            ControlOutcome::Success(json!({"dispatched": true}))
        );

        let mut client = std::os::unix::net::UnixStream::connect(server.socket_path()).unwrap();
        client
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let mut bytes = serialize_request(&ControlRequest {
            id: 5,
            command: ControlCommand::Attach,
        })
        .unwrap();
        bytes.extend_from_slice(b"after-attach\n");
        client.write_all(&bytes).unwrap();
        let mut connection = receiver
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        assert_eq!(connection.id, 5);
        assert_eq!(connection.stream.read_timeout().unwrap(), None);
        // Bytes after the attach line arrive either pre-buffered or on the stream.
        let mut received = connection.buffered.clone();
        connection
            .stream
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        while !received.ends_with(b"\n") {
            let mut chunk = [0; 64];
            let read = connection.stream.read(&mut chunk).unwrap();
            assert!(read > 0);
            received.extend_from_slice(&chunk[..read]);
        }
        assert_eq!(received, b"after-attach\n");
        connection.stream.write_all(b"pong\n").unwrap();
        let mut reply = [0; 5];
        client.read_exact(&mut reply).unwrap();
        assert_eq!(&reply, b"pong\n");
    }

    #[test]
    fn window_listeners_dispatch_attach_requests() {
        let root = test_root();
        let server = ControlServer::start_at(&root.join("window"), 1240, |request| {
            assert_eq!(request.command, ControlCommand::Attach);
            ControlResponse {
                id: Some(request.id),
                outcome: ControlOutcome::Success(json!({"refused": true})),
            }
        })
        .unwrap();
        let response = send_request(
            server.socket_path(),
            &ControlRequest {
                id: 3,
                command: ControlCommand::Attach,
            },
        )
        .unwrap();
        assert_eq!(response.id, Some(3));
        assert_eq!(
            response.outcome,
            ControlOutcome::Success(json!({"refused": true}))
        );
    }

    fn current_uid() -> u32 {
        unsafe { libc::geteuid() }
    }
}
