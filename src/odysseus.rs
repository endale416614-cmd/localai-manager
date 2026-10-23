//! Connects every local model server in apps.toml to Odysseus. Each one becomes a row in Odysseus'
//! model_endpoints table, inserted with plain sqlite3 by Odysseus' venv Python (it has dotenv and
//! knows where the database is). Odysseus' own code is not imported on purpose: that would run its
//! init_db() (create_all + migrations) in a second process next to the live server.
//! Rows that already exist (same base URL) are left alone.

use crate::supervisor::{self, Shared};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Mutex;
use std::thread;

pub const ID: &str = "odysseus";

/// Printed instead of a count when there is no database to write to yet.
const NOT_SET_UP: &str = "not-set-up";

/// Prints how many rows it added. Names stay out of the output: after the commit the console code
/// page used to mangle non-ASCII ones. Never creates a database (mode=rw) and only touches model_endpoints.
const SCRIPT: &str = r#"
import json, os, sqlite3, sys, uuid
from datetime import datetime, timezone
from pathlib import Path
try:
    from dotenv import load_dotenv
    load_dotenv(encoding="utf-8-sig")
except ImportError:
    pass
dburl = os.environ.get("DATABASE_URL")
if dburl and not dburl.startswith("sqlite:///"):
    print("DATABASE_URL is not sqlite:///<path>, so no model was connected")
    sys.exit(0)
path = dburl[len("sqlite:///"):] if dburl else os.path.join(os.environ.get("ODYSSEUS_DATA_DIR", "data"), "app.db")
if not os.path.isfile(path):
    print("not-set-up")
    sys.exit(0)
db = sqlite3.connect(Path(os.path.abspath(path)).as_uri() + "?mode=rw", uri=True, timeout=30)
try:
    if not db.execute("select 1 from sqlite_master where type = 'table' and name = 'model_endpoints'").fetchone():
        print("not-set-up")
        sys.exit(0)
    norm = lambda u: (u or "").rstrip("/").replace("://localhost:", "://127.0.0.1:")
    seen = {norm(r[0]) for r in db.execute("select base_url from model_endpoints")}
    now = datetime.now(timezone.utc).strftime("%Y-%m-%d %H:%M:%S.%f")
    added = 0
    for name, url in json.loads(os.environ["LAI_ENDPOINTS"]):
        if norm(url) not in seen:
            db.execute(
                "insert into model_endpoints (id, name, base_url, is_enabled, model_type, endpoint_kind, model_refresh_mode, created_at, updated_at) values (?, ?, ?, ?, ?, ?, ?, ?, ?)",
                (uuid.uuid4().hex[:8], name, url, 1, "llm", "local", "auto", now, now),
            )
            seen.add(norm(url))
            added += 1
    db.commit()
    print(added)
finally:
    db.close()
"#;

/// The (name, base URL) pairs Odysseus should know: chat apps (llama-server) and Ollama.
fn endpoints(sup: &supervisor::Supervisor) -> Vec<(String, String)> {
    sup.slots
        .iter()
        .filter(|s| s.def.chat || s.def.id == "ollama")
        .filter_map(|s| Some((format!("{} (LocalAI)", s.def.name), format!("http://127.0.0.1:{}/v1", s.def.port?))))
        .collect()
}

