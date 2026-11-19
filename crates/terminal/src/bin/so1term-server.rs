//! `so1term-server`: persistent headless terminal session server (Phase 5 T10/T11b).
//!
//! The server owns the shell PTYs and terminal models of one named session and
//! serves the so1term control protocol on a private Unix socket at
//! `<runtime dir>/so1term-server-<session>.sock`. Clients (`so1ctl`, and the
//! so1term window through the attach stream) can connect and disconnect freely;
//! the shells keep running. The server exits when the last pane closes or on
//! `so1ctl shutdown`, removing its socket. It ignores SIGHUP so closing the
//! launching terminal does not end the session.
//!
//! Attach: a control connection whose request is `attach` is upgraded to an
//! attach stream (`so1_term_core::attach`) for at most one client at a time; a
//! second attach is refused with `OPERATION_REFUSED` while one is attached. An
//! attached client receives `Exit` (`shutdown` or `last-shell-exited`) before
//! the server exits.
//!
//! Threading model:
//! - The main thread runs the server loop. It alone owns the `Session` and the
//!   attached client's frame state, and it never writes to an attach socket.
//!   It handles events in batches and, after each batch, flushes one layout
//!   message and one frame per changed visible pane into the client's outbox.
//! - PTY reader threads only signal `Output(pane)` (deduplicated per pane).
//! - Control client threads (one per connection, at most 16) send `Control`
//!   requests and wait up to 30 s for the reply; the attach hook runs on such a
//!   thread, claims the single client slot, writes the control reply, and hands
//!   the stream to the loop.
//! - Per attached client, one reader thread parses lines (16 MiB cap) into
//!   `Client` events. It stops reading while 64 of its events are unprocessed,
//!   which pushes back on a flooding client instead of growing a queue.
//! - Per attached client, one writer thread drains the bounded `Outbox`. The
//!   loop only pushes: when frames do not fit they are dropped and the affected
//!   panes get a full frame once the writer has drained the queue and woken the
//!   loop. Non-frame messages are never dropped; a client that lets more than
//!   1024 of them pile up is detached. Socket writes time out after 30 s.
//! - Detach (message, EOF, I/O error) closes the outbox, shuts the socket down
//!   so both client threads return, joins the writer, and frees the slot.
//! - Kitty graphics (T11d): panes run the shared graphics pipeline; image
//!   decodes run on worker threads that report back as `GraphicsDecoded`
//!   events. Before a frame that places an image, the image is announced and
//!   its data sent in chunks, once per image and attached client (a new attach
//!   starts over). Image lines are never dropped; a client with more than
//!   256 MiB of them queued gets `IMAGE_BACKLOG` and is detached without
//!   waiting for its writer. Freed or replaced image data is reported with
//!   `ImageRemoved` when the client had received it.
//! - Known limit shared with `send-text`: PTY writes for `Input` are blocking,
//!   so a shell that stops reading its terminal can stall the loop.

use std::process::ExitCode;

const USAGE: &str = "Usage: so1term-server [--session NAME]\n\
Runs a persistent so1term session (default name: default) that outlives its clients.\n\
Control it with so1ctl; stop it with `so1ctl shutdown` or by exiting its last shell.";

fn main() -> ExitCode {
    #[cfg(unix)]
    {
        unix::run()
    }
    #[cfg(not(unix))]
    {
        eprintln!("so1term-server: persistent sessions are not supported on this platform yet");
        ExitCode::from(1)
    }
}

/// Parses the command line; `Ok(None)` means help was printed.
fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Option<String>, String> {
    let mut args = args.into_iter();
    let mut session = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(None);
            }
            "--session" if session.is_none() => {
                session = Some(args.next().ok_or("--session requires a name")?);
            }
            "--session" => return Err("--session may only be supplied once".into()),
            other => return Err(format!("unexpected argument: {other}\n{USAGE}")),
        }
    }
    let session = session.unwrap_or_else(|| "default".to_owned());
    so1_terminal::control::validate_session_name(&session)?;
    Ok(Some(session))
}

