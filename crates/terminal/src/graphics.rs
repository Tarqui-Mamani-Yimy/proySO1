//! Bounded kitty graphics protocol ingestion and display-command decoding.

use base64::{Engine as _, engine::general_purpose::STANDARD};
use flate2::{Decompress, FlushDecompress, Status};
use so1_terminal::layout::PaneId;
use std::{
    collections::{BTreeMap, VecDeque},
    fs::{File, OpenOptions},
    io::{Cursor, Read},
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
};

pub const PANE_LIMIT: usize = 128 * 1024 * 1024;
pub const PROCESS_LIMIT: usize = 512 * 1024 * 1024;
pub const DIMENSION_LIMIT: u32 = 10_000;
const CHUNK_LIMIT: usize = PANE_LIMIT.div_ceil(3) * 4 + 8;
/// Display requests are accepted only while placement and textured rendering are enabled.
pub const GRAPHICS_DISPLAY_ENABLED: bool = true;
const SUPPORTED_KEYS: &[&str] = &[
    "a", "d", "f", "i", "I", "s", "v", "t", "o", "S", "O", "m", "q", "p", "x", "y", "w", "h", "c",
    "r", "X", "Y", "z", "C",
];

/// Canonical filesystem roots captured once when the terminal starts.
#[derive(Clone, Debug, Default)]
pub struct GraphicsFilePolicy {
    allowed_roots: Vec<AllowedRoot>,
    temp_root: Option<AllowedRoot>,
}

#[derive(Clone, Debug)]
struct AllowedRoot {
    path: PathBuf,
    directory: Arc<File>,
}

impl AllowedRoot {
    fn open(path: PathBuf) -> Option<Self> {
        let directory = open_root_directory(&path).ok()?;
        Some(Self {
            path,
            directory: Arc::new(directory),
        })
    }
}

impl GraphicsFilePolicy {
    pub fn from_env() -> Self {
        let roots = std::env::var_os("SO1TERM_GRAPHICS_ALLOWED_ROOTS")
            .into_iter()
            .flat_map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>())
            .filter(|path| !path.as_os_str().is_empty())
            .collect();
        Self::from_roots(roots, Some(std::env::temp_dir()))
    }

    fn from_roots(roots: Vec<PathBuf>, temp: Option<PathBuf>) -> Self {
        let allowed_roots = roots
            .into_iter()
            .filter_map(|path| path.canonicalize().ok())
            .filter(|path| path.is_dir())
            .filter_map(AllowedRoot::open)
            .collect();
        let temp_root = temp
            .and_then(|path| path.canonicalize().ok())
            .and_then(AllowedRoot::open);
        Self {
            allowed_roots,
            temp_root,
        }
    }

    fn startup() -> &'static Self {
        static POLICY: OnceLock<GraphicsFilePolicy> = OnceLock::new();
        POLICY.get_or_init(Self::from_env)
    }
}

impl Default for GraphicsProtocol {
    fn default() -> Self {
        Self::new(GraphicsFilePolicy::startup().clone())
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ControlData(BTreeMap<String, String>);

impl ControlData {
    pub fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).map(String::as_str)
    }
    fn number(&self, key: &str) -> Result<Option<u32>, GraphicsError> {
        self.get(key)
            .map(|v| {
                v.parse()
                    .map_err(|_| GraphicsError::invalid("invalid numeric field"))
            })
            .transpose()
    }
    fn number_u64(&self, key: &str) -> Result<Option<u64>, GraphicsError> {
        self.get(key)
            .map(|v| {
                v.parse()
                    .map_err(|_| GraphicsError::invalid("invalid numeric field"))
            })
            .transpose()
    }
    fn signed_number(&self, key: &str) -> Result<Option<i32>, GraphicsError> {
        self.get(key)
            .map(|v| {
                v.parse()
                    .map_err(|_| GraphicsError::invalid("invalid numeric field"))
            })
            .transpose()
    }
}

pub fn parse_control(bytes: &[u8]) -> Result<ControlData, GraphicsError> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| GraphicsError::invalid("control data is not ASCII"))?;
    if !text.is_ascii() {
        return Err(GraphicsError::invalid("control data is not ASCII"));
    }
    let mut fields = BTreeMap::new();
    if text.is_empty() {
        return Ok(ControlData(fields));
    }
    for field in text.split(',') {
        let (key, value) = field
            .split_once('=')
            .ok_or_else(|| GraphicsError::invalid("expected key=value"))?;
        if key.is_empty()
            || value.is_empty()
            || !key.bytes().all(|b| b.is_ascii_alphanumeric())
            || !value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
            || !SUPPORTED_KEYS.contains(&key)
            || fields.insert(key.to_owned(), value.to_owned()).is_some()
        {
            return Err(GraphicsError::invalid(
                "malformed or unsupported control field",
            ));
        }
    }
    Ok(ControlData(fields))
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GraphicsError {
    code: &'static str,
    message: &'static str,
}

