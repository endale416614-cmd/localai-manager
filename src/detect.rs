//! Finds local LLMs already on this PC and adds the ones apps.toml does not know yet: GGUF files
//! (run with llama-server), Ollama, and an Odysseus checkout. Runs at every startup, so a model
//! downloaded while the manager was closed shows up on the next start.

use crate::config::{self, AppDef};
use crate::setup::toml_str;
use crate::supervisor::{self, Shared};
use std::collections::HashSet;
use std::fs;
use std::io::{BufReader, Read};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Known model families: display names and launch flags. Every substring must be in the lowercase
/// file name; the first match wins, so abliterated builds come before their originals.
const KNOWN: &[(&[&str], &str, &str, &[&str])] = &[
    (&["huihui", "qwen3.6-35b-a3b", "abliterated"], "Qwen3.6 Uncensored", "whole-book translation · thinking off", NO_THINK),
    (&["qwen3.8-35b-a3b", "abliterated"], "Qwen3.8 35B Distill Uncensored", "translation candidate · community distill · thinking off", NO_THINK),
    (&["qwen3.8-27b", "abliterated"], "Qwen3.8 27B Uncensored", "hard passages · slower · thinking off", NO_THINK),
    (&["qwen3.6-35b-a3b"], "Qwen3.6", "daily chat, coding, agents", QWEN36),
    (&["qwen3.8-27b"], "Qwen3.8 27B", "hard coding problems · slower", &[]),
    (&["translategemma"], "TranslateGemma 12B", "bulk translation", &[]),
    (&["typhoon", "translate"], "Typhoon Translate", "Thai ↔ English translation", &[]),
    (&["typhoon2.5"], "Typhoon 2.5", "Thai chat with tool calling", &[]),
];
/// Thinking off for the translation builds: faster, and a translation needs no reasoning trace.
const NO_THINK: &[&str] = &["--reasoning-budget", "0"];
/// Same samplers as start-qwen36.bat (Qwen3.6 model card, thinking mode, general tasks).
const QWEN36: &[&str] = &["--temp", "1.0", "--top-p", "0.95", "--top-k", "20", "--min-p", "0", "--presence-penalty", "1.5"];

/// GGUF architectures that are not chat models: ComfyUI image/video models, text encoders,
/// embedding models and vision projectors.
const NOT_CHAT: &[&str] = &[
    "clip", "flux", "sd1", "sdxl", "sd3", "aura", "hidream", "cosmos", "ltxv", "hyvid", "wan", "lumina2", "qwen_image", "t5", "t5encoder",
    "bert", "nomic-bert", "jina-bert-v2",
];

const FIRST_PORT: u16 = 8090;
const MARK: &str = "# --- Auto-detected";
const HEADER: &str = "# --- Auto-detected by LocalAI Manager at startup. To hide one, comment out its lines: the file name\n\
# stays in this file, so it is not added again. Odysseus gets every chat app below as a model endpoint. ---\n\n";

#[derive(Debug)]
pub struct Model {
    pub path: PathBuf,
    /// Lowercase file name: the key for "already in apps.toml" and for de-duplicating copies.
    pub file: String,
    pub arch: String,
    pub title: String,
}

#[derive(Default)]
pub struct Found {
    pub server: Option<PathBuf>,
    pub models: Vec<Model>,
    pub ollama: Option<(PathBuf, Vec<String>)>,
    pub odysseus: Option<PathBuf>,
}

/// Scans on a worker thread (a sleeping USB disk can take seconds to answer), adds what it found to
/// the running manager, then connects every chat model to Odysseus.
pub fn spawn(shared: Shared, path: PathBuf) {
    std::thread::spawn(move || {
        match run(&path).and_then(|names| if names.is_empty() { Ok(()) } else { config::load(&path).map(|apps| supervisor::add(&shared, apps)) }) {
            Ok(()) => {}
            Err(e) => shared.lock().unwrap().notice = format!("model scan: {e}"),
        }
        crate::odysseus::link(&shared);
    });
}

/// Scans, then appends new entries to apps.toml (creating it if needed). Returns the names added.
pub fn run(path: &Path) -> Result<Vec<String>, String> {
    let path = std::path::absolute(path).map_err(|e| e.to_string())?;
    let home = path.parent().unwrap_or(Path::new("."));
    let found = Found { server: llama_server(home), models: models(home), ollama: ollama(), odysseus: odysseus() };
    update(&path, &found)
}

/// Reads apps.toml only now, after the scan (a sleeping disk can take seconds), so edits saved in the
/// meantime are merged rather than overwritten. A file that cannot be read is never written over.
fn update(path: &Path, found: &Found) -> Result<Vec<String>, String> {
    let text = config::read(path)?.unwrap_or_default();
    let read_bat = |p: &str| fs::read(p).map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default();
    let Some((out, names)) = merge(&text, found, read_bat)? else { return Ok(Vec::new()) };
    config::save(path, &out)?;
    Ok(names)
}

