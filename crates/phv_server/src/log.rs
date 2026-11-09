//! The request log: one line per request on stderr, and in `[server]
//! access_log` when phv.toml sets one.
//!
//! The lines are written by a thread of their own, in batches: a request
//! never waits for the terminal or the disk (stderr is unbuffered, and a
//! write to it, or to the log file, held a lock every request shared).

use std::io::Write;
use std::sync::mpsc;
use std::time::Duration;

/// Lines waiting to be written; a request waits for room past this. Lines
/// show at most `LOG_PATH_MAX` bytes of a path, so this holds a few MB.
const QUEUE: usize = 4 * 1024;

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
            let _ = wait.recv_timeout(Duration::from_secs(5));
        }
    }
}

/// While lines keep coming, the thread takes them this often instead of
/// waiting in the queue: a request whose line finds the thread waiting must
/// wake it (a system call, about a hundredth of a trivial request).
const BATCH: Duration = Duration::from_millis(1);

/// Writes lines as they come; buffers are flushed whenever none is waiting.
fn write_lines(rx: mpsc::Receiver<Msg>, file: Option<std::fs::File>) {
    let mut err = std::io::BufWriter::new(std::io::stderr());
    let mut file = file.map(std::io::BufWriter::new);
    while let Ok(first) = rx.recv() {
        let mut next = Some(first);
        // Until a pause of `BATCH` with no line.
        while next.is_some() {
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
            std::thread::sleep(BATCH);
            next = rx.try_recv().ok();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every line is written, in the order each thread sent its lines, both
    /// while lines keep coming (taken in batches) and after a pause (the
    /// thread waits in the queue again and is woken by the next line).
    #[test]
    fn lines_are_written_in_order_in_batches_and_after_a_pause() {
        let dir = std::env::temp_dir().join(format!("phv-log-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("access.log");
        let log = Logger::start(Some(std::fs::File::create(&path).unwrap()));
        let senders: Vec<_> = (0..4)
            .map(|t| {
                let log = log.clone();
                std::thread::spawn(move || {
                    for n in 0..300 {
                        // Nothing on standard error: the test's output stays clean.
                        log.line(String::new(), Some(format!("{t} {n}\n")));
                        if n % 50 == 0 {
                            std::thread::sleep(Duration::from_millis(3));
                        }
                    }
                })
            })
            .collect();
        for s in senders {
            s.join().unwrap();
        }
        log.flush();
        let text = std::fs::read_to_string(&path).unwrap();
        let mut next = [0; 4];
        for line in text.lines() {
            let (t, n) = line.split_once(' ').unwrap();
            let (t, n): (usize, usize) = (t.parse().unwrap(), n.parse().unwrap());
            assert_eq!(n, next[t], "thread {t}'s lines out of order");
            next[t] += 1;
        }
        assert_eq!(next, [300; 4]);
        // After a pause, a line is written without a flush.
        std::thread::sleep(Duration::from_millis(50));
        log.line(String::new(), Some("late\n".into()));
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !std::fs::read_to_string(&path).unwrap().ends_with("late\n") {
            assert!(std::time::Instant::now() < deadline, "the line after a pause was not written");
            std::thread::sleep(Duration::from_millis(5));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