impl GraphicsError {
    fn invalid(message: &'static str) -> Self {
        Self {
            code: "EINVAL",
            message,
        }
    }
    fn unsupported(message: &'static str) -> Self {
        Self {
            code: "ENOTSUP",
            message,
        }
    }
    fn too_large() -> Self {
        Self {
            code: "E2BIG",
            message: "image exceeds configured limits",
        }
    }
    fn decode() -> Self {
        Self {
            code: "EINVAL",
            message: "invalid image data",
        }
    }
    fn permission() -> Self {
        Self {
            code: "EPERM",
            message: "file transmission denied",
        }
    }
    pub fn not_found() -> Self {
        Self {
            code: "ENOENT",
            message: "image not found",
        }
    }
    pub fn invalid_placement() -> Self {
        Self::invalid("invalid image placement geometry")
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResponseContext {
    pub image_id: Option<u32>,
    pub image_number: Option<u32>,
    pub placement_id: Option<u32>,
    pub quiet: u8,
}

impl ResponseContext {
    fn from(control: &ControlData) -> Result<Self, GraphicsError> {
        let image_id = control.number("i")?;
        let image_number = control.number("I")?;
        if image_id.is_some() && image_number.is_some() {
            return Err(GraphicsError::invalid("i and I are mutually exclusive"));
        }
        let quiet = control.number("q")?.unwrap_or(0);
        if quiet > 2 {
            return Err(GraphicsError::invalid("q must be 0, 1, or 2"));
        }
        Ok(Self {
            image_id,
            image_number,
            placement_id: control.number("p")?,
            quiet: quiet as u8,
        })
    }
}

pub fn reply(context: &ResponseContext, result: Result<(), GraphicsError>) -> Option<Vec<u8>> {
    let success = result.is_ok();
    if context.image_id.is_none() && context.image_number.is_none() {
        return None;
    }
    if (success && context.quiet == 1) || (!success && context.quiet == 2) {
        return None;
    }
    let mut out = b"\x1b_G".to_vec();
    if let Some(id) = context.image_id {
        out.extend_from_slice(format!("i={id}").as_bytes());
    }
    if let Some(number) = context.image_number {
        if context.image_id.is_some() {
            out.push(b',');
        }
        out.extend_from_slice(format!("I={number}").as_bytes());
    }
    if let Some(placement_id) = context.placement_id {
        if context.image_id.is_some() || context.image_number.is_some() {
            out.push(b',');
        }
        out.extend_from_slice(format!("p={placement_id}").as_bytes());
    }
    out.push(b';');
    match result {
        Ok(()) => out.extend_from_slice(b"OK"),
        Err(error) => out.extend_from_slice(format!("{}:{}", error.code, error.message).as_bytes()),
    }
    out.extend_from_slice(b"\x1b\\");
    Some(out)
}

#[derive(Debug)]
pub struct DecodeJob {
    pub payload: Vec<u8>,
    control: ControlData,
    pub response: ResponseContext,
    pub action: String,
    pub placement: Option<PlacementOptions>,
    file_policy: GraphicsFilePolicy,
}

pub struct GraphicsCompletion {
    pub response: ResponseContext,
    pub action: String,
    pub placement: Option<PlacementOptions>,
    pub image: Result<DecodedImage, GraphicsError>,
}

pub fn decode_transfer(job: DecodeJob) -> GraphicsCompletion {
    let response = job.response.clone();
    let action = job.action.clone();
    let placement = job.placement;
    let file_medium = matches!(job.control.get("t"), Some("f" | "t"));
    let image = decode(job).map_err(|error| {
        if file_medium {
            // Do not turn path, permission, or file-size probes into distinct replies.
            GraphicsError::permission()
        } else {
            error
        }
    });
    GraphicsCompletion {
        response,
        action,
        placement,
        image,
    }
}

#[derive(Debug)]
pub enum Ingest {
    Ignored,
    Pending,
    Decode(DecodeJob),
    Delete(DeleteCommand),
    Place(PlaceCommand),
    Reply(Option<Vec<u8>>),
}

#[derive(Clone, Copy, Debug, Default)]
pub struct PlacementOptions {
    pub placement_id: Option<u32>,
    pub source: (u32, u32, u32, u32),
    pub cells: (Option<usize>, Option<usize>),
    pub offset: (u32, u32),
    pub z: i32,
    pub no_cursor_move: bool,
}

#[derive(Clone, Debug)]
pub struct PlaceCommand {
    pub image_id: Option<u32>,
    pub image_number: Option<u32>,
    pub response: ResponseContext,
    pub options: PlacementOptions,
}

#[derive(Debug)]
pub struct DeleteCommand {
    pub response: ResponseContext,
    pub selector: crate::placements::DeleteSelector,
    pub frees_data: bool,
}

pub struct GraphicsProtocol {
    chunk: Option<Chunk>,
    decoding: bool,
    file_policy: GraphicsFilePolicy,
}

struct Chunk {
    control: ControlData,
    response: ResponseContext,
    encoded: Vec<u8>,
}

impl GraphicsProtocol {
    pub fn new(file_policy: GraphicsFilePolicy) -> Self {
        Self {
            chunk: None,
            decoding: false,
            file_policy,
        }
    }

    pub fn ingest(&mut self, apc: &[u8]) -> Ingest {
        if apc.first() != Some(&b'G') {
            return Ingest::Ignored;
        }
        let separator = apc.iter().position(|&b| b == b';').unwrap_or(apc.len());
        let control = match parse_control(&apc[1..separator]) {
            Ok(control) => control,
            Err(error) => {
                let context = self
                    .chunk
                    .take()
                    .map(|chunk| chunk.response)
                    .unwrap_or_else(|| response_hint(&apc[1..separator]));
                return Ingest::Reply(reply(&context, Err(error)));
            }
        };
        let response = match ResponseContext::from(&control) {
            Ok(response) => response,
            Err(error) => {
                self.chunk = None;
                let context = response_hint(&apc[1..separator]);
                return Ingest::Reply(reply(&context, Err(error)));
            }
        };
        if self.decoding {
            return Ingest::Reply(reply(
                &response,
                Err(GraphicsError {
                    code: "EBUSY",
                    message: "image decode already in progress",
                }),
            ));
        }
        let encoded = apc.get(separator.saturating_add(1)..).unwrap_or_default();
        let continuation = control.get("m").unwrap_or("0");
        if continuation != "0" && continuation != "1" {
            let context = self
                .chunk
                .take()
                .map(|chunk| chunk.response)
                .unwrap_or(response);
            return Ingest::Reply(reply(
                &context,
                Err(GraphicsError::invalid("m must be 0 or 1")),
            ));
        }
        let starts_new = control.get("a").is_some()
            || control.get("f").is_some()
            || control.get("i").is_some()
            || control.get("I").is_some();
        let (metadata, combined, ctx) = if let Some(mut chunk) = self.chunk.take() {
            if starts_new {
                // A new metadata-bearing transfer aborts the old in-flight sequence.
                let next = encoded.to_vec();
                if next.len() > CHUNK_LIMIT {
                    return Ingest::Reply(reply(&response, Err(GraphicsError::too_large())));
                }
                if continuation == "1" {
                    self.chunk = Some(Chunk {
                        control,
                        response,
                        encoded: next,
                    });
                    return Ingest::Pending;
                }
                (control, next, response)
            } else {
                if control.0.keys().any(|k| k != "m" && k != "q") {
                    return Ingest::Reply(reply(
                        &chunk.response,
                        Err(GraphicsError::invalid("malformed continuation chunk")),
                    ));
                }
                if let Some(q) = control.get("q") {
                    match q.parse::<u8>() {
                        Ok(q @ 0..=2) => chunk.response.quiet = q,
                        _ => {
                            return Ingest::Reply(reply(
                                &chunk.response,
                                Err(GraphicsError::invalid("invalid quiet mode")),
                            ));
                        }
                    }
                }
                if chunk.encoded.len().saturating_add(encoded.len()) > CHUNK_LIMIT {
                    return Ingest::Reply(reply(&chunk.response, Err(GraphicsError::too_large())));
                }
                if STANDARD.decode(encoded).is_err() {
                    return Ingest::Reply(reply(&chunk.response, Err(GraphicsError::decode())));
                }
                chunk.encoded.extend_from_slice(encoded);
                if continuation == "1" {
                    self.chunk = Some(chunk);
                    return Ingest::Pending;
                }
                (chunk.control, chunk.encoded, chunk.response)
            }
        } else if continuation == "0" {
            (control, encoded.to_vec(), response)
        } else {
            let chunk = Chunk {
                control,
                response,
                encoded: encoded.to_vec(),
            };
            if chunk.encoded.len() > CHUNK_LIMIT {
                return Ingest::Reply(reply(&chunk.response, Err(GraphicsError::too_large())));
            }
            self.chunk = Some(chunk);
            return Ingest::Pending;
        };
        if combined.len() > CHUNK_LIMIT {
            return Ingest::Reply(reply(&ctx, Err(GraphicsError::too_large())));
        }
        let action = metadata.get("a").unwrap_or("t").to_owned();
        if action == "d" {
            self.chunk = None;
            if !combined.is_empty() {
                return Ingest::Reply(reply(
                    &ctx,
                    Err(GraphicsError::invalid(
                        "delete action cannot include image data",
                    )),
                ));
            }
            return match parse_delete(&metadata, ctx) {
                Ok(command) => Ingest::Delete(command),
                Err((response, error)) => Ingest::Reply(reply(&response, Err(error))),
            };
        }
        if action == "p" {
            if !combined.is_empty() {
                return Ingest::Reply(reply(
                    &ctx,
                    Err(GraphicsError::invalid(
                        "put action cannot include image data",
                    )),
                ));
            }
            let options = match placement_options(&metadata) {
                Ok(options) => options,
                Err(error) => return Ingest::Reply(reply(&ctx, Err(error))),
            };
            return Ingest::Place(PlaceCommand {
                image_id: ctx.image_id,
                image_number: ctx.image_number,
                response: ctx,
                options,
            });
        }
        if let Err(error) = validate_request(&metadata, &action) {
            return Ingest::Reply(reply(&ctx, Err(error)));
        }
        let decoded = match STANDARD.decode(combined) {
            Ok(payload) if payload.len() <= PANE_LIMIT => payload,
            Ok(_) => return Ingest::Reply(reply(&ctx, Err(GraphicsError::too_large()))),
            Err(_) => return Ingest::Reply(reply(&ctx, Err(GraphicsError::decode()))),
        };
        self.decoding = true;
        let placement = if action == "T" {
            match placement_options(&metadata) {
                Ok(options) => Some(options),
                Err(error) => return Ingest::Reply(reply(&ctx, Err(error))),
            }
        } else {
            None
        };
        Ingest::Decode(DecodeJob {
            payload: decoded,
            control: metadata,
            response: ctx,
            action,
            placement,
            file_policy: self.file_policy.clone(),
        })
    }
    pub fn finish_decode(&mut self) {
        self.decoding = false;
    }
}

fn path_from_bytes(bytes: Vec<u8>) -> Result<PathBuf, GraphicsError> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        Ok(PathBuf::from(std::ffi::OsString::from_vec(bytes)))
    }
    #[cfg(not(unix))]
    {
        String::from_utf8(bytes)
            .map(PathBuf::from)
            .map_err(|_| GraphicsError::invalid("file path is not valid UTF-8"))
    }
}