/// apps.toml text with entries for everything in `found` it does not cover yet, or None.
fn merge(text: &str, found: &Found, read_bat: impl Fn(&str) -> String) -> Result<Option<(String, Vec<String>)>, String> {
    let apps: Vec<AppDef> = if text.trim().is_empty() { Vec::new() } else { config::parse(text)? };
    // A model counts as known when its file name appears in apps.toml or in a .bat an app runs.
    let mut known = text.to_lowercase();
    for a in &apps {
        for c in &a.cmd {
            let l = c.to_lowercase();
            if l.ends_with(".bat") || l.ends_with(".cmd") {
                // cmd.exe finds a relative script from the app's working directory.
                let p = a.cwd.as_ref().map_or_else(|| PathBuf::from(c), |d| d.join(c));
                known.push_str(&read_bat(&p.display().to_string()).to_lowercase());
            }
        }
    }
    let mut ids: HashSet<String> = apps.iter().map(|a| a.id.clone()).collect();
    let mut ports: HashSet<u16> = apps.iter().filter_map(|a| a.port).collect();
    let mut add = String::new();
    let mut names = Vec::new();

    if let Some(server) = &found.server {
        for m in found.models.iter().filter(|m| !mentions(&known, &m.file)) {
            let (name, job, extra) = KNOWN
                .iter()
                .find(|(keys, ..)| keys.iter().all(|k| m.file.contains(k)))
                .map(|&(_, n, j, x)| (n.to_string(), j.to_string(), x))
                .unwrap_or((m.title.clone(), m.arch.clone(), &[] as &[&str]));
            let base = slug(m.path.file_stem().unwrap_or_default().to_string_lossy().as_ref());
            let id = unique(&mut ids, if base.is_empty() { "model" } else { &base });
            let port = (FIRST_PORT..).find(|p| !ports.contains(p)).expect("a free port");
            ports.insert(port);
            let gb = fs::metadata(&m.path).map(|x| x.len()).unwrap_or(0) as f64 / (1u64 << 30) as f64;
            let mut cmd: Vec<String> = [server.display().to_string(), "-m".into(), m.path.display().to_string(), "-a".into(), id.clone()].into();
            // No -ngl: --fit (default on) splits layers across every GPU and keeps the rest in RAM.
            let port_s = port.to_string();
            let flags = ["--host", "127.0.0.1", "--port", port_s.as_str(), "-c", "32768", "-fa", "on", "-ctk", "q8_0", "-ctv", "q8_0"];
            cmd.extend(flags.iter().chain(extra).map(|s| s.to_string()));
            add += &entry(&id, &name, &format!("{job} · {gb:.1} GB · auto-detected"), "gpu", true, &cmd, server.parent(), Some((port, "/health")), &m.path.display().to_string());
            names.push(name);
        }
    }

    if let Some((exe, tags)) = &found.ollama {
        if !ports.contains(&11434) && !known.contains("ollama.exe") {
            let list = match tags.len() {
                0 => "no models pulled yet".to_string(),
                n if n <= 3 => tags.join(" · "),
                n => format!("{} +{}", tags[..3].join(" · "), n - 3),
            };
            let note = "Odysseus talks to every model Ollama has pulled. If the Ollama tray app already serves :11434, quit it before starting Ollama here.";
            add += &entry(&unique(&mut ids, "ollama"), "Ollama", &list, "svc", false, &[exe.display().to_string(), "serve".into()], None, Some((11434, "/api/version")), note);
            names.push("Ollama".into());
        }
    }

    if let Some(dir) = &found.odysseus {
        // Listed under another id, or :7000 taken: a second entry would only fight over the port.
        let listed = apps.iter().any(|a| a.cwd.as_deref().is_some_and(|c| same_dir(c, dir)));
        if !ids.contains(crate::odysseus::ID) && !listed && !ports.contains(&7000) {
            let py = dir.join("venv").join("Scripts").join("python.exe").display().to_string();
            let cmd = [py.as_str(), "-m", "uvicorn", "app:app", "--host", "127.0.0.1", "--port", "7000"].map(|s| s.to_string());
            let note = "Opens inside LocalAI. Every chat model here is added to it as a model endpoint. Run setup.py once first to create the admin login.";
            add += &entry(crate::odysseus::ID, "Odysseus", "AI workspace · chat, agents, research, docs", "svc", false, &cmd, Some(dir), Some((7000, "/api/health")), note);
            names.push("Odysseus".into());
        }
    }

    if add.is_empty() {
        return Ok(None);
    }
    let mut out = text.to_string();
    // A blank line first, also when the header is already there: an editor may have dropped the final newline.
    if !out.is_empty() && !out.ends_with("\n\n") {
        out.push_str(if out.ends_with('\n') { "\n" } else { "\n\n" });
    }
    if !out.contains(MARK) {
        out.push_str(HEADER);
    }
    out.push_str(&add);
    config::parse(&out).map_err(|e| format!("auto-detected entry rejected: {e}"))?;
    Ok(Some((out, names)))
}

