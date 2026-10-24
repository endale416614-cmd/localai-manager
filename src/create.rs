//! Create tab backend: runs one of three bundled ComfyUI workflows (workflows\*.json) through ComfyUI's
//! HTTP API on a worker thread, as chat.rs does for the model server: upload the picture, queue the job,
//! poll /history, download the files into `creations\` beside the exe.

use crate::names;
use serde_json::{Value, json};
use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// The apps.toml id of the ComfyUI app; the tab talks to its port.
pub const ID: &str = "comfy";
/// 49 frames at 16 per second are about 3 seconds; Wan wants 4n+1 frames.
pub const FRAMES: u32 = 49;
/// A cold video run takes about 4 minutes on the 8 GB card; anything far past that is stuck.
const GIVE_UP: Duration = Duration::from_secs(30 * 60);
/// Failed polls in a row before the job counts as lost (ComfyUI can stall a moment when RAM is full).
const MAX_MISSES: u32 = 5;
/// ComfyUI refuses uploads over 100 MB.
const MAX_PICTURE: u64 = 100 << 20;
/// A result is a picture or a short clip; a limit keeps a wrong server from filling the disk.
const MAX_RESULT: u64 = 1 << 30;

#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub enum Mode {
    #[default]
    Image,
    Edit,
    Video,
}

impl Mode {
    pub fn from_index(i: i32) -> Mode {
        match i {
            1 => Mode::Edit,
            2 => Mode::Video,
            _ => Mode::Image,
        }
    }

    pub fn needs_picture(self) -> bool {
        self != Mode::Image
    }

