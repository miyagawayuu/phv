//! `phv dap`: a Debug Adapter Protocol server over stdin/stdout, for VS Code
//! and other editors. It runs a script with breakpoints, stepping (over, in,
//! out), pause, the call stack, variables (lists, maps and objects can be
//! opened), and evaluation of a variable name (hover).
//!
//! The script is compiled with a marker before each statement; the VM calls
//! `Hook` there, on the script's thread. While the script is stopped, the hook
//! waits for commands and answers the requests that need the VM (stack,
//! variables); the main thread reads the protocol and forwards them.

use phv_driver::{compile_for_debug, find_root, load_config, run_debug, RunOptions};
use phv_vm::vm::{DebugFrame, DebugHook};
use phv_vm::{Key, Limits, Program, Value, Vm, VmError};
use serde_json::{json, Value as Json};
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};

/// Items of a list or map shown when it is opened.
const MAX_CHILDREN: usize = 1000;
/// Longest value text shown.
const MAX_TEXT: usize = 500;

/// A message, framed as in the LSP (`Content-Length` header); `Null` when it is not JSON.
fn read_message(input: &mut impl BufRead) -> Option<Json> {
    crate::lsp::read_message(input).map(|(msg, _)| msg.unwrap_or(Json::Null))
}

/// Writes protocol messages, numbering them.
#[derive(Clone)]
struct Out {
    w: Arc<Mutex<Box<dyn Write + Send>>>,
    seq: Arc<AtomicI64>,
}

impl Out {
    fn send(&self, mut msg: Json) {
        msg["seq"] = json!(self.seq.fetch_add(1, Ordering::SeqCst) + 1);
        let body = msg.to_string();
        if let Ok(mut w) = self.w.lock() {
            let _ = write!(w, "Content-Length: {}\r\n\r\n{body}", body.len());
            let _ = w.flush();
        }
    }

    fn respond(&self, req: &Json, body: Json) {
        self.send(json!({
            "type": "response", "request_seq": req["seq"], "success": true,
            "command": req["command"], "body": body,
        }));
    }

    fn fail(&self, req: &Json, message: &str) {
        self.send(json!({
            "type": "response", "request_seq": req["seq"], "success": false,
            "command": req["command"], "message": message,
        }));
    }

    fn event(&self, event: &str, body: Json) {
        self.send(json!({ "type": "event", "event": event, "body": body }));
    }
}

/// What the stopped script is told to do.
enum Cmd {
    /// A request that needs the VM: answered by the hook.
    Inspect(Json),
    Continue,
    Next,
    StepIn,
    StepOut,
    Terminate,
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Run,
    /// Stop at the next statement of the project.
    StepIn,
    /// Stop at the next statement in a frame at most this deep.
    Next(usize),
    /// ... less deep.
    Out(usize),
}

/// Shared between the protocol thread and the script's thread.
struct Shared {
    /// Lines with breakpoints, by the file's name in the program (`lib/x.phv`).
    breakpoints: Mutex<HashMap<String, HashSet<usize>>>,
    /// Changes whenever `breakpoints` does, so the hook copies them only then.
    generation: std::sync::atomic::AtomicU64,
    pause: AtomicBool,
    terminate: AtomicBool,
    /// The script is stopped and waiting for commands.
    stopped: AtomicBool,
}

struct Hook {
    out: Out,
    shared: Arc<Shared>,
    rx: Receiver<Cmd>,
    prog: Arc<Program>,
    root: PathBuf,
    mode: Mode,
    /// Bytes of the script's output already sent (read again when it ends).
    sent: Arc<std::sync::atomic::AtomicUsize>,
    /// The statement before this one: (file, line, depth).
    prev: Option<(u32, usize, usize)>,
    /// The breakpoints by file id, as of `generation` (`u64::MAX`: not read yet).
    breakpoints: HashMap<u32, HashSet<usize>>,
    generation: u64,
    /// Values that can be opened in the variables view while stopped.
    values: Vec<Value>,
    /// Where the script is stopped (the top frame's statement).
    here: (u32, u32),
}

fn line_of(prog: &Program, file: u32, offset: u32) -> usize {
    prog.files.get(file as usize).map_or(1, |f| f.line_starts.partition_point(|s| *s <= offset).max(1))
}