#[allow(clippy::too_many_arguments)]
fn entry(id: &str, name: &str, sub: &str, group: &str, chat: bool, cmd: &[String], cwd: Option<&Path>, port: Option<(u16, &str)>, note: &str) -> String {
    let mut s = format!("[[app]]\nid = {}\nname = {}\nsub = {}\ngroup = \"{group}\"\n", toml_str(id), toml_str(name), toml_str(sub));
    if chat {
        s += "chat = true\n";
    }
    s += &format!("cmd = [{}]\n", cmd.iter().map(|c| toml_str(c)).collect::<Vec<_>>().join(", "));
    if let Some(d) = cwd {
        s += &format!("cwd = {}\n", toml_str(&d.display().to_string()));
    }
    if let Some((p, health)) = port {
        s += &format!("port = {p}\nhealth = \"{health}\"\nurl = \"http://127.0.0.1:{p}\"\n");
    }
    s + &format!("note = {}\n\n", toml_str(note))
}

/// `file` appears as a whole file name, not as the tail of a longer one ("x-model.gguf" ≠ "model.gguf").
fn mentions(known: &str, file: &str) -> bool {
    known.match_indices(file).any(|(i, _)| i == 0 || known[..i].ends_with(['\\', '/', '"', '\'', ' ', '=']))
}

fn slug(s: &str) -> String {
    let mut out = String::new();
    for c in s.to_lowercase().chars() {
        if c.is_ascii_alphanumeric() || c == '.' {
            out.push(c);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_matches('-').to_string()
}

fn unique(ids: &mut HashSet<String>, base: &str) -> String {
    let id = (1..).map(|n| if n == 1 { base.to_string() } else { format!("{base}-{n}") }).find(|c| !ids.contains(c)).expect("a free id");
    ids.insert(id.clone());
    id
}

/// Same folder, ignoring case, slash direction and a trailing separator.
fn same_dir(a: &Path, b: &Path) -> bool {
    let norm = |p: &Path| p.to_string_lossy().replace('/', "\\").trim_end_matches('\\').to_lowercase();
    norm(a) == norm(b)
}

fn on_path(exe: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?).map(|d| d.join(exe)).find(|p| p.is_file())
}

fn user_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE").map(PathBuf::from)
}

/// Fixed drives only: an empty card reader or a slow network share never stalls startup.
fn drives() -> impl Iterator<Item = PathBuf> {
    (b'C'..=b'Z').map(|d| PathBuf::from(format!("{}:\\", d as char))).filter(|p| {
        let w: Vec<u16> = p.as_os_str().encode_wide().chain([0]).collect();
        unsafe { windows_sys::Win32::Storage::FileSystem::GetDriveTypeW(w.as_ptr()) == windows_sys::Win32::System::WindowsProgramming::DRIVE_FIXED }
    })
}

/// The one setup installs beside the exe, else a `winget install llama.cpp` on PATH.
fn llama_server(home: &Path) -> Option<PathBuf> {
    let local = home.join("llama.cpp").join("llama-server.exe");
    if local.is_file() { Some(local) } else { on_path("llama-server.exe") }
}

/// Every chat GGUF in the usual model folders: beside the exe, LM Studio, the Hugging Face cache,
/// and X:\AI, X:\models, X:\LLM, X:\LocalAI\models on every drive.
pub fn models(home: &Path) -> Vec<Model> {
    let mut roots = vec![home.join("models")];
    if let Some(u) = user_dir() {
        roots.extend([".lmstudio\\models", ".cache\\lm-studio\\models", ".cache\\huggingface\\hub", "models"].map(|d| u.join(d)));
    }
    for d in drives() {
        roots.extend(["AI", "models", "LLM", "LocalAI\\models"].map(|s| d.join(s)));
    }
    let mut files = Vec::new();
    for r in &roots {
        walk(r, 4, &mut files);
    }
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for path in files {
        let file = path.file_name().unwrap_or_default().to_string_lossy().to_lowercase();
        // Vision projectors and later split parts are loaded through their main file.
        let later_part = file.contains("-of-0") && !file.contains("-00001-of-");
        if file.contains("mmproj") || later_part || path.to_string_lossy().to_lowercase().contains("comfyui") || seen.contains(&file) {
            continue;
        }
        let Some((arch, name)) = gguf_meta(&path) else { continue };
        if NOT_CHAT.contains(&arch.as_str()) {
            continue;
        }
        seen.insert(file.clone());
        let title = if name.is_empty() { path.file_stem().unwrap_or_default().to_string_lossy().into_owned() } else { name };
        out.push(Model { path, file, arch, title });
    }
    out
}