#[cfg(unix)]
mod unix {
    use so1_terminal::{
        attach::{
            self, AttachError, AttachErrorCode, ClientMessage, MAX_ATTACH_LINE_BYTES, ServerMessage,
        },
        blocks::GridAnchor,
        control::{
            self, AttachConnection, AttachHook, ControlError, ControlErrorCode, ControlOutcome,
            ControlRequest, ControlResponse, ControlServer, serialize_response,
        },
        graphics::GraphicsCompletion,
        layout::PaneId,
        session::{
            self, AttachFrames, ControlEffect, DEFAULT_COLS, DEFAULT_ROWS, DecodeNotifier,
            ImageEvent, Outbox, OutputNotifier, Session,
        },
    };
    use std::{
        collections::BTreeSet,
        io::{self, BufRead, BufReader, Read, Write},
        net::Shutdown,
        os::unix::net::UnixStream,
        path::PathBuf,
        process::ExitCode,
        sync::{
            Arc, Condvar, Mutex, MutexGuard, PoisonError,
            atomic::{AtomicBool, AtomicUsize, Ordering},
            mpsc,
        },
        thread,
        time::{Duration, Instant},
    };

    /// Matches the window's control request timeout.
    const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
    /// Fallback interval for noticing exited shells without new output.
    const REAP_INTERVAL: Duration = Duration::from_millis(250);
    /// Upper bound for letting clients read their final reply before exit.
    const CLIENT_DRAIN: Duration = Duration::from_secs(2);
    /// Most events handled before frames are flushed.
    const MAX_BATCH: usize = 256;
    /// Unprocessed events a client reader may have queued before it pauses.
    const MAX_CLIENT_INFLIGHT: usize = 64;
    /// A client that does not accept a line for this long is detached.
    const CLIENT_WRITE_TIMEOUT: Duration = Duration::from_secs(30);

    const EXIT_SHUTDOWN: &str = "shutdown";
    const EXIT_LAST_SHELL: &str = "last-shell-exited";

    enum Event {
        Output(PaneId),
        /// A kitty image decode finished on its worker thread.
        GraphicsDecoded(PaneId, GridAnchor, GraphicsCompletion),
        Control(ControlRequest, mpsc::SyncSender<ControlResponse>),
        /// An upgraded connection whose client slot the attach hook already claimed.
        Attach(AttachConnection),
        /// Parsed input from the reader thread of client `generation`.
        Client(u64, ClientEvent),
        /// The writer of client `generation` failed.
        WriterFailed(u64),
        /// A writer drained its queue after frames were dropped; the next flush resends them.
        Wake,
    }

    enum ClientEvent {
        Message(ClientMessage),
        Malformed(AttachError),
        Closed,
    }

    type SharedOutbox = Arc<(Mutex<Outbox>, Condvar)>;

    fn lock(outbox: &SharedOutbox) -> MutexGuard<'_, Outbox> {
        outbox.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn unavailable(id: u64) -> ControlResponse {
        ControlResponse {
            id: Some(id),
            outcome: ControlOutcome::Failure(ControlError::for_request(
                Some(id),
                ControlErrorCode::Unavailable,
                "session server is not answering",
            )),
        }
    }

    fn write_control(stream: &mut UnixStream, response: &ControlResponse) -> io::Result<()> {
        let line =
            serialize_response(response).map_err(|error| io::Error::other(error.to_string()))?;
        stream.write_all(&line)
    }