/// The file's name in the program, as the editor's path is matched against it.
fn rel_name(root: &Path, path: &Path) -> Option<String> {
    let rel = match path.strip_prefix(root) {
        Ok(rel) => rel.to_path_buf(),
        // The editor may spell the path differently (case, links, short names).
        Err(_) => path.canonicalize().ok()?.strip_prefix(root.canonicalize().ok()?).ok()?.to_path_buf(),
    };
    Some(rel.to_string_lossy().replace('\\', "/"))
}

impl Hook {
    fn flush_output(&mut self, vm: &Vm) {
        let buf = &vm.out[0].buf;
        let sent = self.sent.load(Ordering::SeqCst);
        if buf.len() > sent {
            if let Some(text) = buf.get(sent..) {
                self.out.event("output", json!({ "category": "stdout", "output": text }));
                self.sent.store(buf.len(), Ordering::SeqCst);
            }
        }
    }

    fn file_name(&self, file: u32) -> &str {
        self.prog.files.get(file as usize).map_or("", |f| f.name.as_str())
    }

    fn source(&self, file: u32) -> Json {
        let name = self.file_name(file);
        if name.starts_with('<') {
            json!({ "name": name, "presentationHint": "deemphasize" })
        } else {
            json!({ "name": name, "path": self.root.join(name).to_string_lossy() })
        }
    }

    /// The frames, innermost first, with the file and line of each.
    fn frames(&self, vm: &Vm) -> Vec<(DebugFrame, u32, usize)> {
        let frames = vm.debug_frames();
        let n = frames.len();
        frames
            .into_iter()
            .enumerate()
            .rev()
            .map(|(i, f)| {
                let func = &self.prog.funcs[f.func as usize];
                let offset = if i + 1 == n {
                    self.here.1
                } else {
                    func.offsets.get((f.pc as usize).saturating_sub(1)).copied().unwrap_or(0)
                };
                (f, func.file, line_of(&self.prog, func.file, offset))
            })
            .collect()
    }

    fn keep(&mut self, v: &Value) -> i64 {
        let openable = match v {
            Value::List(l) => !l.is_empty(),
            Value::Map(m) => !m.is_empty(),
            Value::Object(_) => true,
            _ => false,
        };
        if !openable {
            return 0;
        }
        self.values.push(v.clone());
        (VALUE_REF + self.values.len() - 1) as i64
    }

    fn variable(&mut self, name: String, v: &Value) -> Json {
        let reference = self.keep(v);
        json!({ "name": name, "value": show(&self.prog, v), "type": v.type_name(), "variablesReference": reference })
    }

    fn inspect(&mut self, vm: &Vm, req: &Json) {
        let args = &req["arguments"];
        match req["command"].as_str().unwrap_or("") {
            "stackTrace" => {
                let frames: Vec<Json> = self
                    .frames(vm)
                    .into_iter()
                    .enumerate()
                    .map(|(i, (f, file, line))| {
                        json!({
                            "id": i, "name": self.prog.funcs[f.func as usize].name,
                            "source": self.source(file), "line": line, "column": 1,
                        })
                    })
                    .collect();
                self.out.respond(req, json!({ "stackFrames": frames, "totalFrames": frames.len() }));
            }
            "scopes" => {
                let id = args["frameId"].as_u64().unwrap_or(0) as usize;
                self.out.respond(
                    req,
                    json!({ "scopes": [{ "name": "Locals", "variablesReference": FRAME_REF + id, "expensive": false }] }),
                );
            }
            "variables" => {
                let reference = args["variablesReference"].as_u64().unwrap_or(0) as usize;
                let vars = self.children(vm, reference);
                self.out.respond(req, json!({ "variables": vars }));
            }
            "evaluate" => {
                let name = args["expression"].as_str().unwrap_or("").trim().trim_start_matches('$').to_string();
                let frame = args["frameId"].as_u64().unwrap_or(0) as usize;
                match self.locals(vm, frame).into_iter().find(|(n, _)| *n == name) {
                    Some((_, v)) => {
                        let reference = self.keep(&v);
                        self.out
                            .respond(req, json!({ "result": show(&self.prog, &v), "variablesReference": reference }));
                    }
                    None => self.out.fail(req, "only the name of a variable can be evaluated"),
                }
            }
            _ => self.out.fail(req, "not supported"),
        }
    }

