#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod chat;
mod config;
mod detect;
mod guard;
mod job;
mod metrics;
mod odysseus;
mod setup;
mod supervisor;
mod web;

use config::Group;
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};
use std::cell::{Cell, RefCell};
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

fn open_url(shared: &Shared, i: usize) {
    if let Some(url) = shared.lock().unwrap().slots[i].def.url.clone() {
        web::open_external(&url);
    }
}

/// Odysseus opens inside the manager window; everything else, and Odysseus without WebView2, in the browser.
fn open_app(ui: &AppWindow, shared: &Shared, web: &RefCell<Option<web::Web>>, i: usize) {
    let (name, url) = {
        let sup = shared.lock().unwrap();
        let d = &sup.slots[i].def;
        (d.name.clone(), d.url.clone().filter(|_| d.id == odysseus::ID))
    };
    let Some(url) = url else { return open_url(shared, i) };
    let mut w = web.borrow_mut();
    if w.is_none() {
        match web::Web::open(ui.window(), &url) {
            Ok(v) => *w = Some(v),
            Err(e) => {
                supervisor::note(shared, i, format!("built-in view unavailable ({e}), opened in the browser"));
                return open_url(shared, i);
            }
        }
    }
    ui.set_web_title(name.into());
    ui.set_web_open(true);
    if let Some(v) = w.as_ref() {
        v.show(ui.window(), true);
    }
}

fn refresh(ui: &AppWindow, shared: &Shared, chat: &chat::Shared, st: &mut UiState, m: &Models, metrics: &Arc<Mutex<metrics::Metrics>>) {
    // A queued start fires once its blocker has fully stopped.
    if let Some(q) = st.queued {
        let ready = {
            let sup = shared.lock().unwrap();
            let s = &sup.slots[q];
            let gpu_clear = s.def.group != Group::Gpu || (guard::gpu_holder(&groups(&sup), Some(q)).is_none() && !gpu_stopping(&sup));
            !s.status.is_on() && s.status != Status::Stopping && gpu_clear
        };
        if ready {
            st.queued = None;
            supervisor::start(shared, q);
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
        ui.set_open_label(if s.def.id == odysseus::ID { "Open" } else { "Open in browser" }.into());
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
    let web: Rc<RefCell<Option<web::Web>>> = Rc::default();
    let chat: chat::Shared = Arc::default();
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
        let (ui, shared, chat, state, models, metrics) = (ui.as_weak(), shared.clone(), chat.clone(), state.clone(), models.clone(), metrics.clone());
        Rc::new(move || {
            if let Some(ui) = ui.upgrade() {
                refresh(&ui, &shared, &chat, &mut state.borrow_mut(), &models, &metrics);
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
                    if let Some(v) = web.borrow().as_ref() {
                        v.show(ui.window(), false);
                    }
                    ui.set_web_open(false);
                }
            });
        });
    }
    {
        let web = web.clone();
        ui.on_web_reload(move || {
            if let Some(v) = web.borrow().as_ref() {
                v.reload();
            }
        });
    }
    {
        let (ui_weak, web) = (ui.as_weak(), web.clone());
        ui.on_web_layout(move || {
            if let (Some(ui), Some(v)) = (ui_weak.upgrade(), web.borrow().as_ref()) {
                v.layout(ui.window());
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
        let (ui_weak, shared, state, web, r) = (ui.as_weak(), shared.clone(), state.clone(), web.clone(), redraw.clone());
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
                    Some(Action::StopAll) => supervisor::stop_all(&shared),
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
            if let (Some(ui), Some(v)) = (ui_weak.upgrade(), web.borrow().as_ref()) {
                if ui.get_web_open() {
                    v.layout(ui.window());
                }
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
