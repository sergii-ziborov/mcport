use crate::{protocol, serve_message, ToolServer, Value};
use std::io::{self, BufRead, Write};

const MIN_RESPONSE_BYTES: usize = 256;

/// Byte budgets enforced by mcport's newline-delimited stream adapter.
///
/// The request limit excludes the line delimiter. The response limit includes
/// the complete JSON-RPC message and its trailing newline.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TransportLimits {
    /// Maximum UTF-8 bytes accepted in one request, excluding the delimiter.
    pub max_request_bytes: usize,
    /// Maximum bytes emitted for one response, including the delimiter.
    pub max_response_bytes: usize,
}

impl TransportLimits {
    /// Conservative defaults for local MCP traffic.
    pub const DEFAULT: Self = Self {
        max_request_bytes: 8 * 1024 * 1024,
        max_response_bytes: 8 * 1024 * 1024,
    };

    /// Creates explicit request and response byte budgets.
    #[must_use]
    pub const fn new(max_request_bytes: usize, max_response_bytes: usize) -> Self {
        Self {
            max_request_bytes,
            max_response_bytes,
        }
    }

    pub(crate) fn validate(self) -> io::Result<Self> {
        if self.max_request_bytes == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "max_request_bytes must be greater than zero",
            ));
        }
        if self.max_response_bytes < MIN_RESPONSE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("max_response_bytes must be at least {MIN_RESPONSE_BYTES}"),
            ));
        }
        Ok(self)
    }
}

impl Default for TransportLimits {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Controls when a stream adapter flushes complete response frames.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum FlushPolicy {
    /// Flush every response for interactive stdio latency.
    #[default]
    PerMessage,
    /// Flush after this many complete frames, and always at end of stream.
    Batch {
        /// Maximum complete frames accumulated between flushes.
        max_messages: usize,
    },
}

/// Complete framing and flushing policy for the blocking stream adapter.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TransportConfig {
    /// Per-message byte budgets.
    pub limits: TransportLimits,
    /// Response flush policy.
    pub flush_policy: FlushPolicy,
}

impl TransportConfig {
    pub(crate) fn validate(self) -> io::Result<Self> {
        self.limits.validate()?;
        if matches!(self.flush_policy, FlushPolicy::Batch { max_messages: 0 }) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "batch max_messages must be greater than zero",
            ));
        }
        Ok(self)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FrameStatus {
    Complete,
    EndOfStream,
    Incomplete,
    Oversized,
}

pub(crate) struct ResponseBuffer {
    bytes: Vec<u8>,
    limit: usize,
    exceeded: bool,
}