    /// The variables of frame `index` (0 = innermost) and their values.
    fn locals(&self, vm: &Vm, index: usize) -> Vec<(String, Value)> {
        let Some((f, _, _)) = self.frames(vm).into_iter().nth(index) else { return Vec::new() };
        let func = &self.prog.funcs[f.func as usize];
        let mut out: Vec<(String, Value)> = Vec::new();
        for (name, reg, nullable) in &func.locals {
            if out.iter().any(|(n, _)| n == name) {
                continue;
            }
            match vm.register(f.base + *reg as usize) {
                // Not assigned yet in this call.
                Some(Value::Null) if !nullable => {}
                Some(v) => out.push((name.clone(), v.clone())),
                None => {}
            }
        }
        out
    }

    fn children(&mut self, vm: &Vm, reference: usize) -> Vec<Json> {
        if (FRAME_REF..VALUE_REF).contains(&reference) {
            let locals = self.locals(vm, reference - FRAME_REF);
            return locals.into_iter().map(|(n, v)| self.variable(format!("${n}"), &v)).collect();
        }
        let Some(v) = self.values.get(reference.wrapping_sub(VALUE_REF)).cloned() else { return Vec::new() };
        let mut out = Vec::new();
        match &v {
            Value::List(items) => {
                for (i, x) in items.iter().take(MAX_CHILDREN).enumerate() {
                    out.push(self.variable(format!("[{i}]"), x));
                }
            }
            Value::Map(m) => {
                for (k, x) in m.iter().take(MAX_CHILDREN) {
                    let key = match k {
                        Key::Int(i) => i.to_string(),
                        Key::Str(s) => quoted_prefix(s),
                    };
                    out.push(self.variable(format!("[{key}]"), x));
                }
            }
            Value::Object(o) => {
                let o = o.borrow();
                let fields: Vec<(String, Value)> = self.prog.classes[o.class as usize]
                    .fields
                    .iter()
                    .zip(o.fields.iter())
                    .map(|(f, x)| (f.name.clone(), x.clone()))
                    .collect();
                drop(o);
                for (name, x) in fields {
                    out.push(self.variable(format!("${name}"), &x));
                }
            }
            _ => {}
        }
        out
    }
}

/// References of the variables view: a frame's locals, then kept values.
const FRAME_REF: usize = 1;
const VALUE_REF: usize = 100_000;

/// A short text for a value.
/// A string quoted with escapes, from its first characters only (a huge string
/// is not copied to be shown).
fn quoted_prefix(s: &str) -> String {
    let mut chars = s.chars();
    let head: String = chars.by_ref().take(MAX_TEXT).collect();
    let more = chars.next().is_some();
    format!("{head:?}{}", if more { "…" } else { "" })
}

fn show(prog: &Program, v: &Value) -> String {
    let s = match v {
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Int(i) => i.to_string(),
        Value::Float(f) => phv_vm::value::format_float(*f),
        Value::Str(s) => quoted_prefix(s),
        Value::Html(s) => format!("Html({})", quoted_prefix(s)),
        Value::List(l) => format!("list({})", l.len()),
        Value::Map(m) => format!("map({})", m.len()),
        Value::Object(o) => {
            let o = o.borrow();
            let class = &prog.classes[o.class as usize];
            let name = class.fields.iter().position(|f| f.name == "name").and_then(|i| o.fields.get(i));
            match (class.cases.is_empty(), name) {
                (false, Some(Value::Str(case))) => format!("{}::{}", class.name, case),
                _ => class.name.clone(),
            }
        }
        Value::Closure(_) => "closure".to_string(),
    };
    if s.chars().count() > MAX_TEXT {
        s.chars().take(MAX_TEXT).collect::<String>() + "…"
    } else {
        s
    }
}

