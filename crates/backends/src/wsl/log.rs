//! One Environment's log, on the daemon's disk.
//!
//! The natural source of a WSL Environment's log is the supervisor: it is the
//! foreground owner of the runtime, its stdout is already a protocol the daemon
//! reads, and everything the guest says outside that protocol is worth keeping.
//! So the supervisor's two streams are teed — line by line, as they arrive —
//! into an append-only file the daemon owns:
//!
//! ```text
//! data-root/wsl/env/<environment-id>.log     one JSON object per line
//! ```
//!
//! Two properties matter and both come from that shape:
//!
//! * **A slow reader cannot block the workload.** The workload writes to a pipe;
//!   a dedicated thread drains it into the file. A follower reads the *file*, so
//!   a follower that stops consuming costs a reader nothing — the workload never
//!   waits on it.
//! * **`tail`, `since`, and `follow` are exact.** Every record carries the
//!   sequence number assigned when it was written, so `since` is a cursor and
//!   not a race, and a tail cannot pick up a line the caller already saw.
//!
//! Following is a poll of the file's growth from the byte offset already
//! consumed, which is why a follower sees every line and not a sample of them.

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::contract::{LogLine, LogOptions, LogStream};
use crate::error::{BackendError, Result};

/// How often a followed stream looks for new lines. Small enough to feel live,
/// large enough that an idle follower is not a busy loop.
pub const FOLLOW_POLL: Duration = Duration::from_millis(50);

/// The append side of one Environment's log.
///
/// One instance, behind a mutex: two threads (the supervisor's stdout and its
/// stderr) write through it, so records never interleave and the sequence
/// numbers are assigned under the same lock that publishes them.
pub struct LogSink {
    path: PathBuf,
    file: File,
    seq: u64,
}

impl LogSink {
    /// Open (or create) the log for `path`, continuing the sequence where the
    /// file left off, so a restart does not renumber old lines.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|err| BackendError::BackendUnavailable {
                detail: format!("cannot create {}: {err}", parent.display()),
            })?;
        }
        let seq = count_records(path);
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|err| BackendError::BackendUnavailable {
                detail: format!("cannot open the Environment log {}: {err}", path.display()),
            })?;
        Ok(Self {
            path: path.to_path_buf(),
            file,
            seq,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append one line. A write failure is swallowed: a log that cannot be
    /// written is a diagnostic problem, never a reason to fail a workload.
    pub fn append(&mut self, resource: &str, stream: &str, line: &str) -> Option<LogLine> {
        self.seq += 1;
        let record = LogLine {
            seq: self.seq,
            at: now_rfc3339_millis(),
            resource: resource.to_string(),
            stream: stream.to_string(),
            line: line.to_string(),
        };
        let mut encoded = serde_json::to_vec(&record).ok()?;
        encoded.push(b'\n');
        self.file.write_all(&encoded).ok()?;
        self.file.flush().ok()?;
        Some(record)
    }
}

/// Drain `reader` into the log for the whole life of the child that owns it.
///
/// `forward` carries the line to the caller that is waiting for readiness; it is
/// cleared once nothing is listening any more, and the thread keeps logging.
pub fn spawn_tee<R: Read + Send + 'static>(
    reader: R,
    sink: Arc<Mutex<LogSink>>,
    stream: &'static str,
    resource: &'static str,
    forward: Arc<AtomicBool>,
    listener: Option<Sender<String>>,
) {
    std::thread::spawn(move || {
        let mut lines = BufReader::new(reader);
        let mut raw = Vec::new();
        loop {
            raw.clear();
            match read_line_bytes(&mut lines, &mut raw) {
                Ok(0) | Err(_) => return,
                Ok(_) => {}
            }
            let text = String::from_utf8_lossy(&raw);
            let text = text.trim_end_matches(['\n', '\r']);
            if let Ok(mut sink) = sink.lock() {
                sink.append(resource, stream, text);
            }
            if let Some(listener) = listener.as_ref() {
                if forward.load(Ordering::SeqCst) && listener.send(text.to_string()).is_err() {
                    // Nobody is waiting for readiness any more: keep logging.
                    forward.store(false, Ordering::SeqCst);
                }
            }
        }
    });
}

/// `BufRead::read_until` without pulling `BufRead` into the signature: a line,
/// including its terminator, or the byte count read.
fn read_line_bytes<R: std::io::BufRead>(reader: &mut R, into: &mut Vec<u8>) -> std::io::Result<usize> {
    reader.read_until(b'\n', into)
}

/// Read a log back, honouring `target`, `since`, and `tail` in that order.
pub fn read(
    path: &Path,
    target: Option<&str>,
    options: &LogOptions,
) -> Result<Vec<LogLine>> {
    let records = read_from(path, 0)?.0;
    Ok(select(records, target, options))
}

