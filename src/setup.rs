//! First-run installer (PLAN.md, "one-click install"): llama.cpp + Qwen3.6-35B-A3B into the folder
//! LocalAI.exe sits in. Every download is pinned by URL, size and SHA-256, and resumes after a drop.

use sha2::{Digest, Sha256};
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::Command;

struct Asset {
    url: &'static str,
    file: &'static str,
    size: u64,
    sha256: &'static str,
}

const LLAMA_CUDA: &[Asset] = &[
    Asset {
        url: "https://github.com/ggml-org/llama.cpp/releases/download/b11719/llama-b11719-bin-win-cuda-12.4-x64.zip",
        file: "llama-b11719-bin-win-cuda-12.4-x64.zip",
        size: 267483221,
        sha256: "3a7c399487a315cad888cd5394f1e144993ec25676148930cf2069a6c2fd6360",
    },
    Asset {
        url: "https://github.com/ggml-org/llama.cpp/releases/download/b11719/cudart-llama-bin-win-cuda-12.4-x64.zip",
        file: "cudart-llama-bin-win-cuda-12.4-x64.zip",
        size: 391443627,
        sha256: "8c79a9b226de4b3cacfd1f83d24f962d0773be79f1e7b75c6af4ded7e32ae1d6",
    },
];

const LLAMA_VULKAN: &[Asset] = &[Asset {
    url: "https://github.com/ggml-org/llama.cpp/releases/download/b11719/llama-b11719-bin-win-vulkan-x64.zip",
    file: "llama-b11719-bin-win-vulkan-x64.zip",
    size: 32306255,
    sha256: "08dde7f4133443ac629431c35f0435e267ca73251fc002cb95e10625424e2963",
}];

const MODEL: Asset = Asset {
    url: "https://huggingface.co/unsloth/Qwen3.6-35B-A3B-GGUF/resolve/main/Qwen3.6-35B-A3B-UD-Q4_K_XL.gguf",
    file: "Qwen3.6-35B-A3B-UD-Q4_K_XL.gguf",
    size: 22360456160,
    sha256: "707a55a8a4397ecde44de0c499d3e68c1ad1d240d1da65826b4949d1043f4450",
};

const GB: f64 = 1024.0 * 1024.0 * 1024.0;

/// Runs the installer in its own console window (the GUI exe has none), then closes it.
/// Returns false when setup failed and the manager should not open.
pub fn run_in_console() -> bool {
    unsafe {
        windows_sys::Win32::System::Console::AllocConsole();
    }
    let release_console = use_own_console();
    let dir =std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf)).unwrap_or_else(|| PathBuf::from("."));
    println!("LocalAI setup: installing into {}\n(move LocalAI.exe to another folder first to install there)\n", dir.display());
    let ok = match run(&dir) {
        Ok(()) => {
            println!("\nDone. Opening LocalAI...");
            true
        }
        Err(e) => {
            println!("\nSetup failed: {e}\nRun LocalAI.exe again to resume. Press Enter to close.");
            let _ = io::stdin().read_line(&mut String::new());
            false
        }
    };
    release_console();
    unsafe {
        windows_sys::Win32::System::Console::FreeConsole();
    }
    ok
}

/// Points stdin/stdout/stderr (and so curl's progress bar) at the new console. Handles inherited
/// from a parent, such as LocalAI-Setup.exe, would otherwise get the output and leave it blank.
/// Returns the undo: while those handles stay open, FreeConsole leaves the console window up.
fn use_own_console() -> impl FnOnce() {
    use windows_sys::Win32::Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING};
    use windows_sys::Win32::System::Console::{GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, SetStdHandle};
    let ids = [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE];
    let before = ids.map(|id| unsafe { GetStdHandle(id) });
    let open = |name: &str| {
        let w: Vec<u16> = name.encode_utf16().chain([0]).collect();
        unsafe { CreateFileW(w.as_ptr(), GENERIC_READ | GENERIC_WRITE, FILE_SHARE_READ | FILE_SHARE_WRITE, std::ptr::null(), OPEN_EXISTING, 0, std::ptr::null_mut()) }
    };
    let (out, inp) = (open("CONOUT$"), open("CONIN$"));
    unsafe {
        if out != INVALID_HANDLE_VALUE {
            SetStdHandle(STD_OUTPUT_HANDLE, out);
            SetStdHandle(STD_ERROR_HANDLE, out);
        }
        if inp != INVALID_HANDLE_VALUE {
            SetStdHandle(STD_INPUT_HANDLE, inp);
        }
    }
    move || unsafe {
        for (id, h) in ids.into_iter().zip(before) {
            SetStdHandle(id, h);
        }
        for h in [out, inp] {
            if h != INVALID_HANDLE_VALUE {
                CloseHandle(h);
            }
        }
    }
}