    fn workflow(self) -> &'static str {
        match self {
            Mode::Image => include_str!("../workflows/image.json"),
            Mode::Edit => include_str!("../workflows/edit.json"),
            Mode::Video => include_str!("../workflows/video.json"),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Request {
    pub mode: Mode,
    pub prompt: String,
    pub picture: Option<PathBuf>,
    /// Video length; the tab always asks for FRAMES.
    pub frames: u32,
}

pub struct Done {
    /// The picture to show: the result, or for a video the start picture.
    pub image: Option<PathBuf>,
    pub video: Option<PathBuf>,
    pub secs: u64,
}

#[derive(Default)]
pub struct Create {
    pub busy: bool,
    /// When the worker began; the status line counts from here.
    pub started: Option<Instant>,
    pub error: String,
    pub done: Option<Done>,
    /// Bumped on every change so the UI only reloads the picture when something moved.
    pub version: u64,
    /// The queued job, so Cancel can tell ComfyUI which one to drop.
    prompt_id: Option<String>,
    cancel: Arc<AtomicBool>,
}

pub type Shared = Arc<Mutex<Create>>;

/// Results go to `creations` beside the exe.
pub fn dir() -> PathBuf {
    crate::config::beside_exe("creations")
}

/// Opens a file with its default app, or a folder in Explorer, through explorer.exe by full path.
pub fn open(path: &Path) {
    let _ = std::process::Command::new(crate::setup::windows_dir().join("explorer.exe")).arg(path).spawn();
}

/// (extension, MIME type) of a picture ComfyUI can be given.
fn picture_type(path: &Path) -> Result<(String, &'static str), String> {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or_default().to_ascii_lowercase();
    match ext.as_str() {
        "png" => Ok((ext, "image/png")),
        "jpg" | "jpeg" => Ok((ext, "image/jpeg")),
        "webp" => Ok((ext, "image/webp")),
        _ => Err("Pictures must be png, jpg or webp.".into()),
    }
}

/// What can be wrong with a request and costs nothing to find out: the picture's type and size, and a folder
/// for the result. Asked before anything is stopped or started for the job.
pub fn check(req: &Request, results: &Path) -> Result<(), String> {
    if let (true, Some(picture)) = (req.mode.needs_picture(), &req.picture) {
        picture_type(picture)?;
        let size = std::fs::metadata(picture).map_err(|e| format!("Cannot read the picture: {e}"))?.len();
        if size > MAX_PICTURE {
            return Err("That picture is over 100 MB, more than ComfyUI takes.".into());
        }
    }
    std::fs::create_dir_all(results).map_err(|e| format!("Cannot make the folder for the result, {}: {e}", results.display()))
}

fn reset(create: &Shared, error: String) {
    let mut c = create.lock().unwrap();
    *c = Create { error, version: c.version + 1, ..Default::default() };
}

/// Forgets the last result, or error.
pub fn clear(create: &Shared) {
    reset(create, String::new());
}

pub fn fail(create: &Shared, why: String) {
    reset(create, why);
}

/// Returns at once. The worker's later writes are ignored, and ComfyUI is told to drop the job.
pub fn cancel(create: &Shared, port: u16) {
    let mut c = create.lock().unwrap();
    c.cancel.store(true, Ordering::Relaxed);
    let id = c.prompt_id.take();
    *c = Create { version: c.version + 1, ..Default::default() };
    drop(c);
    if let Some(id) = id {
        thread::spawn(move || Comfy::new(port).abort(&id));
    }
}

pub fn start(create: &Shared, port: u16, req: Request, dir: PathBuf) {
    let mut c = create.lock().unwrap();
    if c.busy {
        return;
    }
    let cancel = Arc::new(AtomicBool::new(false));
    *c = Create { busy: true, started: Some(Instant::now()), version: c.version + 1, cancel: cancel.clone(), ..Default::default() };
    drop(c);

    let create = create.clone();
    thread::spawn(move || {
        let r = run(&Comfy::new(port), &req, &dir, &cancel, |id| {
            let mut c = create.lock().unwrap();
            if !cancel.load(Ordering::Relaxed) {
                c.prompt_id = Some(id.to_string());
            }
        });
        let mut c = create.lock().unwrap();
        if cancel.load(Ordering::Relaxed) {
            return; // the user moved on; Cancel already reset the tab
        }
        c.busy = false;
        c.version += 1;
        c.prompt_id = None;
        match r {
            Ok(done) => c.done = Some(done),
            Err(e) => c.error = e,
        }
    });
}

struct Comfy {
    agent: ureq::Agent,
    base: String,
    /// Time between two looks at /history.
    poll: Duration,
    /// How long a job may take before it is dropped.
    give_up: Duration,
    /// The most a download may be.
    max_result: u64,
}

impl Comfy {
    fn new(port: u16) -> Comfy {
        let agent = crate::net::local_agent(Some(Duration::from_secs(120)));
        Comfy { agent, base: format!("http://127.0.0.1:{port}"), poll: Duration::from_secs(1), give_up: GIVE_UP, max_result: MAX_RESULT }
    }

    /// (HTTP status, JSON body; null when the body is not JSON).
    fn reply(&self, r: Result<ureq::http::Response<ureq::Body>, ureq::Error>) -> Result<(u16, Value), String> {
        let mut r = r.map_err(net)?;
        let code = r.status().as_u16();
        Ok((code, r.body_mut().read_json::<Value>().unwrap_or(Value::Null)))
    }

    /// Stores the picture in ComfyUI's input folder under a name of our own; returns the name LoadImage takes.
    fn upload(&self, path: &Path, stamp: &str) -> Result<String, String> {
        let (ext, mime) = picture_type(path)?;
        let bytes = std::fs::read(path).map_err(|e| format!("Cannot read the picture: {e}"))?;
        let boundary = format!("localai{:016x}{:016x}", random(), random());
        let body = multipart(&boundary, &format!("{stamp}.{ext}"), mime, &bytes);
        let r = self.agent.post(format!("{}/upload/image", self.base)).content_type(format!("multipart/form-data; boundary={boundary}")).send(&body);
        let (code, v) = self.reply(r)?;
        let name = v["name"].as_str().filter(|_| code == 200).ok_or(format!("ComfyUI did not take the picture (HTTP {code})."))?;
        Ok(match v["subfolder"].as_str().filter(|s| !s.is_empty()) {
            Some(sub) => format!("{sub}/{name}"),
            None => name.to_string(),
        })
    }

    fn submit(&self, workflow: &Value) -> Result<String, String> {
        let r = self.agent.post(format!("{}/prompt", self.base)).send_json(json!({ "prompt": workflow, "client_id": "localai" }));
        let (code, v) = self.reply(r)?;
        if code != 200 {
            return Err(rejected(code, &v));
        }
        v["prompt_id"].as_str().map(str::to_string).ok_or_else(|| "ComfyUI did not name the job.".into())
    }

    fn history(&self, id: &str) -> Result<Poll, String> {
        let (_, v) = self.reply(self.agent.get(format!("{}/history/{id}", self.base)).call())?;
        Ok(parse_history(&v, id))
    }

    fn wait(&self, id: &str, cancel: &AtomicBool) -> Result<Vec<FileRef>, String> {
        let (give_up, mut misses) = (Instant::now() + self.give_up, 0);
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err("Cancelled.".into());
            }
            if Instant::now() > give_up {
                // Cancel can no longer name the job (it is forgotten once this returns), so drop it here:
                // otherwise ComfyUI would go on holding the GPU.
                self.abort(id);
                return Err("ComfyUI is taking too long; gave up after 30 minutes.".into());
            }
            match self.history(id) {
                Ok(Poll::Done(files)) => return Ok(files),
                Ok(Poll::Failed(why)) => return Err(why),
                Ok(Poll::Pending) => misses = 0,
                Err(e) => {
                    misses += 1;
                    if misses >= MAX_MISSES {
                        return Err(e);
                    }
                }
            }
            thread::sleep(self.poll);
        }
    }

    fn download(&self, f: &FileRef, dir: &Path) -> Result<PathBuf, String> {
        // The server names the file. Only a plain name is taken, so nothing is written outside `dir`, and
        // nothing to a device or an alternate stream.
        if !names::is_plain(&f.name) {
            return Err("ComfyUI sent a file name that cannot be saved here.".into());
        }
        std::fs::create_dir_all(dir).map_err(|e| format!("Cannot create {}: {e}", dir.display()))?;
        let dest = dir.join(&f.name);
        let r = self.agent.get(format!("{}/view", self.base)).query("filename", &f.name).query("subfolder", &f.subfolder).query("type", &f.kind).call().map_err(net)?;
        if r.status() != 200 {
            return Err(format!("ComfyUI did not give up the result (HTTP {}).", r.status()));
        }
        // create_new: a result never replaces a file that is already there.
        let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(&dest).map_err(|e| format!("Cannot save {}: {e}", dest.display()))?;
        let mut body = r.into_body().into_with_config().limit(self.max_result).reader();
        if let Err(e) = std::io::copy(&mut body, &mut file) {
            drop(file);
            let _ = std::fs::remove_file(&dest); // the half-written file this call made a moment ago
            return Err(format!("Cannot save {}: {e}", dest.display()));
        }
        Ok(dest)
    }