/// A followed stream: the selected backlog first, then every new record as the
/// file grows.
pub fn follow(path: &Path, target: Option<&str>, options: &LogOptions) -> Result<LogStream> {
    let (records, offset) = read_from(path, 0)?;
    let pending: VecDeque<LogLine> = select(records, target, options).into();
    let cursor = pending.iter().map(|line| line.seq).max().unwrap_or(0);
    let state = State {
        path: path.to_path_buf(),
        offset,
        cursor,
        pending,
        target: target.map(str::to_string),
    };
    let stream = futures_util::stream::unfold(state, |mut state| async move {
        loop {
            if let Some(line) = state.pending.pop_front() {
                state.cursor = state.cursor.max(line.seq);
                return Some((Ok(line), state));
            }
            tokio::time::sleep(FOLLOW_POLL).await;
            match read_from(&state.path, state.offset) {
                Ok((records, offset)) => {
                    state.offset = offset;
                    for line in records {
                        if line.seq > state.cursor
                            && state
                                .target
                                .as_deref()
                                .is_none_or(|name| line.resource == name)
                        {
                            state.pending.push_back(line);
                        }
                    }
                }
                // A log that is being rewritten, or that is not there yet, is
                // not an error for a follower: wait for it to come back.
                Err(_) => {}
            }
        }
    });
    Ok(Box::pin(stream) as LogStream)
}

struct State {
    path: PathBuf,
    offset: u64,
    cursor: u64,
    pending: VecDeque<LogLine>,
    target: Option<String>,
}

/// Every record from `offset`, plus the offset just past the last complete one.
fn read_from(path: &Path, offset: u64) -> Result<(Vec<LogLine>, u64)> {
    let mut file = File::open(path).map_err(|err| BackendError::NotProvisioned {
        detail: format!("no log at {}: {err}", path.display()),
    })?;
    if offset > 0 {
        file.seek(SeekFrom::Start(offset))
            .map_err(|err| BackendError::BackendUnavailable {
                detail: format!("cannot read the Environment log {}: {err}", path.display()),
            })?;
    }
    let mut text = String::new();
    file.read_to_string(&mut text)
        .map_err(|err| BackendError::BackendUnavailable {
            detail: format!("cannot read the Environment log {}: {err}", path.display()),
        })?;

    let mut records = Vec::new();
    let mut consumed = 0usize;
    for chunk in text.split_inclusive('\n') {
        if !chunk.ends_with('\n') {
            // A half-written trailer is not a record yet. A follower will see it
            // once it is complete.
            break;
        }
        consumed += chunk.len();
        if let Ok(record) = serde_json::from_str::<LogLine>(chunk.trim_end()) {
            records.push(record);
        }
    }
    Ok((records, offset + consumed as u64))
}

/// `target`, then `since`, then `tail`: filtering first, bounding last, is what
/// makes `tail=2` the *newest* two lines of what was asked for.
fn select(records: Vec<LogLine>, target: Option<&str>, options: &LogOptions) -> Vec<LogLine> {
    let selected: Vec<LogLine> = records
        .into_iter()
        .filter(|line| target.is_none_or(|name| line.resource == name))
        .filter(|line| after_cursor(line, options.since.as_deref()))
        .collect();
    match options.tail {
        Some(tail) => {
            let start = selected.len().saturating_sub(tail);
            selected[start..].to_vec()
        }
        None => selected,
    }
}

/// `since` is a sequence number (exact) or an RFC 3339 timestamp.
fn after_cursor(line: &LogLine, since: Option<&str>) -> bool {
    match since.map(str::trim).filter(|raw| !raw.is_empty()) {
        None => true,
        Some(raw) => match raw.parse::<u64>() {
            Ok(seq) => line.seq > seq,
            Err(_) => line.at.as_str() >= raw,
        },
    }
}

/// How many records the file already holds: the next sequence number is one
/// more, so a restart continues the same counter.
fn count_records(path: &Path) -> u64 {
    let Ok(raw) = std::fs::read(path) else {
        return 0;
    };
    let text = String::from_utf8_lossy(&raw);
    text.split_inclusive('\n')
        .filter(|chunk| chunk.ends_with('\n'))
        .count() as u64
}