pub fn run(dir: &Path) -> Result<(), String> {
    let llama = dir.join("llama.cpp");
    let downloads = dir.join("_downloads");
    let model = dir.join("models").join(MODEL.file);
    // TODO: an existing llama.cpp folder is trusted as-is; delete it to reinstall a different build.
    let runtime: &[Asset] = if llama.join("llama-server.exe").exists() { &[] } else { pick_backend() };
    // Models already on this PC (LM Studio, D:\AI, ...) make the 22 GB Qwen3.6 download unnecessary;
    // detect.rs adds them to apps.toml when the manager opens. A small test model does not count.
    let others: Vec<String> = crate::detect::models(dir).into_iter().filter(|m| m.file != MODEL.file.to_lowercase() && len(&m.path) >= 1 << 30).map(|m| m.title).collect();
    if !others.is_empty() {
        println!("Found {} local model(s), skipping the Qwen3.6 download: {}", others.len(), others.join(", "));
    }
    let model_job = others.is_empty().then(|| (&MODEL, model.clone()));

    let jobs: Vec<(&Asset, PathBuf)> = runtime.iter().map(|a| (a, downloads.join(a.file))).chain(model_job).collect();
    check_space(dir, &jobs)?;

    for (a, path) in &jobs[..runtime.len()] {
        fetch(a, path)?;
        extract(path, &llama)?;
        let _ = fs::remove_file(path);
    }
    if others.is_empty() {
        fetch(&MODEL, &model)?;
        write_bat(dir)?;
        register(dir)?;
    } else {
        let added = crate::detect::run(&dir.join("apps.toml"))?;
        println!("added to apps.toml: {}", added.join(", "));
    }
    if let Err(e) = shortcut(dir) {
        println!("warning: desktop shortcut not created: {e}");
    }
    Ok(())
}

/// NVML loads only with an NVIDIA driver present; everything else gets the Vulkan build.
fn pick_backend() -> &'static [Asset] {
    if nvml_wrapper::Nvml::init().is_ok() {
        println!("GPU: NVIDIA, using the CUDA 12.4 build of llama.cpp");
        LLAMA_CUDA
    } else {
        println!("GPU: no NVIDIA driver found, using the Vulkan build of llama.cpp");
        LLAMA_VULKAN
    }
}

fn check_space(dir: &Path, jobs: &[(&Asset, PathBuf)]) -> Result<(), String> {
    let missing: u64 = jobs.iter().map(|(a, p)| if p.exists() { 0 } else { a.size - len(&part(p)).min(a.size) }).sum();
    // Zips are extracted next to themselves, so they need room twice; 2 GB margin on top.
    let zips: u64 = jobs.iter().filter(|(a, _)| a.file.ends_with(".zip")).map(|(a, _)| a.size).sum();
    let need = missing + zips + 2 * 1024 * 1024 * 1024;
    let abs = std::path::absolute(dir).map_err(|e| e.to_string())?;
    let disks = sysinfo::Disks::new_with_refreshed_list();
    let disk = disks.iter().filter(|d| abs.starts_with(d.mount_point())).max_by_key(|d| d.mount_point().as_os_str().len());
    match disk {
        Some(d) if d.available_space() < need => Err(format!(
            "need {:.1} GB free on {}, have {:.1} GB",
            need as f64 / GB,
            d.mount_point().display(),
            d.available_space() as f64 / GB
        )),
        _ => Ok(()),
    }
}

fn part(path: &Path) -> PathBuf {
    let mut p = path.as_os_str().to_owned();
    p.push(".part");
    PathBuf::from(p)
}