    /// Drops a job whether it still waits (/queue) or already runs (/interrupt). In this order, so it
    /// cannot slip from the queue into a run in between.
    fn abort(&self, id: &str) {
        let _ = self.agent.post(format!("{}/queue", self.base)).send_json(json!({ "delete": [id] }));
        let _ = self.agent.post(format!("{}/interrupt", self.base)).send_json(json!({ "prompt_id": id }));
    }
}

/// The whole job; `submitted` gets the job id as soon as ComfyUI has queued it.
fn run(comfy: &Comfy, req: &Request, dir: &Path, cancel: &AtomicBool, mut submitted: impl FnMut(&str)) -> Result<Done, String> {
    let begun = Instant::now();
    let stamp = format!("localai-{}", SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()));
    let image = match (&req.picture, req.mode.needs_picture()) {
        (Some(p), true) => comfy.upload(p, &stamp)?,
        (None, true) => return Err("Choose a picture first.".into()),
        _ => String::new(),
    };
    let prefix = format!("LocalAI/{stamp}");
    let vars = Vars { prompt: &req.prompt, seed: random() >> 16, image: &image, prefix: &prefix, frames: req.frames };
    let id = comfy.submit(&fill(req.mode.workflow(), &vars)?)?;
    submitted(&id);
    if cancel.load(Ordering::Relaxed) {
        comfy.abort(&id);
        return Err("Cancelled.".into());
    }
    let files = comfy.wait(&id, cancel)?;
    let saved = files.iter().map(|f| comfy.download(f, dir)).collect::<Result<Vec<_>, _>>()?;
    let secs = begun.elapsed().as_secs();
    if req.mode == Mode::Video {
        let video = with_ext(&saved, &["mp4", "webm", "mkv", "mov"]).ok_or("ComfyUI finished but made no video.")?;
        Ok(Done { image: req.picture.clone(), video: Some(video), secs })
    } else {
        let image = with_ext(&saved, &["png", "jpg", "jpeg", "webp"]).ok_or("ComfyUI finished but made no picture.")?;
        Ok(Done { image: Some(image), video: None, secs })
    }
}

fn with_ext(files: &[PathBuf], exts: &[&str]) -> Option<PathBuf> {
    files.iter().find(|p| p.extension().and_then(|e| e.to_str()).is_some_and(|e| exts.contains(&e.to_ascii_lowercase().as_str()))).cloned()
}

/// A number nobody can guess, from the keys the standard library seeds its hash maps with.
fn random() -> u64 {
    RandomState::new().build_hasher().finish()
}

struct Vars<'a> {
    prompt: &'a str,
    seed: u64,
    image: &'a str,
    prefix: &'a str,
    frames: u32,
}

/// The workflow with its placeholders filled. A placeholder is a whole string value (`"{{seed}}"`), swapped
/// on the parsed JSON, so a prompt with quotes, newlines or backslashes needs no escaping and can never
/// change the workflow around it.
fn fill(template: &str, v: &Vars) -> Result<Value, String> {
    fn swap(node: &mut Value, v: &Vars) {
        match node {
            Value::String(s) => {
                *node = match s.as_str() {
                    "{{prompt}}" => json!(v.prompt),
                    "{{seed}}" => json!(v.seed),
                    "{{image}}" => json!(v.image),
                    "{{prefix}}" => json!(v.prefix),
                    "{{frames}}" => json!(v.frames),
                    _ => return,
                }
            }
            Value::Array(a) => a.iter_mut().for_each(|x| swap(x, v)),
            Value::Object(o) => o.values_mut().for_each(|x| swap(x, v)),
            _ => {}
        }
    }
    let mut workflow: Value = serde_json::from_str(template).map_err(|e| format!("Bad workflow file: {e}"))?;
    swap(&mut workflow, v);
    Ok(workflow)
}

/// A multipart/form-data body with one file field, "image", as ComfyUI's /upload/image reads it.
fn multipart(boundary: &str, filename: &str, content_type: &str, bytes: &[u8]) -> Vec<u8> {
    let mut body = format!("--{boundary}\r\nContent-Disposition: form-data; name=\"image\"; filename=\"{filename}\"\r\nContent-Type: {content_type}\r\n\r\n").into_bytes();
    body.extend_from_slice(bytes);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    body
}

#[derive(Debug, PartialEq)]
struct FileRef {
    name: String,
    subfolder: String,
    kind: String,
}

