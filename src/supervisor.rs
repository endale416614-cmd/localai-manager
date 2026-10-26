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

/// Starts app `i`, and with it the chat app its `needs_chat` names when no chat app is running. What happened to
/// the chat app goes to the log of app `i`. Called on the UI thread only, so no other start runs in between.
// TODO: needed_chat cannot see a start main.rs has queued, so a chat app started within one refresh (200 ms)
// of a GPU switch keeps the queued app waiting until the chat app stops; pass the queue in if that ever bites.
pub fn start(shared: &Shared, i: usize) {
    start_one(shared, i);
    let mut sup = shared.lock().unwrap();
    let c = match needed_chat(&sup, i) {
        Some(Ok(c)) => c,
        Some(Err(why)) => return sup.slots[i].push_log(why),
        None => return,
    };
    let (name, port) = (sup.slots[c].def.name.clone(), sup.slots[c].def.port.expect("validated in config"));
    drop(sup); // the probe takes up to 200 ms, and start_one locks again
    if port_open(port) {
        // A chat server started outside the manager answers already; start_one would mark the tile Failed.
        return note(shared, i, format!("{name} not started: port {port} answers already (started outside the manager?)"));
    }
    start_one(shared, c);
    let mut sup = shared.lock().unwrap();
    let line = if sup.slots[c].status.is_on() {
        format!("started {name} for chat (needs_chat)")
    } else {
        format!("{name} did not start: {}", sup.slots[c].error)
    };
    sup.slots[i].push_log(line);
}

/// The chat app to start for app `i`: None when `i` needs none, is not on, or a chat app is on already; Err
/// when there is no such chat app, it is still stopping, or another GPU app holds the GPU (nothing is stopped
/// to make room, and nothing is queued).
fn needed_chat(sup: &Supervisor, i: usize) -> Option<Result<usize, String>> {
    let want = sup.slots[i].def.needs_chat.as_ref()?;
    if !sup.slots[i].status.is_on() || sup.slots.iter().any(|s| s.def.chat && s.status.is_on()) {
        return None;
    }
    let Some(target) = sup.slots.iter().position(|s| s.def.id == *want && s.def.chat) else {
        return Some(Err(format!("needs_chat: no chat app \"{want}\" in apps.toml")));
    };
    let name = &sup.slots[target].def.name;
    if sup.slots[target].status == Status::Stopping {
        return Some(Err(format!("{name} not started: it is still stopping")));
    }
    if sup.slots[target].def.group == Group::Gpu {
        // Unlike guard::gpu_holder, a GPU app that is still stopping counts: its VRAM is not free yet.
        let holder = sup.slots.iter().enumerate().find(|&(j, s)| {
            j != target && s.def.group == Group::Gpu && (s.status.is_on() || s.status == Status::Stopping)
        });
        if let Some((_, h)) = holder {
            let doing = if h.status == Status::Stopping { "is still releasing" } else { "is using" };
            return Some(Err(format!("{name} not started: {} {doing} the GPU", h.def.name)));
        }
    }
    Some(Ok(target))
}