fn len(path: &Path) -> u64 {
    fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

/// Downloads `a` to `dest`: resumable into `<dest>.part`, verified, then renamed. An existing `dest`
/// is verified and never overwritten.
fn fetch(a: &Asset, dest: &Path) -> Result<(), String> {
    if dest.exists() {
        println!("{}: already present, verifying", a.file);
        return verify(a, dest).map_err(|e| format!("{e}; delete {} and run setup again", dest.display()));
    }
    if let Some(d) = dest.parent() {
        fs::create_dir_all(d).map_err(|e| format!("cannot create {}: {e}", d.display()))?;
    }
    let tmp = part(dest);
    if len(&tmp) < a.size {
        println!("{}: downloading {:.1} GB", a.file, a.size as f64 / GB);
        let status = Command::new(system32("curl.exe"))
            .args(["-L", "-C", "-", "--fail", "--retry", "5", "--retry-delay", "10", "--progress-bar", "-o"])
            .arg(&tmp)
            .arg(a.url)
            .status()
            .map_err(|e| format!("cannot run curl.exe: {e}"))?;
        if !status.success() {
            return Err(format!("{}: download failed ({status})", a.file));
        }
    }
    if let Err(e) = verify(a, &tmp) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    fs::rename(&tmp, dest).map_err(|e| format!("cannot rename {}: {e}", tmp.display()))
}

fn verify(a: &Asset, path: &Path) -> Result<(), String> {
    let got = len(path);
    if got != a.size {
        return Err(format!("{}: size {got}, expected {}", a.file, a.size));
    }
    println!("{}: checking SHA-256", a.file);
    let hash = sha256_file(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    if hash != a.sha256 {
        return Err(format!("{}: sha256 mismatch (got {hash})", a.file));
    }
    Ok(())
}

fn sha256_file(path: &Path) -> io::Result<String> {
    let mut f = fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// Windows' own tar (bsdtar) reads zips; a Git-for-Windows tar earlier on PATH does not, hence the full path.
fn extract(zip: &Path, to: &Path) -> Result<(), String> {
    fs::create_dir_all(to).map_err(|e| format!("cannot create {}: {e}", to.display()))?;
    println!("extracting {}", zip.display());
    let status = Command::new(system32("tar.exe")).arg("-xf").arg(zip).arg("-C").arg(to).status().map_err(|e| format!("cannot run tar.exe: {e}"))?;
    if status.success() { Ok(()) } else { Err(format!("extracting {} failed ({status})", zip.display())) }
}

pub(crate) fn windows_dir() -> PathBuf {
    std::env::var_os("SystemRoot").map_or_else(|| PathBuf::from("C:\\Windows"), PathBuf::from)
}

/// Windows tools by full path: a bare name is also searched for in the current folder first.
pub(crate) fn system32(exe: &str) -> PathBuf {
    windows_dir().join("System32").join(exe)
}

pub(crate) const POWERSHELL: &str = "WindowsPowerShell\\v1.0\\powershell.exe";

/// Kept if it already exists, so hand-tuned flags survive a re-run.
fn write_bat(dir: &Path) -> Result<(), String> {
    let path = dir.join("start-qwen36.bat");
    if path.exists() {
        return Ok(());
    }
    let d = dir.display();
    let text = format!(
        "@echo off\r\n\
rem Qwen3.6-35B-A3B, written by LocalAI setup. No -ngl / --n-cpu-moe: --fit (default on) places layers and MoE experts to fit free VRAM.\r\n\
rem Samplers: Qwen3.6 model card, thinking mode, general tasks.\r\n\
\"{d}\\llama.cpp\\llama-server.exe\" ^\r\n\
\x20 -m \"{d}\\models\\{model}\" ^\r\n\
\x20 -a qwen3.6-35b-a3b ^\r\n\
\x20 --host 127.0.0.1 --port 8084 ^\r\n\
\x20 -c 65536 -fa on -ctk q8_0 -ctv q8_0 ^\r\n\
\x20 --temp 1.0 --top-p 0.95 --top-k 20 --min-p 0 --presence-penalty 1.5\r\n",
        model = MODEL.file
    );
    fs::write(&path, text).map_err(|e| format!("cannot write {}: {e}", path.display()))
}

pub(crate) fn toml_str(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

fn entry(dir: &Path) -> String {
    let d = dir.display().to_string();
    let bat = dir.join("start-qwen36.bat").display().to_string();
    format!(
        "[[app]]\nid = \"qwen36\"\nname = \"Qwen3.6\"\nsub = \"Qwen3.6 35B-A3B · UD-Q4_K_XL · chat\"\ngroup = \"gpu\"\nchat = true\n\
cmd = [\"cmd.exe\", \"/c\", {}]\ncwd = {}\nport = 8084\nhealth = \"/health\"\nurl = \"http://127.0.0.1:8084\"\n\
note = \"Thinks before it answers. Installed by LocalAI setup; flags live in start-qwen36.bat.\"\n\n",
        toml_str(&bat),
        toml_str(&d)
    )
}

/// apps.toml with the qwen36 entry first (the Chat tab defaults to the first chat app), or None if
/// it is already there. Comments and every other entry are kept as written.
fn with_entry(existing: Option<&str>, dir: &Path) -> Result<Option<String>, String> {
    let header = "# LocalAI Manager app list. group: \"gpu\" (one at a time), \"svc\" (service), \"app\" (launch only).\n# health: HTTP path that returns 200 when ready, or \"tcp\".\n\n";
    let text = match existing {
        Some(t) if crate::config::parse(t)?.iter().any(|a| a.id == "qwen36") => return Ok(None),
        Some(t) => match t.find("[[app]]") {
            Some(i) => format!("{}{}{}", &t[..i], entry(dir), &t[i..]),
            None => format!("{t}\n{}", entry(dir)),
        },
        None => format!("{header}{}", entry(dir)),
    };
    crate::config::parse(&text)?;
    Ok(Some(text))
}

fn register(dir: &Path) -> Result<(), String> {
    let path = dir.join("apps.toml");
    let existing = crate::config::read(&path)?;
    if let Some(text) = with_entry(existing.as_deref(), dir).map_err(|e| format!("apps.toml: {e}"))? {
        crate::config::save(&path, &text)?;
        println!("added Qwen3.6 to {}", path.display());
    }
    Ok(())
}

/// Paths go in through environment variables so no quoting can break the PowerShell command.
fn shortcut(dir: &Path) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let out = Command::new(system32(POWERSHELL))
        .args(["-NoProfile", "-NonInteractive", "-Command", "$s=(New-Object -ComObject WScript.Shell).CreateShortcut([Environment]::GetFolderPath('Desktop')+'\\LocalAI.lnk');$s.TargetPath=$env:LAI_EXE;$s.WorkingDirectory=$env:LAI_DIR;$s.Save()"])
        .env("LAI_EXE", &exe)
        .env("LAI_DIR", dir)
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() { Ok(()) } else { Err(String::from_utf8_lossy(&out.stderr).trim().to_string()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entry_goes_first_and_parses() {
        let dir = Path::new("E:\\My \"AI\"");
        let fresh = with_entry(None, dir).unwrap().unwrap();
        let apps = crate::config::parse(&fresh).unwrap();
        assert_eq!(apps[0].cmd[2], "E:\\My \"AI\"\\start-qwen36.bat");

        let old = "# keep me\n[[app]]\nid='x'\nname='X'\ngroup='app'\ncmd=['x.exe']\n";
        let merged = with_entry(Some(old), dir).unwrap().unwrap();
        assert!(merged.starts_with("# keep me\n[[app]]\nid = \"qwen36\""));
        assert_eq!(crate::config::parse(&merged).unwrap().len(), 2);
        assert_eq!(with_entry(Some(&merged), dir).unwrap(), None);
        assert!(with_entry(Some("[[app]]\nbad"), dir).is_err());
    }

    #[test]
    fn bat_keeps_indented_continuations() {
        let dir = std::env::temp_dir().join("localai-bat-test");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        write_bat(&dir).unwrap();
        let bat = fs::read_to_string(dir.join("start-qwen36.bat")).unwrap();
        assert!(bat.contains("llama-server.exe\" ^\r\n  -m \""));
        assert!(bat.ends_with("--presence-penalty 1.5\r\n"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn sha256_matches_known_vector() {
        let p = std::env::temp_dir().join("localai-sha-test.txt");
        fs::write(&p, "abc").unwrap();
        assert_eq!(sha256_file(&p).unwrap(), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        let _ = fs::remove_file(&p);
    }
}