/// RFC 3339, UTC, millisecond precision: comparable as a string.
pub fn now_rfc3339_millis() -> String {
    let now = time::OffsetDateTime::now_utc();
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        now.year(),
        u8::from(now.month()),
        now.day(),
        now.hour(),
        now.minute(),
        now.second(),
        now.nanosecond() / 1_000_000
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "agentify-wsl-log-{label}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir.join("environment.log")
    }

    fn seeded(label: &str, lines: &[(&str, &str)]) -> PathBuf {
        let path = scratch(label);
        let mut sink = LogSink::open(&path).expect("open");
        for (resource, line) in lines {
            sink.append(resource, "stdout", line);
        }
        path
    }

    #[test]
    fn a_record_round_trips_through_the_file() {
        let path = seeded(
            "round-trip",
            &[("supervisor", "first"), ("supervisor", "second")],
        );
        let lines = read(&path, None, &LogOptions::default()).expect("read");
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].seq, 1);
        assert_eq!(lines[1].seq, 2);
        assert_eq!(lines[1].line, "second");
        assert_eq!(lines[1].stream, "stdout");
        assert!(lines[1].at.ends_with('Z'), "{}", lines[1].at);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn the_sequence_continues_across_a_reopen() {
        let path = seeded("reopen", &[("supervisor", "a"), ("supervisor", "b")]);
        drop(LogSink::open(&path).expect("reopen"));
        let mut again = LogSink::open(&path).expect("reopen");
        let record = again.append("supervisor", "stdout", "c").expect("append");
        assert_eq!(record.seq, 3, "a restart does not renumber the log");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn tail_and_since_bound_what_is_returned() {
        let path = seeded(
            "tail",
            &[
                ("supervisor", "one"),
                ("supervisor", "two"),
                ("supervisor", "three"),
            ],
        );
        let tail = read(
            &path,
            None,
            &LogOptions {
                tail: Some(2),
                ..LogOptions::default()
            },
        )
        .expect("read");
        assert_eq!(tail.len(), 2, "tail is a bound, not a filter");
        assert_eq!(tail[1].line, "three", "a tail keeps the newest lines");

        let since = read(&path, None, &LogOptions::default().since("2")).expect("read");
        assert_eq!(since.len(), 1);
        assert_eq!(since[0].seq, 3);

        let timestamp = read(&path, None, &LogOptions::default().since(&tail[0].at))
            .expect("read");
        assert!(
            timestamp.iter().all(|line| line.at >= tail[0].at),
            "an RFC 3339 cursor is a timestamp comparison"
        );

        let other = read(&path, Some("web"), &LogOptions::default()).expect("read");
        assert!(other.is_empty(), "a Resource filter that matches nothing");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn a_missing_log_is_not_provisioned() {
        let path = scratch("missing");
        let err = read(&path, None, &LogOptions::default()).expect_err("no log");
        assert_eq!(err.code(), "NotProvisioned");
    }

    #[test]
    fn a_half_written_trailer_is_not_a_record() {
        let path = seeded("partial", &[("supervisor", "complete")]);
        {
            let mut file = OpenOptions::new().append(true).open(&path).expect("append");
            file.write_all(b"{\"seq\":9,\"at\":\"trunc").expect("write");
        }
        let lines = read(&path, None, &LogOptions::default()).expect("read");
        assert_eq!(lines.len(), 1, "a partial line is skipped, not guessed at");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[tokio::test]
    async fn a_follower_sees_lines_written_after_it_opened() {
        use futures_util::StreamExt;

        let path = seeded("follow", &[("supervisor", "before")]);
        let mut stream = follow(&path, None, &LogOptions::following()).expect("follow");
        let mut appender = LogSink::open(&path).expect("open");

        let first = tokio::time::timeout(Duration::from_secs(2), stream.next())
            .await
            .expect("the backlog arrives")
            .expect("a line")
            .expect("a good line");
        assert_eq!(first.line, "before");

        appender.append("supervisor", "stdout", "after");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        loop {
            let next = tokio::time::timeout_at(deadline, stream.next())
                .await
                .expect("a follow must see a new line");
            let line = next.expect("still open").expect("a good line");
            if line.line == "after" {
                break;
            }
        }
        drop(stream);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn a_tee_writes_every_line_and_forwards_while_someone_listens() {
        let path = scratch("tee");
        let sink = Arc::new(Mutex::new(LogSink::open(&path).expect("open")));
        let forward = Arc::new(AtomicBool::new(true));
        let (sender, receiver) = std::sync::mpsc::channel();
        spawn_tee(
            std::io::Cursor::new(b"agentify-supervise: ready 41000\nsecond\n".to_vec()),
            sink.clone(),
            "stdout",
            "supervisor",
            forward,
            Some(sender),
        );

        assert_eq!(
            receiver.recv_timeout(Duration::from_secs(2)).expect("first"),
            "agentify-supervise: ready 41000"
        );
        assert_eq!(receiver.recv_timeout(Duration::from_secs(2)).expect("second"), "second");
        let lines = read(&path, None, &LogOptions::default()).expect("read");
        assert_eq!(lines.len(), 2, "the file holds everything the pipe carried");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