fn start_one(shared: &Shared, i: usize) {
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

    // Slots: 0 qwen (chat), 1 coder (chat), 2 comfy (not chat), 3 odysseus (needs qwen), 4 wiki, 5 api (svc chat),
    // 6 agent (gpu, needs api), 7 notes (needs a commented-out app).
    const APPS: &str = "app = [
        {id='qwen', name='Qwen3.6', group='gpu', chat=true, cmd=['x'], port=1, health='tcp'},
        {id='coder', name='Coder', group='gpu', chat=true, cmd=['x'], port=2, health='tcp'},
        {id='comfy', name='ComfyUI', group='gpu', cmd=['x'], port=3, health='tcp'},
        {id='odysseus', name='Odysseus', group='svc', needs_chat='qwen', cmd=['x'], port=4, health='tcp'},
        {id='wiki', name='Wikipedia', group='svc', cmd=['x'], port=5, health='tcp'},
        {id='api', name='API', group='svc', chat=true, cmd=['x'], port=6, health='tcp'},
        {id='agent', name='Agent', group='gpu', needs_chat='api', cmd=['x'], port=7, health='tcp'},
        {id='notes', name='Notes', group='svc', needs_chat='gone', cmd=['x'], port=8, health='tcp'},
    ]";

    /// `needed_chat` for app `id` once the apps named in `on` have the given status.
    fn needed(id: &str, on: &[(&str, Status)]) -> Option<Result<usize, String>> {
        let shared = Supervisor::new(crate::config::parse(APPS).unwrap());
        let mut sup = shared.lock().unwrap();
        for s in &mut sup.slots {
            if let Some(&(_, status)) = on.iter().find(|(n, _)| *n == s.def.id) {
                s.status = status;
            }
        }
        let i = sup.slots.iter().position(|s| s.def.id == id).unwrap();
        needed_chat(&sup, i)
    }

    #[test]
    fn needs_chat_starts_the_chat_app_when_none_is_on() {
        assert_eq!(needed("odysseus", &[("odysseus", Status::Starting)]), Some(Ok(0)));
        assert_eq!(needed("odysseus", &[("odysseus", Status::Ready), ("qwen", Status::Failed)]), Some(Ok(0)));
        // A svc chat app needs no GPU, so the GPU app that asks for it may hold the GPU.
        assert_eq!(needed("agent", &[("agent", Status::Starting)]), Some(Ok(5)));
        let why = "needs_chat: no chat app \"gone\" in apps.toml";
        assert_eq!(needed("notes", &[("notes", Status::Starting)]), Some(Err(why.into())));
    }

    #[test]
    fn needs_chat_does_nothing_when_a_chat_app_is_on() {
        for (chat, status) in [("coder", Status::Ready), ("coder", Status::Starting), ("qwen", Status::Ready)] {
            assert_eq!(needed("odysseus", &[("odysseus", Status::Starting), (chat, status)]), None, "{chat} {status:?}");
        }
    }

    #[test]
    fn needs_chat_never_takes_the_gpu_from_another_app() {
        for status in [Status::Ready, Status::Starting] {
            let why = "Qwen3.6 not started: ComfyUI is using the GPU";
            assert_eq!(needed("odysseus", &[("odysseus", Status::Starting), ("comfy", status)]), Some(Err(why.into())), "{status:?}");
        }
        for holder in ["comfy", "coder"] {
            let got = needed("odysseus", &[("odysseus", Status::Starting), (holder, Status::Stopping)]);
            assert!(got.as_ref().is_some_and(|r| r.as_ref().is_err_and(|e| e.ends_with("is still releasing the GPU"))), "{got:?}");
        }
        let why = "Qwen3.6 not started: it is still stopping";
        assert_eq!(needed("odysseus", &[("odysseus", Status::Starting), ("qwen", Status::Stopping)]), Some(Err(why.into())));
    }

    /// start() with real processes: a chat port that answers already is left alone, and a chat app that fails to
    /// start is reported in the log of the app that needed it.
    #[test]
    fn start_reports_what_happened_to_the_chat_app() {
        let outside = std::net::TcpListener::bind("127.0.0.1:0").unwrap(); // a chat server started outside
        let busy = outside.local_addr().unwrap().port();
        let free = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let apps = format!(
            "app = [
            {{id='chat', name='Chat', group='gpu', chat=true, cmd=['no-such-program-localai'], port={busy}, health='tcp'}},
            {{id='dep', name='Dep', group='svc', needs_chat='chat', cmd=['ping', '-n', '30', '127.0.0.1'], port={free}, health='tcp'}},
        ]"
        );
        let shared = Supervisor::new(crate::config::parse(&apps).unwrap());
        let last_log = || shared.lock().unwrap().slots[1].log.back().cloned().unwrap_or_default();
        start(&shared, 1);
        assert_eq!(shared.lock().unwrap().slots[1].status, Status::Starting);
        assert_eq!(shared.lock().unwrap().slots[0].status, Status::Stopped);
        assert!(last_log().starts_with(&format!("Chat not started: port {busy} answers already")), "{}", last_log());
        drop(outside);
        start(&shared, 1); // dep is on already, so only the chat app is tried
        assert_eq!(shared.lock().unwrap().slots[0].status, Status::Failed);
        assert!(last_log().starts_with("Chat did not start: start failed:"), "{}", last_log());
        stop(&shared, 1);
    }

    #[test]
    fn needs_chat_only_follows_an_app_that_asks_and_is_on() {
        for status in [Status::Stopped, Status::Stopping, Status::Failed] {
            assert_eq!(needed("odysseus", &[("odysseus", status)]), None, "{status:?}");
        }
        assert_eq!(needed("wiki", &[("wiki", Status::Starting)]), None);
        assert_eq!(needed("comfy", &[("comfy", Status::Ready)]), None);
    }
}