fn read_file_payload(
    path: &Path,
    control: &ControlData,
    policy: &GraphicsFilePolicy,
    temporary: bool,
) -> Result<Vec<u8>, GraphicsError> {
    if policy.allowed_roots.is_empty() || !path.is_absolute() {
        return Err(GraphicsError::permission());
    }
    #[cfg(windows)]
    {
        let _ = (path, control, policy, temporary);
        Err(GraphicsError::permission())
    }
    #[cfg(not(windows))]
    {
        let canonical = path
            .canonicalize()
            .map_err(|_| GraphicsError::permission())?;
        let allowed_root = policy
            .allowed_roots
            .iter()
            .find(|root| canonical.starts_with(&root.path));
        let temp_root = policy
            .temp_root
            .as_ref()
            .filter(|root| canonical.starts_with(&root.path));
        let root = if temporary {
            allowed_root.or(temp_root)
        } else {
            allowed_root
        }
        .ok_or_else(GraphicsError::permission)?;
        if temporary
            && !canonical
                .to_string_lossy()
                .contains("tty-graphics-protocol")
        {
            return Err(GraphicsError::permission());
        }
        let relative = canonical
            .strip_prefix(&root.path)
            .map_err(|_| GraphicsError::permission())?;
        if relative.as_os_str().is_empty() {
            return Err(GraphicsError::permission());
        }

        let canonical_metadata =
            std::fs::metadata(&canonical).map_err(|_| GraphicsError::permission())?;
        if !canonical_metadata.is_file() {
            return Err(GraphicsError::permission());
        }
        let mut opened = open_beneath(root, relative)?;
        let file = &mut opened.file;
        let opened_metadata = file.metadata().map_err(|_| GraphicsError::permission())?;
        if !opened_metadata.is_file() || !same_file(&canonical_metadata, &opened_metadata) {
            return Err(GraphicsError::permission());
        }
        let offset = control.number_u64("O")?.unwrap_or(0);
        let file_len = opened_metadata.len();
        if offset > file_len {
            return Err(GraphicsError::invalid("file offset is out of bounds"));
        }
        let count = control.number_u64("S")?.unwrap_or(file_len - offset);
        let end = offset
            .checked_add(count)
            .ok_or_else(GraphicsError::permission)?;
        if end > file_len {
            return Err(GraphicsError::permission());
        }
        if count > PANE_LIMIT.min(PROCESS_LIMIT) as u64 {
            return Err(GraphicsError::too_large());
        }
        use std::io::{Seek, SeekFrom};
        file.seek(SeekFrom::Start(offset))
            .map_err(|_| GraphicsError::decode())?;
        let mut bytes = Vec::with_capacity(count as usize);
        file.take(count)
            .read_to_end(&mut bytes)
            .map_err(|_| GraphicsError::permission())?;
        if bytes.len() as u64 != count {
            return Err(GraphicsError::permission());
        }
        if temporary {
            unlink_opened_file(&opened)?;
        }
        Ok(bytes)
    }
}

#[cfg(unix)]
struct OpenedFile {
    file: File,
    parent: std::os::fd::OwnedFd,
    name: std::ffi::CString,
}

#[cfg(unix)]
fn open_root_directory(path: &Path) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
}

#[cfg(not(unix))]
fn open_root_directory(_path: &Path) -> std::io::Result<File> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "no safe directory open",
    ))
}

