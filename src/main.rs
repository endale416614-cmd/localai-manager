#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod chat;
mod config;
mod create;
mod detect;
mod guard;
mod job;
mod metrics;
mod names;
mod net;
mod odysseus;
mod pick;
mod setup;
mod supervisor;
mod train;
mod web;

use config::Group;
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};
use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use supervisor::{Shared, Slot, Status};

slint::include_modules!();

enum Action {
    Start(usize),
    Stop(usize),
    Open(usize),
    StopAll,
}

#[derive(Default)]
struct UiState {
    selected: usize,
    /// (running GPU app, app the user wants instead), waiting for the user to confirm.
    pending: Option<(usize, usize)>,
    /// Start this app as soon as whatever it waits on has stopped.
    queued: Option<usize>,
    actions: Vec<Action>,
    query: String,
    log_seen: Option<(usize, u64)>,
    chat_seen: Option<u64>,
    /// Odysseus has been Ready once this session, and its model endpoints were linked then.
    odysseus_linked: bool,
    create_mode: create::Mode,
    create_picture: Option<PathBuf>,
    /// A Create click that waits for ComfyUI to come up; the job starts once it is Ready.
    create_wait: Option<create::Request>,
    create_seen: Option<u64>,
    /// "Open voice trainer" was clicked, and when: the window shows the app once it is Ready (see `voice_step`).
    voice_wait: Option<Instant>,
    /// An app whose page the window should show now; set while refreshing, done right after.
    open_now: Option<usize>,
}

struct Models {
    gpu: Rc<VecModel<AppTile>>,
    svc: Rc<VecModel<AppTile>>,
    app: Rc<VecModel<AppTile>>,
    facts: Rc<VecModel<Fact>>,
    palette: Rc<VecModel<PaletteItem>>,
    chat: Rc<VecModel<ChatRow>>,
}

const LAUNCHED_FOR: Duration = Duration::from_secs(4);

fn status_code(slot: &Slot) -> i32 {
    if slot.launched.is_some_and(|t| t.elapsed() < LAUNCHED_FOR) {
        return 5;
    }
    match slot.status {
        Status::Stopped => 0,
        Status::Starting => 1,
        Status::Ready => 2,
        Status::Stopping => 3,
        Status::Failed => 4,
    }
}

fn status_label(slot: &Slot, gpu_busy: bool) -> &'static str {
    match (slot.def.group, status_code(slot)) {
        (_, 5) => "Launched",
        (Group::App, _) => "Ready to launch",
        (_, 1) => "Starting…",
        (_, 2) => "Running",
        (_, 3) => "Stopping…",
        (_, 4) => "Failed",
        (Group::Gpu, _) if gpu_busy => "Stopped · GPU busy",
        _ => "Stopped",
    }
}

fn launch_name(slot: &Slot) -> String {
    let last = slot.def.cmd.last().map(String::as_str).unwrap_or_default();
    let file = last.rsplit('\\').next().unwrap_or(last);
    if slot.def.cmd.len() > 1 && !last.contains('\\') {
        // e.g. `opencode.exe serve --port 4096`: show the program plus its arguments
        let prog = slot.def.cmd[0].rsplit('\\').next().unwrap_or(&slot.def.cmd[0]);
        return format!("{prog} {}", slot.def.cmd[1..].join(" "));
    }
    file.to_string()
}

fn or_dash(s: &str) -> String {
    if s.is_empty() { "—".into() } else { s.into() }
}

/// Only touches rows that changed, so tiles keep their running animations.
fn sync<T: Clone + PartialEq + 'static>(model: &VecModel<T>, rows: Vec<T>) {
    if model.row_count() != rows.len() {
        model.set_vec(rows);
        return;
    }
    for (i, r) in rows.into_iter().enumerate() {
        if model.row_data(i).as_ref() != Some(&r) {
            model.set_row_data(i, r);
        }
    }
}

fn groups(sup: &supervisor::Supervisor) -> Vec<(Group, Status)> {
    sup.slots.iter().map(|s| (s.def.group, s.status)).collect()
}

fn gpu_stopping(sup: &supervisor::Supervisor) -> bool {
    sup.slots.iter().any(|s| s.def.group == Group::Gpu && s.status == Status::Stopping)
}

/// Starts `i`, or asks to switch when another GPU app holds the GPU.
fn request_start(shared: &Shared, st: &mut UiState, i: usize) {
    let holder = {
        let sup = shared.lock().unwrap();
        if sup.slots[i].def.group == Group::Gpu { guard::gpu_holder(&groups(&sup), Some(i)) } else { None }
    };
    st.selected = i;
    match holder {
        Some(h) => st.pending = Some((h, i)),
        None => supervisor::start(shared, i),
    }
}

/// The running chat app, else the first one in apps.toml (setup puts Qwen3.6 there).
fn chat_target(sup: &supervisor::Supervisor) -> Option<usize> {
    let chat = || sup.slots.iter().enumerate().filter(|(_, s)| s.def.chat);
    chat().find(|(_, s)| s.status.is_on()).or_else(|| chat().next()).map(|(i, _)| i)
}

/// While the model only thinks, show the tail of its thoughts; once it answers, a one-line summary.
fn chat_row(m: &chat::Msg) -> ChatRow {
    let thinking = match (m.thinking.trim(), m.text.is_empty()) {
        ("", _) => String::new(),
        (t, true) => {
            let tail = t.char_indices().rev().nth(600).map_or(t, |(i, _)| &t[i..]);
            format!("Thinking… {tail}")
        }
        (t, false) => format!("Thought for {} words", t.split_whitespace().count()),
    };
    ChatRow { user: m.user, text: m.text.as_str().into(), thinking: thinking.into(), error: m.error.as_str().into() }
}

