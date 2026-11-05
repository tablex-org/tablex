//! An incremental Server-Sent Events parser over bytes.
//!
//! It works on whatever chunking the transport delivers: a line, a UTF-8 sequence or a `\r\n` may
//! be split anywhere. Lines end with `\n`, `\r` or `\r\n`; lines starting with `:` are comments;
//! several `data:` lines of one event are joined with `\n`; `id:` and `retry:` are ignored. An
//! event is delivered at its blank line; one still open when the stream ends is dropped (as the
//! SSE specification says). Buffers are reused between events.

use crate::provider::ProtocolError;

/// Most bytes one event (or one line) may take; the API's events are far smaller.
const MAX_EVENT_BYTES: usize = 32 * 1024 * 1024;

#[derive(Debug, Clone, Copy)]
pub(crate) struct SseEvent<'a> {
    /// The `event:` name; the API repeats it as the data's `type`, which is what is dispatched on.
    #[allow(dead_code)]
    pub event: Option<&'a str>,
    pub data: &'a str,
}

#[derive(Default)]
pub(crate) struct SseParser {
    /// The unfinished line.
    line: Vec<u8>,
    event: Vec<u8>,
    data: Vec<u8>,
    has_data: bool,
    /// The last chunk ended with `\r`: a `\n` at the start of the next one belongs to it.
    skip_lf: bool,
}

impl SseParser {
    /// Feeds a chunk; `on_event` runs for each completed event and returns `Ok(true)` to stop
    /// (the rest of the chunk is then ignored).
    pub fn feed<E: From<ProtocolError>>(
        &mut self,
        mut chunk: &[u8],
        on_event: &mut impl FnMut(SseEvent<'_>) -> Result<bool, E>,
    ) -> Result<bool, E> {
        if self.skip_lf {
            if chunk.first() == Some(&b'\n') {
                chunk = &chunk[1..];
            }
            self.skip_lf = false;
        }
        while !chunk.is_empty() {
            let Some(end) = chunk.iter().position(|byte| matches!(byte, b'\r' | b'\n')) else {
                self.line.extend_from_slice(chunk);
                self.check_size()?;
                return Ok(false);
            };
            self.line.extend_from_slice(&chunk[..end]);
            let terminator = chunk[end];
            chunk = &chunk[end + 1..];
            if terminator == b'\r' {
                match chunk.first() {
                    Some(b'\n') => chunk = &chunk[1..],
                    None => self.skip_lf = true,
                    Some(_) => {}
                }
            }
            if self.finish_line(on_event)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn check_size(&self) -> Result<(), ProtocolError> {
        if self.line.len() + self.data.len() > MAX_EVENT_BYTES {
            return Err(ProtocolError::EventTooLarge {
                limit: MAX_EVENT_BYTES,
            });
        }
        Ok(())
    }

    fn finish_line<E: From<ProtocolError>>(
        &mut self,
        on_event: &mut impl FnMut(SseEvent<'_>) -> Result<bool, E>,
    ) -> Result<bool, E> {
        let line = std::mem::take(&mut self.line);
        let result = if line.is_empty() {
            self.dispatch(on_event)
        } else {
            self.field(&line).map_err(E::from).map(|()| false)
        };
        // Keep the allocation for the next line.
        self.line = line;
        self.line.clear();
        result
    }

    fn field(&mut self, line: &[u8]) -> Result<(), ProtocolError> {
        if line[0] == b':' {
            return Ok(());
        }
        let (name, value) = match line.iter().position(|byte| *byte == b':') {
            Some(colon) => {
                let value = &line[colon + 1..];
                (&line[..colon], value.strip_prefix(b" ").unwrap_or(value))
            }
            None => (line, &line[line.len()..]),
        };
        match name {
            b"event" => {
                self.event.clear();
                self.event.extend_from_slice(value);
            }
            b"data" => {
                if self.has_data {
                    self.data.push(b'\n');
                }
                self.has_data = true;
                self.data.extend_from_slice(value);
                self.check_size()?;
            }
            _ => {}
        }
        Ok(())
    }

    fn dispatch<E: From<ProtocolError>>(
        &mut self,
        on_event: &mut impl FnMut(SseEvent<'_>) -> Result<bool, E>,
    ) -> Result<bool, E> {
        let had_data = std::mem::take(&mut self.has_data);
        let outcome = if had_data && !self.data.is_empty() {
            let data = std::str::from_utf8(&self.data).map_err(|_| ProtocolError::InvalidUtf8)?;
            let event = match self.event.as_slice() {
                [] => None,
                name => Some(std::str::from_utf8(name).map_err(|_| ProtocolError::InvalidUtf8)?),
            };
            on_event(SseEvent { event, data })
        } else {
            Ok(false)
        };
        self.data.clear();
        self.event.clear();
        outcome
    }
}
