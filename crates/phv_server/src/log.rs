//! The request log: one line per request on stderr, and in `[server]
//! access_log` when phv.toml sets one.
//!
//! The lines are written by a thread of their own, in batches: a request
//! never waits for the terminal or the disk (stderr is unbuffered, and a
//! write to it, or to the log file, held a lock every request shared).

use std::io::Write;
use std::sync::mpsc;

/// Lines waiting to be written; a request waits for room past this.
const QUEUE: usize = 16 * 1024;

enum Msg {
    /// A line for stderr and, if there is one, for the access log.
    Line(String, Option<String>),
    /// Write everything sent before, then answer.
    Flush(mpsc::SyncSender<()>),
}

#[derive(Clone)]
pub(crate) struct Logger {
    tx: mpsc::SyncSender<Msg>,
    /// Whether lines go to an access log too.
    has_file: bool,
}

impl Logger {
    /// Starts the thread that writes the lines.
    pub(crate) fn start(file: Option<std::fs::File>) -> Logger {
        let (tx, rx) = mpsc::sync_channel(QUEUE);
        let has_file = file.is_some();
        let spawned = std::thread::Builder::new().name("phv-log".into()).spawn(move || write_lines(rx, file));
        if let Err(e) = spawned {
            eprintln!("warning: cannot start the request log ({e})");
        }
        Logger { tx, has_file }
    }

    pub(crate) fn has_file(&self) -> bool {
        self.has_file
    }

    pub(crate) fn line(&self, stderr: String, access: Option<String>) {
        let _ = self.tx.send(Msg::Line(stderr, access));
    }

    /// Waits until the lines sent so far are written (when the server stops).
    pub(crate) fn flush(&self) {
        let (done, wait) = mpsc::sync_channel(1);
        if self.tx.send(Msg::Flush(done)).is_ok() {
            let _ = wait.recv_timeout(std::time::Duration::from_secs(5));
        }
    }
}

/// Writes lines as they come; buffers are flushed whenever none is waiting.
fn write_lines(rx: mpsc::Receiver<Msg>, file: Option<std::fs::File>) {
    let mut err = std::io::BufWriter::new(std::io::stderr());
    let mut file = file.map(std::io::BufWriter::new);
    while let Ok(first) = rx.recv() {
        let mut next = Some(first);
        let mut flushed = Vec::new();
        while let Some(msg) = next {
            match msg {
                Msg::Line(line, access) => {
                    let _ = err.write_all(line.as_bytes());
                    if let (Some(f), Some(a)) = (file.as_mut(), access) {
                        let _ = f.write_all(a.as_bytes());
                    }
                }
                Msg::Flush(done) => flushed.push(done),
            }
            next = rx.try_recv().ok();
        }
        let _ = err.flush();
        if let Some(f) = file.as_mut() {
            let _ = f.flush();
        }
        for done in flushed {
            let _ = done.send(());
        }
    }
}