impl ResponseBuffer {
    pub(crate) fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(1024.min(limit)),
            limit,
            exceeded: false,
        }
    }

    #[inline]
    pub(crate) fn clear(&mut self) {
        self.bytes.clear();
        self.exceeded = false;
    }

    #[inline]
    pub(crate) fn exceeded(&self) -> bool {
        self.exceeded
    }

    #[inline]
    pub(crate) fn as_slice(&self) -> &[u8] {
        &self.bytes
    }

    pub(crate) fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    #[inline]
    fn append(&mut self, bytes: &[u8]) -> io::Result<()> {
        if bytes.len() > self.limit - self.bytes.len() {
            self.exceeded = true;
            return Err(io::Error::other("response exceeds max_response_bytes"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(())
    }
}

impl Write for ResponseBuffer {
    #[inline]
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.append(bytes)?;
        Ok(bytes.len())
    }

    #[inline]
    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.append(bytes)
    }

    #[inline]
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(crate) fn serve_streams_bounded(
    server: &mut impl ToolServer,
    reader: impl BufRead,
    writer: impl Write,
    limits: TransportLimits,
) -> io::Result<()> {
    serve_streams_impl::<true>(server, reader, writer, limits.validate()?, 1)
}

pub(crate) fn serve_streams_configured(
    server: &mut impl ToolServer,
    reader: impl BufRead,
    writer: impl Write,
    config: TransportConfig,
) -> io::Result<()> {
    let config = config.validate()?;
    match config.flush_policy {
        FlushPolicy::PerMessage => {
            serve_streams_impl::<true>(server, reader, writer, config.limits, 1)
        }
        FlushPolicy::Batch { max_messages } => {
            serve_streams_impl::<false>(server, reader, writer, config.limits, max_messages)
        }
    }
}

fn serve_streams_impl<const FLUSH_EACH: bool>(
    server: &mut impl ToolServer,
    mut reader: impl BufRead,
    mut writer: impl Write,
    limits: TransportLimits,
    max_messages: usize,
) -> io::Result<()> {
    let mut frame = Vec::with_capacity(512);
    let mut response = ResponseBuffer::new(limits.max_response_bytes);
    let mut pending_messages = 0;

    loop {
        match read_frame(&mut reader, &mut frame, limits.max_request_bytes)? {
            FrameStatus::EndOfStream => break,
            FrameStatus::Incomplete => {
                write_transport_error(
                    &mut response,
                    -32_700,
                    "incomplete JSON-RPC message at EOF",
                )?;
                write_buffered::<FLUSH_EACH>(
                    &mut writer,
                    &response,
                    &mut pending_messages,
                    max_messages,
                )?;
                break;
            }
            FrameStatus::Oversized => {
                write_transport_error(
                    &mut response,
                    -32_000,
                    format!(
                        "request exceeds max_request_bytes ({})",
                        limits.max_request_bytes
                    ),
                )?;
                write_buffered::<FLUSH_EACH>(
                    &mut writer,
                    &response,
                    &mut pending_messages,
                    max_messages,
                )?;
            }
            FrameStatus::Complete => {
                if let Ok(line) = std::str::from_utf8(&frame) {
                    response.clear();
                    match serve_message(server, line, &mut response) {
                        Ok(true) => write_buffered::<FLUSH_EACH>(
                            &mut writer,
                            &response,
                            &mut pending_messages,
                            max_messages,
                        )?,
                        Ok(false) => {}
                        Err(_) if response.exceeded() => {
                            write_response_limit_error(
                                line,
                                &mut response,
                                limits.max_response_bytes,
                            )?;
                            write_buffered::<FLUSH_EACH>(
                                &mut writer,
                                &response,
                                &mut pending_messages,
                                max_messages,
                            )?;
                        }
                        Err(error) => return Err(error),
                    }
                } else {
                    write_transport_error(
                        &mut response,
                        -32_700,
                        "JSON-RPC message is not valid UTF-8",
                    )?;
                    write_buffered::<FLUSH_EACH>(
                        &mut writer,
                        &response,
                        &mut pending_messages,
                        max_messages,
                    )?;
                }
            }
        }
    }
    if pending_messages > 0 {
        writer.flush()?;
    }
    Ok(())
}

pub(crate) fn read_frame(
    reader: &mut impl BufRead,
    frame: &mut Vec<u8>,
    max_request_bytes: usize,
) -> io::Result<FrameStatus> {
    frame.clear();
    let mut oversized = false;
    let buffered_limit = max_request_bytes.saturating_add(2);

    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok(if frame.is_empty() && !oversized {
                FrameStatus::EndOfStream
            } else {
                FrameStatus::Incomplete
            });
        }

        let newline = available.iter().position(|byte| *byte == b'\n');
        let consumed = newline.map_or(available.len(), |position| position + 1);
        if !oversized {
            let remaining = buffered_limit.saturating_sub(frame.len());
            if consumed <= remaining {
                frame.extend_from_slice(&available[..consumed]);
            } else {
                frame.extend_from_slice(&available[..remaining]);
                oversized = true;
            }
        }
        reader.consume(consumed);

        if newline.is_some() {
            if oversized {
                return Ok(FrameStatus::Oversized);
            }
            frame.pop();
            if frame.last() == Some(&b'\r') {
                frame.pop();
            }
            return Ok(if frame.len() > max_request_bytes {
                FrameStatus::Oversized
            } else {
                FrameStatus::Complete
            });
        }
    }
}

fn write_buffered<const FLUSH_EACH: bool>(
    writer: &mut impl Write,
    response: &ResponseBuffer,
    pending_messages: &mut usize,
    max_messages: usize,
) -> io::Result<()> {
    writer.write_all(response.as_slice())?;
    *pending_messages += 1;
    if FLUSH_EACH || *pending_messages >= max_messages {
        writer.flush()?;
        *pending_messages = 0;
    }
    Ok(())
}

