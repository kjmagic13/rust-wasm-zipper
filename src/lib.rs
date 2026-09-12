//! Streams an uncompressed (STORED) ZIP archive of browser byte sources as a JS `ReadableStream`.
//!
//! Each entry's CRC and size go in a data descriptor after its contents, so sources are
//! piped through chunk by chunk instead of being loaded into memory. ZIP64 isn't
//! supported: the archive must stay under 4 GiB and 65,535 entries.

use crc32fast::Hasher;
use futures::{channel::mpsc, SinkExt, StreamExt};
use js_sys::{Array, ArrayBuffer, Uint8Array};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::{spawn_local, JsFuture};
use wasm_streams::ReadableStream;
use web_sys::{Blob, File, FileSystemFileHandle, Response};

/// ZIP spec version 2.0, the minimum for data descriptors.
const VERSION: u16 = 20;
/// General-purpose flags: bit 3 = CRC and sizes follow in a data descriptor, bit 11 = UTF-8 names.
const FLAGS: u16 = (1 << 3) | (1 << 11);

#[wasm_bindgen(typescript_custom_section)]
const ZIP_SOURCE: &'static str = r#"
/** Anything the archiver can read bytes from. */
export type ZipSource =
    | ReadableStream<Uint8Array>
    | Blob
    | Response
    | FileSystemFileHandle
    | ArrayBuffer
    | ArrayBufferView;
"#;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(typescript_type = "{ name: string; source: ZipSource }")]
    pub type ZipInput;

    #[wasm_bindgen(method, getter)]
    fn name(this: &ZipInput) -> String;

    #[wasm_bindgen(method, getter)]
    fn source(this: &ZipInput) -> JsValue;
}

/// Resolves any supported source into a `ReadableStream` of its bytes.
///
/// A `ReadableStream` is used as-is, so the caller owns how the bytes are produced
/// (`fetch` bodies, `File.stream()`, transform pipelines, hand-rolled streams); the
/// other forms are conveniences that stream just the same.
async fn to_stream(source: JsValue) -> Result<web_sys::ReadableStream, JsValue> {
    if source.is_instance_of::<web_sys::ReadableStream>() {
        return Ok(source.unchecked_into());
    }
    // `File` is a `Blob`, so this covers `<input type="file">` picks too.
    if source.is_instance_of::<Blob>() {
        return Ok(source.unchecked_ref::<Blob>().stream());
    }
    if source.is_instance_of::<Response>() {
        return source
            .unchecked_ref::<Response>()
            .body()
            .ok_or_else(|| JsValue::from_str("response has no body"));
    }
    if source.is_instance_of::<FileSystemFileHandle>() {
        let file: File = JsFuture::from(source.unchecked_ref::<FileSystemFileHandle>().get_file())
            .await?
            .unchecked_into();
        return Ok(file.stream());
    }
    // Buffers are wrapped in a `Blob` rather than copied into a hand-built stream.
    if source.is_instance_of::<ArrayBuffer>() || ArrayBuffer::is_view(&source) {
        return Ok(Blob::new_with_u8_array_sequence(&Array::of1(&source))?.stream());
    }
    Err(JsValue::from_str(
        "unsupported source: expected a ReadableStream, Blob/File, Response, \
         FileSystemFileHandle, ArrayBuffer or typed array",
    ))
}

/// Returns a `ReadableStream` of the ZIP archive's bytes.
#[wasm_bindgen]
pub fn zip_files(files: Vec<ZipInput>) -> web_sys::ReadableStream {
    // Capacity 1 gives backpressure: the writer waits until the reader pulls.
    let (tx, rx) = mpsc::channel(1);

    spawn_local(async move {
        let mut out = Output { tx, offset: 0 };
        if let Err(err) = write_zip(&mut out, files).await {
            // Only fails if the reader already cancelled, so there's no one to tell.
            let _ = out.tx.send(Err(err)).await;
        }
    });

    ReadableStream::from_stream(rx).into_raw()
}

