//! The response body of a stream, read with a grace period once the turn is complete.
//!
//! A Chat Completions stream ends with `data: [DONE]` and the end of the body, but some servers
//! and proxies send the last chunk (the `finish_reason`) and then keep the connection open. Once
//! a finish reason has been seen, the rest of the body is read on a helper thread, so that the
//! reader can stop waiting for it after a grace period: it then closes the connection and reports
//! the end of the body.

use engine_agent::CancelToken;
use std::cell::Cell;
use std::io::{self, Read};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

const CHUNK: usize = 16 * 1024;

type Chunk = io::Result<Vec<u8>>;

enum Mode {
    /// Read on the calling thread.
    Direct(Box<dyn Read + Send>),
    /// Read on a helper thread; `pending` is what was received and not handed out yet.
    Tail {
        chunks: Receiver<Chunk>,
        pending: Vec<u8>,
        offset: usize,
        deadline: Instant,
    },
    Ended,
}

/// Reads the body of one turn. `finished_at` is set by the reader of the events when the finish
/// reason arrives; from the next read on, the body is read on a helper thread until `grace` after
/// that moment. `connection` is cancelled to close the connection when the grace is over.
pub(super) struct GraceBody<'a> {
    mode: Mode,
    finished_at: &'a Cell<Option<Instant>>,
    grace: Duration,
    connection: CancelToken,
}

impl<'a> GraceBody<'a> {
    pub fn new(
        body: Box<dyn Read + Send>,
        finished_at: &'a Cell<Option<Instant>>,
        grace: Duration,
        connection: CancelToken,
    ) -> Self {
        Self {
            mode: Mode::Direct(body),
            finished_at,
            grace,
            connection,
        }
    }

    /// Moves the body to a helper thread. If none can be started the connection is closed and the
    /// body is over.
    fn start_tail(&mut self, mut body: Box<dyn Read + Send>, finished_at: Instant) {
        let (send, chunks) = mpsc::channel::<Chunk>();
        let spawned = std::thread::Builder::new()
            .name("ai-stream-tail".into())
            .spawn(move || {
                let mut buffer = vec![0u8; CHUNK];
                loop {
                    let chunk = match body.read(&mut buffer) {
                        Ok(0) => return,
                        Ok(read) => Ok(buffer[..read].to_vec()),
                        Err(error) => Err(error),
                    };
                    let failed = chunk.is_err();
                    if send.send(chunk).is_err() || failed {
                        return;
                    }
                }
            });
        self.mode = match spawned {
            Ok(_) => Mode::Tail {
                chunks,
                pending: Vec::new(),
                offset: 0,
                deadline: finished_at + self.grace,
            },
            Err(_) => {
                self.connection.cancel_detached();
                Mode::Ended
            }
        };
    }
}

impl Read for GraceBody<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if let Mode::Direct(body) = &mut self.mode {
            let Some(finished_at) = self.finished_at.get() else {
                return body.read(buf);
            };
            if let Mode::Direct(body) = std::mem::replace(&mut self.mode, Mode::Ended) {
                self.start_tail(body, finished_at);
            }
        }
        let Mode::Tail {
            chunks,
            pending,
            offset,
            deadline,
        } = &mut self.mode
        else {
            return Ok(0);
        };
        if *offset == pending.len() {
            match chunks.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(Ok(next)) => {
                    *pending = next;
                    *offset = 0;
                }
                Ok(Err(error)) => return Err(error),
                // The helper is done: the body ended.
                Err(RecvTimeoutError::Disconnected) => {
                    self.mode = Mode::Ended;
                    return Ok(0);
                }
                // The server keeps the connection open: close it, which also frees the helper.
                Err(RecvTimeoutError::Timeout) => {
                    self.connection.cancel_detached();
                    self.mode = Mode::Ended;
                    return Ok(0);
                }
            }
        }
        let count = buf.len().min(pending.len() - *offset);
        buf[..count].copy_from_slice(&pending[*offset..*offset + count]);
        *offset += count;
        Ok(count)
    }
}