#[derive(Debug, PartialEq)]
enum Poll {
    /// ComfyUI adds a job to /history only once it has finished.
    Pending,
    Done(Vec<FileRef>),
    Failed(String),
}

fn parse_history(body: &Value, id: &str) -> Poll {
    let Some(job) = body.get(id) else { return Poll::Pending };
    if job["status"]["status_str"] == "error" {
        return Poll::Failed(failure(&job["status"]));
    }
    // outputs: node id -> { "images": [..] } for SaveImage, { "gifs": [..] } for VideoHelperSuite.
    let files: Vec<FileRef> = job["outputs"]
        .as_object()
        .into_iter()
        .flat_map(|nodes| nodes.values())
        .filter_map(Value::as_object)
        .flat_map(|node| node.values())
        .filter_map(Value::as_array)
        .flatten()
        .filter_map(|f| {
            let name = f["filename"].as_str()?;
            let kind = f["type"].as_str().unwrap_or("output");
            // "temp" files are previews.
            (kind == "output").then(|| FileRef { name: name.into(), subfolder: f["subfolder"].as_str().unwrap_or_default().into(), kind: kind.into() })
        })
        .collect();
    if files.is_empty() { Poll::Failed("ComfyUI finished but made no file.".into()) } else { Poll::Done(files) }
}

fn failure(status: &Value) -> String {
    for m in status["messages"].as_array().into_iter().flatten() {
        match (m[0].as_str(), &m[1]) {
            (Some("execution_interrupted"), _) => return "Cancelled.".into(),
            (Some("execution_error"), d) => {
                let why = d["exception_message"].as_str().unwrap_or_default();
                if why.to_lowercase().contains("out of memory") {
                    return "ComfyUI ran out of memory. Stop other apps, then try again.".into();
                }
                return format!("ComfyUI stopped in {}: {}", d["node_type"].as_str().unwrap_or("a step"), short(why, 200));
            }
            _ => {}
        }
    }
    "ComfyUI could not finish the job.".into()
}

/// What /prompt says when it refuses a workflow: the first failing node tells why (a model file that is missing, say).
fn rejected(code: u16, body: &Value) -> String {
    let (node, e) = match body["node_errors"].as_object().and_then(|n| n.values().next()) {
        Some(n) => (n["class_type"].as_str(), &n["errors"][0]),
        None => (None, &body["error"]),
    };
    let mut why = e["message"].as_str().unwrap_or_default().to_string();
    if let Some(d) = e["details"].as_str().filter(|d| !d.is_empty()) {
        why = format!("{why}: {d}");
    }
    if let Some(node) = node {
        why = format!("{node}: {why}");
    }
    if why.trim_matches([' ', ':']).is_empty() { format!("ComfyUI refused the job (HTTP {code}).") } else { format!("ComfyUI refused the job: {}", short(&why, 240)) }
}

fn net(e: ureq::Error) -> String {
    match e {
        ureq::Error::Timeout(_) => "ComfyUI took too long to answer.".into(),
        ureq::Error::Io(_) | ureq::Error::ConnectionFailed | ureq::Error::HostNotFound => "Cannot reach ComfyUI. It may have stopped.".into(),
        e => format!("ComfyUI request failed: {e}"),
    }
}