/// An app's web page opens inside the manager window; without WebView2, in the browser.
fn open_app(ui: &AppWindow, shared: &Shared, web: &RefCell<web::Views>, i: usize) {
    let (id, name, url) = {
        let sup = shared.lock().unwrap();
        let d = &sup.slots[i].def;
        (d.id.clone(), d.name.clone(), d.url.clone())
    };
    let Some(url) = url else { return };
    if let Err(e) = web.borrow_mut().show(ui.window(), &id, &url) {
        supervisor::note(shared, i, format!("built-in view unavailable ({e}), opened in the browser"));
        return web::open_external(&url);
    }
    ui.set_web_title(name.into());
    ui.set_web_open(true);
}

/// Drops the view of every app that is not running: its page is a session of the old process, and an app such as the
/// voice trainer (Gradio) keeps showing a dead one until Reload. The next open builds a fresh view; logins stay in
/// the shared profile. When that page was on screen the window goes back to the apps.
fn forget_stopped(ui: &AppWindow, shared: &Shared, web: &RefCell<web::Views>) {
    // Skipped while a view is being made or shown (that pumps messages); the next redraw is 200 ms away.
    let Ok(mut views) = web.try_borrow_mut() else { return };
    // The lock is taken for each question only: closing a view is not done under it.
    if views.forget(|id| shared.lock().unwrap().slots.iter().any(|s| s.def.id == id && !s.status.is_on())) {
        ui.set_web_open(false);
    }
}

/// What the Create tab needs to know about the ComfyUI app, copied out so that no lock is held while the tab works.
struct Comfy {
    index: usize,
    port: u16,
    name: String,
    status: Status,
    error: String,
    /// The GPU app that is in the way: (index, name).
    holder: Option<(usize, String)>,
}

fn comfy_info(sup: &supervisor::Supervisor) -> Option<Comfy> {
    let index = sup.slots.iter().position(|s| s.def.id == create::ID && s.def.port.is_some())?;
    let s = &sup.slots[index];
    let holder = guard::gpu_holder(&groups(sup), Some(index)).map(|h| (h, sup.slots[h].def.name.clone()));
    Some(Comfy { index, port: s.def.port?, name: s.def.name.clone(), status: s.status, error: s.error.clone(), holder })
}

