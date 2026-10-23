use crate::config::{AppDef, Group};
use crate::job::Job;
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const LOG_LINES: usize = 400;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    Stopped,
    Starting,
    Ready,
    Stopping,
    Failed,
}

impl Status {
    pub fn is_on(self) -> bool {
        matches!(self, Status::Starting | Status::Ready)
    }
}

struct Running {
    job: Job,
    child: Child,
}

pub struct Slot {
    pub def: AppDef,
    pub status: Status,
    pub error: String,
    pub log: VecDeque<String>,
    /// Bumped on every log change so the UI only copies text when it changed.
    pub log_gen: u64,
    pub launched: Option<Instant>,
    run: Option<Running>,
}

impl Slot {
    fn push_log(&mut self, line: String) {
        if self.log.len() == LOG_LINES {
            self.log.pop_front();
        }
        self.log.push_back(line);
        self.log_gen += 1;
    }
}

pub type Shared = Arc<Mutex<Supervisor>>;

pub struct Supervisor {
    pub slots: Vec<Slot>,
    /// A problem found after startup (the background model scan), shown like an apps.toml error.
    pub notice: String,
}

fn slot(def: AppDef) -> Slot {
    Slot { def, status: Status::Stopped, error: String::new(), log: VecDeque::new(), log_gen: 0, launched: None, run: None }
}

impl Supervisor {
    pub fn new(apps: Vec<AppDef>) -> Shared {
        Arc::new(Mutex::new(Supervisor { slots: apps.into_iter().map(slot).collect(), notice: String::new() }))
    }
}

/// Appends apps whose id is new. Slots are only ever appended, so indexes held by the UI stay valid.
pub fn add(shared: &Shared, apps: Vec<AppDef>) {
    let mut sup = shared.lock().unwrap();
    for def in apps {
        if !sup.slots.iter().any(|s| s.def.id == def.id) {
            sup.slots.push(slot(def));
        }
    }
}

fn command(def: &AppDef) -> Command {
    let mut c = Command::new(&def.cmd[0]);
    c.args(&def.cmd[1..]);
    if let Some(cwd) = &def.cwd {
        c.current_dir(cwd);
    }
    c
}

pub fn start(shared: &Shared, i: usize) {
    let mut sup = shared.lock().unwrap();
    let slot = &mut sup.slots[i];
    if slot.def.group == Group::App {
        // Desktop/terminal apps are fire-and-forget: they outlive the manager on purpose.
        match command(&slot.def).spawn() {
            Ok(_) => {
                slot.launched = Some(Instant::now());
                slot.error.clear();
                slot.push_log(format!("launched: {}", slot.def.cmd.join(" ")));
            }
            Err(e) => {
                slot.error = format!("launch failed: {e}");
                slot.push_log(slot.error.clone());
            }
        }
        return;
    }
    if slot.status.is_on() || slot.status == Status::Stopping {
        return;
    }
    let port = slot.def.port.expect("validated in config");
    if port_open(port) {
        slot.status = Status::Failed;
        slot.error = format!("port {port} is already in use (started outside the manager?)");
        slot.push_log(slot.error.clone());
        return;
    }
    let spawned = Job::new().and_then(|job| {
        let mut c = command(&slot.def);
        c.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        job.spawn(&mut c).map(|child| (job, child))
    });
    let (job, mut child) = match spawned {
        Ok(x) => x,
        Err(e) => {
            slot.status = Status::Failed;
            slot.error = format!("start failed: {e}");
            slot.push_log(slot.error.clone());
            return;
        }
    };
    slot.log.clear();
    slot.log_gen += 1;
    slot.error.clear();
    slot.push_log(format!("> {}", slot.def.cmd.join(" ")));
    if let Some(out) = child.stdout.take() {
        pump(shared.clone(), i, out);
    }
    if let Some(err) = child.stderr.take() {
        pump(shared.clone(), i, err);
    }
    slot.run = Some(Running { job, child });
    slot.status = Status::Starting;
    let health = slot.def.health.clone().expect("validated in config");
    drop(sup);
    monitor(shared.clone(), i, port, health);
}