/// The first line, cut to `max` characters.
fn short(s: &str, max: usize) -> String {
    let line = s.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or_default();
    match line.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &line[..i]),
        None => line.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::OnceLock;

    fn vars<'a>(prompt: &'a str) -> Vars<'a> {
        Vars { prompt, seed: 7, image: "a b.png", prefix: "LocalAI/x", frames: 17 }
    }

    const MODES: [Mode; 3] = [Mode::Image, Mode::Edit, Mode::Video];

    /// Every node of a filled workflow: (class_type, inputs).
    fn nodes(wf: &Value) -> Vec<(&str, &Value)> {
        wf.as_object().unwrap().values().map(|n| (n["class_type"].as_str().unwrap(), &n["inputs"])).collect()
    }

    #[test]
    fn prompts_survive_filling() {
        let prompts = ["plain", "say \"hi\"", "two\nlines\r\nand\ta tab", r#"C:\dir\file \n is not a newline \\ \""#, "ünï 😀", "{{seed}} {{image}} {{prefix}}", ""];
        for p in prompts {
            for mode in MODES {
                let wf = fill(mode.workflow(), &vars(p)).unwrap();
                // What goes over the wire is the JSON text of it, so the text has to survive that trip.
                let back: Value = serde_json::from_str(&wf.to_string()).unwrap();
                assert_eq!(back, wf);
                assert!(nodes(&back).iter().any(|(c, i)| *c == "CLIPTextEncode" && i["text"] == p), "{mode:?}: {p:?}");
                // A prompt that looks like a placeholder is text, not another placeholder.
                assert_eq!(back.to_string().matches(&json!(p).to_string()).count(), 1, "{mode:?}: {p:?}");
            }
        }
    }

    #[test]
    fn placeholders_get_their_types_and_none_is_left() {
        for mode in MODES {
            let wf = fill(mode.workflow(), &vars("a cat")).unwrap();
            let text = wf.to_string();
            assert!(!text.contains("{{"), "{mode:?}: {text}");
            let all: Vec<&Value> = nodes(&wf).into_iter().map(|(_, i)| i).collect();
            let has = |key: &str, want: Value| all.iter().any(|i| i[key] == want);
            assert!(has("filename_prefix", json!("LocalAI/x")), "{mode:?}");
            assert!(has("noise_seed", json!(7)), "{mode:?}");
            assert_eq!(has("image", json!("a b.png")), mode.needs_picture(), "{mode:?}");
            assert_eq!(has("length", json!(17)), mode == Mode::Video, "{mode:?}");
        }
    }

    #[test]
    fn workflows_are_wired() {
        for mode in MODES {
            let wf: Value = serde_json::from_str(mode.workflow()).unwrap();
            let outputs = nodes(&wf).iter().filter(|(c, _)| matches!(*c, "SaveImage" | "VHS_VideoCombine")).count();
            assert_eq!(outputs, 1, "{mode:?}: one node saves the result");
            for (id, node) in wf.as_object().unwrap() {
                for (name, v) in node["inputs"].as_object().unwrap() {
                    // A link is [node id, output slot]; it has to point at a node of this workflow.
                    if let Some([from, slot]) = v.as_array().map(Vec::as_slice) {
                        assert!(wf.get(from.as_str().unwrap()).is_some() && slot.is_u64(), "{mode:?}: node {id} input {name} links nowhere");
                    }
                }
            }
        }
    }

    #[test]
    fn multipart_keeps_the_file_bytes_as_they_are() {
        // The data holds what looks like a boundary line and a NUL: it must go through untouched.
        let data = b"\x89PNG\r\n--B--\r\n\x00\xff";
        let body = multipart("B", "in.png", "image/png", data);
        let mut want = b"--B\r\nContent-Disposition: form-data; name=\"image\"; filename=\"in.png\"\r\nContent-Type: image/png\r\n\r\n".to_vec();
        want.extend_from_slice(data);
        want.extend_from_slice(b"\r\n--B--\r\n");
        assert_eq!(body, want);
    }

    #[test]
    fn history_tells_pending_done_and_failed() {
        let id = "j1";
        let job = |status: Value, outputs: Value| json!({ id: { "status": status, "outputs": outputs } });
        let ok = json!({ "status_str": "success", "completed": true, "messages": [] });
        let file = |n: &str, sub: &str, t: &str| json!({ "filename": n, "subfolder": sub, "type": t });
        let f = |n: &str, sub: &str| FileRef { name: n.into(), subfolder: sub.into(), kind: "output".into() };

        assert_eq!(parse_history(&json!({}), id), Poll::Pending);
        assert_eq!(parse_history(&json!({ "other": {} }), id), Poll::Pending);
        // SaveImage: previews ("temp") are skipped.
        let images = json!({ "13": { "images": [file("a_00001_.png", "LocalAI", "output"), file("p.png", "", "temp")] } });
        assert_eq!(parse_history(&job(ok.clone(), images), id), Poll::Done(vec![f("a_00001_.png", "LocalAI")]));
        // VideoHelperSuite lists the video under "gifs"; core SaveVideo adds "animated": [true].
        let video = json!({ "16": { "gifs": [file("v_00001.mp4", "LocalAI", "output")], "animated": [true] } });
        assert_eq!(parse_history(&job(ok.clone(), video), id), Poll::Done(vec![f("v_00001.mp4", "LocalAI")]));
        assert!(matches!(parse_history(&job(ok, json!({})), id), Poll::Failed(m) if m.contains("no file")));

        let err = |m: Value| job(json!({ "status_str": "error", "completed": false, "messages": [["execution_start", {}], m] }), json!({}));
        let boom = json!(["execution_error", { "node_type": "VAEDecode", "exception_message": "bad shape\nTraceback..." }]);
        assert_eq!(parse_history(&err(boom), id), Poll::Failed("ComfyUI stopped in VAEDecode: bad shape".into()));
        let oom = json!(["execution_error", { "node_type": "KSampler", "exception_message": "CUDA out of memory. Tried to allocate 2 GiB" }]);
        assert!(matches!(parse_history(&err(oom), id), Poll::Failed(m) if m.contains("ran out of memory")));
        assert_eq!(parse_history(&err(json!(["execution_interrupted", {}])), id), Poll::Failed("Cancelled.".into()));
        assert_eq!(parse_history(&err(json!(["other", {}])), id), Poll::Failed("ComfyUI could not finish the job.".into()));
    }

    #[test]
    fn refusals_name_the_failing_node() {
        let body = json!({
            "error": { "type": "prompt_outputs_failed_validation", "message": "Prompt outputs failed validation", "details": "" },
            "node_errors": { "1": { "class_type": "UnetLoaderGGUF", "errors": [{ "message": "Value not in list", "details": "unet_name: 'x.gguf' not in ['a.gguf']" }] } }
        });
        assert_eq!(rejected(400, &body), "ComfyUI refused the job: UnetLoaderGGUF: Value not in list: unet_name: 'x.gguf' not in ['a.gguf']");
        let plain = json!({ "error": { "message": "No prompt provided", "details": "No prompt provided" }, "node_errors": {} });
        assert_eq!(rejected(400, &plain), "ComfyUI refused the job: No prompt provided: No prompt provided");
        assert_eq!(rejected(500, &Value::Null), "ComfyUI refused the job (HTTP 500).");
    }

    #[test]
    fn short_keeps_one_line() {
        assert_eq!(short("\n  first \nsecond", 50), "first");
        assert_eq!(short("abcdef", 3), "abc…");
        assert_eq!(short("äöü", 3), "äöü");
        assert_eq!(short("", 3), "");
    }

    /// What the fake server saw of a request: (request line, body).
    type Seen = (String, Vec<u8>);

    fn read_request(s: &mut TcpStream) -> Seen {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        let end = loop {
            let n = s.read(&mut chunk).unwrap();
            assert!(n > 0, "client hung up");
            buf.extend_from_slice(&chunk[..n]);
            if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break i + 4;
            }
        };
        let head = String::from_utf8_lossy(&buf[..end]).to_string();
        let len = head.lines().find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap())).unwrap_or(0);
        while buf.len() < end + len {
            let n = s.read(&mut chunk).unwrap();
            assert!(n > 0, "client hung up in the middle of its body");
            buf.extend_from_slice(&chunk[..n]);
        }
        (head.lines().next().unwrap().to_string(), buf[end..end + len].to_vec())
    }

    /// A stand-in for ComfyUI on a free port, so a whole job runs without a GPU. `answer` gets the request line
    /// and the body, and returns the content type and the reply. The server stops after the first request whose
    /// line contains `last`, and hands back everything it was asked.
    fn fake_comfyui(mut answer: impl FnMut(&str, &[u8]) -> (&'static str, Vec<u8>) + Send + 'static, last: &'static str) -> (u16, thread::JoinHandle<Vec<Seen>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = thread::spawn(move || {
            let mut seen = Vec::new();
            loop {
                let (mut s, _) = listener.accept().unwrap();
                let (line, body) = read_request(&mut s);
                let (ctype, reply) = answer(&line, &body);
                let head = format!("HTTP/1.1 200 OK\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", reply.len());
                s.write_all(head.as_bytes()).unwrap();
                s.write_all(&reply).unwrap();
                let stop = line.contains(last);
                seen.push((line, body));
                if stop {
                    return seen;
                }
            }
        });
        (port, server)
    }

    /// One reply, byte for byte, to the first request (a response that lies about its length, say).
    fn raw_server(response: Vec<u8>) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            read_request(&mut s);
            let _ = s.write_all(&response);
        });
        port
    }

    #[test]
    fn a_job_runs_end_to_end_against_a_fake_comfyui() {
        let dir = std::env::temp_dir().join("localai-create-test-fake");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let picture = dir.join("pic.PNG");
        std::fs::write(&picture, b"\x89PNG not really").unwrap();

        // ComfyUI lists a job in /history only once it has finished: the second look finds it.
        let mut looks = 0;
        let answer = move |line: &str, _: &[u8]| -> (&'static str, Vec<u8>) {
            match line.split(' ').nth(1).unwrap() {
                "/upload/image" => ("application/json", br#"{"name":"up.png","subfolder":"","type":"input"}"#.to_vec()),
                "/prompt" => ("application/json", br#"{"prompt_id":"job-1","number":1,"node_errors":{}}"#.to_vec()),
                "/history/job-1" => {
                    looks += 1;
                    let done = br#"{"job-1":{"status":{"status_str":"success","completed":true,"messages":[]},"outputs":{"19":{"images":[{"filename":"r_00001_.png","subfolder":"LocalAI","type":"output"}]}}}}"#;
                    ("application/json", if looks < 2 { b"{}".to_vec() } else { done.to_vec() })
                }
                p if p.starts_with("/view?") => ("image/png", b"RESULT".to_vec()),
                p => panic!("unexpected request {p}"),
            }
        };
        let (port, server) = fake_comfyui(answer, "/view?");
        let comfy = Comfy { poll: Duration::from_millis(10), ..Comfy::new(port) };
        let req = Request { mode: Mode::Edit, prompt: "make it \"blue\"\n".into(), picture: Some(picture), frames: FRAMES };
        let mut ids = Vec::new();
        let done = run(&comfy, &req, &dir.join("creations"), &AtomicBool::new(false), |id| ids.push(id.to_string())).unwrap();

        assert_eq!(ids, ["job-1"]);
        let image = done.image.unwrap();
        assert_eq!(image, dir.join("creations").join("r_00001_.png"));
        assert_eq!(std::fs::read(&image).unwrap(), b"RESULT");
        assert!(done.video.is_none());

        let seen = server.join().unwrap();
        let lines: Vec<&str> = seen.iter().map(|(l, _)| l.as_str()).collect();
        assert_eq!(lines.len(), 5, "{lines:?}");
        assert!(lines[0].starts_with("POST /upload/image "));
        assert!(lines[1].starts_with("POST /prompt "));
        assert!(lines[2].starts_with("GET /history/job-1 ") && lines[3].starts_with("GET /history/job-1 "));
        assert!(lines[4].starts_with("GET /view?filename=r_00001_.png&subfolder=LocalAI&type=output "), "{}", lines[4]);
        // The upload carries the file under our own name, with the extension lowercased.
        let upload = String::from_utf8_lossy(&seen[0].1);
        assert!(upload.contains("name=\"image\"; filename=\"localai-") && upload.contains(".png\"\r\nContent-Type: image/png"));
        assert!(seen[0].1.windows(15).any(|w| w == b"\x89PNG not really"));
        // The job names the uploaded file and carries the prompt as typed.
        let sent: Value = serde_json::from_slice(&seen[1].1).unwrap();
        let wf = &sent["prompt"];
        assert!(nodes(wf).iter().any(|(c, i)| *c == "LoadImage" && i["image"] == "up.png"));
        assert!(nodes(wf).iter().any(|(c, i)| *c == "CLIPTextEncode" && i["text"] == "make it \"blue\"\n"));
        assert_eq!(sent["client_id"], "localai");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cancel_frees_the_tab_and_late_results_are_dropped() {
        let create: Shared = Arc::default();
        // No picture: the worker fails at once, and the cancel may land before or after that.
        let req = Request { mode: Mode::Edit, prompt: "x".into(), picture: None, frames: FRAMES };
        start(&create, 9, req, std::env::temp_dir());
        cancel(&create, 9);
        thread::sleep(Duration::from_millis(300));
        let c = create.lock().unwrap();
        assert!(!c.busy && c.error.is_empty() && c.done.is_none());
    }

    /// An empty folder of its own under the temp folder.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("localai-create-test-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_job_that_takes_too_long_is_dropped_in_comfyui() {
        // /history never lists the job. After the give-up time it has to leave the queue, or be interrupted if it
        // runs, because nothing else can name it once the worker has returned.
        let (port, server) = fake_comfyui(|line, _| ("application/json", if line.contains("/history/") { b"{}".to_vec() } else { Vec::new() }), "/interrupt");
        let comfy = Comfy { poll: Duration::from_millis(10), give_up: Duration::from_millis(100), ..Comfy::new(port) };
        let err = comfy.wait("job-1", &AtomicBool::new(false)).unwrap_err();
        assert!(err.contains("taking too long"), "{err}");
        let seen = server.join().unwrap();
        let posts: Vec<&Seen> = seen.iter().filter(|(l, _)| l.starts_with("POST")).collect();
        assert_eq!(posts.len(), 2, "{seen:?}");
        assert!(posts[0].0.starts_with("POST /queue ") && String::from_utf8_lossy(&posts[0].1).contains("job-1"));
        assert!(posts[1].0.starts_with("POST /interrupt ") && String::from_utf8_lossy(&posts[1].1).contains("job-1"));
    }

    #[test]
    fn a_result_is_saved_only_under_a_plain_name() {
        let dir = std::env::temp_dir().join("localai-create-test-names");
        let _ = std::fs::remove_dir_all(&dir);
        // Port 1 answers nobody: a name that is not refused first would end in "Cannot reach ComfyUI".
        let comfy = Comfy::new(1);
        for name in ["", "..", "a/b.png", "..\\x.png", "C:\\x.png", "x.png:hidden", "x.png.", "x.png ", "CON", "nul.png", "COM3.png", "lpt1"] {
            let f = FileRef { name: name.into(), subfolder: String::new(), kind: "output".into() };
            let e = comfy.download(&f, &dir).unwrap_err();
            assert!(e.contains("cannot be saved here"), "{name:?}: {e}");
        }
        assert!(!dir.exists(), "refused before anything was made");
    }

    #[test]
    fn a_download_over_the_limit_or_cut_short_leaves_no_file() {
        let dir = scratch("limit");
        let f = |name: &str| FileRef { name: name.into(), subfolder: "LocalAI".into(), kind: "output".into() };
        let head = b"HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: 100\r\nConnection: close\r\n\r\n".as_slice();

        // More than the limit allows.
        let comfy = Comfy { max_result: 10, ..Comfy::new(raw_server([head, &[7u8; 100]].concat())) };
        let e = comfy.download(&f("big.png"), &dir).unwrap_err();
        assert!(e.starts_with("Cannot save"), "{e}");
        assert!(!dir.join("big.png").exists(), "the partial file is removed");

        // The server promises 100 bytes, sends 40 and hangs up.
        let comfy = Comfy::new(raw_server([head, &[7u8; 40]].concat()));
        let e = comfy.download(&f("cut.png"), &dir).unwrap_err();
        assert!(e.starts_with("Cannot save"), "{e}");
        assert!(!dir.join("cut.png").exists(), "the partial file is removed");

        // Within the limit it is saved whole.
        let comfy = Comfy { max_result: 150, ..Comfy::new(raw_server([head, &[7u8; 100]].concat())) };
        assert_eq!(std::fs::read(comfy.download(&f("fine.png"), &dir).unwrap()).unwrap(), vec![7u8; 100]);
        // And a file that is there already is not replaced.
        let comfy = Comfy::new(raw_server([head, &[9u8; 100]].concat()));
        let e = comfy.download(&f("fine.png"), &dir).unwrap_err();
        assert!(e.starts_with("Cannot save"), "{e}");
        assert_eq!(std::fs::read(dir.join("fine.png")).unwrap(), vec![7u8; 100]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_request_that_cannot_run_is_refused_before_anything_starts() {
        let dir = scratch("check");
        let results = dir.join("creations");
        let req = |mode, picture: Option<PathBuf>| Request { mode, prompt: "x".into(), picture, frames: FRAMES };
        let file = |name: &str, size: u64| {
            let p = dir.join(name);
            std::fs::File::create(&p).unwrap().set_len(size).unwrap();
            p
        };

        // An image needs no picture, and the folder for the result is made.
        assert_eq!(check(&req(Mode::Image, None), &results), Ok(()));
        assert!(results.is_dir());
        for ok in ["a.png", "b.JPG", "c.jpeg", "d.webp"] {
            assert_eq!(check(&req(Mode::Edit, Some(file(ok, 10))), &results), Ok(()), "{ok}");
        }
        // The dialog lets a typed name through: only png, jpg and webp are pictures.
        for bad in ["e.gif", "f.bmp", "g.txt", "h"] {
            assert_eq!(check(&req(Mode::Video, Some(file(bad, 10))), &results), Err("Pictures must be png, jpg or webp.".into()), "{bad}");
        }
        let missing = check(&req(Mode::Edit, Some(dir.join("gone.png"))), &results).unwrap_err();
        assert!(missing.starts_with("Cannot read the picture"), "{missing}");
        // ComfyUI refuses an upload over 100 MB (the files are made without being written, so they cost no time).
        let exact = file("exact.png", MAX_PICTURE);
        assert_eq!(check(&req(Mode::Edit, Some(exact.clone())), &results), Ok(()));
        std::fs::remove_file(exact).unwrap();
        let big = check(&req(Mode::Edit, Some(file("big.png", MAX_PICTURE + 1))), &results).unwrap_err();
        assert_eq!(big, "That picture is over 100 MB, more than ComfyUI takes.");
        // No folder for the result: a file is in the way.
        let blocked = dir.join("blocked");
        std::fs::write(&blocked, "x").unwrap();
        let e = check(&req(Mode::Image, None), &blocked.join("creations")).unwrap_err();
        assert!(e.starts_with("Cannot make the folder for the result"), "{e}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // The tests below talk to a running ComfyUI (port 8188, or LOCALAI_COMFY_PORT) and use the GPU:
    // cargo test -- --ignored --test-threads=1

    fn live(req: Request) -> Done {
        static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());
        let _turn = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
        let port = std::env::var("LOCALAI_COMFY_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(8188);
        let out = std::env::temp_dir().join("localai-create-test");
        let t = Instant::now();
        let done = run(&Comfy::new(port), &req, &out, &AtomicBool::new(false), |_| {}).unwrap_or_else(|e| panic!("{:?}: {e}", req.mode));
        eprintln!("{:?}: {} s", req.mode, t.elapsed().as_secs());
        done
    }

    fn assert_file(path: &Path, magic: &[u8], at: usize) {
        let bytes = std::fs::read(path).unwrap();
        eprintln!("{} ({} bytes)", path.display(), bytes.len());
        assert!(bytes.len() > 10_000 && &bytes[at..at + magic.len()] == magic, "{}", path.display());
    }

    /// The Image result, reused as the start picture of the other two tests.
    fn sample() -> PathBuf {
        static PICTURE: OnceLock<PathBuf> = OnceLock::new();
        PICTURE.get_or_init(|| live(Request { mode: Mode::Image, prompt: "a lighthouse on a cliff at sunset, photo".into(), picture: None, frames: FRAMES }).image.unwrap()).clone()
    }

    #[test]
    #[ignore = "needs ComfyUI and the GPU"]
    fn image_workflow_makes_a_picture() {
        let done = live(Request { mode: Mode::Image, prompt: "a red bicycle against a brick wall, photo".into(), picture: None, frames: FRAMES });
        assert_file(&done.image.unwrap(), b"\x89PNG", 0);
    }

    #[test]
    #[ignore = "needs ComfyUI and the GPU"]
    fn edit_workflow_changes_a_picture() {
        let done = live(Request { mode: Mode::Edit, prompt: "make the sky green".into(), picture: Some(sample()), frames: FRAMES });
        assert_file(&done.image.unwrap(), b"\x89PNG", 0);
    }

    #[test]
    #[ignore = "needs ComfyUI, the GPU and 14 GB of free RAM"]
    fn video_workflow_makes_a_clip() {
        let mut sys = sysinfo::System::new();
        sys.refresh_memory();
        let free = sys.available_memory() as f64 / (1u64 << 30) as f64;
        if free < 14.0 {
            eprintln!("skipped: only {free:.1} GB of RAM free, a video run needs 14");
            return;
        }
        eprintln!("free RAM before the video: {free:.1} GB");
        // 17 frames instead of 49: the same path, a third of the time.
        let done = live(Request { mode: Mode::Video, prompt: "waves roll in, the camera pushes in slowly".into(), picture: Some(sample()), frames: 17 });
        assert_file(&done.video.unwrap(), b"ftyp", 4);
    }
}
