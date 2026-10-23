//! LocalAI-Setup.exe: one double-click installs or updates LocalAI Manager, then opens it.
//! The manager's own first run then downloads llama.cpp (and Qwen3.6 when no local model is found).
//!
//! Where it installs, first match wins:
//! 1. `--dir <folder>`;
//! 2. the folder the existing Desktop\LocalAI.lnk points into, if it starts a `localai*.exe` that has
//!    an apps.toml beside it (an update keeps apps.toml and models);
//! 3. `<fixed drive with the most free space>:\LocalAI` (models need 20+ GB each).

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

static MANAGER: &[u8] = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../target/release/localai-manager.exe"));

fn main() {
    println!("LocalAI setup\n");
    // A warning, a note or an old version still running is easy to miss if the window closes at once.
    let mut notes = false;
    let ok = match install(&mut notes) {
        Ok(msg) => {
            println!("\n{msg}");
            true
        }
        Err(e) => {
            println!("\nSetup failed: {e}");
            false
        }
    };
    if !ok || notes || std::env::args().any(|a| a == "--wait") {
        println!("Press Enter to close.");
        let _ = std::io::stdin().read(&mut [0u8]);
    }
    if !ok {
        std::process::exit(1);
    }
}

fn install(notes: &mut bool) -> Result<String, String> {
    let exe = target()?;
    let dir = exe.parent().ok_or("bad install path")?.to_path_buf();
    println!("Installing to {}", exe.display());
    fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;

    // The new exe is written beside the old one first: a full disk or a failed write must not leave
    // the folder without a working LocalAI.exe. A running exe cannot be overwritten but can be
    // renamed. The .old copy stays as a backup of the previous version until the next update.
    let (old, new) = (with_suffix(&exe, ".old"), with_suffix(&exe, ".new"));
    fs::write(&new, MANAGER).map_err(|e| format!("cannot write {}: {e}", new.display()))?;
    let mut running = false;
    let replaced = exe.exists();
    if replaced {
        if old.exists() {
            fs::remove_file(&old).map_err(|_| format!("{} is still running from the last update; quit LocalAI and run setup again", old.display()))?;
        }
        fs::rename(&exe, &old).map_err(|e| format!("cannot replace {}: {e}", exe.display()))?;
        // Windows refuses write access to a running exe.
        running = fs::OpenOptions::new().write(true).open(&old).is_err();
    }
    if let Err(e) = fs::rename(&new, &exe) {
        if replaced {
            let _ = fs::rename(&old, &exe);
        }
        return Err(format!("cannot install {}: {e}", exe.display()));
    }
    println!("Wrote {} ({:.1} MB)", exe.display(), MANAGER.len() as f64 / 1e6);

    for (folder, label) in [("Desktop", "desktop"), ("Programs", "Start menu")] {
        match shortcut(folder, &exe, &dir) {
            Ok(()) => println!("Added the LocalAI shortcut to the {label}"),
            Err(e) => {
                println!("warning: no {label} shortcut: {e}");
                *notes = true;
            }
        }
    }
    if !webview2_installed() {
        println!(
            "note: the WebView2 Runtime is missing, so Odysseus will open in your browser instead of inside LocalAI.\n      Get it from https://go.microsoft.com/fwlink/p/?LinkId=2124703"
        );
        *notes = true;
    }

    if running {
        *notes = true;
        return Ok("LocalAI is running the old version. Quit it and open it again to use the new one.".into());
    }
    let fresh = !dir.join("apps.toml").exists();
    // No inherited handles: the manager's first-run console and the GUI must not hold this console.
    Command::new(&exe)
        .current_dir(&dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("cannot start {}: {e}", exe.display()))?;
    Ok(if fresh { "Done. LocalAI is starting; first it downloads llama.cpp (and Qwen3.6 if no local model is found) in its own window." } else { "Done. LocalAI is starting." }.into())
}

/// The manager exe to write, from the rules at the top of this file.
fn target() -> Result<PathBuf, String> {
    let args: Vec<String> = std::env::args().collect();
    if let Some(i) = args.iter().position(|a| a == "--dir") {
        let dir = args.get(i + 1).ok_or("--dir needs a folder")?;
        return Ok(std::path::absolute(dir).map_err(|e| e.to_string())?.join("LocalAI.exe"));
    }
    if let Some(existing) = shortcut_target() {
        return Ok(existing);
    }
    let drive = biggest_drive().ok_or("no fixed drive found")?;
    Ok(drive.join("LocalAI").join("LocalAI.exe"))
}

fn with_suffix(p: &Path, s: &str) -> PathBuf {
    let mut o = p.as_os_str().to_owned();
    o.push(s);
    PathBuf::from(o)
}