#[cold]
#[inline(never)]
fn write_transport_error(
    response: &mut ResponseBuffer,
    code: i64,
    message: impl Into<String>,
) -> io::Result<()> {
    response.clear();
    blazingly_json::to_writer(
        &mut *response,
        &protocol::error(&Value::Null, code, message),
    )?;
    response.write_all(b"\n")
}

#[cold]
#[inline(never)]
fn write_response_limit_error(
    line: &str,
    response: &mut ResponseBuffer,
    max_response_bytes: usize,
) -> io::Result<()> {
    let request = blazingly_json::from_str::<Value>(line).ok();
    let id = request
        .as_ref()
        .and_then(|request| request.get("id"))
        .unwrap_or(&Value::Null);
    response.clear();
    let error = protocol::error(
        id,
        -32_000,
        format!("response exceeds max_response_bytes ({max_response_bytes})"),
    );
    if blazingly_json::to_writer(&mut *response, &error)
        .and_then(|()| response.write_all(b"\n").map_err(Into::into))
        .is_err()
        && response.exceeded()
    {
        write_transport_error(response, -32_000, "response exceeds max_response_bytes")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        read_frame, serve_streams_configured, FlushPolicy, FrameStatus, ResponseBuffer,
        TransportConfig, TransportLimits,
    };
    use crate::McpServer;
    use std::io::{Cursor, Write};

    #[derive(Default)]
    struct CountingWriter {
        bytes: Vec<u8>,
        flushes: usize,
    }

    impl Write for CountingWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            self.flushes += 1;
            Ok(())
        }
    }

    #[test]
    fn bounded_reader_handles_fragments_crlf_and_oversized_frames() {
        let input = b"one\r\ntoolong\nok\n";
        let mut reader = Cursor::new(input);
        let mut frame = Vec::new();

        assert_eq!(
            read_frame(&mut reader, &mut frame, 4).unwrap(),
            FrameStatus::Complete
        );
        assert_eq!(frame, b"one");
        assert_eq!(
            read_frame(&mut reader, &mut frame, 4).unwrap(),
            FrameStatus::Oversized
        );
        assert_eq!(
            read_frame(&mut reader, &mut frame, 4).unwrap(),
            FrameStatus::Complete
        );
        assert_eq!(frame, b"ok");
        assert_eq!(
            read_frame(&mut reader, &mut frame, 4).unwrap(),
            FrameStatus::EndOfStream
        );
    }

    #[test]
    fn bounded_reader_rejects_partial_eof() {
        let mut reader = Cursor::new(b"partial");
        let mut frame = Vec::new();
        assert_eq!(
            read_frame(&mut reader, &mut frame, 32).unwrap(),
            FrameStatus::Incomplete
        );
    }

    #[test]
    fn response_buffer_never_keeps_partial_overflow_write() {
        let mut buffer = ResponseBuffer::new(4);
        buffer.write_all(b"1234").unwrap();
        assert!(buffer.write_all(b"5").is_err());
        assert!(buffer.exceeded());
        assert_eq!(buffer.as_slice(), b"1234");
    }

    #[test]
    fn limits_reject_an_error_budget_that_cannot_frame_errors() {
        assert!(TransportLimits::new(1, 1).validate().is_err());
    }

    #[test]
    fn batching_flushes_at_the_threshold_and_end_of_stream() {
        let input = concat!(
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\n",
            "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"ping\"}\n",
            "{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"ping\"}\n"
        );
        let mut server = McpServer::new("test", "1");
        let mut writer = CountingWriter::default();
        serve_streams_configured(
            &mut server,
            input.as_bytes(),
            &mut writer,
            TransportConfig {
                limits: TransportLimits::default(),
                flush_policy: FlushPolicy::Batch { max_messages: 2 },
            },
        )
        .unwrap();
        assert_eq!(writer.flushes, 2);
        assert_eq!(String::from_utf8(writer.bytes).unwrap().lines().count(), 3);

        assert!(TransportConfig {
            limits: TransportLimits::default(),
            flush_policy: FlushPolicy::Batch { max_messages: 0 },
        }
        .validate()
        .is_err());
    }
}