fn walk(dir: &Path, depth: u32, out: &mut Vec<PathBuf>) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        match e.file_type() {
            Ok(t) if t.is_dir() => {
                if depth > 0 {
                    walk(&p, depth - 1, out);
                }
            }
            Ok(_) if p.extension().is_some_and(|x| x.eq_ignore_ascii_case("gguf")) => out.push(p),
            _ => {}
        }
    }
}

/// `general.architecture` and `general.name` from a GGUF header. None when the file is not GGUF v2+,
/// is malformed, or is not a chat model: adapters, projectors and vocab files (`general.type`) and
/// embedding models (a `*.pooling_type` key).
fn gguf_meta(path: &Path) -> Option<(String, String)> {
    let file = fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let mut r = BufReader::new(file);
    let mut magic = [0u8; 4];
    r.read_exact(&mut magic).ok()?;
    if &magic != b"GGUF" || num::<4>(&mut r)? < 2 {
        return None;
    }
    num::<8>(&mut r)?; // tensor count
    let (mut arch, mut name) = (String::new(), String::new());
    for _ in 0..num::<8>(&mut r)? {
        let key = string(&mut r)?;
        // Everything above comes before the tokenizer, whose huge arrays are most of the header.
        if key.starts_with("tokenizer.") {
            break;
        }
        let ty = num::<4>(&mut r)?;
        match (ty, key.as_str()) {
            (8, "general.architecture") => arch = clean(&string(&mut r)?),
            (8, "general.name") => name = clean(&string(&mut r)?),
            (8, "general.type") => {
                if string(&mut r)? != "model" {
                    return None;
                }
            }
            _ if key.ends_with(".pooling_type") => return None,
            _ => skip(&mut r, ty, len, 0)?,
        }
    }
    (!arch.is_empty()).then_some((arch, name))
}

/// Header text ends up in apps.toml: no control characters (a newline would break the file), 80 characters at most.
fn clean(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).collect::<String>().trim().chars().take(80).collect()
}

fn num<const N: usize>(r: &mut impl Read) -> Option<u64> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b[..N]).ok()?;
    Some(u64::from_le_bytes(b))
}

fn string(r: &mut impl Read) -> Option<String> {
    let n = num::<8>(r)?;
    if n > 1 << 16 {
        return None;
    }
    let mut b = vec![0u8; n as usize];
    r.read_exact(&mut b).ok()?;
    Some(String::from_utf8_lossy(&b).into_owned())
}

/// Skips one value of GGUF type `ty` in a file of `len` bytes; `depth` counts the arrays around it.
/// A bad type, EOF, or a size that cannot fit in the file ends the scan with None (a negative seek
/// from a huge length would loop forever).
fn skip(r: &mut BufReader<fs::File>, ty: u64, len: u64, depth: u32) -> Option<()> {
    let size: u64 = match ty {
        0 | 1 | 7 => 1,
        2 | 3 => 2,
        4..=6 => 4,
        10..=12 => 8,
        8 => num::<8>(r)?,
        9 => {
            if depth >= 2 {
                return None;
            }
            let (t, n) = (num::<4>(r)?, num::<8>(r)?);
            if t == 8 || t == 9 {
                if n > 10_000_000 {
                    return None;
                }
                for _ in 0..n {
                    skip(r, t, len, depth + 1)?;
                }
                return Some(());
            }
            let each = match t {
                0 | 1 | 7 => 1,
                2 | 3 => 2,
                4..=6 => 4,
                10..=12 => 8,
                _ => return None,
            };
            n.checked_mul(each)?
        }
        _ => return None,
    };
    if size > len {
        return None;
    }
    r.seek_relative(i64::try_from(size).ok()?).ok()
}