    /// Claims the single client slot, answers the attach request, and hands the
    /// stream to the server loop. Runs on the connection's control thread.
    fn attach_hook(
        session_name: String,
        slot: Arc<AtomicBool>,
        events: mpsc::Sender<Event>,
    ) -> AttachHook {
        Arc::new(move |mut connection: AttachConnection| {
            let id = Some(connection.id);
            if slot
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                let refused = ControlResponse {
                    id,
                    outcome: ControlOutcome::Failure(ControlError::for_request(
                        id,
                        ControlErrorCode::OperationRefused,
                        format!("another client is attached to session {session_name}"),
                    )),
                };
                // The connection closes when the hook returns.
                let _ = write_control(&mut connection.stream, &refused);
                return;
            }
            let attached = ControlResponse {
                id,
                outcome: ControlOutcome::Success(
                    serde_json::json!({"attached": true, "session": session_name}),
                ),
            };
            if write_control(&mut connection.stream, &attached).is_err()
                || events.send(Event::Attach(connection)).is_err()
            {
                slot.store(false, Ordering::Release);
            }
        })
    }

    enum Line {
        Complete,
        TooLarge,
        Eof,
    }

    /// Reads one newline-terminated line of at most `MAX_ATTACH_LINE_BYTES`.
    /// An oversized line is consumed up to its newline and reported as `TooLarge`.
    fn read_line(reader: &mut impl BufRead, line: &mut Vec<u8>) -> io::Result<Line> {
        line.clear();
        let mut too_large = false;
        loop {
            let available = reader.fill_buf()?;
            if available.is_empty() {
                return Ok(Line::Eof);
            }
            let (take, done) = match available.iter().position(|byte| *byte == b'\n') {
                Some(end) => (end + 1, true),
                None => (available.len(), false),
            };
            if too_large || line.len() + take > MAX_ATTACH_LINE_BYTES {
                too_large = true;
                line.clear();
            } else {
                line.extend_from_slice(&available[..take]);
            }
            reader.consume(take);
            if done {
                return Ok(if too_large {
                    Line::TooLarge
                } else {
                    Line::Complete
                });
            }
        }
    }

    fn read_client(
        stream: UnixStream,
        buffered: Vec<u8>,
        generation: u64,
        events: mpsc::Sender<Event>,
        inflight: Arc<AtomicUsize>,
        alive: Arc<AtomicBool>,
    ) {
        let mut reader = BufReader::new(io::Cursor::new(buffered).chain(stream));
        let mut line = Vec::new();
        loop {
            let event = match read_line(&mut reader, &mut line) {
                Ok(Line::Complete) => match attach::parse_client(&line) {
                    Ok(message) => ClientEvent::Message(message),
                    Err(error) => ClientEvent::Malformed(error),
                },
                Ok(Line::TooLarge) => ClientEvent::Malformed(AttachError::new(
                    AttachErrorCode::LineTooLarge,
                    "attach line exceeds 16 MiB",
                )),
                Ok(Line::Eof) | Err(_) => ClientEvent::Closed,
            };
            let last = matches!(
                event,
                ClientEvent::Closed | ClientEvent::Message(ClientMessage::Detach)
            );
            // Backpressure: stop reading while the loop has not caught up.
            while alive.load(Ordering::Acquire)
                && inflight.load(Ordering::Acquire) >= MAX_CLIENT_INFLIGHT
            {
                thread::sleep(Duration::from_millis(2));
            }
            if !alive.load(Ordering::Acquire) {
                return;
            }
            inflight.fetch_add(1, Ordering::AcqRel);
            if events.send(Event::Client(generation, event)).is_err() || last {
                return;
            }
        }
    }

    fn write_client(
        mut stream: UnixStream,
        outbox: SharedOutbox,
        generation: u64,
        events: mpsc::Sender<Event>,
    ) {
        loop {
            let line = {
                let mut guard = lock(&outbox);
                loop {
                    if let Some(line) = guard.pop() {
                        break Some(line);
                    }
                    if guard.closed {
                        break None;
                    }
                    if guard.wake_wanted {
                        guard.wake_wanted = false;
                        let _ = events.send(Event::Wake);
                    }
                    guard = outbox.1.wait(guard).unwrap_or_else(PoisonError::into_inner);
                }
            };
            let Some(line) = line else { return };
            if stream.write_all(&line).is_err() {
                let _ = events.send(Event::WriterFailed(generation));
                return;
            }
        }
    }

    struct Client {
        generation: u64,
        stream: UnixStream,
        outbox: SharedOutbox,
        frames: AttachFrames,
        /// Images whose data this client received (or has queued).
        sent_images: BTreeSet<(PaneId, u32)>,
        welcomed: bool,
        inflight: Arc<AtomicUsize>,
        alive: Arc<AtomicBool>,
        writer: Option<thread::JoinHandle<()>>,
    }

    struct Server {
        name: String,
        session: Session,
        client: Option<Client>,
        generation: u64,
        slot: Arc<AtomicBool>,
        /// Set by `finish`: the client slot then stays claimed so an attach that
        /// races the shutdown is refused instead of accepted and dropped.
        shutting_down: bool,
        events: mpsc::Sender<Event>,
    }

    impl Server {
        /// Handles one event; returns the exit reason when the session must stop.
        fn handle(&mut self, event: Event) -> Option<&'static str> {
            match event {
                Event::Output(pane) => self.session.receive_output(pane),
                Event::GraphicsDecoded(pane, anchor, completion) => {
                    self.session.finish_graphics(pane, anchor, completion)
                }
                Event::Control(request, reply) => {
                    let (response, effect) = self.session.handle_control(request);
                    let _ = reply.send(response);
                    if effect == ControlEffect::Shutdown {
                        return Some(EXIT_SHUTDOWN);
                    }
                }
                Event::Attach(connection) => self.attach(connection),
                Event::Client(generation, event) => {
                    let client = self.current(generation)?;
                    client.inflight.fetch_sub(1, Ordering::AcqRel);
                    match event {
                        ClientEvent::Closed => self.detach(),
                        ClientEvent::Malformed(error) => self.send(&ServerMessage::Error {
                            code: error.code.as_str().to_owned(),
                            message: error.message,
                        }),
                        ClientEvent::Message(message) => self.client_message(message),
                    }
                }
                Event::WriterFailed(generation) => {
                    if self.current(generation).is_some() {
                        self.detach();
                    }
                }
                // The flush after this batch sends the full frames.
                Event::Wake => {}
            }
            None
        }

        fn current(&mut self, generation: u64) -> Option<&mut Client> {
            self.client
                .as_mut()
                .filter(|client| client.generation == generation)
        }

        fn attach(&mut self, connection: AttachConnection) {
            // The hook's slot guarantees no client is attached; stay defensive.
            if let Some(client) = self.client.take() {
                close_client(client);
            }
            self.generation += 1;
            let generation = self.generation;
            let streams = connection
                .stream
                .set_write_timeout(Some(CLIENT_WRITE_TIMEOUT))
                .and_then(|()| {
                    Ok((
                        connection.stream.try_clone()?,
                        connection.stream.try_clone()?,
                    ))
                });
            let Ok((reader_stream, writer_stream)) = streams else {
                let _ = connection.stream.shutdown(Shutdown::Both);
                self.slot.store(false, Ordering::Release);
                return;
            };
            let outbox: SharedOutbox = Arc::new((Mutex::new(Outbox::default()), Condvar::new()));
            let inflight = Arc::new(AtomicUsize::new(0));
            let alive = Arc::new(AtomicBool::new(true));
            let writer = {
                let outbox = outbox.clone();
                let events = self.events.clone();
                thread::Builder::new()
                    .name("so1term-attach-writer".into())
                    .spawn(move || write_client(writer_stream, outbox, generation, events))
            };
            let reader = {
                let events = self.events.clone();
                let inflight = inflight.clone();
                let alive = alive.clone();
                let buffered = connection.buffered;
                thread::Builder::new()
                    .name("so1term-attach-reader".into())
                    .spawn(move || {
                        read_client(reader_stream, buffered, generation, events, inflight, alive)
                    })
            };
            let client = Client {
                generation,
                stream: connection.stream,
                outbox,
                frames: AttachFrames::default(),
                sent_images: BTreeSet::new(),
                welcomed: false,
                inflight,
                alive,
                writer: writer.ok(),
            };
            if client.writer.is_none() || reader.is_err() {
                eprintln!("so1term-server: cannot start attach client threads");
                close_client(client);
                self.slot.store(false, Ordering::Release);
                return;
            }
            self.client = Some(client);
        }

        fn detach(&mut self) {
            if let Some(client) = self.client.take() {
                close_client(client);
            }
            if !self.shutting_down {
                self.slot.store(false, Ordering::Release);
            }
        }

        fn client_message(&mut self, message: ClientMessage) {
            let Some(client) = self.client.as_mut() else {
                return;
            };
            if !client.welcomed {
                match message {
                    ClientMessage::Hello {
                        width,
                        height,
                        cell_width,
                        cell_height,
                    } => match self.session.set_client_geometry(
                        width,
                        height,
                        cell_width,
                        cell_height,
                    ) {
                        Ok(()) => {
                            client.welcomed = true;
                            client.frames.mark_full(None);
                            let welcome = ServerMessage::Welcome {
                                session: self.name.clone(),
                            };
                            self.send(&welcome);
                        }
                        Err(error) => self.send(&session::attach_error(
                            session::ERROR_INVALID_GEOMETRY,
                            error,
                        )),
                    },
                    ClientMessage::Detach => self.detach(),
                    _ => self.send(&ServerMessage::Error {
                        code: "HELLO_REQUIRED".into(),
                        message: "the first attach message must be hello".into(),
                    }),
                }
                return;
            }
            match message {
                ClientMessage::Hello { .. } => self.send(&ServerMessage::Error {
                    code: "UNEXPECTED_HELLO".into(),
                    message: "hello was already received; use resize".into(),
                }),
                ClientMessage::Detach => self.detach(),
                ClientMessage::Resync { pane: None } => client.frames.mark_full(None),
                ClientMessage::Resync { pane: Some(pane) } => {
                    if self.session.has_pane(PaneId(pane)) {
                        client.frames.mark_full(Some(PaneId(pane)));
                    } else {
                        self.send(&session::attach_error(
                            session::ERROR_NOT_FOUND,
                            format!("pane {pane} does not exist"),
                        ));
                    }
                }
                message @ ClientMessage::Resize { .. } => {
                    match self.session.handle_client(message) {
                        Some(error) => self.send(&error),
                        None => {
                            if let Some(client) = self.client.as_mut() {
                                client.frames.mark_full(None);
                            }
                        }
                    }
                }
                message => {
                    if let Some(reply) = self.session.handle_client(message) {
                        self.send(&reply);
                    }
                }
            }
        }

        /// Queues a message that is never dropped; detaches a client that stopped reading.
        fn send(&mut self, message: &ServerMessage) {
            let Some(client) = self.client.as_ref() else {
                return;
            };
            let line = attach::encode_server(message).unwrap_or_else(|error| {
                eprintln!("so1term-server: cannot encode attach message: {error}");
                attach::encode_server(&ServerMessage::Error {
                    code: error.code.as_str().to_owned(),
                    message: error.message,
                })
                .unwrap_or_default()
            });
            let queued = {
                let mut outbox = lock(&client.outbox);
                let queued = outbox.push_message(line);
                client.outbox.1.notify_one();
                queued
            };
            if !queued {
                eprintln!("so1term-server: attached client stopped reading; detaching it");
                self.detach();
            }
        }

        /// Sends image removals, the layout (when changed), the image data the
        /// new frames place, and frames of changed visible panes, in that order.
        fn flush(&mut self) {
            // Drained every time so the events never pile up without a client.
            let image_events = self.session.take_image_events();
            let Some(client) = self.client.as_mut() else {
                return;
            };
            if !client.welcomed {
                return;
            }
            let mut removed = Vec::new();
            for event in image_events {
                // Replaced data is dropped too and re-sent when a frame places it.
                let (ImageEvent::Stored(pane, image) | ImageEvent::Freed(pane, image)) = event;
                if client.sent_images.remove(&(pane, image)) {
                    removed.push(ServerMessage::ImageRemoved {
                        pane: pane.0,
                        image,
                    });
                }
            }
            client
                .sent_images
                .retain(|(pane, _)| self.session.has_pane(*pane));
            let layout = client.frames.layout_message(&self.session);
            let frames = client.frames.frames(&self.session);
            for message in &removed {
                self.send(message);
            }
            if let Some(layout) = layout {
                self.send(&layout);
            }
            if !self.send_placed_images(&frames) {
                return;
            }
            let mut errors = Vec::new();
            if let Some(client) = self.client.as_mut() {
                let mut dropped = Vec::new();
                {
                    let mut outbox = lock(&client.outbox);
                    for frame in frames {
                        let pane = frame.pane;
                        match attach::encode_server(&ServerMessage::Frame(frame)) {
                            Ok(line) => dropped.extend(outbox.push_frame(pane, line)),
                            Err(error) => {
                                eprintln!(
                                    "so1term-server: cannot encode frame for pane {pane}: {error}"
                                );
                                errors.push(ServerMessage::Error {
                                    code: error.code.as_str().to_owned(),
                                    message: format!("frame for pane {pane}: {}", error.message),
                                });
                            }
                        }
                    }
                    client.outbox.1.notify_one();
                }
                for pane in dropped {
                    client.frames.mark_full(Some(PaneId(pane)));
                }
            }
            for error in errors {
                self.send(&error);
            }
        }

        /// Queues announcement and data of every image the frames place that
        /// the client has not received. Returns `false` when the client was
        /// detached because its image backlog exceeded the outbox budget.
        fn send_placed_images(&mut self, frames: &[attach::PaneFrame]) -> bool {
            let Some(client) = self.client.as_mut() else {
                return false;
            };
            let mut overflow = false;
            'frames: for frame in frames {
                for placement in &frame.placements {
                    let key = (PaneId(frame.pane), placement.image);
                    if client.sent_images.contains(&key) {
                        continue;
                    }
                    let Some(image) = self.session.image(key.0, key.1) else {
                        continue;
                    };
                    let messages = std::iter::once(attach::image_announcement(
                        frame.pane,
                        placement.image,
                        image.width,
                        image.height,
                    ))
                    .chain(attach::image_data_messages(
                        frame.pane,
                        placement.image,
                        &image.rgba,
                    ));
                    // The backlog budget is checked once per image, so an image
                    // larger than the budget is still queued whole.
                    if !lock(&client.outbox).can_start_image() {
                        overflow = true;
                        break 'frames;
                    }
                    for message in messages {
                        let line = match attach::encode_server(&message) {
                            Ok(line) => line,
                            Err(error) => {
                                eprintln!(
                                    "so1term-server: cannot encode image {} of pane {}: {error}",
                                    placement.image, frame.pane
                                );
                                continue;
                            }
                        };
                        lock(&client.outbox).push_image(line);
                        client.outbox.1.notify_one();
                    }
                    client.sent_images.insert(key);
                }
            }
            if overflow {
                self.drop_client_for_image_backlog();
            }
            !overflow
        }

        /// Detaches a client that let too much image data queue up. The outbox
        /// is replaced by one error line; the writer sends it and exits on its
        /// own and is not joined, so a stalled client cannot block the loop.
        fn drop_client_for_image_backlog(&mut self) {
            let Some(client) = self.client.take() else {
                return;
            };
            eprintln!(
                "so1term-server: attached client has more than 256 MiB of image data queued; detaching it"
            );
            let error = attach::encode_server(&session::attach_error(
                session::ERROR_IMAGE_BACKLOG,
                "queued image data exceeds 256 MiB; detaching",
            ));
            {
                let mut outbox = lock(&client.outbox);
                outbox.discard_all();
                if let Ok(line) = error {
                    outbox.push_message(line);
                }
                outbox.closed = true;
            }
            client.outbox.1.notify_all();
            client.alive.store(false, Ordering::Release);
            let _ = client.stream.shutdown(Shutdown::Read);
            if !self.shutting_down {
                self.slot.store(false, Ordering::Release);
            }
        }

        /// Tells an attached client why the session ends and lets it read that.
        fn finish(&mut self, reason: &str) {
            self.shutting_down = true;
            self.slot.store(true, Ordering::Release);
            if self.client.is_none() {
                return;
            }
            self.send(&ServerMessage::Exit {
                reason: reason.to_owned(),
            });
            let Some(client) = self.client.as_mut() else {
                return;
            };
            lock(&client.outbox).closed = true;
            client.outbox.1.notify_all();
            let deadline = Instant::now() + CLIENT_DRAIN;
            while client
                .writer
                .as_ref()
                .is_some_and(|writer| !writer.is_finished())
                && Instant::now() < deadline
            {
                thread::sleep(Duration::from_millis(5));
            }
            self.detach();
        }
    }

    /// Stops both client threads: closes the outbox and shuts the socket down.
    fn close_client(mut client: Client) {
        client.alive.store(false, Ordering::Release);
        lock(&client.outbox).closed = true;
        client.outbox.1.notify_all();
        let _ = client.stream.shutdown(Shutdown::Both);
        if let Some(writer) = client.writer.take() {
            let _ = writer.join();
        }
    }

    pub fn run() -> ExitCode {
        let session_name = match super::parse_args(std::env::args().skip(1)) {
            Ok(Some(name)) => name,
            Ok(None) => return ExitCode::SUCCESS,
            Err(message) => {
                eprintln!("so1term-server: {message}");
                return ExitCode::from(2);
            }
        };
        // The session must survive the terminal that launched it.
        unsafe { libc::signal(libc::SIGHUP, libc::SIG_IGN) };

        let (events, receiver) = mpsc::channel::<Event>();
        let control_events = events.clone();
        let slot = Arc::new(AtomicBool::new(false));
        let hook = attach_hook(session_name.clone(), slot.clone(), events.clone());
        let started = control::runtime_directory().and_then(|directory| {
            ControlServer::start_session_with_attach(
                &directory,
                &session_name,
                move |request| {
                    let id = request.id;
                    let (reply, response) = mpsc::sync_channel(1);
                    if control_events.send(Event::Control(request, reply)).is_err() {
                        return unavailable(id);
                    }
                    response
                        .recv_timeout(REQUEST_TIMEOUT)
                        .unwrap_or_else(|_| unavailable(id))
                },
                hook,
            )
        });
        let server = match started {
            Ok(server) => server,
            Err(error) if error.kind() == io::ErrorKind::AddrInUse => {
                eprintln!("so1term-server: session {session_name} is already running");
                return ExitCode::from(1);
            }
            Err(error) => {
                eprintln!("so1term-server: cannot start the control socket: {error}");
                return ExitCode::from(1);
            }
        };
        // Exported before the first shell starts so every pane inherits it.
        unsafe { std::env::set_var("SO1TERM_SOCKET", server.socket_path()) };

        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        let output_events = events.clone();
        let notify: OutputNotifier = Arc::new(move |pane| {
            let _ = output_events.send(Event::Output(pane));
        });
        let decode_events = events.clone();
        let decode: DecodeNotifier = Arc::new(move |pane, anchor, completion| {
            let _ = decode_events.send(Event::GraphicsDecoded(pane, anchor, completion));
        });
        let session = match Session::start(DEFAULT_COLS, DEFAULT_ROWS, cwd, notify, decode) {
            Ok(session) => session,
            Err(error) => {
                eprintln!("so1term-server: {error}");
                return ExitCode::from(1);
            }
        };
        println!(
            "so1term-server: session {session_name} listening on {}",
            server.socket_path().display()
        );

        let mut state = Server {
            name: session_name,
            session,
            client: None,
            generation: 0,
            slot,
            shutting_down: false,
            events,
        };
        let reason = 'serve: loop {
            let first = match receiver.recv_timeout(REAP_INTERVAL) {
                Ok(event) => Some(event),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => break None,
            };
            let batch = first
                .into_iter()
                .chain(std::iter::from_fn(|| receiver.try_recv().ok()))
                .take(MAX_BATCH);
            for event in batch {
                if let Some(reason) = state.handle(event) {
                    break 'serve Some(reason);
                }
            }
            state.session.reap_exited();
            if state.session.is_empty() {
                break Some(EXIT_LAST_SHELL);
            }
            state.flush();
        };
        if let Some(reason) = reason {
            state.finish(reason);
        }
        state.detach();
        // Requests queued behind the final one fail fast instead of waiting for the drain.
        while let Ok(event) = receiver.try_recv() {
            if let Event::Control(request, reply) = event {
                let _ = reply.send(unavailable(request.id));
            }
        }
        drop(state);
        server.wait_for_clients(CLIENT_DRAIN);
        drop(server);
        ExitCode::SUCCESS
    }
}

#[cfg(test)]
mod tests {
    use super::parse_args;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn session_defaults_to_default_and_accepts_one_name() {
        assert_eq!(parse_args(args(&[])), Ok(Some("default".into())));
        assert_eq!(
            parse_args(args(&["--session", "work"])),
            Ok(Some("work".into()))
        );
        assert!(parse_args(args(&["--session"])).is_err());
        assert!(parse_args(args(&["--session", "a", "--session", "b"])).is_err());
        assert!(parse_args(args(&["--bogus"])).is_err());
        assert!(
            parse_args(args(&["--session", "../x"]))
                .unwrap_err()
                .contains("invalid session name")
        );
    }
}