/// Windows tools by full path: a bare name is searched for in the installer's own folder first, and
/// that is often Downloads.
fn system32(exe: &str) -> PathBuf {
    let root = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
    Path::new(&root).join("System32").join(exe)
}

/// Paths reach PowerShell through environment variables, so no quoting can break the command.
fn powershell(script: &str, env: &[(&str, &Path)]) -> Result<String, String> {
    let mut c = Command::new(system32("WindowsPowerShell\\v1.0\\powershell.exe"));
    c.args(["-NoProfile", "-NonInteractive", "-Command", script]);
    for (k, v) in env {
        c.env(k, v);
    }
    let out = c.output().map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

fn shortcut(folder: &str, exe: &Path, dir: &Path) -> Result<(), String> {
    let script = format!(
        "$s=(New-Object -ComObject WScript.Shell).CreateShortcut([Environment]::GetFolderPath('{folder}')+'\\LocalAI.lnk');$s.TargetPath=$env:LAI_EXE;$s.WorkingDirectory=$env:LAI_DIR;$s.Save()"
    );
    powershell(&script, &[("LAI_EXE", exe), ("LAI_DIR", dir)]).map(|_| ())
}

/// The exe an existing Desktop\LocalAI.lnk starts, if it is a LocalAI install (see `is_install`).
/// UTF-8 output keeps a path with non-ASCII letters intact.
fn shortcut_target() -> Option<PathBuf> {
    let script = "[Console]::OutputEncoding=[Text.Encoding]::UTF8;$p=[Environment]::GetFolderPath('Desktop')+'\\LocalAI.lnk';if(Test-Path $p){(New-Object -ComObject WScript.Shell).CreateShortcut($p).TargetPath}";
    let t = PathBuf::from(powershell(script, &[]).ok()?);
    is_install(&t).then_some(t)
}

/// A `localai*.exe` that exists with an apps.toml beside it. Any other shortcut that happens to be
/// called LocalAI.lnk is not ours, and an update must not write over what it points to.
fn is_install(exe: &Path) -> bool {
    let name = exe.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
    name.starts_with("localai") && name.ends_with(".exe") && exe.is_file() && exe.with_file_name("apps.toml").is_file()
}

fn biggest_drive() -> Option<PathBuf> {
    let out = powershell("Get-CimInstance Win32_LogicalDisk -Filter 'DriveType=3' | ForEach-Object { \"$($_.FreeSpace) $($_.DeviceID)\" }", &[]).ok()?;
    out.lines()
        .filter_map(|l| {
            let (free, id) = l.trim().split_once(' ')?;
            Some((free.parse::<u64>().ok()?, id.to_string()))
        })
        .max()
        .map(|(_, id)| PathBuf::from(format!("{id}\\")))
}

fn webview2_installed() -> bool {
    let key = r"SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}";
    [format!(r"HKLM\{key}"), format!(r"HKCU\{}", key.replace(r"WOW6432Node\", ""))]
        .iter()
        .any(|k| Command::new(system32("reg.exe")).args(["query", k, "/v", "pv"]).output().is_ok_and(|o| o.status.success()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embeds_the_manager() {
        assert_eq!(&MANAGER[..2], b"MZ");
        assert!(MANAGER.len() > 1_000_000);
    }

    #[test]
    fn old_copy_name() {
        assert_eq!(with_suffix(Path::new("D:\\LocalAI\\LocalAI.exe"), ".old"), PathBuf::from("D:\\LocalAI\\LocalAI.exe.old"));
    }

    #[test]
    fn new_copy_name() {
        assert_eq!(with_suffix(Path::new("D:\\LocalAI\\LocalAI.exe"), ".new"), PathBuf::from("D:\\LocalAI\\LocalAI.exe.new"));
    }

    #[test]
    fn only_a_localai_exe_with_apps_toml_counts_as_an_install() {
        let dir = std::env::temp_dir().join("localai-setup-test");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        for f in ["LocalAI.exe", "localai-manager.exe", "notepad.exe", "LocalAI.bat"] {
            fs::write(dir.join(f), b"MZ").unwrap();
        }
        assert!(!is_install(&dir.join("LocalAI.exe")), "no apps.toml beside it yet");
        fs::write(dir.join("apps.toml"), b"").unwrap();
        assert!(is_install(&dir.join("LocalAI.exe")));
        assert!(is_install(&dir.join("localai-manager.exe")));
        assert!(!is_install(&dir.join("notepad.exe")));
        assert!(!is_install(&dir.join("LocalAI.bat")));
        assert!(!is_install(&dir.join("LocalAI-gone.exe")));
        assert!(!is_install(Path::new("")));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn windows_tools_come_from_system32() {
        assert!(system32("reg.exe").is_absolute() && system32("reg.exe").ends_with("System32\\reg.exe"));
    }
}
