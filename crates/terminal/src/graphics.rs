//! Bounded kitty graphics protocol ingestion (rendering and placements are separate tasks).

use base64::{Engine as _, engine::general_purpose::STANDARD};
use flate2::{Decompress, FlushDecompress, Status};
use so1_terminal::layout::PaneId;
use std::{
    collections::{BTreeMap, VecDeque},
    io::Cursor,
};

pub const PANE_LIMIT: usize = 128 * 1024 * 1024;
pub const PROCESS_LIMIT: usize = 512 * 1024 * 1024;
pub const DIMENSION_LIMIT: u32 = 10_000;
const CHUNK_LIMIT: usize = PANE_LIMIT.div_ceil(3) * 4 + 8;
const SUPPORTED_KEYS: &[&str] = &["a", "f", "i", "I", "s", "v", "t", "o", "S", "m", "q"];

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
            || !value.bytes().all(|b| b.is_ascii_alphanumeric())
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
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResponseContext {
    pub image_id: Option<u32>,
    pub image_number: Option<u32>,
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
}

pub struct GraphicsCompletion {
    pub response: ResponseContext,
    pub action: String,
    pub image: Result<DecodedImage, GraphicsError>,
}

pub fn decode_transfer(job: DecodeJob) -> GraphicsCompletion {
    let response = job.response.clone();
    let action = job.action.clone();
    GraphicsCompletion {
        response,
        action,
        image: decode(job),
    }
}

#[derive(Debug)]
pub enum Ingest {
    Ignored,
    Pending,
    Decode(DecodeJob),
    Reply(Option<Vec<u8>>),
}

#[derive(Default)]
pub struct GraphicsProtocol {
    chunk: Option<Chunk>,
    decoding: bool,
}

struct Chunk {
    control: ControlData,
    response: ResponseContext,
    encoded: Vec<u8>,
}

impl GraphicsProtocol {
    pub fn ingest(&mut self, apc: &[u8]) -> Ingest {
        if apc.first() != Some(&b'G') {
            return Ingest::Ignored;
        }
        let Some(separator) = apc.iter().position(|&b| b == b';') else {
            self.chunk = None;
            return Ingest::Reply(None);
        };
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
        let encoded = &apc[separator + 1..];
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
        if let Err(error) = validate_request(&metadata, &action) {
            return Ingest::Reply(reply(&ctx, Err(error)));
        }
        let decoded = match STANDARD.decode(combined) {
            Ok(payload) if payload.len() <= PANE_LIMIT => payload,
            Ok(_) => return Ingest::Reply(reply(&ctx, Err(GraphicsError::too_large()))),
            Err(_) => return Ingest::Reply(reply(&ctx, Err(GraphicsError::decode()))),
        };
        self.decoding = true;
        Ingest::Decode(DecodeJob {
            payload: decoded,
            control: metadata,
            response: ctx,
            action,
        })
    }
    pub fn finish_decode(&mut self) {
        self.decoding = false;
    }
}

fn response_hint(control: &[u8]) -> ResponseContext {
    let mut image_id = None;
    let mut image_number = None;
    let mut quiet = 0;
    if let Ok(text) = std::str::from_utf8(control) {
        for field in text.split(',') {
            if let Some((key, value)) = field.split_once('=') {
                match key {
                    "i" => image_id = value.parse().ok(),
                    "I" => image_number = value.parse().ok(),
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
        quiet,
    }
}

fn validate_request(control: &ControlData, action: &str) -> Result<(), GraphicsError> {
    match action {
        "t" | "T" | "q" => {}
        "p" | "d" => {
            return Err(GraphicsError::unsupported(
                "placements are not implemented yet",
            ));
        }
        _ => return Err(GraphicsError::invalid("invalid action")),
    }
    match control.get("t").unwrap_or("d") {
        "d" => {}
        "f" | "t" | "s" => {
            return Err(GraphicsError::unsupported(
                "transmission medium is not implemented yet",
            ));
        }
        _ => return Err(GraphicsError::invalid("invalid transmission medium")),
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
    } else if control.get("o") == Some("z") && control.number("S")?.is_none() {
        return Err(GraphicsError::invalid("compressed PNG requires S"));
    }
    Ok(())
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
    let mut bytes = job.payload;
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
        sync::{Arc, Mutex},
    };

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
            GraphicsProtocol::default().ingest(b"Gt=f,f=100,i=7;AQ==")
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
}