fn ollama() -> Option<(PathBuf, Vec<String>)> {
    let exe = std::env::var_os("LOCALAPPDATA")
        .map(|d| PathBuf::from(d).join("Programs\\Ollama\\ollama.exe"))
        .filter(|p| p.is_file())
        .or_else(|| on_path("ollama.exe"))?;
    let store = std::env::var_os("OLLAMA_MODELS").map(PathBuf::from).or_else(|| user_dir().map(|u| u.join(".ollama\\models")));
    let mut tags = Vec::new();
    // manifests\registry.ollama.ai\<namespace>\<model>\<tag>; "library" is the default namespace.
    let ls = |p: &Path| fs::read_dir(p).into_iter().flatten().flatten().map(|e| e.path()).collect::<Vec<_>>();
    let name = |p: &Path| p.file_name().unwrap_or_default().to_string_lossy().into_owned();
    if let Some(s) = store {
        for ns in ls(&s.join("manifests\\registry.ollama.ai")) {
            for model in ls(&ns) {
                for tag in ls(&model) {
                    let prefix = if name(&ns) == "library" { String::new() } else { format!("{}/", name(&ns)) };
                    tags.push(format!("{prefix}{}:{}", name(&model), name(&tag)));
                }
            }
        }
    }
    tags.sort();
    Some((exe, tags))
}

/// An `odysseus` folder with app.py and its venv, at a drive root, one folder below it, or in the
/// user folder (launch-windows.ps1 builds the venv in place), owned by someone we trust.
fn odysseus() -> Option<PathBuf> {
    let ok = |d: &Path| d.join("app.py").is_file() && d.join("venv\\Scripts\\python.exe").is_file() && trusted(d);
    for base in drives().chain(user_dir()) {
        let mut cands = vec![base.join("odysseus")];
        cands.extend(fs::read_dir(&base).into_iter().flatten().flatten().map(|e| e.path().join("odysseus")));
        if let Some(c) = cands.into_iter().find(|c| ok(c)) {
            return Some(c);
        }
    }
    None
}