pub fn stop(shared: &Shared, i: usize) {
    let mut sup = shared.lock().unwrap();
    let slot = &mut sup.slots[i];
    if let Some(run) = &slot.run {
        slot.status = Status::Stopping;
        run.job.kill();
    }
}

/// Adds a line to an app's log from outside its process (e.g. the Odysseus link result).
pub fn note(shared: &Shared, i: usize, line: String) {
    shared.lock().unwrap().slots[i].push_log(line);
}

pub fn stop_all(shared: &Shared) {
    let n = shared.lock().unwrap().slots.len();
    (0..n).for_each(|i| stop(shared, i));
}

fn pump(shared: Shared, i: usize, stream: impl Read + Send + 'static) {
    thread::spawn(move || {
        let mut r = BufReader::new(stream);
        let mut buf = Vec::new();
        while let Ok(n) = r.read_until(b'\n', &mut buf) {
            if n == 0 {
                break;
            }
            let line = strip_ansi(String::from_utf8_lossy(&buf).trim_end());
            buf.clear();
            if !line.is_empty() {
                shared.lock().unwrap().slots[i].push_log(line);
            }
        }
    });
}

/// Watches one running app: exit → Stopped/Failed, health OK → Ready.
fn monitor(shared: Shared, i: usize, port: u16, health: String) {
    thread::spawn(move || {
        loop {
            let ready = {
                let mut sup = shared.lock().unwrap();
                let slot = &mut sup.slots[i];
                let Some(run) = slot.run.as_mut() else { return };
                if let Ok(Some(code)) = run.child.try_wait() {
                    run.job.kill(); // the batch file ended; take any leftovers with it
                    slot.run = None;
                    if slot.status == Status::Stopping {
                        slot.status = Status::Stopped;
                        slot.push_log("stopped".into());
                    } else {
                        slot.status = Status::Failed;
                        slot.error = format!("exited with {code}");
                        slot.push_log(slot.error.clone());
                    }
                    return;
                }
                slot.status == Status::Ready
            };
            thread::sleep(Duration::from_millis(if ready { 3000 } else { 700 }));
            if !ready && probe(port, &health) {
                let mut sup = shared.lock().unwrap();
                let slot = &mut sup.slots[i];
                if slot.status == Status::Starting {
                    slot.status = Status::Ready;
                    slot.push_log(format!("ready on 127.0.0.1:{port}"));
                }
            }
        }
    });
}

fn port_open(port: u16) -> bool {
    TcpStream::connect_timeout(&SocketAddr::from(([127, 0, 0, 1], port)), Duration::from_millis(200)).is_ok()
}

/// "tcp": the port accepts connections. "/path": GET returns 200.
pub fn probe(port: u16, health: &str) -> bool {
    let Ok(mut s) = TcpStream::connect_timeout(&SocketAddr::from(([127, 0, 0, 1], port)), Duration::from_millis(300)) else {
        return false;
    };
    if health == "tcp" {
        return true;
    }
    let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
    if write!(s, "GET {health} HTTP/1.0\r\nHost: 127.0.0.1:{port}\r\n\r\n").is_err() {
        return false;
    }
    let mut first = String::new();
    BufReader::new(s).read_line(&mut first).is_ok() && status_is_ok(&first)
}

fn status_is_ok(status_line: &str) -> bool {
    status_line.split_whitespace().nth(1) == Some("200")
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' && chars.peek() == Some(&'[') {
            chars.next();
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_status_line() {
        assert!(status_is_ok("HTTP/1.1 200 OK\r\n"));
        assert!(!status_is_ok("HTTP/1.1 503 Service Unavailable\r\n"));
        assert!(!status_is_ok(""));
    }

    #[test]
    fn strips_ansi_colors() {
        assert_eq!(strip_ansi("\x1b[32m[INFO]\x1b[0m Starting server"), "[INFO] Starting server");
        assert_eq!(strip_ansi("plain"), "plain");
    }
}