/// The name of the file or folder at the end of a path, for the labels.
fn file_name(p: Option<&Path>) -> String {
    p.and_then(Path::file_name).map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

/// The Create tab: starts a waiting job once ComfyUI is Ready, and sets the tab's labels and result.
fn refresh_create(ui: &AppWindow, comfy: Option<&Comfy>, create: &create::Shared, st: &mut UiState) {
    // A click that found ComfyUI down waits here. While its own start is still queued, the status is
    // the old one (the last run failed, say): it only counts once that start has been tried.
    let tried = comfy.filter(|c| st.create_wait.is_some() && st.queued != Some(c.index));
    if let Some(c) = tried {
        match c.status {
            Status::Ready => {
                let req = st.create_wait.take().unwrap();
                create::start(create, c.port, req, create::dir());
            }
            Status::Starting => {}
            Status::Failed => {
                st.create_wait = None;
                create::fail(create, format!("ComfyUI did not start: {}", c.error));
            }
            _ => {
                st.create_wait = None;
                create::fail(create, "ComfyUI was stopped.".into());
            }
        }
    }

    let waiting = st.create_wait.is_some();
    // Copied out of the shared state, so that the picture below is decoded with nothing locked.
    let (busy, started, error, version, done) = {
        let c = create.lock().unwrap();
        (c.busy, c.started, c.error.clone(), c.version, c.done.as_ref().map(|d| (d.secs, d.image.clone(), d.video.clone())))
    };
    let (code, text) = if comfy.is_none() {
        (0, "add an app with id comfy to apps.toml".to_string())
    } else if waiting {
        match comfy.map(|c| c.status) {
            Some(Status::Starting) => (1, "Starting ComfyUI…".to_string()),
            _ => (1, "Waiting for the GPU…".to_string()),
        }
    } else if busy {
        (1, format!("Working… {} s", started.map_or(0, |t| t.elapsed().as_secs())))
    } else if !error.is_empty() {
        (4, error)
    } else if let Some((secs, ..)) = &done {
        (2, format!("Done in {secs} s. Saved in the creations folder."))
    } else {
        match comfy.map(|c| c.status) {
            Some(Status::Ready) => (2, "ComfyUI is ready.".to_string()),
            Some(Status::Starting) => (1, "Starting ComfyUI…".to_string()),
            _ => (0, "ComfyUI is stopped. Create starts it.".to_string()),
        }
    };
    // Like the Chat tab: when another GPU app holds the GPU, the button names it and the click confirms.
    let label = match comfy {
        Some(Comfy { holder: Some((_, other)), name, status, .. }) if !status.is_on() => format!("Stop {other}, start {name}"),
        _ => "Create".to_string(),
    };
    ui.set_create_mode(st.create_mode as i32);
    ui.set_create_picture(file_name(st.create_picture.as_deref()).into());
    ui.set_create_label(label.into());
    ui.set_create_busy(busy || waiting);
    ui.set_create_status(code);
    ui.set_create_status_label(text.into());
    if st.create_seen != Some(version) {
        st.create_seen = Some(version);
        let image = done.as_ref().and_then(|(_, image, _)| image.as_deref()).and_then(|p| slint::Image::load_from_path(p).ok());
        ui.set_create_has_image(image.is_some());
        ui.set_create_image(image.unwrap_or_default());
        ui.set_create_video(file_name(done.as_ref().and_then(|(_, _, video)| video.as_deref())).into());
    }
}

/// The voice trainer app, if apps.toml has one that can be opened: (index, status).
fn voice_info(sup: &supervisor::Supervisor) -> Option<(usize, Status)> {
    let i = sup.slots.iter().position(|s| s.def.id == train::VOICE_ID && s.def.url.is_some())?;
    Some((i, sup.slots[i].status))
}

/// How long "Open voice trainer" waits for the app: its start may queue behind another GPU app, but not for ever.
const VOICE_WAIT: Duration = Duration::from_secs(180);

#[derive(Debug, PartialEq)]
enum Voice {
    Wait,
    Open,
    Forget,
}

/// What a click on "Open voice trainer" does next. The page opens only where the click left the user: the Apps tab,
/// the voice tile still selected, no app page open. Anywhere else, the time running out, or the app stopping or
/// failing ends the wait, so the page never opens over something else the user has gone on to do.
fn voice_step(waited: Duration, web_open: bool, tab: i32, selected: usize, voice: Option<(usize, Status)>, start_waits: bool) -> Voice {
    let Some((i, status)) = voice else { return Voice::Forget };
    match status {
        _ if waited > VOICE_WAIT || web_open || selected != i => Voice::Forget,
        // Its start may still wait for the GPU to free up, or for a confirmation: the status is the old one till then.
        _ if start_waits => Voice::Wait,
        Status::Ready if tab == 0 => Voice::Open,
        Status::Starting => Voice::Wait,
        // Ready with the user on another tab, stopped, failed (the Apps view says why) or declined: nothing to wait for.
        _ => Voice::Forget,
    }
}

/// The training data panel, and "Open voice trainer": once the app is Ready, the window shows it.
fn refresh_train(ui: &AppWindow, voice: Option<(usize, Status)>, train: &train::Shared, st: &mut UiState) {
    if let Some(since) = st.voice_wait {
        let start_waits = voice.is_some_and(|(i, _)| st.queued == Some(i) || st.pending.is_some_and(|(_, to)| to == i));
        match voice_step(since.elapsed(), ui.get_web_open(), ui.get_tab(), st.selected, voice, start_waits) {
            Voice::Wait => {}
            Voice::Open => {
                st.voice_wait = None;
                st.open_now = voice.map(|(i, _)| i);
            }
            Voice::Forget => st.voice_wait = None,
        }
    }
    let (busy, text, code, folder) = {
        let t = train.lock().unwrap();
        (t.busy, t.text.clone(), t.code, t.folder.clone())
    };
    ui.set_train_busy(busy);
    ui.set_train_status(text.into());
    ui.set_train_status_code(code);
    ui.set_train_folder(folder.map(|p| p.display().to_string()).unwrap_or_default().into());
    ui.set_train_voice(voice.is_some());
}

#[derive(Debug, PartialEq)]
enum Queue {
    Wait,
    Start,
    Forget,
}

/// What a queued start does next: it starts once the app has fully stopped and nothing else holds the GPU. If
/// the app is up already (it was started some other way) the entry is stale and goes: left in, it would start the
/// app again the next time it stops, and a Create click waiting on it would wait for ever.
fn queue_step(status: Status, gpu_clear: bool) -> Queue {
    match status {
        s if s.is_on() => Queue::Forget,
        Status::Stopping => Queue::Wait,
        _ if gpu_clear => Queue::Start,
        _ => Queue::Wait,
    }
}

fn refresh(ui: &AppWindow, shared: &Shared, chat: &chat::Shared, st: &mut UiState, m: &Models, metrics: &Arc<Mutex<metrics::Metrics>>) {
    if let Some(q) = st.queued {
        let step = {
            let sup = shared.lock().unwrap();
            let s = &sup.slots[q];
            let gpu_clear = s.def.group != Group::Gpu || (guard::gpu_holder(&groups(&sup), Some(q)).is_none() && !gpu_stopping(&sup));
            queue_step(s.status, gpu_clear)
        };
        match step {
            Queue::Wait => {}
            Queue::Forget => st.queued = None,
            Queue::Start => {
                st.queued = None;
                supervisor::start(shared, q);
            }
        }
    }

    let sup = shared.lock().unwrap();
    if !sup.notice.is_empty() && ui.get_config_error().is_empty() {
        ui.set_config_error(sup.notice.as_str().into());
    }
    let gpu_busy = guard::gpu_holder(&groups(&sup), None).is_some();
    let tiles = |g: Group| -> Vec<AppTile> {
        sup.slots
            .iter()
            .enumerate()
            .filter(|(_, s)| s.def.group == g)
            .map(|(i, s)| AppTile {
                index: i as i32,
                name: s.def.name.as_str().into(),
                sub: s.def.sub.as_str().into(),
                port_label: s.def.port.map(|p| format!(":{p}")).unwrap_or_default().into(),
                status: status_code(s),
                status_label: status_label(s, gpu_busy && !s.status.is_on()).into(),
                stat: s.def.stat.as_str().into(),
                selected: i == st.selected,
            })
            .collect()
    };
    sync(&m.gpu, tiles(Group::Gpu));
    sync(&m.svc, tiles(Group::Svc));
    sync(&m.app, tiles(Group::App));
    ui.set_running_count(sup.slots.iter().filter(|s| s.status.is_on()).count() as i32);

    if let Some(s) = sup.slots.get(st.selected) {
        let on = s.status.is_on();
        let is_app = s.def.group == Group::App;
        ui.set_sel_name(s.def.name.as_str().into());
        ui.set_sel_sub(s.def.sub.as_str().into());
        ui.set_sel_status(status_code(s));
        ui.set_sel_status_label(status_label(s, gpu_busy && !on).into());
        sync(
            &m.facts,
            vec![
                Fact { k: "Endpoint".into(), v: s.def.port.map(|p| format!("127.0.0.1:{p}")).unwrap_or("—".into()).into() },
                Fact { k: "Launch".into(), v: launch_name(s).into() },
                Fact { k: "Peak VRAM (tested)".into(), v: or_dash(&s.def.vram).into() },
                Fact { k: "RAM (tested)".into(), v: or_dash(&s.def.ram).into() },
            ],
        );
        let (note, err) = if s.error.is_empty() { (s.def.note.clone(), false) } else { (s.error.clone(), true) };
        ui.set_sel_note(note.into());
        ui.set_sel_note_error(err);
        let (label, kind) = match (is_app, s.status) {
            (true, _) => ("Launch", 1),
            (_, Status::Stopping) => ("Stopping…", 0),
            (_, x) if x.is_on() => ("Stop", 0),
            _ => ("Start", 1),
        };
        ui.set_primary_label(label.into());
        ui.set_primary_kind(kind);
        ui.set_can_open(s.status == Status::Ready && s.def.url.is_some());
        ui.set_can_restart(s.status == Status::Ready && !is_app);
        ui.set_log_source(if on { launch_name(s) } else { "idle".into() }.into());
        let key = (st.selected, s.log_gen);
        if st.log_seen != Some(key) {
            st.log_seen = Some(key);
            let text = if s.log.is_empty() { "— not running —".to_string() } else { s.log.iter().map(String::as_str).collect::<Vec<_>>().join("\n") };
            ui.set_log_text(text.into());
        }
    }

    match chat_target(&sup) {
        Some(t) => {
            let s = &sup.slots[t];
            let waiting = st.queued == Some(t) && !s.status.is_on();
            ui.set_chat_model(s.def.name.as_str().into());
            ui.set_chat_status(if waiting { 1 } else { status_code(s) });
            ui.set_chat_status_label(if waiting { "Waiting for the GPU…" } else { status_label(s, false) }.into());
            ui.set_chat_ready(s.status == Status::Ready);
            let start = match guard::gpu_holder(&groups(&sup), Some(t)) {
                _ if waiting || s.status.is_on() || s.status == Status::Stopping => String::new(),
                Some(h) => format!("Stop {}, start {}", sup.slots[h].def.name, s.def.name),
                None => format!("Start {}", s.def.name),
            };
            ui.set_chat_start_label(start.into());
        }
        None => {
            ui.set_chat_model("No chat model".into());
            ui.set_chat_status(0);
            ui.set_chat_status_label("set chat = true on an app in apps.toml".into());
            ui.set_chat_ready(false);
            ui.set_chat_start_label("".into());
        }
    }

    match st.pending {
        Some((from, to)) if sup.slots[from].status.is_on() => {
            let (f, t) = (&sup.slots[from].def, &sup.slots[to].def);
            let peak = if t.vram.is_empty() { String::new() } else { format!(" (tested peak {})", t.vram) };
            ui.set_pending(true);
            ui.set_pending_text(format!("{} is holding the GPU. Two GPU apps do not fit in VRAM together, so switching stops {} first, then starts {}{peak}.", f.name, f.name, t.name).into());
            ui.set_pending_cta(format!("Stop {}, start {}", f.name, t.name).into());
        }
        Some(_) => {
            // The holder stopped on its own: nothing to confirm any more.
            st.pending = None;
            ui.set_pending(false);
        }
        None => ui.set_pending(false),
    }

    if ui.get_palette_open() {
        let q = st.query.trim().to_lowercase();
        let mut acts = Vec::new();
        let mut rows = Vec::new();
        for (i, s) in sup.slots.iter().enumerate() {
            let code = status_code(s);
            let hay = format!("{} {} {}", s.def.name, s.def.id, s.def.sub).to_lowercase();
            let mut push = |a: Action, title: String, detail: String| {
                if q.is_empty() || format!("{} {hay}", title.to_lowercase()).contains(&q) {
                    acts.push(a);
                    rows.push(PaletteItem { title: title.into(), detail: detail.into(), status: code });
                }
            };
            if s.status == Status::Ready && s.def.url.is_some() {
                push(Action::Open(i), format!("Open {}", s.def.name), s.def.url.clone().unwrap_or_default().replace("http://", ""));
            }
            if s.status.is_on() {
                push(Action::Stop(i), format!("Stop {}", s.def.name), launch_name(s));
            } else if s.status != Status::Stopping {
                let other = if s.def.group == Group::Gpu { guard::gpu_holder(&groups(&sup), Some(i)) } else { None };
                let verb = if s.def.group == Group::App { "Launch" } else { "Start" };
                let detail = match (other, s.def.port) {
                    (Some(o), _) => format!("switches GPU from {}", sup.slots[o].def.name),
                    (None, Some(p)) => format!(":{p}"),
                    (None, None) => launch_name(s),
                };
                push(Action::Start(i), format!("{verb} {}", s.def.name), detail);
            }
        }
        if q.is_empty() || "stop all apps quit kill everything".contains(&q) {
            acts.push(Action::StopAll);
            rows.push(PaletteItem { title: "Stop all apps".into(), detail: "frees GPU and RAM".into(), status: 0 });
        }
        st.actions = acts;
        let n = rows.len() as i32;
        sync(&m.palette, rows);
        if ui.get_palette_index() >= n {
            ui.set_palette_index((n - 1).max(0));
        }
    }
    // Link again once Odysseus is up: on a first run its database only exists from then on.
    let link = !st.odysseus_linked && sup.slots.iter().any(|s| s.def.id == odysseus::ID && s.status == Status::Ready);
    drop(sup);
    if link {
        st.odysseus_linked = true;
        odysseus::link(shared);
    }

    let c = chat.lock().unwrap();
    ui.set_chat_busy(c.busy);
    if st.chat_seen != Some(c.version) {
        st.chat_seen = Some(c.version);
        sync(&m.chat, c.msgs.iter().map(chat_row).collect());
    }
    drop(c);

    let mt = *metrics.lock().unwrap();
    if mt.vram_total > 0.0 {
        ui.set_vram_label(format!("{:.1} / {:.0} GB", mt.vram_used, mt.vram_total).into());
        ui.set_vram_frac(mt.vram_used / mt.vram_total);
    } else {
        ui.set_vram_label("n/a".into());
        ui.set_vram_frac(0.0);
    }
    if mt.ram_total > 0.0 {
        let swap = if mt.swap_used >= 1.0 { format!(" +{:.0} swap", mt.swap_used) } else { String::new() };
        ui.set_ram_label(format!("{:.1} / {:.0} GB{swap}", mt.ram_used, mt.ram_total).into());
        ui.set_ram_frac(mt.ram_used / mt.ram_total);
    }
}

fn main() {
    let t0 = Instant::now();
    let bench = std::env::args().any(|a| a == "--bench-startup");

    // First run (no apps.toml yet) or --setup: install llama.cpp + Qwen3.6 before the window opens.
    if !bench && (std::env::args().any(|a| a == "--setup") || !config::locate().exists()) && !setup::run_in_console() {
        return;
    }

    let ui = AppWindow::new().expect("create window");
    if bench {
        println!("window_new_ms={:.1}", t0.elapsed().as_secs_f64() * 1000.0);
    }
    let config_path = config::locate();
    let (apps, err) = match config::load(&config_path) {
        Ok(a) => (a, String::new()),
        Err(e) => (Vec::new(), e),
    };
    ui.set_config_error(err.into());
    let home = std::path::absolute(&config_path).ok().and_then(|p| p.parent().map(|d| d.display().to_string()));
    ui.set_home(home.unwrap_or_default().into());
    let shared = supervisor::Supervisor::new(apps);
    // Only an apps.toml beside the exe is the user's list (locate() gives its absolute path); a dev run
    // from the repo reads the shipped one from the current folder and must not append to it.
    if !bench && config_path.is_absolute() {
        // Adds local models, Ollama and Odysseus that apps.toml does not list yet, then links Odysseus.
        detect::spawn(shared.clone(), config_path.clone());
    }
    let web: Rc<RefCell<web::Views>> = Rc::default();
    let chat: chat::Shared = Arc::default();
    let create: create::Shared = Arc::default();
    let train: train::Shared = Arc::default();
    let metrics = metrics::spawn();
    let state = Rc::new(RefCell::new(UiState::default()));
    let models = Rc::new(Models {
        gpu: Rc::new(VecModel::default()),
        svc: Rc::new(VecModel::default()),
        app: Rc::new(VecModel::default()),
        facts: Rc::new(VecModel::default()),
        palette: Rc::new(VecModel::default()),
        chat: Rc::new(VecModel::default()),
    });
    ui.set_gpu_tiles(ModelRc::from(models.gpu.clone()));
    ui.set_svc_tiles(ModelRc::from(models.svc.clone()));
    ui.set_app_tiles(ModelRc::from(models.app.clone()));
    ui.set_sel_facts(ModelRc::from(models.facts.clone()));
    ui.set_palette_items(ModelRc::from(models.palette.clone()));
    ui.set_chat_rows(ModelRc::from(models.chat.clone()));

    // Every callback mutates state, then redraws from the supervisor right away for a snappy feel.
    let redraw = {
        let (ui, shared, chat, create, train, web, state, models, metrics) =
            (ui.as_weak(), shared.clone(), chat.clone(), create.clone(), train.clone(), web.clone(), state.clone(), models.clone(), metrics.clone());
        Rc::new(move || {
            let Some(ui) = ui.upgrade() else { return };
            let open = {
                let mut st = state.borrow_mut();
                refresh(&ui, &shared, &chat, &mut st, &models, &metrics);
                // The Create tab works from a copy of what the supervisor knows, so no lock is held while it does.
                let (comfy, voice) = {
                    let sup = shared.lock().unwrap();
                    (comfy_info(&sup), voice_info(&sup))
                };
                refresh_create(&ui, comfy.as_ref(), &create, &mut st);
                refresh_train(&ui, voice, &train, &mut st);
                st.open_now.take()
            };
            forget_stopped(&ui, &shared, &web);
            // With the state let go: opening a page runs a message loop for a moment.
            if let Some(i) = open {
                open_app(&ui, &shared, &web, i);
            }
        })
    };
    redraw();
    if bench {
        println!("first_refresh_ms={:.1}", t0.elapsed().as_secs_f64() * 1000.0);
    }

    {
        let (state, r) = (state.clone(), redraw.clone());
        ui.on_select(move |i| {
            state.borrow_mut().selected = i as usize;
            r();
        });
    }
    {
        let (shared, state, r) = (shared.clone(), state.clone(), redraw.clone());
        ui.on_primary(move || {
            {
                let mut st = state.borrow_mut();
                let i = st.selected;
                let (on, is_app) = {
                    let sup = shared.lock().unwrap();
                    match sup.slots.get(i) {
                        Some(s) => (s.status.is_on(), s.def.group == Group::App),
                        None => return,
                    }
                };
                if on && !is_app {
                    supervisor::stop(&shared, i);
                } else {
                    request_start(&shared, &mut st, i);
                }
            }
            r();
        });
    }
    {
        let (ui_weak, shared, state, web) = (ui.as_weak(), shared.clone(), state.clone(), web.clone());
        ui.on_open_url(move || {
            if let Some(ui) = ui_weak.upgrade() {
                open_app(&ui, &shared, &web, state.borrow().selected);
            }
        });
    }
    {
        let (ui_weak, web) = (ui.as_weak(), web.clone());
        ui.on_web_back(move || {
            let (ui_weak, web) = (ui_weak.clone(), web.clone());
            // After the click finishes: removing the bar inside its own click handler left Slint's
            // mouse state pointing at it, and the next click on the apps view was lost.
            slint::Timer::single_shot(Duration::ZERO, move || {
                if let Some(ui) = ui_weak.upgrade() {
                    web.borrow_mut().hide(ui.window());
                    ui.set_web_open(false);
                }
            });
        });
    }
    {
        let web = web.clone();
        ui.on_web_reload(move || web.borrow().reload());
    }
    {
        let (ui_weak, web) = (ui.as_weak(), web.clone());
        ui.on_web_layout(move || {
            if let Some(ui) = ui_weak.upgrade() {
                web.borrow().layout(ui.window());
            }
        });
    }
    {
        let (shared, state, r) = (shared.clone(), state.clone(), redraw.clone());
        ui.on_restart(move || {
            {
                let mut st = state.borrow_mut();
                supervisor::stop(&shared, st.selected);
                st.queued = Some(st.selected);
            }
            r();
        });
    }
    {
        let (shared, state, r) = (shared.clone(), state.clone(), redraw.clone());
        ui.on_confirm_switch(move || {
            {
                let mut st = state.borrow_mut();
                if let Some((from, to)) = st.pending.take() {
                    supervisor::stop(&shared, from);
                    st.queued = Some(to);
                }
            }
            r();
        });
    }
    {
        let (state, r) = (state.clone(), redraw.clone());
        ui.on_cancel_switch(move || {
            state.borrow_mut().pending = None;
            r();
        });
    }
    {
        let (state, r) = (state.clone(), redraw.clone());
        ui.on_query_edited(move |q: SharedString| {
            state.borrow_mut().query = q.to_string();
            r();
        });
    }
    {
        let (ui_weak, shared, state, web, create, r) = (ui.as_weak(), shared.clone(), state.clone(), web.clone(), create.clone(), redraw.clone());
        ui.on_run_action(move |idx| {
            {
                let mut st = state.borrow_mut();
                match st.actions.get(idx as usize) {
                    Some(&Action::Start(i)) => request_start(&shared, &mut st, i),
                    Some(&Action::Stop(i)) => {
                        st.selected = i;
                        supervisor::stop(&shared, i);
                    }
                    Some(&Action::Open(i)) => {
                        if let Some(ui) = ui_weak.upgrade() {
                            open_app(&ui, &shared, &web, i);
                        }
                    }
                    Some(Action::StopAll) => {
                        supervisor::stop_all(&shared);
                        // Nothing that waited for a start may run once everything is stopped.
                        st.queued = None;
                        st.voice_wait = None;
                        if st.create_wait.take().is_some() {
                            create::fail(&create, "Cancelled: all apps were stopped.".into());
                        }
                    }
                    None => {}
                }
            }
            r();
        });
    }

    {
        let (shared, chat, r) = (shared.clone(), chat.clone(), redraw.clone());
        ui.on_chat_send(move |text: SharedString| {
            let port = {
                let sup = shared.lock().unwrap();
                chat_target(&sup).map(|t| &sup.slots[t]).filter(|s| s.status == Status::Ready).and_then(|s| s.def.port)
            };
            if let Some(port) = port {
                chat::send(&chat, port, text.to_string());
            }
            r();
        });
    }
    {
        let (chat, r) = (chat.clone(), redraw.clone());
        ui.on_chat_stop(move || {
            chat.lock().unwrap().stop();
            r();
        });
    }
    {
        let (chat, r) = (chat.clone(), redraw.clone());
        ui.on_chat_clear(move || {
            chat.lock().unwrap().clear();
            r();
        });
    }
    {
        let (shared, state, r) = (shared.clone(), state.clone(), redraw.clone());
        // The button label already names the app it stops, so the click is the confirmation.
        ui.on_chat_start(move || {
            {
                let mut st = state.borrow_mut();
                let target = {
                    let sup = shared.lock().unwrap();
                    chat_target(&sup).map(|t| (t, guard::gpu_holder(&groups(&sup), Some(t))))
                };
                if let Some((t, holder)) = target {
                    if let Some(h) = holder {
                        supervisor::stop(&shared, h);
                    }
                    st.pending = None;
                    st.selected = t;
                    st.queued = Some(t);
                }
            }
            r();
        });
    }

    {
        let (state, create, r) = (state.clone(), create.clone(), redraw.clone());
        ui.on_create_set_mode(move |i| {
            {
                let mut st = state.borrow_mut();
                // The result belongs to the mode that made it, and a running job keeps its mode.
                if st.create_wait.is_some() || create.lock().unwrap().busy {
                    return;
                }
                st.create_mode = create::Mode::from_index(i);
                create::clear(&create);
            }
            r();
        });
    }
    {
        let (ui_weak, state, create, r) = (ui.as_weak(), state.clone(), create.clone(), redraw.clone());
        ui.on_create_pick(move || {
            let (ui_weak, state, create, r) = (ui_weak.clone(), state.clone(), create.clone(), r.clone());
            // After the click finishes and with nothing borrowed: the dialog runs a message loop of its own.
            slint::Timer::single_shot(Duration::ZERO, move || {
                let Some(ui) = ui_weak.upgrade() else { return };
                match pick::picture(ui.window()) {
                    Ok(Some(p)) => state.borrow_mut().create_picture = Some(p),
                    Ok(None) => {}
                    Err(why) => create::fail(&create, why),
                }
                r();
            });
        });
    }
    {
        let (shared, state, create, r) = (shared.clone(), state.clone(), create.clone(), redraw.clone());
        ui.on_create_start(move |prompt: SharedString| {
            let (shared, state, create, r) = (shared.clone(), state.clone(), create.clone(), r.clone());
            // After the click finishes: this button gives way to Cancel, and removing it inside its own click
            // handler loses the next click (as with the Back bar in on_web_back).
            slint::Timer::single_shot(Duration::ZERO, move || {
                {
                    let mut st = state.borrow_mut();
                    let (mode, prompt) = (st.create_mode, prompt.trim().to_string());
                    if prompt.is_empty() || st.create_wait.is_some() || create.lock().unwrap().busy || (mode.needs_picture() && st.create_picture.is_none()) {
                        return;
                    }
                    let Some(comfy) = comfy_info(&shared.lock().unwrap()) else { return };
                    let req = create::Request { mode, prompt, picture: st.create_picture.clone().filter(|_| mode.needs_picture()), frames: create::FRAMES };
                    // The cheap things first: nothing is stopped or started for a request that cannot run.
                    if let Err(why) = create::check(&req, &create::dir()) {
                        create::fail(&create, why);
                    } else {
                        create::clear(&create);
                        if comfy.status == Status::Ready {
                            create::start(&create, comfy.port, req, create::dir());
                        } else {
                            // The button names the app it stops, so the click is the confirmation (as in the Chat tab).
                            if !comfy.status.is_on() {
                                if let Some((holder, _)) = comfy.holder {
                                    supervisor::stop(&shared, holder);
                                }
                                st.pending = None;
                                st.selected = comfy.index;
                                st.queued = Some(comfy.index);
                            }
                            st.create_wait = Some(req);
                        }
                    }
                }
                r();
            });
        });
    }
    {
        let (shared, state, create, r) = (shared.clone(), state.clone(), create.clone(), redraw.clone());
        ui.on_create_cancel(move || {
            let (shared, state, create, r) = (shared.clone(), state.clone(), create.clone(), r.clone());
            // After the click finishes: Cancel gives way to Create, as Create gives way to it.
            slint::Timer::single_shot(Duration::ZERO, move || {
                {
                    let mut st = state.borrow_mut();
                    let (i, port) = {
                        let sup = shared.lock().unwrap();
                        let i = sup.slots.iter().position(|s| s.def.id == create::ID);
                        (i, i.and_then(|i| sup.slots[i].def.port))
                    };
                    // A start that was only waiting for the GPU to free up is not wanted any more either.
                    if st.create_wait.take().is_some() && st.queued == i {
                        st.queued = None;
                    }
                    create::cancel(&create, port.unwrap_or_default());
                }
                r();
            });
        });
    }
    {
        let create = create.clone();
        ui.on_create_open_video(move || {
            let video = create.lock().unwrap().done.as_ref().and_then(|d| d.video.clone());
            if let Some(v) = video {
                create::open(&v);
            }
        });
    }

    {
        let (ui_weak, train, r) = (ui.as_weak(), train.clone(), redraw.clone());
        ui.on_train_import(move |kind: i32, set: SharedString| {
            let (ui_weak, train, r) = (ui_weak.clone(), train.clone(), r.clone());
            // After the click finishes and with nothing borrowed: the dialog runs a message loop of its own.
            slint::Timer::single_shot(Duration::ZERO, move || {
                let Some(ui) = ui_weak.upgrade() else { return };
                if train.lock().unwrap().busy {
                    return;
                }
                let kind = train::Kind::from_index(kind);
                match pick::files(ui.window(), kind.title(), kind.filter()) {
                    Ok(files) => train::start(&train, kind, files, set.to_string(), train::root()),
                    Err(why) => train::fail(&train, why),
                }
                r();
            });
        });
    }
    {
        let train = train.clone();
        ui.on_train_cancel(move || train::cancel(&train));
    }
    {
        let train = train.clone();
        ui.on_train_open_folder(move || {
            let folder = train.lock().unwrap().folder.clone();
            if let Some(f) = folder {
                create::open(&f);
            }
        });
    }
    {
        let (ui_weak, shared, state, r) = (ui.as_weak(), shared.clone(), state.clone(), redraw.clone());
        ui.on_train_open_voice(move || {
            let (ui_weak, shared, state, r) = (ui_weak.clone(), shared.clone(), state.clone(), r.clone());
            // After the click finishes: the tab switch removes this very button, and doing that inside its own
            // click handler loses the next click (as with the Back bar in on_web_back).
            slint::Timer::single_shot(Duration::ZERO, move || {
                {
                    let mut st = state.borrow_mut();
                    let Some((i, status)) = voice_info(&shared.lock().unwrap()) else { return };
                    match status {
                        Status::Ready | Status::Starting => st.selected = i,
                        // Just stopped: start it again once it has gone.
                        Status::Stopping => {
                            st.selected = i;
                            st.queued = Some(i);
                        }
                        // As its tile does: starts it, or asks first when another GPU app has the GPU.
                        _ => request_start(&shared, &mut st, i),
                    }
                    // The Apps tab shows it start and holds that question; the page opens inside the window once it is Ready.
                    st.voice_wait = Some(Instant::now());
                    if let Some(ui) = ui_weak.upgrade() {
                        ui.set_tab(0);
                    }
                }
                r();
            });
        });
    }

    let timer = slint::Timer::default();
    {
        let (r, ui_weak, web) = (redraw.clone(), ui.as_weak(), web.clone());
        let was_minimized = Cell::new(false);
        timer.start(slint::TimerMode::Repeated, Duration::from_millis(200), move || {
            r();
            if let Some(ui) = ui_weak.upgrade() {
                // Slint keeps drawing while minimized (this timer changes values), which uses up its
                // one full repaint; after the restore it presents only what changed since, and the
                // rest of the window shows what is behind it. Repaint all of it once.
                let minimized = ui.window().is_minimized();
                if was_minimized.replace(minimized) && !minimized {
                    ui.set_repaint_flip(!ui.get_repaint_flip());
                }
            }
            // Moving to a monitor with another scale keeps the logical size, so the width/height
            // handlers stay silent; the view's pixels still have to follow.
            if let Some(ui) = ui_weak.upgrade().filter(|ui| ui.get_web_open()) {
                web.borrow().layout(ui.window());
            }
        });
    }

    if bench {
        let mut done = false;
        let notifier = ui.window().set_rendering_notifier(move |s, _| {
            if !done && matches!(s, slint::RenderingState::AfterRendering) {
                done = true;
                println!("first_frame_ms={:.1}", t0.elapsed().as_secs_f64() * 1000.0);
                let _ = slint::quit_event_loop();
            }
        });
        if notifier.is_err() {
            slint::Timer::single_shot(Duration::ZERO, move || {
                println!("event_loop_ms={:.1}", t0.elapsed().as_secs_f64() * 1000.0);
                let _ = slint::quit_event_loop();
            });
        }
    }

    ui.run().expect("run event loop");
    supervisor::stop_all(&shared);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_queued_start_waits_starts_or_is_forgotten() {
        use Queue::*;
        use Status::*;
        // It waits while the app is still stopping, or something holds the GPU.
        assert_eq!(queue_step(Stopping, true), Wait);
        assert_eq!(queue_step(Stopped, false), Wait);
        assert_eq!(queue_step(Failed, false), Wait);
        // It starts once the way is clear.
        assert_eq!(queue_step(Stopped, true), Start);
        assert_eq!(queue_step(Failed, true), Start);
        // It is forgotten when the app came up some other way, whatever the GPU says.
        for status in [Starting, Ready] {
            for gpu_clear in [true, false] {
                assert_eq!(queue_step(status, gpu_clear), Forget, "{status:?} {gpu_clear}");
            }
        }
    }

    #[test]
    fn the_voice_wait_opens_only_where_the_click_left_the_user() {
        use Status::*;
        use Voice::*;
        // (seconds waited, a page is open, tab, selected tile, the voice app's status, its start still waits)
        let at = |waited, web_open, tab, selected, status: Option<Status>, start_waits| voice_step(Duration::from_secs(waited), web_open, tab, selected, status.map(|s| (3, s)), start_waits);
        // Where the click left the user: the Apps tab, the voice tile selected, no page open.
        assert_eq!(at(1, false, 0, 3, Some(Ready), false), Open);
        assert_eq!(at(1, false, 0, 3, Some(Starting), false), Wait);
        // A start that waits for the GPU or for a confirmation shows the old status: still waiting, whatever that was.
        for old in [Stopped, Failed, Stopping, Ready] {
            assert_eq!(at(1, false, 0, 3, Some(old), true), Wait, "{old:?}");
        }
        // Stopped, failed or declined: nothing to wait for.
        for status in [Stopped, Failed, Stopping] {
            assert_eq!(at(1, false, 0, 3, Some(status), false), Forget, "{status:?}");
        }
        // The user went on to something else: another app's page (Odysseus, say), or another tile.
        for status in [Starting, Ready] {
            assert_eq!(at(1, true, 0, 3, Some(status), false), Forget, "a page is open, {status:?}");
            assert_eq!(at(1, false, 0, 4, Some(status), false), Forget, "another tile, {status:?}");
            assert_eq!(at(1, true, 0, 3, Some(status), true), Forget, "a page is open, its start waits, {status:?}");
        }
        // Ready with the user on another tab: it does not open there. Still starting, it may yet.
        assert_eq!(at(1, false, 1, 3, Some(Ready), false), Forget);
        assert_eq!(at(1, false, 2, 3, Some(Ready), false), Forget);
        assert_eq!(at(1, false, 1, 3, Some(Starting), false), Wait);
        // The wait runs out: the page never opens minutes later over whatever is on screen then.
        assert_eq!(at(180, false, 0, 3, Some(Starting), false), Wait);
        for status in [Starting, Ready, Stopped] {
            assert_eq!(at(181, false, 0, 3, Some(status), true), Forget, "{status:?}");
            assert_eq!(at(181, false, 0, 3, Some(status), false), Forget, "{status:?}");
        }
        // No voice app (any more): nothing.
        assert_eq!(at(1, false, 0, 3, None, false), Forget);
    }

    #[test]
    fn the_tabs_find_their_apps_by_id() {
        let shared = supervisor::Supervisor::new(config::parse(include_str!("../apps.toml")).unwrap());
        let mut sup = shared.lock().unwrap();
        let slot = |sup: &supervisor::Supervisor, id: &str| sup.slots.iter().position(|s| s.def.id == id).unwrap();

        let comfy = comfy_info(&sup).unwrap();
        assert_eq!((comfy.name.as_str(), comfy.port, comfy.status, comfy.holder.is_none()), ("ComfyUI", 8188, Status::Stopped, true));
        // Qwen3.6 runs: it is the one in the way, and the button will name it.
        let qwen = slot(&sup, "qwen36");
        sup.slots[qwen].status = Status::Ready;
        assert_eq!(comfy_info(&sup).unwrap().holder, Some((qwen, "Qwen3.6".to_string())));

        let voice = slot(&sup, train::VOICE_ID);
        assert_eq!(voice_info(&sup), Some((voice, Status::Stopped)));
        sup.slots[voice].status = Status::Starting;
        assert_eq!(voice_info(&sup), Some((voice, Status::Starting)));
        // No voice trainer in apps.toml, no button.
        sup.slots[voice].def.id = "other".into();
        assert_eq!(voice_info(&sup), None);
        // Nor without a page to show.
        sup.slots[voice].def.id = train::VOICE_ID.into();
        sup.slots[voice].def.url = None;
        assert_eq!(voice_info(&sup), None);
    }

    #[test]
    fn labels_show_the_name_of_what_a_path_ends_in() {
        assert_eq!(file_name(None), "");
        assert_eq!(file_name(Some(Path::new("C:\\Pictures\\Holiday photos\\a.png"))), "a.png");
        assert_eq!(file_name(Some(Path::new("C:\\Pictures\\Holiday photos"))), "Holiday photos");
    }
}