/// A found Odysseus runs its venv Python at the next start, so only a folder owned by this user,
/// Administrators or SYSTEM is taken. Asked of PowerShell (full path, folder in an env var), and only
/// for a folder that already looks like Odysseus.
fn trusted(dir: &Path) -> bool {
    const SCRIPT: &str = "$o=(Get-Acl -LiteralPath $env:LAI_DIR).GetOwner([Security.Principal.SecurityIdentifier]).Value;\
$me=[Security.Principal.WindowsIdentity]::GetCurrent().User.Value;\
[Console]::Out.Write([int]($o -eq $me -or $o -eq 'S-1-5-32-544' -or $o -eq 'S-1-5-18'))";
    Command::new(crate::setup::system32(crate::setup::POWERSHELL))
        .args(["-NoProfile", "-NonInteractive", "-Command", SCRIPT])
        .env("LAI_DIR", dir)
        .creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW)
        .output()
        .is_ok_and(|o| o.stdout.trim_ascii() == b"1")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put_str(b: &mut Vec<u8>, t: &str) {
        b.extend((t.len() as u64).to_le_bytes());
        b.extend(t.as_bytes());
    }

    /// A GGUF header announcing `kvs` keys, as llama.cpp writes it.
    fn header(kvs: u64) -> Vec<u8> {
        let mut b = b"GGUF".to_vec();
        b.extend(3u32.to_le_bytes());
        b.extend(0u64.to_le_bytes());
        b.extend(kvs.to_le_bytes());
        b
    }

    fn str_kv(b: &mut Vec<u8>, key: &str, val: &str) {
        put_str(b, key);
        b.extend(8u32.to_le_bytes());
        put_str(b, val);
    }

    /// Keys in the order llama.cpp writes them: general.*, then the tokenizer. The tokenizer key is
    /// cut off after its name: the reader must stop there without looking at its value.
    fn gguf_with(extra: &[(&str, &str)], arch: &str, name: &str) -> Vec<u8> {
        let mut b = header(4 + extra.len() as u64);
        // an array of strings before the keys that matter
        put_str(&mut b, "general.tags");
        b.extend(9u32.to_le_bytes());
        b.extend(8u32.to_le_bytes());
        b.extend(2u64.to_le_bytes());
        put_str(&mut b, "a");
        put_str(&mut b, "bc");
        for (k, v) in extra {
            str_kv(&mut b, k, v);
        }
        str_kv(&mut b, "general.architecture", arch);
        str_kv(&mut b, "general.name", name);
        put_str(&mut b, "tokenizer.ggml.tokens");
        b
    }

    fn gguf(arch: &str, name: &str) -> Vec<u8> {
        gguf_with(&[], arch, name)
    }

    /// An embedding model: the general keys first, then `<arch>.pooling_type` (a u32) among the model keys.
    fn embedding_gguf(arch: &str) -> Vec<u8> {
        let mut b = header(4);
        str_kv(&mut b, "general.architecture", arch);
        str_kv(&mut b, "general.name", "Embedder");
        put_str(&mut b, &format!("{arch}.pooling_type"));
        b.extend(4u32.to_le_bytes());
        b.extend(1u32.to_le_bytes());
        put_str(&mut b, "tokenizer.ggml.tokens");
        b
    }

    /// Writes `bytes` to a fresh file and runs `gguf_meta` on a thread, so a parser that never
    /// returns fails the test instead of hanging it.
    fn meta_of(bytes: &[u8]) -> Option<(String, String)> {
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!("localai-gguf-test-{}-{}", std::process::id(), N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("m.gguf");
        fs::write(&path, bytes).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn({
            let path = path.clone();
            move || tx.send(gguf_meta(&path)).unwrap()
        });
        let got = rx.recv_timeout(std::time::Duration::from_secs(10)).expect("gguf_meta did not return");
        let _ = fs::remove_dir_all(&dir);
        got
    }

    #[test]
    fn reads_gguf_header_and_skips_non_chat() {
        let dir = std::env::temp_dir().join("localai-detect-test");
        let _ = fs::remove_dir_all(&dir);
        let models = dir.join("models").join("sub");
        fs::create_dir_all(&models).unwrap();
        fs::write(models.join("Qwen3.6-35B-A3B-UD-IQ4_XS.gguf"), gguf("qwen3moe", "Qwen3.6-35B-A3B")).unwrap();
        fs::write(models.join("flux-2-klein-4b-Q8_0.gguf"), gguf("flux", "")).unwrap();
        fs::write(models.join("mmproj-F16.gguf"), gguf("clip", "")).unwrap();
        fs::write(models.join("big-00002-of-00003.gguf"), gguf("llama", "x")).unwrap();
        fs::write(models.join("not-really.gguf"), b"nope").unwrap();
        fs::write(models.join("my-lora.gguf"), gguf_with(&[("general.type", "adapter")], "llama", "lora")).unwrap();
        fs::write(models.join("bge-small.gguf"), embedding_gguf("llama")).unwrap();
        assert_eq!(gguf_meta(&models.join("Qwen3.6-35B-A3B-UD-IQ4_XS.gguf")), Some(("qwen3moe".into(), "Qwen3.6-35B-A3B".into())));
        let found: Vec<_> = models_in(&dir);
        assert_eq!(found, ["qwen3.6-35b-a3b-ud-iq4_xs.gguf"]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn skips_adapters_and_embedding_models() {
        assert_eq!(meta_of(&gguf_with(&[("general.type", "model")], "llama", "ok")), Some(("llama".into(), "ok".into())));
        for kind in ["adapter", "mmproj", "imatrix", ""] {
            assert_eq!(meta_of(&gguf_with(&[("general.type", kind)], "llama", "x")), None, "{kind}");
        }
        assert_eq!(meta_of(&embedding_gguf("qwen3")), None);
        assert_eq!(meta_of(&embedding_gguf("nomic-bert")), None);
    }

    #[test]
    fn header_strings_are_cleaned() {
        let long = "x".repeat(200);
        let got = meta_of(&gguf("lla\nma", &format!("  Bad\r\nName\t\u{7}\u{85}Here  {long}"))).unwrap();
        assert_eq!(got.0, "llama");
        assert!(got.1.starts_with("BadNameHere  xxx"), "{:?}", got.1);
        assert_eq!(got.1.chars().count(), 80);
        // a title like that must not break the apps.toml entry built from it
        let found = Found {
            server: Some(PathBuf::from("C:\\l\\llama-server.exe")),
            models: vec![Model { path: PathBuf::from("D:\\AI\\a.gguf"), file: "a.gguf".into(), arch: got.0, title: got.1 }],
            ..Default::default()
        };
        let (out, _) = merge("", &found, |_| String::new()).unwrap().unwrap();
        assert_eq!(config::parse(&out).unwrap().len(), 1);
    }

    #[test]
    fn malformed_headers_return_none_fast() {
        let kv = |value: &dyn Fn(&mut Vec<u8>)| {
            let mut b = header(2);
            put_str(&mut b, "x.y");
            value(&mut b);
            put_str(&mut b, "general.architecture");
            b.extend(8u32.to_le_bytes());
            put_str(&mut b, "llama");
            b
        };
        let bad_len = |len: u64| move |b: &mut Vec<u8>| {
            b.extend(8u32.to_le_bytes());
            b.extend(len.to_le_bytes());
            b.extend([0u8; 64]);
        };
        // lengths that go negative as i64 (they once seeked backwards forever), or pass the file's end
        assert_eq!(meta_of(&kv(&bad_len(u64::MAX))), None);
        assert_eq!(meta_of(&kv(&bad_len(1 << 63))), None);
        assert_eq!(meta_of(&kv(&bad_len(1 << 40))), None);
        let array = |ty: u32, n: u64| move |b: &mut Vec<u8>| {
            b.extend(9u32.to_le_bytes());
            b.extend(ty.to_le_bytes());
            b.extend(n.to_le_bytes());
            b.extend([0u8; 64]);
        };
        assert_eq!(meta_of(&kv(&array(8, u64::MAX))), None, "giant array of strings");
        assert_eq!(meta_of(&kv(&array(9, 10_000_001))), None, "giant array of arrays");
        assert_eq!(meta_of(&kv(&array(4, 1 << 62))), None, "byte size overflows u64");
        assert_eq!(meta_of(&kv(&array(4, 1 << 61))), None, "byte size overflows i64");
        assert_eq!(meta_of(&kv(&array(4, 1 << 40))), None, "bigger than the file");
        assert_eq!(meta_of(&kv(&array(99, 1))), None, "unknown element type");
        // arrays may nest twice, not three times
        let nested = |levels: usize| move |b: &mut Vec<u8>| {
            b.extend(9u32.to_le_bytes());
            for _ in 1..levels {
                b.extend(9u32.to_le_bytes());
                b.extend(1u64.to_le_bytes());
            }
            b.extend(4u32.to_le_bytes());
            b.extend(2u64.to_le_bytes());
            b.extend([0u8; 8]);
        };
        assert_eq!(meta_of(&kv(&nested(1))), Some(("llama".into(), String::new())));
        assert_eq!(meta_of(&kv(&nested(2))), Some(("llama".into(), String::new())));
        assert_eq!(meta_of(&kv(&nested(3))), None);
        assert_eq!(meta_of(&kv(&nested(400))), None);
    }

    fn models_in(home: &Path) -> Vec<String> {
        models(home).into_iter().filter(|m| m.path.starts_with(home)).map(|m| m.file).collect()
    }

    fn model(file: &str) -> Model {
        Model { path: PathBuf::from(format!("D:\\AI\\{file}")), file: file.to_lowercase(), arch: "qwen3".into(), title: "Some Model".into() }
    }

    #[test]
    fn merge_adds_new_models_once() {
        let old = "[[app]]\nid='qwen36'\nname='Q'\ngroup='gpu'\nchat=true\ncmd=['cmd.exe','/c','D:\\x\\start-qwen36.bat']\nport=8090\nhealth='/health'\n";
        let found = Found {
            server: Some(PathBuf::from("D:\\LocalAI\\llama.cpp\\llama-server.exe")),
            models: vec![model("Qwen3.6-35B-A3B-UD-Q4_K_XL.gguf"), model("Huihui-Qwen3.6-35B-A3B-abliterated.IQ4_XS.gguf"), model("Ornith-1.5-35B-Q4_K_M.gguf")],
            ollama: Some((PathBuf::from("C:\\O\\ollama.exe"), vec!["translategemma:12b".into()])),
            odysseus: Some(PathBuf::from("C:\\Odysseus")),
        };
        let bat = |p: &str| if p.ends_with("start-qwen36.bat") { "-m \"D:\\x\\Qwen3.6-35B-A3B-UD-Q4_K_XL.gguf\"".to_string() } else { String::new() };
        let (out, names) = merge(old, &found, bat).unwrap().unwrap();
        assert_eq!(names, ["Qwen3.6 Uncensored", "Some Model", "Ollama", "Odysseus"]);
        let apps = config::parse(&out).unwrap();
        let abl = &apps[1];
        assert_eq!((abl.port, abl.chat), (Some(8091), true));
        assert!(abl.cmd.ends_with(&["--reasoning-budget".into(), "0".into()]));
        assert_eq!(abl.cmd[4], "huihui-qwen3.6-35b-a3b-abliterated.iq4-xs");
        assert_eq!(apps[2].port, Some(8092));
        assert_eq!(apps[3].port, Some(11434));
        assert_eq!(apps[4].cmd[0], "C:\\Odysseus\\venv\\Scripts\\python.exe");
        assert!(out.starts_with(old) && out.contains(MARK));
        // A second scan finds nothing new, and commenting an entry out keeps it hidden.
        assert!(merge(&out, &found, bat).unwrap().is_none());
        let start = out.find("[[app]]\nid = \"ornith").unwrap();
        let end = start + out[start..].find("\n\n").unwrap();
        let block: String = out[start..end].lines().map(|l| format!("# {l}\n")).collect();
        let hidden = format!("{}{block}{}", &out[..start], &out[end + 1..]);
        assert_eq!(config::parse(&hidden).unwrap().len(), apps.len() - 1);
        assert!(merge(&hidden, &found, bat).unwrap().is_none());
    }

    #[test]
    fn mentions_needs_a_whole_file_name() {
        assert!(mentions("-m \"d:\\models\\a.gguf\"", "a.gguf"));
        assert!(mentions("a.gguf", "a.gguf"));
        assert!(!mentions("-m \"d:\\models\\big-a.gguf\"", "a.gguf"));
    }

    #[test]
    fn empty_file_gets_header() {
        let found = Found { server: Some(PathBuf::from("C:\\l\\llama-server.exe")), models: vec![model("a.gguf")], ..Default::default() };
        let (out, _) = merge("", &found, |_| String::new()).unwrap().unwrap();
        assert!(out.starts_with(HEADER));
        assert_eq!(config::parse(&out).unwrap()[0].id, "a");
    }

    #[test]
    fn appends_after_a_file_saved_without_its_final_newline() {
        let found = Found { server: Some(PathBuf::from("C:\\l\\llama-server.exe")), models: vec![model("a.gguf")], ..Default::default() };
        let (first, _) = merge("", &found, |_| String::new()).unwrap().unwrap();
        let more = Found { models: vec![model("a.gguf"), model("b.gguf")], ..found };
        // the header is already there; the last line has no newline
        let (out, names) = merge(first.trim_end(), &more, |_| String::new()).unwrap().unwrap();
        assert_eq!(names, ["Some Model"]);
        assert_eq!(config::parse(&out).unwrap().len(), 2);
    }

    #[test]
    fn unreadable_apps_toml_is_never_overwritten() {
        let dir = std::env::temp_dir().join("localai-update-test");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join("apps.toml");
        // what Notepad's "Unicode" encoding writes: not UTF-8, so reading fails
        let mut utf16 = vec![0xff, 0xfe];
        utf16.extend("[[app]]\n".encode_utf16().flat_map(u16::to_le_bytes));
        fs::write(&p, &utf16).unwrap();
        let found = Found { server: Some(PathBuf::from("C:\\l\\llama-server.exe")), models: vec![model("a.gguf")], ..Default::default() };
        assert!(update(&p, &found).unwrap_err().contains("cannot read"));
        assert_eq!(fs::read(&p).unwrap(), utf16);
        assert!(!dir.join("apps.toml.tmp").exists() && !dir.join("apps.toml.bak").exists());
        // A missing file is simply created; an existing one is replaced and its old text kept as .bak.
        fs::remove_file(&p).unwrap();
        assert_eq!(update(&p, &found).unwrap(), ["Some Model"]);
        let first = fs::read_to_string(&p).unwrap();
        let more = Found { models: vec![model("a.gguf"), model("b.gguf")], ..found };
        update(&p, &more).unwrap();
        assert_eq!(fs::read_to_string(dir.join("apps.toml.bak")).unwrap(), first);
        assert_eq!(config::parse(&fs::read_to_string(&p).unwrap()).unwrap().len(), 2);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn odysseus_is_not_added_again_under_another_id_or_port() {
        let found = Found { odysseus: Some(PathBuf::from("C:\\Odysseus")), ..Default::default() };
        let same_folder = "[[app]]\nid='ody'\nname='O'\ngroup='svc'\ncmd=['x.exe']\ncwd='c:/ODYSSEUS/'\nport=7001\nhealth='tcp'\n";
        assert!(merge(same_folder, &found, |_| String::new()).unwrap().is_none());
        let port_taken = "[[app]]\nid='web'\nname='W'\ngroup='svc'\ncmd=['x.exe']\nport=7000\nhealth='tcp'\n";
        assert!(merge(port_taken, &found, |_| String::new()).unwrap().is_none());
        assert!(merge("", &found, |_| String::new()).unwrap().is_some());
    }

    #[test]
    fn relative_bat_is_read_from_the_apps_cwd() {
        let toml = "[[app]]\nid='a'\nname='A'\ngroup='gpu'\nchat=true\ncmd=['cmd.exe','/c','run.bat']\ncwd='D:\\LocalAI'\nport=8084\nhealth='/health'\n";
        let found = Found { server: Some(PathBuf::from("C:\\l\\llama-server.exe")), models: vec![model("a.gguf")], ..Default::default() };
        let asked = std::cell::RefCell::new(Vec::new());
        let bat = |p: &str| {
            asked.borrow_mut().push(p.to_string());
            "-m \"D:\\AI\\a.gguf\"".to_string()
        };
        assert!(merge(toml, &found, bat).unwrap().is_none(), "the model is named in run.bat");
        assert_eq!(*asked.borrow(), ["D:\\LocalAI\\run.bat"]);
    }

    #[test]
    fn owner_check_accepts_a_folder_made_by_this_user() {
        let dir = std::env::temp_dir().join("localai-owner-test");
        fs::create_dir_all(&dir).unwrap();
        assert!(trusted(&dir));
        let _ = fs::remove_dir_all(&dir);
    }
}