impl DebugHook for Hook {
    fn statement(&mut self, vm: &Vm, file: u32, offset: u32) -> Result<(), VmError> {
        if self.shared.terminate.load(Ordering::SeqCst) {
            return Err(VmError::Fatal("stopped by the debugger".into()));
        }
        // The standard library is not stepped through.
        if self.file_name(file).starts_with('<') {
            return Ok(());
        }
        let line = line_of(&self.prog, file, offset);
        let depth = vm.debug_frames().len();
        let here = (file, line, depth);
        let same_line = self.prev == Some(here);
        self.prev = Some(here);
        let generation = self.shared.generation.load(Ordering::SeqCst);
        if generation != self.generation {
            self.generation = generation;
            let by_name = self.shared.breakpoints.lock().map(|b| b.clone()).unwrap_or_default();
            self.breakpoints = (0..self.prog.files.len() as u32)
                .filter_map(|f| by_name.get(self.file_name(f)).map(|lines| (f, lines.clone())))
                .collect();
        }
        let at_breakpoint = !same_line && self.breakpoints.get(&file).is_some_and(|l| l.contains(&line));
        let reason = if self.shared.pause.swap(false, Ordering::SeqCst) {
            "pause"
        } else if at_breakpoint {
            "breakpoint"
        } else {
            match self.mode {
                Mode::StepIn if !same_line => "step",
                Mode::Next(d) if depth <= d && !same_line => "step",
                Mode::Out(d) if depth < d => "step",
                _ => return Ok(()),
            }
        };
        self.flush_output(vm);
        self.here = (file, offset);
        self.values.clear();
        self.shared.stopped.store(true, Ordering::SeqCst);
        self.out.event("stopped", json!({ "reason": reason, "threadId": 1, "allThreadsStopped": true }));
        loop {
            let cmd = self.rx.recv().unwrap_or(Cmd::Terminate);
            self.mode = match cmd {
                Cmd::Inspect(req) => {
                    self.inspect(vm, &req);
                    continue;
                }
                Cmd::Continue => Mode::Run,
                Cmd::Next => Mode::Next(depth),
                Cmd::StepIn => Mode::StepIn,
                Cmd::StepOut => Mode::Out(depth),
                Cmd::Terminate => {
                    self.shared.stopped.store(false, Ordering::SeqCst);
                    return Err(VmError::Fatal("stopped by the debugger".into()));
                }
            };
            break;
        }
        self.shared.stopped.store(false, Ordering::SeqCst);
        Ok(())
    }
}

/// Starts the script on its own thread; it reports its end with events.
fn launch(
    out: &Out,
    shared: &Arc<Shared>,
    args: &Json,
    rx: Receiver<Cmd>,
) -> Result<std::thread::JoinHandle<()>, String> {
    let program = PathBuf::from(args["program"].as_str().ok_or("launch needs \"program\", the script to run")?);
    let root = find_root(&program);
    let compiled = compile_for_debug(&root, &program).map_err(|e| e.render(false))?;
    let config = load_config(&root)?;
    let mut host = (*config.host).clone();
    host.cli = true;
    host.no_stdin = true;
    host.args = args["args"]
        .as_array()
        .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    let limits = Limits { max_memory: Some(config.memory_limit_mb as usize * 1024 * 1024), ..Limits::default() };
    let mode = if args["stopOnEntry"].as_bool().unwrap_or(false) { Mode::StepIn } else { Mode::Run };
    let (out, shared) = (out.clone(), shared.clone());
    let sent = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let handle = std::thread::Builder::new()
        .stack_size(64 * 1024 * 1024)
        .spawn(move || {
            let hook = Hook {
                out: out.clone(),
                shared,
                rx,
                prog: compiled.program.clone(),
                root,
                mode,
                sent: sent.clone(),
                prev: None,
                breakpoints: HashMap::new(),
                generation: u64::MAX,
                values: Vec::new(),
                here: (0, 0),
            };
            let opts = RunOptions { html: false, limits, http: None, host: Some(Arc::new(host)) };
            let outcome = run_debug(&compiled, opts, Box::new(hook));
            // What the script wrote after it last stopped.
            let rest = outcome.output.get(sent.load(Ordering::SeqCst)..).unwrap_or("");
            if !rest.is_empty() {
                out.event("output", json!({ "category": "stdout", "output": rest }));
            }
            let code = match outcome.error {
                Some(e) => {
                    out.event("output", json!({ "category": "stderr", "output": format!("{}\n", e.render()) }));
                    1
                }
                None => 0,
            };
            out.event("exited", json!({ "exitCode": code }));
            out.event("terminated", json!({}));
        })
        .map_err(|e| e.to_string())?;
    Ok(handle)
}