/// Runs on a worker thread (~1.5 s of Python start-up); the outcome lands in Odysseus' log.
pub fn link(shared: &Shared) {
    let job = {
        let sup = shared.lock().unwrap();
        sup.slots.iter().position(|s| s.def.id == ID).and_then(|i| {
            let def = &sup.slots[i].def;
            let cwd = def.cwd.clone()?;
            // apps.toml runs Odysseus with its venv Python; fall back to the venv launch-windows.ps1 builds.
            let python = if def.cmd[0].to_lowercase().ends_with("python.exe") { PathBuf::from(&def.cmd[0]) } else { cwd.join("venv\\Scripts\\python.exe") };
            Some((i, python, cwd, endpoints(&sup)))
        })
    };
    let Some((i, python, cwd, list)) = job else { return };
    if list.is_empty() {
        return;
    }
    let shared = shared.clone();
    thread::spawn(move || {
        // The startup link and the one on Ready can overlap; two runs would both insert the same rows.
        static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());
        let _turn = ONE_AT_A_TIME.lock();
        let line = match run_script(&python, &cwd, &list) {
            Ok(o) if o.status.success() => match String::from_utf8_lossy(&o.stdout).trim() {
                NOT_SET_UP => "Odysseus is not set up yet: start it once".to_string(),
                "0" => format!("Odysseus: all {} local models already connected", list.len()),
                n if n.parse::<u32>().is_ok() => format!("Odysseus: connected {n} of {} local models", list.len()),
                msg => format!("Odysseus: {msg}"),
            },
            Ok(o) => {
                let err = String::from_utf8_lossy(&o.stderr);
                let why = err.lines().rev().find(|l| l.contains("Error")).unwrap_or("unknown error").trim().to_string();
                format!("Odysseus: could not connect models ({why})")
            }
            Err(e) => format!("Odysseus: cannot run {}: {e}", python.display()),
        };
        supervisor::note(&shared, i, line);
    });
}

/// UTF-8 mode keeps non-ASCII text intact on the pipes. The script ends by itself: sqlite waits at
/// most 30 s for a lock.
fn run_script(python: &Path, cwd: &Path, list: &[(String, String)]) -> std::io::Result<Output> {
    Command::new(python)
        .args(["-c", SCRIPT])
        .current_dir(cwd)
        .env("LAI_ENDPOINTS", serde_json::to_string(list).unwrap_or_default())
        .env("PYTHONUTF8", "1")
        .stdin(Stdio::null())
        .creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW)
        .output()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sqlite(python: &Path, db: &Path, sql: &str) {
        let code = "import sqlite3, sys; c = sqlite3.connect(sys.argv[1]); c.executescript(sys.argv[2]); c.close()";
        assert!(Command::new(python).args(["-c", code]).arg(db).arg(sql).status().unwrap().success());
    }

    /// Runs the real script with any Python that has sqlite3 (skipped when there is none).
    #[test]
    fn script_adds_each_url_once_and_never_creates_a_database() {
        let python = Path::new("python.exe");
        if !Command::new(python).args(["-c", "import sqlite3"]).output().is_ok_and(|o| o.status.success()) {
            eprintln!("skipped: no Python with sqlite3 on PATH");
            return;
        }
        let dir = std::env::temp_dir().join("localai-odysseus-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("data")).unwrap();
        let db = dir.join("data").join("app.db");
        let ep = |n: &str, u: &str| (n.to_string(), u.to_string());
        let run = |list: &[(String, String)]| {
            let o = run_script(python, &dir, list).unwrap();
            assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
            String::from_utf8_lossy(&o.stdout).trim().to_string()
        };

        let list = [ep("A", "http://127.0.0.1:8090/v1"), ep("B ünï", "http://localhost:8084/v1"), ep("C", "http://127.0.0.1:8090/v1/")];
        assert_eq!(run(&list), NOT_SET_UP);
        assert!(!db.exists(), "no database may be created");
        sqlite(python, &db, "create table other (x)");
        assert_eq!(run(&list), NOT_SET_UP);
        sqlite(
            python,
            &db,
            "create table model_endpoints (id varchar primary key, name varchar not null, base_url varchar not null, is_enabled boolean, model_type varchar, \
             endpoint_kind varchar, model_refresh_mode varchar, created_at datetime not null, updated_at datetime not null); \
             insert into model_endpoints (id, name, base_url, created_at, updated_at) values ('x', 'old', 'http://127.0.0.1:8084/v1/', 't', 't')",
        );
        // 8084 is there already (as localhost, with a slash), and C is A again.
        assert_eq!(run(&list), "1");
        assert_eq!(run(&list), "0");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