#[cfg(unix)]
fn open_beneath(root: &AllowedRoot, relative: &Path) -> Result<OpenedFile, GraphicsError> {
    use std::{
        ffi::CString,
        os::{
            fd::{AsRawFd, FromRawFd, OwnedFd},
            unix::ffi::OsStrExt,
        },
        path::Component,
    };
    let components: Vec<_> = relative
        .components()
        .map(|component| match component {
            Component::Normal(name) => {
                CString::new(name.as_bytes()).map_err(|_| GraphicsError::permission())
            }
            _ => Err(GraphicsError::permission()),
        })
        .collect::<Result<_, _>>()?;
    let (last, parents) = components
        .split_last()
        .ok_or_else(GraphicsError::permission)?;
    let root_fd = unsafe { libc::dup(root.directory.as_raw_fd()) };
    if root_fd < 0 {
        return Err(GraphicsError::permission());
    }
    let mut current = unsafe { OwnedFd::from_raw_fd(root_fd) };
    for component in parents {
        let fd = unsafe {
            libc::openat(
                current.as_raw_fd(),
                component.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(GraphicsError::permission());
        }
        current = unsafe { OwnedFd::from_raw_fd(fd) };
    }
    let fd = unsafe {
        libc::openat(
            current.as_raw_fd(),
            last.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(GraphicsError::permission());
    }
    Ok(OpenedFile {
        file: unsafe { File::from_raw_fd(fd) },
        parent: current,
        name: last.clone(),
    })
}

#[cfg(unix)]
fn unlink_opened_file(opened: &OpenedFile) -> Result<(), GraphicsError> {
    use std::{mem::MaybeUninit, os::fd::AsRawFd};
    let mut opened_stat = MaybeUninit::<libc::stat>::uninit();
    if unsafe { libc::fstat(opened.file.as_raw_fd(), opened_stat.as_mut_ptr()) } != 0 {
        return Err(GraphicsError::permission());
    }
    let opened_stat = unsafe { opened_stat.assume_init() };
    let mut current = MaybeUninit::<libc::stat>::uninit();
    if unsafe {
        libc::fstatat(
            opened.parent.as_raw_fd(),
            opened.name.as_ptr(),
            current.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return Err(GraphicsError::permission());
    }
    let current = unsafe { current.assume_init() };
    if opened_stat.st_dev != current.st_dev || opened_stat.st_ino != current.st_ino {
        return Err(GraphicsError::permission());
    }
    if unsafe { libc::unlinkat(opened.parent.as_raw_fd(), opened.name.as_ptr(), 0) } == 0 {
        Ok(())
    } else {
        Err(GraphicsError::permission())
    }
}

#[cfg(not(unix))]
struct OpenedFile {
    file: File,
}

#[cfg(not(unix))]
fn open_beneath(_root: &AllowedRoot, _relative: &Path) -> Result<OpenedFile, GraphicsError> {
    Err(GraphicsError::permission())
}

#[cfg(not(unix))]
fn unlink_opened_file(_opened: &OpenedFile) -> Result<(), GraphicsError> {
    Err(GraphicsError::permission())
}

#[cfg(unix)]
fn same_file(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(not(unix))]
fn same_file(_left: &std::fs::Metadata, _right: &std::fs::Metadata) -> bool {
    false
}

fn response_hint(control: &[u8]) -> ResponseContext {
    let mut image_id = None;
    let mut image_number = None;
    let mut placement_id = None;
    let mut quiet = 0;
    if let Ok(text) = std::str::from_utf8(control) {
        for field in text.split(',') {
            if let Some((key, value)) = field.split_once('=') {
                match key {
                    "i" => image_id = value.parse().ok(),
                    "I" => image_number = value.parse().ok(),
                    "p" => placement_id = value.parse().ok(),
                    "q" => quiet = value.parse::<u8>().ok().filter(|q| *q <= 2).unwrap_or(0),
                    _ => {}
                }
            }
        }
    }
    if image_id.is_some() && image_number.is_some() {
        image_number = None;
    }
    ResponseContext {
        image_id,
        image_number,
        placement_id,
        quiet,
    }
}

fn validate_request(control: &ControlData, action: &str) -> Result<(), GraphicsError> {
    match action {
        "t" | "q" => {}
        "T" | "p" if GRAPHICS_DISPLAY_ENABLED => {}
        "T" | "p" => return Err(GraphicsError::unsupported("image display is not enabled")),
        _ => return Err(GraphicsError::invalid("invalid action")),
    }
    let medium = control.get("t").unwrap_or("d");
    match medium {
        "d" | "f" | "t" => {}
        "s" => return Err(GraphicsError::unsupported("shared memory is not supported")),
        _ => return Err(GraphicsError::invalid("invalid transmission medium")),
    }
    if medium == "f" || medium == "t" {
        if medium == "t" && (control.get("O").is_some() || control.get("S").is_some()) {
            return Err(GraphicsError::invalid(
                "S and O are only supported for file transmission",
            ));
        }
        if control.number_u64("S")? == Some(0) {
            return Err(GraphicsError::invalid("file size must be positive"));
        }
        control.number_u64("O")?;
    } else if control.get("O").is_some() {
        return Err(GraphicsError::invalid(
            "O is only supported for file transmission",
        ));
    }
    let format = control.number("f")?.unwrap_or(32);
    if ![24, 32, 100].contains(&format) {
        return Err(GraphicsError::invalid("unsupported image format"));
    }
    if control.get("o").is_some_and(|o| o != "z") {
        return Err(GraphicsError::invalid("unsupported compression"));
    }
    if format != 100 {
        let width = control
            .number("s")?
            .ok_or_else(|| GraphicsError::invalid("missing width"))?;
        let height = control
            .number("v")?
            .ok_or_else(|| GraphicsError::invalid("missing height"))?;
        checked_pixels(width, height, format)?;
    } else if medium == "d" && control.get("o") == Some("z") && control.number("S")?.is_none() {
        return Err(GraphicsError::invalid("compressed PNG requires S"));
    }
    Ok(())
}

fn placement_options(control: &ControlData) -> Result<PlacementOptions, GraphicsError> {
    let number = |key: &str| control.number(key).map(|v| v.unwrap_or(0));
    let columns = number("c")? as usize;
    let rows = number("r")? as usize;
    let offset_x = number("X")?;
    let offset_y = number("Y")?;
    let cursor_policy = number("C")?;
    if cursor_policy > 1 {
        return Err(GraphicsError::invalid("C must be 0 or 1"));
    }
    Ok(PlacementOptions {
        placement_id: control.number("p")?,
        source: (number("x")?, number("y")?, number("w")?, number("h")?),
        cells: ((columns > 0).then_some(columns), (rows > 0).then_some(rows)),
        offset: (offset_x, offset_y),
        z: control.signed_number("z")?.unwrap_or(0),
        no_cursor_move: cursor_policy == 1,
    })
}

fn parse_delete(
    control: &ControlData,
    response: ResponseContext,
) -> Result<DeleteCommand, (ResponseContext, GraphicsError)> {
    let invalid = |message| (response.clone(), GraphicsError::invalid(message));
    let mode = control.get("d").unwrap_or("a");
    let placement_id = || {
        control
            .number("p")
            .map(|value| value.filter(|id| *id != 0))
            .map_err(|_| invalid("invalid placement id"))
    };
    let selector = match mode {
        "a" | "A" => crate::placements::DeleteSelector::All,
        "i" | "I" => crate::placements::DeleteSelector::Image {
            id: control
                .number("i")
                .map_err(|_| invalid("image id is required"))?
                .ok_or_else(|| invalid("image id is required"))?,
            placement_id: placement_id()?,
        },
        "n" | "N" => crate::placements::DeleteSelector::ImageNumber {
            number: control
                .number("I")
                .map_err(|_| invalid("image number is required"))?
                .ok_or_else(|| invalid("image number is required"))?,
            placement_id: placement_id()?,
        },
        "c" | "C" => crate::placements::DeleteSelector::CursorCell,
        "p" | "P" => crate::placements::DeleteSelector::Cell {
            column: (control
                .number("x")
                .map_err(|_| invalid("x and y are required"))?
                .ok_or_else(|| invalid("x and y are required"))? as usize)
                .saturating_sub(1),
            row: control
                .number("y")
                .map_err(|_| invalid("x and y are required"))?
                .ok_or_else(|| invalid("x and y are required"))? as i64
                - 1,
        },
        "x" | "X" => crate::placements::DeleteSelector::Column {
            column: (control
                .number("x")
                .map_err(|_| invalid("x is required"))?
                .ok_or_else(|| invalid("x is required"))? as usize)
                .saturating_sub(1),
        },
        "y" | "Y" => crate::placements::DeleteSelector::Row {
            row: control
                .number("y")
                .map_err(|_| invalid("y is required"))?
                .ok_or_else(|| invalid("y is required"))? as i64
                - 1,
        },
        "z" | "Z" => crate::placements::DeleteSelector::ZIndex {
            z: control
                .get("z")
                .ok_or_else(|| invalid("z is required"))?
                .parse()
                .map_err(|_| invalid("invalid z-index"))?,
        },
        _ => return Err(invalid("unsupported delete mode")),
    };
    Ok(DeleteCommand {
        response,
        selector,
        frees_data: mode.bytes().next().is_some_and(|c| c.is_ascii_uppercase()),
    })
}

fn checked_pixels(width: u32, height: u32, format: u32) -> Result<usize, GraphicsError> {
    if width == 0 || height == 0 || width > DIMENSION_LIMIT || height > DIMENSION_LIMIT {
        return Err(GraphicsError::too_large());
    }
    let pixels = (width as usize)
        .checked_mul(height as usize)
        .ok_or_else(GraphicsError::too_large)?;
    let rgba = pixels.checked_mul(4).ok_or_else(GraphicsError::too_large)?;
    let raw = pixels
        .checked_mul(if format == 24 { 3 } else { 4 })
        .ok_or_else(GraphicsError::too_large)?;
    if rgba > PANE_LIMIT || raw > PANE_LIMIT {
        return Err(GraphicsError::too_large());
    }
    Ok(raw)
}

pub fn decode(job: DecodeJob) -> Result<DecodedImage, GraphicsError> {
    let mut bytes = if matches!(job.control.get("t").unwrap_or("d"), "f" | "t") {
        let path = path_from_bytes(job.payload)?;
        read_file_payload(
            &path,
            &job.control,
            &job.file_policy,
            job.control.get("t") == Some("t"),
        )?
    } else {
        job.payload
    };
    if job.control.get("o") == Some("z") {
        let format = job.control.number("f")?.unwrap_or(32);
        let limit = if format == 100 {
            job.control
                .number("S")?
                .ok_or_else(|| GraphicsError::invalid("missing PNG size"))? as usize
        } else {
            checked_pixels(
                job.control.number("s")?.unwrap_or(0),
                job.control.number("v")?.unwrap_or(0),
                format,
            )?
        };
        if limit > PANE_LIMIT {
            return Err(GraphicsError::too_large());
        }
        bytes = zlib_bounded(&bytes, limit)?;
    }
    let format = job.control.number("f")?.unwrap_or(32);
    if format == 100 {
        return decode_png(&bytes);
    }
    let width = job
        .control
        .number("s")?
        .ok_or_else(|| GraphicsError::invalid("missing width"))?;
    let height = job
        .control
        .number("v")?
        .ok_or_else(|| GraphicsError::invalid("missing height"))?;
    let expected = checked_pixels(width, height, format)?;
    if bytes.len() != expected {
        return Err(GraphicsError::decode());
    }
    let rgba = if format == 32 {
        bytes
    } else {
        let mut rgba = Vec::with_capacity(expected / 3 * 4);
        for rgb in bytes.as_chunks::<3>().0 {
            rgba.extend_from_slice(&[rgb[0], rgb[1], rgb[2], 255]);
        }
        rgba
    };
    DecodedImage::new(width, height, rgba)
}

fn zlib_bounded(input: &[u8], expected: usize) -> Result<Vec<u8>, GraphicsError> {
    if expected > PANE_LIMIT {
        return Err(GraphicsError::too_large());
    }
    let mut decoder = Decompress::new(true);
    let mut out = vec![0; expected.saturating_add(1)];
    let status = decoder
        .decompress(input, &mut out, FlushDecompress::Finish)
        .map_err(|_| GraphicsError::decode())?;
    let written = decoder.total_out() as usize;
    if status != Status::StreamEnd
        || written != expected
        || decoder.total_in() as usize != input.len()
    {
        return Err(GraphicsError::decode());
    }
    out.truncate(written);
    Ok(out)
}

fn decode_png(bytes: &[u8]) -> Result<DecodedImage, GraphicsError> {
    let mut decoder = png::Decoder::new(Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().map_err(|_| GraphicsError::decode())?;
    let width = reader.info().width;
    let height = reader.info().height;
    checked_pixels(width, height, 32)?;
    let size = reader
        .output_buffer_size()
        .ok_or_else(GraphicsError::too_large)?;
    if size > PANE_LIMIT {
        return Err(GraphicsError::too_large());
    }
    let mut output = vec![0; size];
    let info = reader
        .next_frame(&mut output)
        .map_err(|_| GraphicsError::decode())?;
    let pixels = output
        .get(..info.buffer_size())
        .ok_or_else(GraphicsError::decode)?;
    let mut rgba = Vec::with_capacity(width as usize * height as usize * 4);
    match info.color_type {
        png::ColorType::Rgba => rgba.extend_from_slice(pixels),
        png::ColorType::Rgb => {
            let (pixels, remainder) = pixels.as_chunks::<3>();
            if !remainder.is_empty() {
                return Err(GraphicsError::decode());
            }
            for p in pixels {
                rgba.extend_from_slice(&[p[0], p[1], p[2], 255]);
            }
        }
        png::ColorType::Grayscale => {
            for &g in pixels {
                rgba.extend_from_slice(&[g, g, g, 255]);
            }
        }
        png::ColorType::GrayscaleAlpha => {
            let (pixels, remainder) = pixels.as_chunks::<2>();
            if !remainder.is_empty() {
                return Err(GraphicsError::decode());
            }
            for p in pixels {
                rgba.extend_from_slice(&[p[0], p[0], p[0], p[1]]);
            }
        }
        png::ColorType::Indexed => return Err(GraphicsError::decode()),
    }
    DecodedImage::new(width, height, rgba)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DecodedImage {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

impl DecodedImage {
    pub fn new(width: u32, height: u32, rgba: Vec<u8>) -> Result<Self, GraphicsError> {
        let expected = checked_pixels(width, height, 32)?;
        if rgba.len() != expected {
            return Err(GraphicsError::decode());
        }
        Ok(Self {
            width,
            height,
            rgba,
        })
    }
}

#[derive(Default)]
pub struct GraphicsStore {
    panes: BTreeMap<PaneId, BTreeMap<u32, DecodedImage>>,
    order: VecDeque<(PaneId, u32)>,
    pane_bytes: BTreeMap<PaneId, usize>,
    total_bytes: usize,
    pane_limit: usize,
    process_limit: usize,
    evicted: Vec<(PaneId, u32)>,
}

impl GraphicsStore {
    pub fn new(pane_limit: usize, process_limit: usize) -> Self {
        Self {
            pane_limit,
            process_limit,
            ..Self::default()
        }
    }
    pub fn insert(
        &mut self,
        pane: PaneId,
        id: u32,
        image: DecodedImage,
    ) -> Result<(), GraphicsError> {
        let size = image.rgba.len();
        if size > self.pane_limit || size > self.process_limit {
            return Err(GraphicsError::too_large());
        }
        self.remove(pane, id);
        while self
            .pane_bytes
            .get(&pane)
            .copied()
            .unwrap_or(0)
            .saturating_add(size)
            > self.pane_limit
        {
            if !self.evict_oldest(Some(pane)) {
                return Err(GraphicsError::too_large());
            }
        }
        while self.total_bytes.saturating_add(size) > self.process_limit {
            if !self.evict_oldest(None) {
                return Err(GraphicsError::too_large());
            }
        }
        self.pane_bytes
            .entry(pane)
            .and_modify(|v| *v += size)
            .or_insert(size);
        self.total_bytes += size;
        self.panes.entry(pane).or_default().insert(id, image);
        self.order.push_back((pane, id));
        Ok(())
    }
    pub fn get(&self, pane: PaneId, id: u32) -> Option<&DecodedImage> {
        self.panes.get(&pane)?.get(&id)
    }
    pub fn remove_image(&mut self, pane: PaneId, id: u32) {
        self.remove(pane, id);
    }
    pub fn take_evicted(&mut self) -> Vec<(PaneId, u32)> {
        std::mem::take(&mut self.evicted)
    }
    pub fn remove_pane(&mut self, pane: PaneId) {
        let ids: Vec<_> = self
            .panes
            .get(&pane)
            .into_iter()
            .flat_map(|images| images.keys().copied())
            .collect();
        for id in ids {
            self.remove(pane, id);
        }
        self.panes.remove(&pane);
        self.pane_bytes.remove(&pane);
    }
    fn remove(&mut self, pane: PaneId, id: u32) {
        if let Some(image) = self
            .panes
            .get_mut(&pane)
            .and_then(|images| images.remove(&id))
        {
            self.total_bytes = self.total_bytes.saturating_sub(image.rgba.len());
            let bytes = self.pane_bytes.entry(pane).or_default();
            *bytes = bytes.saturating_sub(image.rgba.len());
        }
        self.order.retain(|entry| *entry != (pane, id));
    }
    fn evict_oldest(&mut self, pane: Option<PaneId>) -> bool {
        let position = self.order.iter().position(|(candidate, id)| {
            pane.is_none_or(|p| p == *candidate)
                && self
                    .panes
                    .get(candidate)
                    .is_some_and(|images| images.contains_key(id))
        });
        let Some(position) = position else {
            return false;
        };
        if let Some((candidate, id)) = self.order.remove(position) {
            self.evicted.push((candidate, id));
            self.remove(candidate, id);
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::Write,
        path::{Path, PathBuf},
        sync::{Arc, Mutex},
    };

    struct TestDir(PathBuf);
    impl TestDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "so1term-graphics-test-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn file_policy(root: &Path) -> GraphicsFilePolicy {
        GraphicsFilePolicy::from_roots(vec![root.to_owned()], Some(std::env::temp_dir()))
    }

    #[derive(Clone)]
    struct SharedWriter(Arc<Mutex<Vec<u8>>>);
    impl Write for SharedWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn tiny_png(width: u32, height: u32, pixels: &[u8]) -> Vec<u8> {
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let mut encoder = png::Encoder::new(SharedWriter(bytes.clone()), width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(pixels).unwrap();
        writer.finish().unwrap();
        bytes.lock().unwrap().clone()
    }

    #[test]
    fn control_fields_parse_as_a_validated_table() {
        for (input, key, value) in [
            (b"a=t,f=24,s=1,v=1".as_slice(), "a", "t"),
            (b"f=32,a=T".as_slice(), "f", "32"),
            (b"o=z,S=17".as_slice(), "S", "17"),
        ] {
            assert_eq!(parse_control(input).unwrap().get(key), Some(value));
        }
        for input in [
            b"a".as_slice(),
            b"=t",
            b"a=",
            b"a=t,a=p",
            b"a=t, bad=x",
            b"a=t,,f=24",
        ] {
            assert!(parse_control(input).is_err(), "{input:?}");
        }
    }

    #[test]
    fn base64_payload_chunks_reassemble_and_decode() {
        let mut protocol = GraphicsProtocol::default();
        assert!(matches!(
            protocol.ingest(b"Ga=t,f=24,s=1,v=1,m=1;AQID"),
            Ingest::Pending
        ));
        let Ingest::Decode(job) = protocol.ingest(b"Gm=0;") else {
            panic!("expected job")
        };
        assert_eq!(job.payload, vec![1, 2, 3]);
        assert_eq!(decode(job).unwrap().rgba, vec![1, 2, 3, 255]);
    }

    #[test]
    fn rejects_rgb_and_rgba_lengths_that_do_not_match_dimensions() {
        for format in [24, 32] {
            let mut p = GraphicsProtocol::default();
            let job = if format == 24 {
                p.ingest(b"Ga=t,f=24,s=1,v=1;AQ==")
            } else {
                p.ingest(b"Ga=t,f=32,s=1,v=1;AQ==")
            };
            let Ingest::Decode(job) = job else {
                panic!("expected decode job")
            };
            assert!(decode(job).is_err());
        }
    }

    #[test]
    fn decodes_test_generated_tiny_png() {
        let data = STANDARD.encode(tiny_png(1, 1, &[10, 20, 30, 255]));
        let payload = format!("Ga=t,f=100;{data}");
        let Ingest::Decode(job) = GraphicsProtocol::default().ingest(payload.as_bytes()) else {
            panic!("expected job")
        };
        assert_eq!(decode(job).unwrap().rgba, vec![10, 20, 30, 255]);
    }

    #[test]
    fn decodes_zlib_raw_round_trip() {
        let input = [12, 34, 56, 78];
        let mut encoder =
            flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(&input).unwrap();
        let data = STANDARD.encode(encoder.finish().unwrap());
        let Ingest::Decode(job) =
            GraphicsProtocol::default().ingest(format!("Ga=t,f=32,s=1,v=1,o=z;{data}").as_bytes())
        else {
            panic!("expected job")
        };
        assert_eq!(decode(job).unwrap().rgba, input);
    }

    #[test]
    fn dimensions_are_rejected_before_pixel_allocation() {
        assert!(matches!(
            GraphicsProtocol::default().ingest(b"Ga=t,f=32,s=10001,v=1;AQ=="),
            Ingest::Reply(_)
        ));
    }

    #[test]
    fn pane_identity_is_part_of_image_storage_key() {
        let mut store = GraphicsStore::new(16, 32);
        store
            .insert(PaneId(1), 1, DecodedImage::new(1, 1, vec![1; 4]).unwrap())
            .unwrap();
        store
            .insert(PaneId(2), 1, DecodedImage::new(1, 1, vec![2; 4]).unwrap())
            .unwrap();
        assert_eq!(store.get(PaneId(1), 1).unwrap().rgba, vec![1; 4]);
        assert_eq!(store.get(PaneId(2), 1).unwrap().rgba, vec![2; 4]);
        store.remove_pane(PaneId(1));
        assert!(store.get(PaneId(1), 1).is_none());
        assert!(store.get(PaneId(2), 1).is_some());
    }

    #[test]
    fn only_one_decode_is_in_flight_per_pane() {
        let mut protocol = GraphicsProtocol::default();
        assert!(matches!(
            protocol.ingest(b"Ga=t,f=32,s=1,v=1,i=1;AQIDBA=="),
            Ingest::Decode(_)
        ));
        let Ingest::Reply(Some(error)) = protocol.ingest(b"Ga=t,f=32,s=1,v=1,i=2;AQIDBA==") else {
            panic!("expected busy error")
        };
        assert!(String::from_utf8(error).unwrap().contains("EBUSY"));
        protocol.finish_decode();
        assert!(matches!(
            protocol.ingest(b"Ga=t,f=32,s=1,v=1,i=2;AQIDBA=="),
            Ingest::Decode(_)
        ));
    }

    #[test]
    fn rejects_zlib_and_png_output_bombs() {
        let mut encoder =
            flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(&vec![1; 4 * 1024 * 1024]).unwrap();
        let data = STANDARD.encode(encoder.finish().unwrap());
        let Ingest::Decode(job) =
            GraphicsProtocol::default().ingest(format!("Ga=t,f=32,s=1,v=1,o=z;{data}").as_bytes())
        else {
            panic!("expected job")
        };
        assert!(decode(job).is_err());
        let data = STANDARD.encode(tiny_png(10001, 1, &vec![0; 10001 * 4]));
        let Ingest::Decode(job) =
            GraphicsProtocol::default().ingest(format!("Ga=t,f=100;{data}").as_bytes())
        else {
            panic!("expected job")
        };
        assert!(decode(job).is_err());
    }

    #[test]
    fn store_evicts_oldest_image_within_pane_quota() {
        let mut store = GraphicsStore::new(8, 16);
        store
            .insert(PaneId(1), 1, DecodedImage::new(1, 1, vec![1; 4]).unwrap())
            .unwrap();
        store
            .insert(
                PaneId(1),
                2,
                DecodedImage {
                    width: 2,
                    height: 1,
                    rgba: vec![2; 8],
                },
            )
            .unwrap();
        assert!(store.get(PaneId(1), 1).is_none());
        assert_eq!(store.get(PaneId(1), 2).unwrap().rgba.len(), 8);
    }

    #[test]
    fn responses_honor_quiet_modes_and_missing_image_id() {
        let ctx = ResponseContext {
            image_id: Some(7),
            image_number: None,
            placement_id: None,
            quiet: 0,
        };
        assert_eq!(reply(&ctx, Ok(())).unwrap(), b"\x1b_Gi=7;OK\x1b\\");
        let mut q1 = ctx.clone();
        q1.quiet = 1;
        assert!(reply(&q1, Ok(())).is_none());
        let mut q2 = ctx.clone();
        q2.quiet = 2;
        assert!(reply(&q2, Err(GraphicsError::invalid("bad"))).is_none());
        assert!(reply(&q1, Err(GraphicsError::invalid("bad"))).is_some());
        assert!(
            reply(
                &ResponseContext {
                    image_id: None,
                    image_number: None,
                    placement_id: None,
                    quiet: 0
                },
                Ok(())
            )
            .is_none()
        );
    }

    #[test]
    fn unsupported_medium_replies_with_error() {
        assert!(parse_control(b"a=t,t=f,f=100,i=7").is_ok());
        let Ingest::Reply(Some(response)) =
            GraphicsProtocol::default().ingest(b"Gt=s,f=100,i=7;AQ==")
        else {
            panic!("expected reply")
        };
        assert!(
            String::from_utf8(response.clone())
                .unwrap()
                .contains("ENOTSUP"),
            "{response:?}"
        );
    }

    #[test]
    fn file_requests_without_opt_in_reply_permission_error() {
        let path = std::env::temp_dir().join("so1term-file-opt-in-test");
        let encoded = STANDARD.encode(path.as_os_str().as_encoded_bytes());
        let Ingest::Decode(job) = GraphicsProtocol::new(GraphicsFilePolicy::default())
            .ingest(format!("Gt=f,f=100,i=12;{encoded}").as_bytes())
        else {
            panic!("expected background file request")
        };
        let completion = decode_transfer(job);
        let response = reply(&completion.response, completion.image.map(|_| ())).unwrap();
        assert!(String::from_utf8(response).unwrap().contains("EPERM"));
    }

    #[test]
    fn transmit_and_display_keeps_decode_errors_and_put_requests_are_enabled() {
        let Ingest::Decode(job) = GraphicsProtocol::default().ingest(b"Ga=T,f=24,s=1,v=1,i=5;AQI=")
        else {
            panic!("display transfer must enter decoding")
        };
        let completion = decode_transfer(job);
        assert!(completion.image.is_err());
        let response =
            String::from_utf8(reply(&completion.response, completion.image.map(|_| ())).unwrap())
                .unwrap();
        assert!(response.contains("i=5"), "{response:?}");
        assert!(response.contains("EINVAL"), "{response:?}");

        let Ingest::Place(command) = GraphicsProtocol::default().ingest(b"Ga=p,i=5;") else {
            panic!("expected placement request")
        };
        assert_eq!(command.image_id, Some(5));
    }

    #[test]
    fn put_request_yields_a_placement_command_with_protocol_fields() {
        let mut protocol = GraphicsProtocol::new(GraphicsFilePolicy::default());
        let result = protocol.ingest(b"Ga=p,i=17,p=8,x=3,y=4,w=20,h=30,X=2,Y=1,c=4,r=5,z=-3,C=1;");
        let Ingest::Place(command) = result else {
            panic!("unexpected result: {result:?}");
        };
        assert_eq!(command.image_id, Some(17));
        assert_eq!(command.options.placement_id, Some(8));
        assert_eq!(command.options.source, (3, 4, 20, 30));
        assert_eq!(command.options.cells, (Some(4), Some(5)));
        assert_eq!(command.options.offset, (2, 1));
        assert_eq!(command.options.z, -3);
        assert!(command.options.no_cursor_move);
    }

    #[test]
    fn transmit_and_display_retains_placement_options_until_successful_decode() {
        let Ingest::Decode(job) =
            GraphicsProtocol::default().ingest(b"Ga=T,f=24,s=1,v=1,i=17,p=8,C=1;AQID")
        else {
            panic!("expected image decode job");
        };
        let completion = decode_transfer(job);
        assert_eq!(completion.placement.unwrap().placement_id, Some(8));
        assert_eq!(
            (
                completion.image.as_ref().unwrap().width,
                completion.image.as_ref().unwrap().height
            ),
            (1, 1)
        );
        let reply = String::from_utf8(reply(&completion.response, Ok(())).unwrap()).unwrap();
        assert!(reply.contains("i=17,p=8;OK"), "{reply}");
    }

    #[test]
    fn delete_requests_reach_the_placement_store_and_keep_quiet_reply_rules() {
        let Ingest::Delete(command) = GraphicsProtocol::default().ingest(b"Ga=d,d=a,i=5;") else {
            panic!("expected delete command")
        };
        assert_eq!(command.selector, crate::placements::DeleteSelector::All);
        assert!(!command.frees_data);
        assert_eq!(
            reply(&command.response, Ok(())).unwrap(),
            b"\x1b_Gi=5;OK\x1b\\"
        );

        let Ingest::Delete(command) = GraphicsProtocol::default().ingest(b"Ga=d,d=Z,z=-2,i=5,q=1;")
        else {
            panic!("expected uppercase z delete command")
        };
        assert!(command.frees_data);
        assert_eq!(
            command.selector,
            crate::placements::DeleteSelector::ZIndex { z: -2 }
        );
        assert!(reply(&command.response, Ok(())).is_none());

        let Ingest::Place(command) = GraphicsProtocol::default().ingest(b"Ga=p,i=5,q=0;") else {
            panic!("placement request must be enabled")
        };
        assert_eq!(command.image_id, Some(5));
    }

    #[test]
    fn protocol_parses_each_supported_delete_selector() {
        let cases = [
            (
                b"Ga=d,d=i,i=9;".as_slice(),
                crate::placements::DeleteSelector::Image {
                    id: 9,
                    placement_id: None,
                },
            ),
            (
                b"Ga=d,d=n,I=12,p=3;".as_slice(),
                crate::placements::DeleteSelector::ImageNumber {
                    number: 12,
                    placement_id: Some(3),
                },
            ),
            (
                b"Ga=d,d=c;".as_slice(),
                crate::placements::DeleteSelector::CursorCell,
            ),
            (
                b"Ga=d,d=p,x=2,y=4;".as_slice(),
                crate::placements::DeleteSelector::Cell { column: 1, row: 3 },
            ),
            (
                b"Ga=d,d=x,x=2;".as_slice(),
                crate::placements::DeleteSelector::Column { column: 1 },
            ),
            (
                b"Ga=d,d=y,y=4;".as_slice(),
                crate::placements::DeleteSelector::Row { row: 3 },
            ),
            (
                b"Ga=d,d=z,z=-2;".as_slice(),
                crate::placements::DeleteSelector::ZIndex { z: -2 },
            ),
        ];
        for (request, expected) in cases {
            let Ingest::Delete(command) = GraphicsProtocol::default().ingest(request) else {
                panic!("expected delete command for {request:?}")
            };
            assert_eq!(command.selector, expected, "request {request:?}");
        }
    }

    #[test]
    fn malformed_controls_reply_only_when_an_image_id_is_recoverable() {
        let Ingest::Reply(Some(response)) =
            GraphicsProtocol::default().ingest(b"Gbad-key=x,i=9;AQ==")
        else {
            panic!("expected error reply")
        };
        let response = String::from_utf8(response).unwrap();
        assert!(response.starts_with("\x1b_Gi=9;EINVAL:"));
        assert!(matches!(
            GraphicsProtocol::default().ingest(b"Gbad-key=x;AQ=="),
            Ingest::Reply(None)
        ));
    }

    #[test]
    fn quiet_mode_and_initial_id_survive_invalid_continuations() {
        assert!(matches!(
            GraphicsProtocol::default().ingest(b"Gi=1,I=2,q=2,f=24,s=1,v=1;AQID"),
            Ingest::Reply(None)
        ));

        let mut protocol = GraphicsProtocol::default();
        assert!(matches!(
            protocol.ingest(b"Gi=9,f=24,s=1,v=1,m=1;AQID"),
            Ingest::Pending
        ));
        let Ingest::Reply(Some(response)) = protocol.ingest(b"Gm=2;") else {
            panic!("expected the aborted transfer error to use its original id")
        };
        assert!(
            String::from_utf8(response)
                .unwrap()
                .starts_with("\x1b_Gi=9;EINVAL:")
        );
    }

    #[test]
    fn interleaved_transfer_aborts_previous_chunk_state() {
        let mut protocol = GraphicsProtocol::default();
        assert!(matches!(
            protocol.ingest(b"Ga=t,f=24,s=1,v=1,m=1;AQID"),
            Ingest::Pending
        ));
        assert!(matches!(
            protocol.ingest(b"Ga=t,f=24,s=1,v=1,m=1;AgME"),
            Ingest::Pending
        ));
        let Ingest::Decode(job) = protocol.ingest(b"Gm=0;") else {
            panic!("expected job")
        };
        assert_eq!(job.payload, vec![2, 3, 4]);
    }

    #[test]
    fn file_medium_is_denied_by_default_and_reads_allowed_slice() {
        let dir = TestDir::new();
        let path = dir.path().join("pixels.bin");
        std::fs::write(&path, [0, 1, 2, 3, 4, 5]).unwrap();
        let encoded = STANDARD.encode(path.as_os_str().as_encoded_bytes());
        let request = format!("Gt=f,f=24,s=1,v=1,S=3,O=2,i=9;{encoded}");
        let Ingest::Decode(job) =
            GraphicsProtocol::new(file_policy(dir.path())).ingest(request.as_bytes())
        else {
            panic!("file request should be decoded asynchronously")
        };
        assert_eq!(decode_transfer(job).image.unwrap().rgba, [2, 3, 4, 255]);

        let policy = GraphicsFilePolicy::from_roots(Vec::new(), Some(std::env::temp_dir()));
        let Ingest::Decode(job) = GraphicsProtocol::new(policy)
            .ingest(format!("Gt=f,f=24,s=1,v=1,i=9;{encoded}").as_bytes())
        else {
            panic!("file requests are validated on the decode worker")
        };
        assert_eq!(decode_transfer(job).image.unwrap_err().code, "EPERM");
    }

    #[test]
    fn file_guards_reject_outside_traversal_directory_bounds_and_oversize() {
        let parent = TestDir::new();
        let dir_path = parent.path().join("allowed");
        std::fs::create_dir(&dir_path).unwrap();
        let dir = TestDir(dir_path);
        let path = parent.path().join("outside.bin");
        std::fs::write(&path, [1, 2, 3, 4]).unwrap();
        let policy = file_policy(dir.path());
        assert!(read_file_payload(&path, &ControlData::default(), &policy, false).is_err());
        assert!(
            read_file_payload(
                &dir.path().join("../outside.bin"),
                &ControlData::default(),
                &policy,
                false
            )
            .is_err()
        );
        assert!(read_file_payload(dir.path(), &ControlData::default(), &policy, false).is_err());

        let within = dir.path().join("small.bin");
        std::fs::write(&within, [1, 2, 3, 4]).unwrap();
        let mut fields = BTreeMap::new();
        fields.insert("S".into(), "3".into());
        fields.insert("O".into(), "2".into());
        assert!(read_file_payload(&within, &ControlData(fields), &policy, false).is_err());
        let huge = dir.path().join("huge.bin");
        let file = std::fs::File::create(&huge).unwrap();
        file.set_len((PANE_LIMIT + 1) as u64).unwrap();
        assert!(read_file_payload(&huge, &ControlData::default(), &policy, false).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_policy_allows_canonical_inside_target_and_rejects_escape() {
        let dir = TestDir::new();
        let outside = TestDir::new();
        let inside_file = dir.path().join("pixels.bin");
        std::fs::write(&inside_file, [1, 2, 3, 4]).unwrap();
        let alias = dir.path().join("alias.bin");
        std::os::unix::fs::symlink(&inside_file, &alias).unwrap();
        let policy = file_policy(dir.path());
        assert_eq!(
            read_file_payload(&alias, &ControlData::default(), &policy, false).unwrap(),
            [1, 2, 3, 4]
        );
        let secret = outside.path().join("secret.bin");
        std::fs::write(&secret, [5, 6, 7, 8]).unwrap();
        let escape = dir.path().join("escape.bin");
        std::os::unix::fs::symlink(secret, &escape).unwrap();
        assert!(read_file_payload(&escape, &ControlData::default(), &policy, false).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn fifo_is_rejected_without_blocking_and_temp_file_cleanup_is_guarded() {
        let dir = TestDir::new();
        let fifo = dir.path().join("pipe");
        let c_path = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
        let policy = file_policy(dir.path());
        assert!(read_file_payload(&fifo, &ControlData::default(), &policy, false).is_err());

        let temp = dir.path().join("tty-graphics-protocol-1.png");
        std::fs::write(&temp, tiny_png(1, 1, &[10, 20, 30, 255])).unwrap();
        let encoded = STANDARD.encode(temp.as_os_str().as_encoded_bytes());
        let Ingest::Decode(job) = GraphicsProtocol::new(policy.clone())
            .ingest(format!("Gt=t,f=100,i=1;{encoded}").as_bytes())
        else {
            panic!("expected file decode")
        };
        assert!(decode_transfer(job).image.is_ok());
        assert!(!temp.exists());

        let denied = dir.path().join("not-a-temp-marker.png");
        std::fs::write(&denied, [1, 2, 3]).unwrap();
        let encoded = STANDARD.encode(denied.as_os_str().as_encoded_bytes());
        let Ingest::Decode(job) =
            GraphicsProtocol::new(policy).ingest(format!("Gt=t,f=100,i=1;{encoded}").as_bytes())
        else {
            panic!("expected file decode")
        };
        assert!(decode_transfer(job).image.is_err());
        assert!(denied.exists());
    }

    #[cfg(unix)]
    #[test]
    fn temp_cleanup_does_not_delete_a_replacement_file() {
        let dir = TestDir::new();
        let original = dir.path().join("tty-graphics-protocol-original.png");
        let moved = dir.path().join("original-moved.png");
        std::fs::write(&original, [1, 2, 3]).unwrap();
        let policy = file_policy(dir.path());
        let root = policy.allowed_roots.first().unwrap();
        let canonical = original.canonicalize().unwrap();
        let opened = open_beneath(root, canonical.strip_prefix(&root.path).unwrap()).unwrap();
        std::fs::rename(&original, &moved).unwrap();
        std::fs::write(&original, [9, 8, 7]).unwrap();

        assert!(unlink_opened_file(&opened).is_err());
        assert_eq!(std::fs::read(&original).unwrap(), [9, 8, 7]);
        assert_eq!(std::fs::read(&moved).unwrap(), [1, 2, 3]);
    }
}