pub fn run() {
    let out = Out { w: Arc::new(Mutex::new(Box::new(std::io::stdout()))), seq: Arc::new(AtomicI64::new(0)) };
    serve(&mut std::io::stdin().lock(), &out);
}

fn serve(input: &mut impl BufRead, out: &Out) {
    let shared = Arc::new(Shared {
        breakpoints: Mutex::new(HashMap::new()),
        generation: std::sync::atomic::AtomicU64::new(0),
        pause: AtomicBool::new(false),
        terminate: AtomicBool::new(false),
        stopped: AtomicBool::new(false),
    });
    let (tx, rx): (Sender<Cmd>, Receiver<Cmd>) = mpsc::channel();
    let mut rx = Some(rx);
    let mut launch_args: Option<Json> = None;
    let mut root: Option<PathBuf> = None;
    let mut script: Option<std::thread::JoinHandle<()>> = None;
    while let Some(req) = read_message(input) {
        if req["type"] != "request" {
            continue;
        }
        let args = &req["arguments"];
        match req["command"].as_str().unwrap_or("") {
            "initialize" => out.respond(
                &req,
                json!({
                    "supportsConfigurationDoneRequest": true,
                    "supportsTerminateRequest": true,
                    "supportsEvaluateForHovers": true,
                }),
            ),
            "launch" => {
                let Some(program) = args["program"].as_str() else {
                    out.fail(&req, "launch needs \"program\", the script to run");
                    continue;
                };
                root = Some(find_root(Path::new(program)));
                launch_args = Some(args.clone());
                out.respond(&req, json!({}));
                out.event("initialized", json!({}));
            }
            "setBreakpoints" => {
                let lines: Vec<usize> = args["breakpoints"]
                    .as_array()
                    .map(|b| b.iter().filter_map(|x| x["line"].as_u64().map(|l| l as usize)).collect())
                    .unwrap_or_default();
                let path = PathBuf::from(args["source"]["path"].as_str().unwrap_or(""));
                let name = root.as_deref().and_then(|r| rel_name(r, &path));
                if let (Some(name), Ok(mut b)) = (&name, shared.breakpoints.lock()) {
                    b.insert(name.clone(), lines.iter().copied().collect());
                    shared.generation.fetch_add(1, Ordering::SeqCst);
                }
                let verified = name.is_some();
                let bps: Vec<Json> = lines.iter().map(|l| json!({ "verified": verified, "line": l })).collect();
                out.respond(&req, json!({ "breakpoints": bps }));
            }
            "configurationDone" => {
                out.respond(&req, json!({}));
                if let (Some(args), Some(rx)) = (&launch_args, rx.take()) {
                    match launch(out, &shared, args, rx) {
                        Ok(h) => script = Some(h),
                        Err(e) => {
                            out.event("output", json!({ "category": "stderr", "output": format!("{e}\n") }));
                            out.event("terminated", json!({}));
                        }
                    }
                }
            }
            "threads" => out.respond(&req, json!({ "threads": [{ "id": 1, "name": "main" }] })),
            "stackTrace" | "scopes" | "variables" | "evaluate" => {
                if shared.stopped.load(Ordering::SeqCst) {
                    let _ = tx.send(Cmd::Inspect(req.clone()));
                } else if req["command"] == "stackTrace" {
                    out.respond(&req, json!({ "stackFrames": [], "totalFrames": 0 }));
                } else {
                    out.fail(&req, "the script is running");
                }
            }
            "continue" | "next" | "stepIn" | "stepOut" => {
                let cmd = match req["command"].as_str().unwrap_or("") {
                    "continue" => Cmd::Continue,
                    "next" => Cmd::Next,
                    "stepIn" => Cmd::StepIn,
                    _ => Cmd::StepOut,
                };
                // Running from now on: requests that come before the script's thread
                // takes the command must not be queued for it (they would be answered
                // at the next stop, with that stop's state).
                if shared.stopped.swap(false, Ordering::SeqCst) {
                    let _ = tx.send(cmd);
                }
                out.respond(&req, json!({ "allThreadsContinued": true }));
            }
            "pause" => {
                shared.pause.store(true, Ordering::SeqCst);
                out.respond(&req, json!({}));
            }
            "terminate" | "disconnect" => {
                shared.terminate.store(true, Ordering::SeqCst);
                let _ = tx.send(Cmd::Terminate);
                out.respond(&req, json!({}));
                if req["command"] == "disconnect" {
                    break;
                }
            }
            _ => out.fail(&req, "not supported"),
        }
    }
    shared.terminate.store(true, Ordering::SeqCst);
    let _ = tx.send(Cmd::Terminate);
    if let Some(h) = script {
        let _ = h.join();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    /// A writer the test reads back.
    #[derive(Clone, Default)]
    struct Buf(Arc<Mutex<Vec<u8>>>);

    impl Write for Buf {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn frame(msg: Json) -> Vec<u8> {
        let body = msg.to_string();
        format!("Content-Length: {}\r\n\r\n{body}", body.len()).into_bytes()
    }

    /// Feeds requests one at a time, each after the server's messages that
    /// `wait` says must come before it.
    struct Session {
        tx: Sender<Vec<u8>>,
        buf: Buf,
        server: std::thread::JoinHandle<()>,
        seq: i64,
    }

    struct ChannelReader {
        rx: Receiver<Vec<u8>>,
        pending: std::io::Cursor<Vec<u8>>,
    }

    impl Read for ChannelReader {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            loop {
                let n = self.pending.read(out)?;
                if n > 0 {
                    return Ok(n);
                }
                match self.rx.recv() {
                    Ok(b) => self.pending = std::io::Cursor::new(b),
                    Err(_) => return Ok(0),
                }
            }
        }
    }

    impl Session {
        fn start() -> Session {
            let (tx, rx) = mpsc::channel();
            let buf = Buf::default();
            let out = Out { w: Arc::new(Mutex::new(Box::new(buf.clone()))), seq: Arc::new(AtomicI64::new(0)) };
            let server = std::thread::spawn(move || {
                let mut input =
                    std::io::BufReader::new(ChannelReader { rx, pending: std::io::Cursor::new(Vec::new()) });
                serve(&mut input, &out);
            });
            Session { tx, buf, server, seq: 0 }
        }

        fn request(&mut self, command: &str, arguments: Json) {
            self.seq += 1;
            let msg = json!({ "seq": self.seq, "type": "request", "command": command, "arguments": arguments });
            self.tx.send(frame(msg)).unwrap();
        }

        fn messages(&self) -> Vec<Json> {
            let data = self.buf.0.lock().unwrap().clone();
            let mut input = &data[..];
            std::iter::from_fn(|| read_message(&mut input)).collect()
        }

        /// Waits until a message matching `f` has been written; returns it.
        fn wait(&self, what: &str, f: impl Fn(&Json) -> bool) -> Json {
            let start = std::time::Instant::now();
            loop {
                if let Some(m) = self.messages().into_iter().find(|m| f(m)) {
                    return m;
                }
                assert!(start.elapsed() < std::time::Duration::from_secs(20), "no {what}: {:?}", self.messages());
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }

        fn stopped_count(&self) -> usize {
            self.messages().iter().filter(|m| m["event"] == "stopped").count()
        }

        fn wait_stopped(&self, n: usize) -> Json {
            self.wait("stop", |_| self.stopped_count() >= n);
            self.messages().into_iter().filter(|m| m["event"] == "stopped").nth(n - 1).unwrap()
        }

        fn response(&self, seq: i64) -> Json {
            self.wait("response", |m| m["type"] == "response" && m["request_seq"] == seq)
        }
    }

    /// A huge string (or map key) is shown from its first characters, cheaply.
    #[test]
    fn huge_values_are_shown_short() {
        let s = "\u{1}".repeat(20_000_000);
        let start = std::time::Instant::now();
        let shown = quoted_prefix(&s);
        assert!(shown.len() < 8 * MAX_TEXT, "{}", shown.len()); // escapes take up to 7 bytes a character
        assert!(shown.ends_with('…'));
        assert!(start.elapsed() < std::time::Duration::from_secs(1));
        assert_eq!(quoted_prefix("a\"b"), "\"a\\\"b\"");
    }

    #[test]
    fn breakpoints_steps_and_variables() {
        let dir = std::env::temp_dir().join(format!("phv-dap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("lib")).unwrap();
        std::fs::write(dir.join("phv.toml"), "[project]\nname = \"d\"\n").unwrap();
        std::fs::write(
            dir.join("lib/math.phv"),
            "export function double(int $n): int {\n    $twice = $n * 2;\n    return $twice;\n}\n",
        )
        .unwrap();
        let main = "<?phv\nimport lib.math.double;\necho \"start\\n\";\n$xs = [1, 2, 3];\n$total = 0;\nforeach ($xs as $x) {\n    $total += double($x);\n}\necho \"total {$total}\\n\";\n";
        std::fs::write(dir.join("main.phv"), main).unwrap();
        let program = dir.join("main.phv");

        let mut s = Session::start();
        s.request("initialize", json!({ "adapterID": "phv" }));
        s.request("launch", json!({ "program": program.to_string_lossy() }));
        s.wait("initialized", |m| m["event"] == "initialized");
        s.request(
            "setBreakpoints",
            json!({ "source": { "path": program.to_string_lossy() }, "breakpoints": [{ "line": 7 }] }),
        );
        assert_eq!(s.response(3)["body"]["breakpoints"][0]["verified"], true);
        s.request("configurationDone", json!({}));
        assert_eq!(s.wait_stopped(1)["body"]["reason"], "breakpoint");

        s.request("stackTrace", json!({ "threadId": 1 }));
        let st = s.response(5);
        assert_eq!(st["body"]["stackFrames"][0]["line"], 7, "{st}");
        s.request("variables", json!({ "variablesReference": FRAME_REF }));
        let vars = s.response(6)["body"]["variables"].clone();
        let find = |name: &str| vars.as_array().unwrap().iter().find(|v| v["name"] == name).cloned().unwrap();
        assert_eq!(find("$x")["value"], "1");
        assert_eq!(find("$xs")["value"], "list(3)");
        let xs_ref = find("$xs")["variablesReference"].as_i64().unwrap();
        s.request("variables", json!({ "variablesReference": xs_ref }));
        assert_eq!(s.response(7)["body"]["variables"][2]["value"], "3");

        // Into `double`, then out again.
        s.request("stepIn", json!({ "threadId": 1 }));
        s.wait_stopped(2);
        s.request("stackTrace", json!({ "threadId": 1 }));
        let st = s.response(9);
        assert_eq!(st["body"]["stackFrames"][0]["name"], "double", "{st}");
        assert_eq!(st["body"]["stackFrames"][0]["line"], 2);
        s.request("evaluate", json!({ "expression": "$n", "frameId": 0, "context": "hover" }));
        assert_eq!(s.response(10)["body"]["result"], "1");
        s.request("stepOut", json!({ "threadId": 1 }));
        s.wait_stopped(3);
        s.request("stackTrace", json!({ "threadId": 1 }));
        assert_eq!(s.response(12)["body"]["stackFrames"][0]["name"], "{main}");

        // The breakpoint again on the next turn of the loop, then to the end.
        s.request("continue", json!({ "threadId": 1 }));
        s.wait_stopped(4);
        // Into `double` a second time: `$twice` is not assigned yet in this call, so it
        // is not shown (its register still held 2 from the first call).
        s.request("stepIn", json!({ "threadId": 1 }));
        s.wait_stopped(5);
        s.request("variables", json!({ "variablesReference": FRAME_REF }));
        let vars = s.response(15)["body"]["variables"].clone();
        let names: Vec<&str> = vars.as_array().unwrap().iter().map(|v| v["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["$n"], "{vars}");
        // A request right after resuming is answered at once, not at the next stop.
        s.request("setBreakpoints", json!({ "source": { "path": program.to_string_lossy() }, "breakpoints": [] }));
        s.request("continue", json!({ "threadId": 1 }));
        s.request("stackTrace", json!({ "threadId": 1 }));
        let st = s.response(18);
        assert!(st["body"]["stackFrames"].as_array().is_none_or(|f| f.is_empty()) || st["success"] == false, "{st}");
        s.wait("terminated", |m| m["event"] == "terminated");
        let output: String = s
            .messages()
            .iter()
            .filter(|m| m["event"] == "output")
            .map(|m| m["body"]["output"].as_str().unwrap_or("").to_string())
            .collect();
        assert_eq!(output, "start\ntotal 12\n", "output before a stop is sent once");
        s.request("disconnect", json!({}));
        drop(s.tx);
        s.server.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
