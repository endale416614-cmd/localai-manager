use serde::Deserialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

#[derive(Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Group {
    Gpu,
    Svc,
    App,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct AppDef {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub sub: String,
    pub group: Group,
    pub cmd: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub port: Option<u16>,
    pub health: Option<String>,
    pub url: Option<String>,
    #[serde(default)]
    pub stat: String,
    #[serde(default)]
    pub vram: String,
    #[serde(default)]
    pub ram: String,
    #[serde(default)]
    pub note: String,
    /// Serves the OpenAI-compatible /v1/chat/completions API, so the Chat tab can talk to it.
    #[serde(default)]
    pub chat: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    #[serde(rename = "app", default)]
    apps: Vec<AppDef>,
}

/// apps.toml next to the exe (an absolute path), else in the current directory (a relative one).
pub fn locate() -> PathBuf {
    let beside_exe = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("apps.toml")));
    match beside_exe {
        Some(p) if p.exists() => p,
        _ => PathBuf::from("apps.toml"),
    }
}

/// `name` in the exe's folder (an absolute path); relative to the current directory if the exe's folder is unknown.
pub fn beside_exe(name: &str) -> PathBuf {
    std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join(name))).unwrap_or_else(|| PathBuf::from(name))
}

pub fn load(path: &Path) -> Result<Vec<AppDef>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    parse(&text).map_err(|e| format!("{}: {e}", path.display()))
}

/// The file's text, or None when there is no such file. Any other read error stays an error, so a
/// file that could not be read (UTF-16, locked) is never mistaken for an empty one and overwritten.
pub fn read(path: &Path) -> Result<Option<String>, String> {
    match std::fs::read_to_string(path) {
        Ok(t) => Ok(Some(t)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("cannot read {}: {e}", path.display())),
    }
}

/// Writes `<path>.tmp`, keeps the file it replaces as `<path>.bak`, then renames the tmp over it:
/// a crash mid-write never leaves a half-written apps.toml, and the last version survives a bad merge.
pub fn save(path: &Path, text: &str) -> Result<(), String> {
    let with = |ext: &str| {
        let mut p = path.as_os_str().to_owned();
        p.push(ext);
        PathBuf::from(p)
    };
    let (tmp, bak) = (with(".tmp"), with(".bak"));
    std::fs::write(&tmp, text).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    if path.exists() {
        std::fs::copy(path, &bak).map_err(|e| format!("cannot back up {}: {e}", path.display()))?;
    }
    std::fs::rename(&tmp, path).map_err(|e| format!("cannot replace {}: {e}", path.display()))
}

pub fn parse(text: &str) -> Result<Vec<AppDef>, String> {
    let file: File = toml::from_str(text).map_err(|e| e.to_string())?;
    let mut ids = HashSet::new();
    for a in &file.apps {
        if !ids.insert(a.id.as_str()) {
            return Err(format!("duplicate app id \"{}\"", a.id));
        }
        if a.cmd.is_empty() {
            return Err(format!("app \"{}\": cmd is empty", a.id));
        }
        if a.group != Group::App && (a.port.is_none() || a.health.is_none()) {
            return Err(format!("app \"{}\": gpu/svc apps need port and health", a.id));
        }
        if a.chat && a.port.is_none() {
            return Err(format!("app \"{}\": chat apps need a port", a.id));
        }
        if let Some(h) = &a.health {
            if h != "tcp" && !h.starts_with('/') {
                return Err(format!("app \"{}\": health must be \"tcp\" or a path starting with /", a.id));
            }
        }
        if let Some(u) = &a.url {
            if !u.starts_with("http://127.0.0.1:") && !u.starts_with("http://localhost:") {
                return Err(format!("app \"{}\": url must point at 127.0.0.1 or localhost", a.id));
            }
        }
    }
    Ok(file.apps)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shipped_apps_toml_parses() {
        let apps = parse(include_str!("../apps.toml")).unwrap();
        assert_eq!(apps.len(), 11);
        assert_eq!(apps.iter().filter(|a| a.group == Group::Gpu).count(), 6);
        assert_eq!(apps.iter().find(|a| a.chat).map(|a| a.id.as_str()), Some("qwen36"));
        // The Create tab looks for these two by id: the ComfyUI app and the voice trainer.
        for id in [crate::create::ID, crate::train::VOICE_ID] {
            assert!(apps.iter().any(|a| a.id == id && a.url.is_some() && a.port.is_some()), "{id}");
        }
    }

    #[test]
    fn rejects_bad_configs() {
        let base = "[[app]]\nid='a'\nname='A'\ngroup='svc'\ncmd=['x']\nport=1\nhealth='/h'\n";
        assert!(parse(base).is_ok());
        assert!(parse(&format!("{base}{base}")).unwrap_err().contains("duplicate"));
        assert!(parse(&base.replace("health='/h'\n", "")).is_err());
        assert!(parse(&base.replace("'/h'", "'h'")).is_err());
        assert!(parse(&format!("{base}url='http://example.com'\n")).is_err());
        assert!(parse(&format!("{base}typo=1\n")).is_err());
    }

    #[test]
    fn save_keeps_a_backup_and_read_tells_missing_from_unreadable() {
        let dir = std::env::temp_dir().join("localai-config-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("apps.toml");
        assert_eq!(read(&p), Ok(None));
        save(&p, "one").unwrap();
        assert!(!dir.join("apps.toml.bak").exists(), "nothing to back up the first time");
        save(&p, "two").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "two");
        assert_eq!(std::fs::read_to_string(dir.join("apps.toml.bak")).unwrap(), "one");
        assert!(!dir.join("apps.toml.tmp").exists());
        std::fs::write(&p, [0xff, 0xfe, b'x', 0]).unwrap();
        assert!(read(&p).unwrap_err().contains("cannot read"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