async fn write_zip(out: &mut Output, files: Vec<ZipInput>) -> Result<(), JsValue> {
    let mut entries = Vec::with_capacity(files.len());

    for input in files {
        let mut entry = Entry::new(input.name(), out.offset)?;
        out.write(entry.local_header()).await?;

        let source = to_stream(input.source()).await?;
        let mut chunks = ReadableStream::from_raw(source).into_stream();
        let mut crc = Hasher::new();
        let mut size = 0u64;

        while let Some(chunk) = chunks.next().await {
            // Byte streams yield `Uint8Array`s; anything else is a caller mistake.
            let chunk: Uint8Array = chunk?.dyn_into().map_err(|_| {
                JsValue::from_str("stream chunk was not a Uint8Array")
            })?;
            crc.update(&chunk.to_vec());
            size += u64::from(chunk.length());
            out.write(chunk).await?;
        }

        entry.crc = crc.finalize();
        entry.size = zip32(size, "file size")?;
        out.write(entry.data_descriptor()).await?;
        entries.push(entry);
    }

    let central_start = out.offset;
    for entry in &entries {
        out.write(entry.central_header()).await?;
    }

    let end = end_of_central_directory(
        zip32(entries.len() as u64, "file count")?,
        zip32(out.offset - central_start, "central directory size")?,
        zip32(central_start, "archive size")?,
    );
    out.write(end).await
}

/// The writing end of the output stream, tracking the archive size so far.
struct Output {
    tx: mpsc::Sender<Result<JsValue, JsValue>>,
    offset: u64,
}

impl Output {
    async fn write(&mut self, bytes: impl Into<Uint8Array>) -> Result<(), JsValue> {
        let bytes = bytes.into();
        self.offset += u64::from(bytes.length());
        self.tx
            .send(Ok(bytes.into()))
            .await
            .map_err(|_| JsValue::from_str("zip stream was cancelled"))
    }
}

struct Entry {
    name: String,
    /// Offset of the entry's local header within the archive.
    offset: u32,
    crc: u32,
    size: u32,
}

impl Entry {
    fn new(name: String, offset: u64) -> Result<Self, JsValue> {
        zip32::<u16>(name.len() as u64, "file name length")?;
        let offset = zip32(offset, "archive size")?;
        Ok(Self {
            name,
            offset,
            crc: 0,
            size: 0,
        })
    }

    fn local_header(&self) -> Record {
        Record::new(0x04034b50)
            .u16(VERSION) // version needed to extract
            .u16(FLAGS)
            .u16(0) // compression: stored
            .u32(0) // DOS modification time and date
            .u32(0) // CRC-32 (in data descriptor)
            .u32(0) // compressed size (in data descriptor)
            .u32(0) // uncompressed size (in data descriptor)
            .u16(self.name.len() as u16)
            .u16(0) // extra field length
            .bytes(self.name.as_bytes())
    }

    fn data_descriptor(&self) -> Record {
        Record::new(0x08074b50)
            .u32(self.crc)
            .u32(self.size) // compressed size
            .u32(self.size) // uncompressed size
    }

    fn central_header(&self) -> Record {
        Record::new(0x02014b50)
            .u16(VERSION) // version made by
            .u16(VERSION) // version needed to extract
            .u16(FLAGS)
            .u16(0) // compression: stored
            .u32(0) // DOS modification time and date
            .u32(self.crc)
            .u32(self.size) // compressed size
            .u32(self.size) // uncompressed size
            .u16(self.name.len() as u16)
            .u16(0) // extra field length
            .u16(0) // comment length
            .u16(0) // disk number
            .u16(0) // internal attributes
            .u32(0) // external attributes
            .u32(self.offset)
            .bytes(self.name.as_bytes())
    }
}

fn end_of_central_directory(count: u16, size: u32, offset: u32) -> Record {
    Record::new(0x06054b50)
        .u16(0) // this disk number
        .u16(0) // disk where central directory starts
        .u16(count) // entries on this disk
        .u16(count) // total entries
        .u32(size)
        .u32(offset)
        .u16(0) // comment length
}

/// Narrows a value to a ZIP field, erroring instead of truncating.
fn zip32<T: TryFrom<u64>>(value: u64, what: &str) -> Result<T, JsValue> {
    T::try_from(value).map_err(|_| {
        JsValue::from_str(&format!("{what} exceeds ZIP limits (ZIP64 is unsupported)"))
    })
}

/// Little-endian byte builder for ZIP records, starting with the record's signature.
struct Record(Vec<u8>);

impl Record {
    fn new(signature: u32) -> Self {
        Self(signature.to_le_bytes().to_vec())
    }

    fn u16(self, value: u16) -> Self {
        self.bytes(&value.to_le_bytes())
    }

    fn u32(self, value: u32) -> Self {
        self.bytes(&value.to_le_bytes())
    }

    fn bytes(mut self, bytes: &[u8]) -> Self {
        self.0.extend_from_slice(bytes);
        self
    }
}

impl From<Record> for Uint8Array {
    fn from(record: Record) -> Self {
        record.0.as_slice().into()
    }
}
