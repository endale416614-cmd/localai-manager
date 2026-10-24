//! Training data import (Create tab): copies the chosen pictures, voice clips or videos into
//! `training\<images|voice|video>\<set>\` beside the exe, on a worker thread. Nothing here trains anything:
//! picture and video sets are only kept, and a voice set is what the voice trainer app is pointed at.

use crate::names;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

/// The apps.toml id of the voice trainer; the Create tab can open it.
pub const VOICE_ID: &str = "voice";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Images,
    Voice,
    Video,
}

impl Kind {
    pub fn from_index(i: i32) -> Kind {
        match i {
            1 => Kind::Voice,
            2 => Kind::Video,
            _ => Kind::Images,
        }
    }

    fn folder(self) -> &'static str {
        match self {
            Kind::Images => "images",
            Kind::Voice => "voice",
            Kind::Video => "video",
        }
    }

    /// The file types of the kind; each has its own signature in `signature_ok`.
    fn exts(self) -> &'static [&'static str] {
        match self {
            Kind::Images => &["png", "jpg", "jpeg", "webp", "bmp"],
            Kind::Voice => &["wav", "mp3", "flac", "ogg", "m4a"],
            Kind::Video => &["mp4", "mov", "m4v", "mkv", "webm", "avi"],
        }
    }

    /// The title of the file dialog.
    pub fn title(self) -> &'static str {
        match self {
            Kind::Images => "Import images",
            Kind::Voice => "Import voice clips",
            Kind::Video => "Import videos",
        }
    }

    /// The file dialog's filter: "name\0pattern\0", ended by one more NUL.
    pub fn filter(self) -> &'static str {
        match self {
            Kind::Images => "Images (png, jpg, webp, bmp)\0*.png;*.jpg;*.jpeg;*.webp;*.bmp\0\0",
            Kind::Voice => "Voice clips (wav, mp3, flac, ogg, m4a)\0*.wav;*.mp3;*.flac;*.ogg;*.m4a\0\0",
            Kind::Video => "Videos (mp4, mov, m4v, mkv, webm, avi)\0*.mp4;*.mov;*.m4v;*.mkv;*.webm;*.avi\0\0",
        }
    }

    /// (what the result line calls the set, why a file was skipped)
    fn words(self) -> (&'static str, &'static str) {
        match self {
            Kind::Images => ("Image set", "not an image"),
            Kind::Voice => ("Voice set", "not audio"),
            Kind::Video => ("Video set", "not a video"),
        }
    }
}

/// Where the sets go: `training` beside the exe.
pub fn root() -> PathBuf {
    crate::config::beside_exe("training")
}

/// The first bytes of a file say what it is, whatever its name says: `ext` must be a type of its kind
/// and the file must start the way that type starts.
fn accepts(kind: Kind, ext: &str, head: &[u8]) -> bool {
    kind.exts().contains(&ext) && signature_ok(ext, head)
}

fn signature_ok(ext: &str, head: &[u8]) -> bool {
    // RIFF, a size, then the form: WEBP, WAVE or "AVI ".
    let riff = |form: &[u8; 4]| head.len() >= 12 && &head[..4] == b"RIFF" && &head[8..12] == form;
    match ext {
        "png" => head.starts_with(&[0x89, b'P', b'N', b'G']),
        "jpg" | "jpeg" => head.starts_with(&[0xFF, 0xD8, 0xFF]),
        "webp" => riff(b"WEBP"),
        "bmp" => head.starts_with(b"BM"),
        "wav" => riff(b"WAVE"),
        // A tagger may put an ID3v2 tag in front of the FLAC stream.
        "flac" => head.starts_with(b"fLaC") || head.starts_with(b"ID3"),
        "ogg" => head.starts_with(b"OggS"),
        // An ID3 tag, or the 11 bits of frame sync (FF, then E0..FF).
        "mp3" => head.starts_with(b"ID3") || (head.len() >= 2 && head[0] == 0xFF && head[1] & 0xE0 == 0xE0),
        // A size, then the type of the first box.
        "mp4" | "m4v" | "m4a" => head.get(4..8).is_some_and(|atom| atom == b"ftyp"),
        // QuickTime files often start with another atom than ftyp.
        "mov" => head.get(4..8).is_some_and(|atom| [b"ftyp", b"moov", b"mdat", b"wide", b"free", b"skip", b"pnot"].iter().any(|a| *a == atom)),
        "mkv" | "webm" => head.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]),
        "avi" => riff(b"AVI "),
        _ => false,
    }
}

/// The set's folder name: what was typed, else the name of the folder the files came from, else `set-YYYY-MM-DD`.
/// Each is cleaned first (see `names::sanitize`), so one that is no use (a device name, only dots) is passed over.
fn set_name(typed: &str, from: Option<&str>, (year, month, day): (u16, u16, u16)) -> String {
    names::sanitize(typed).or_else(|| from.and_then(names::sanitize)).unwrap_or_else(|| format!("set-{year:04}-{month:02}-{day:02}"))
}

fn today() -> (u16, u16, u16) {
    use windows_sys::Win32::Foundation::SYSTEMTIME;
    use windows_sys::Win32::System::SystemInformation::GetLocalTime;
    let mut t: SYSTEMTIME = unsafe { std::mem::zeroed() };
    unsafe { GetLocalTime(&mut t) };
    (t.wYear, t.wMonth, t.wDay)
}

pub struct Report {
    kind: Kind,
    set: String,
    pub added: usize,
    skipped: usize,
    existing: usize,
    /// Files whose name is taken in the set by a different file, and the first such name.
    taken: usize,
    first_taken: String,
    pub failed: usize,
    /// Why the first file that failed did.
    problem: String,
    /// (files done, files asked for) when the import was cancelled.
    cancelled: Option<(usize, usize)>,
    /// The set's folder, if it exists after the import.
    pub folder: Option<PathBuf>,
}

impl Report {
    /// For example "Voice set 'Alice': 14 added, 2 skipped (not audio), 1 already there."
    pub fn line(&self) -> String {
        let (label, why) = self.kind.words();
        let mut parts = vec![format!("{} added", self.added)];
        if self.skipped > 0 {
            parts.push(format!("{} skipped ({why})", self.skipped));
        }
        if self.existing > 0 {
            parts.push(format!("{} already there", self.existing));
        }
        if self.taken > 0 {
            let more = if self.taken > 1 { format!(" and {} more", self.taken - 1) } else { String::new() };
            parts.push(format!("{} not copied: the name is taken by a different file ({}{more})", self.taken, self.first_taken));
        }
        if self.failed > 0 {
            parts.push(format!("{} could not be copied ({})", self.failed, self.problem));
        }
        let line = format!("{label} '{}': {}.", self.set, parts.join(", "));
        match self.cancelled {
            Some((done, all)) => format!("Cancelled after {done} of {all}. {line}"),
            None => line,
        }
    }

    /// The colour of the line (see `Train::code`): green only when everything asked for was added.
    fn code(&self) -> i32 {
        if self.failed > 0 {
            4
        } else if self.added > 0 && self.taken == 0 && self.cancelled.is_none() {
            2
        } else {
            1
        }
    }
}

enum Outcome {
    Added,
    Skipped,
    Existing,
    /// A different file has this name in the set already.
    Taken(String),
    Failed(String),
    Cancelled,
}

/// Copies `files` into `root\<kind>\<set>\`; `progress` gets (the file about to be copied, how many there are).
/// A file of the wrong type or with the wrong signature is skipped; one whose name is in the set already is left
/// alone. Once `cancel` is set the import stops, between files or within one, and removes the copy it was making.
pub fn import(kind: Kind, files: &[PathBuf], typed: &str, root: &Path, cancel: &AtomicBool, mut progress: impl FnMut(usize, usize)) -> Report {
    let from = files.first().and_then(|f| f.parent()).and_then(|p| p.file_name()).and_then(|n| n.to_str());
    let set = set_name(typed, from, today());
    let dir = root.join(kind.folder()).join(&set);
    let mut r = Report { kind, set, added: 0, skipped: 0, existing: 0, taken: 0, first_taken: String::new(), failed: 0, problem: String::new(), cancelled: None, folder: None };
    remove_leftovers(&dir);
    for (i, src) in files.iter().enumerate() {
        let outcome = if cancel.load(Ordering::Relaxed) {
            Outcome::Cancelled
        } else {
            progress(i + 1, files.len());
            copy_one(kind, src, &dir, cancel)
        };
        match outcome {
            Outcome::Added => r.added += 1,
            Outcome::Skipped => r.skipped += 1,
            Outcome::Existing => r.existing += 1,
            Outcome::Taken(name) => {
                r.taken += 1;
                if r.first_taken.is_empty() {
                    r.first_taken = name;
                }
            }
            Outcome::Failed(why) => {
                r.failed += 1;
                if r.problem.is_empty() {
                    r.problem = why;
                }
            }
            Outcome::Cancelled => {
                r.cancelled = Some((i, files.len()));
                break;
            }
        }
    }
    r.folder = dir.is_dir().then_some(dir);
    r
}

/// A copy in progress is named after its file plus `.localai-part`. Only an import makes that suffix, so such a file
/// is a copy an earlier import left behind (the app was closed in the middle of it); a user's own `.part` file stays.
fn remove_leftovers(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        if e.path().extension().is_some_and(|x| x.eq_ignore_ascii_case("localai-part")) && e.file_type().is_ok_and(|t| t.is_file()) {
            let _ = fs::remove_file(e.path());
        }
    }
}

fn copy_one(kind: Kind, src: &Path, dir: &Path, cancel: &AtomicBool) -> Outcome {
    // Only the file's own name goes into the set, whatever the path looks like.
    let Some(name) = src.file_name() else { return Outcome::Failed(format!("{} is not a file", src.display())) };
    let name_text = name.to_string_lossy();
    // Before anything is opened: opening a device name opens the device.
    if let Some(why) = names::copy_problem(name) {
        return Outcome::Failed(format!("{name_text}: {why}"));
    }
    let ext = src.extension().and_then(|e| e.to_str()).unwrap_or_default().to_ascii_lowercase();
    let mut from = match File::open(src) {
        Ok(f) => f,
        Err(e) => return Outcome::Failed(format!("{name_text}: {e}")),
    };
    let mut head = Vec::with_capacity(16);
    if let Err(e) = (&mut from).take(16).read_to_end(&mut head) {
        return Outcome::Failed(format!("{name_text}: {e}"));
    }
    if !accepts(kind, &ext, &head) {
        return Outcome::Skipped;
    }
    let size = match from.metadata() {
        Ok(m) => m.len(),
        Err(e) => return Outcome::Failed(format!("{name_text}: {e}")),
    };
    if let Err(e) = fs::create_dir_all(dir) {
        return Outcome::Failed(format!("the folder {} cannot be made: {e}", dir.display()));
    }
    let dest = dir.join(name);
    // Whatever is in the set under this name stays: the same file, or another one that took the name.
    if fs::symlink_metadata(&dest).is_ok() {
        return clash(&dest, size, &name_text);
    }
    // The copy gets its own name first and the file's only when it is whole, so a copy cut short never looks like the file.
    let mut part = dest.clone().into_os_string();
    part.push(".localai-part");
    let part = PathBuf::from(part);
    let mut to = match OpenOptions::new().write(true).create_new(true).open(&part) {
        Ok(f) => f,
        Err(e) => return Outcome::Failed(format!("{name_text} cannot be written: {e}")),
    };
    let copied = from.rewind().and_then(|_| copy_all(&mut from, &mut to, cancel));
    drop(to);
    match copied {
        Ok(true) => {}
        Ok(false) => {
            let _ = fs::remove_file(&part); // the copy this call was making
            return Outcome::Cancelled;
        }
        Err(e) => {
            let _ = fs::remove_file(&part);
            return Outcome::Failed(format!("{name_text}: {e}"));
        }
    }
    match publish(&part, &dest) {
        Ok(()) => Outcome::Added,
        Err(e) => {
            let _ = fs::remove_file(&part);
            // Something made the name in the meantime.
            if e.kind() == io::ErrorKind::AlreadyExists { clash(&dest, size, &name_text) } else { Outcome::Failed(format!("{name_text}: {e}")) }
        }
    }
}

/// A file of that name is in the set. Of the same size it is taken for the same file: nothing to do.
fn clash(dest: &Path, size: u64, name: &str) -> Outcome {
    match fs::metadata(dest) {
        Ok(m) if m.is_file() && m.len() == size => Outcome::Existing,
        Ok(_) => Outcome::Taken(name.to_string()),
        Err(e) => Outcome::Failed(format!("{name}: {e}")),
    }
}

/// Gives `from` the name `to`, and fails with AlreadyExists when that name is in use: `fs::rename` would replace it.
fn publish(from: &Path, to: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::MoveFileExW;
    let wide = |p: &Path| p.as_os_str().encode_wide().chain([0]).collect::<Vec<u16>>();
    // No MOVEFILE_REPLACE_EXISTING.
    match unsafe { MoveFileExW(wide(from).as_ptr(), wide(to).as_ptr(), 0) } {
        0 => Err(io::Error::last_os_error()),
        _ => Ok(()),
    }
}

/// Videos run to gigabytes: copy in big pieces, and look at `cancel` before each. False when it was set.
fn copy_all(from: &mut File, to: &mut File, cancel: &AtomicBool) -> io::Result<bool> {
    let mut buf = vec![0u8; 1 << 20];
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Ok(false);
        }
        match from.read(&mut buf)? {
            0 => return Ok(true),
            n => to.write_all(&buf[..n])?,
        }
    }
}

#[derive(Default)]
pub struct Train {
    pub busy: bool,
    /// "Importing 12 of 40…" while busy, else the result, or what went wrong.
    pub text: String,
    /// The colour of the line: 1 amber, 2 green (all that was asked for was added), 4 red.
    pub code: i32,
    /// The folder of the last set that was imported into.
    pub folder: Option<PathBuf>,
    /// Set by `cancel`, and looked at by the worker.
    cancel: Arc<AtomicBool>,
}

pub type Shared = Arc<Mutex<Train>>;

/// Imports on a worker thread; the UI polls the shared state.
pub fn start(train: &Shared, kind: Kind, files: Vec<PathBuf>, typed: String, root: PathBuf) {
    let mut t = train.lock().unwrap();
    if t.busy || files.is_empty() {
        return;
    }
    let cancel = Arc::new(AtomicBool::new(false));
    *t = Train { busy: true, text: format!("Importing 0 of {}…", files.len()), code: 1, folder: None, cancel: cancel.clone() };
    drop(t);
    let train = train.clone();
    thread::spawn(move || {
        let r = import(kind, &files, &typed, &root, &cancel, |i, n| train.lock().unwrap().text = format!("Importing {i} of {n}…"));
        *train.lock().unwrap() = Train { busy: false, text: r.line(), code: r.code(), folder: r.folder, ..Default::default() };
    });
}

/// Asks the running import to stop. It removes the file it was copying, then reports how far it got.
pub fn cancel(train: &Shared) {
    let mut t = train.lock().unwrap();
    if t.busy {
        t.cancel.store(true, Ordering::Relaxed);
        t.text = "Cancelling…".into();
    }
}

/// Shows why nothing was imported (the file dialog said no).
pub fn fail(train: &Shared, why: String) {
    let mut t = train.lock().unwrap();
    if !t.busy {
        *t = Train { busy: false, text: why, code: 4, folder: t.folder.take(), ..Default::default() };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PNG: &[u8] = &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 0x0D];
    const JPEG: &[u8] = &[0xFF, 0xD8, 0xFF, 0xE0, 0, 0x10, b'J', b'F', b'I', b'F'];
    const WAV: &[u8] = b"RIFF\x24\x08\0\0WAVEfmt ";

    fn riff(form: &[u8; 4]) -> Vec<u8> {
        [b"RIFF".as_slice(), &[0x24, 0x08, 0, 0], form, b"data"].concat()
    }

    fn ftyp(brand: &[u8; 4]) -> Vec<u8> {
        [&[0, 0, 0, 0x20][..], b"ftyp", brand].concat()
    }

    /// The start of a QuickTime/MP4 file whose first box is `kind`.
    fn atom(kind: &[u8; 4]) -> Vec<u8> {
        [&[0, 0, 0x03, 0x2A][..], kind, b"\0\0\0\0\0\0\0\0"].concat()
    }

    /// An import with nobody cancelling it.
    fn run(kind: Kind, files: &[PathBuf], typed: &str, root: &Path) -> Report {
        import(kind, files, typed, root, &AtomicBool::new(false), |_, _| {})
    }

    /// An empty folder of its own under the temp folder.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("localai-train-test-{name}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn signatures_of_every_type_true_and_false() {
        use Kind::*;
        let mp3_frame = [0xFF, 0xFB, 0x90, 0x44];
        let yes: Vec<(Kind, &str, Vec<u8>)> = vec![
            (Images, "png", PNG.to_vec()),
            (Images, "jpg", JPEG.to_vec()),
            (Images, "jpeg", JPEG.to_vec()),
            (Images, "webp", riff(b"WEBP")),
            (Images, "bmp", b"BM\x36\0\x10\0".to_vec()),
            (Voice, "wav", WAV.to_vec()),
            (Voice, "flac", b"fLaC\0\0\0\"".to_vec()),
            // A tagger's ID3v2 tag in front of the stream.
            (Voice, "flac", b"ID3\x04\0\0\0\0\x1F\x76".to_vec()),
            (Voice, "ogg", b"OggS\0\x02\0\0".to_vec()),
            (Voice, "mp3", b"ID3\x04\0\0\0\0".to_vec()),
            (Voice, "mp3", mp3_frame.to_vec()),
            (Voice, "mp3", vec![0xFF, 0xE3, 0x18]),
            (Voice, "m4a", ftyp(b"M4A ")),
            (Video, "mp4", ftyp(b"isom")),
            (Video, "mov", ftyp(b"qt  ")),
            // QuickTime may open with any of its atoms.
            (Video, "mov", atom(b"moov")),
            (Video, "mov", atom(b"mdat")),
            (Video, "mov", atom(b"wide")),
            (Video, "mov", atom(b"free")),
            (Video, "mov", atom(b"skip")),
            (Video, "mov", atom(b"pnot")),
            (Video, "m4v", ftyp(b"M4V ")),
            (Video, "mkv", vec![0x1A, 0x45, 0xDF, 0xA3, 0x9F]),
            (Video, "webm", vec![0x1A, 0x45, 0xDF, 0xA3, 0x93]),
            (Video, "avi", riff(b"AVI ")),
        ];
        for (kind, ext, head) in &yes {
            assert!(accepts(*kind, ext, head), "{kind:?} .{ext} should pass");
        }
        let no: Vec<(Kind, &str, Vec<u8>)> = vec![
            (Images, "png", JPEG.to_vec()),
            (Images, "jpg", PNG.to_vec()),
            (Images, "png", b"not a picture, just words".to_vec()),
            (Images, "webp", riff(b"WAVE")),
            (Images, "bmp", b"B".to_vec()),
            (Images, "png", vec![]),
            (Images, "gif", b"GIF89a....".to_vec()),
            (Images, "png", PNG[..3].to_vec()),
            (Voice, "wav", riff(b"AVI ")),
            (Voice, "wav", riff(b"WEBP")),
            (Voice, "wav", b"RIFF\0\0\0\0WAV".to_vec()),
            (Voice, "flac", b"fLa".to_vec()),
            (Voice, "flac", b"ID".to_vec()),
            (Voice, "flac", b"OggS\0\x02\0\0".to_vec()),
            (Voice, "ogg", b"Ogg".to_vec()),
            (Voice, "ogg", b"ID3\x04\0\0\0\0".to_vec()),
            (Voice, "mp3", vec![0xFF, 0x00, 0x11]),
            (Voice, "mp3", vec![0xFF]),
            (Voice, "mp3", vec![0x00, 0xFB]),
            (Voice, "m4a", vec![0, 0, 0, 0x20, b'f', b't', b'y']),
            (Voice, "m4a", b"ftyp....".to_vec()),
            (Voice, "mp3", ftyp(b"M4A ")),
            (Video, "mp4", b"....ftyx".to_vec()),
            // Only QuickTime is as loose about its first atom; an MP4 starts with its ftyp.
            (Video, "mp4", atom(b"moov")),
            (Video, "m4v", atom(b"mdat")),
            (Voice, "m4a", atom(b"free")),
            (Video, "mov", b"....junk....".to_vec()),
            (Video, "mov", atom(b"MOOV")),
            (Video, "mov", b"\0\0\0\x08moo".to_vec()),
            (Video, "mkv", vec![0x1A, 0x45, 0xDF]),
            (Video, "avi", riff(b"WAVE")),
            (Video, "mp4", vec![]),
            // A type that is not of the kind never passes, whatever the file holds.
            (Images, "wav", WAV.to_vec()),
            (Voice, "png", PNG.to_vec()),
            (Voice, "mp4", ftyp(b"isom")),
            (Video, "m4a", ftyp(b"M4A ")),
            (Video, "png", PNG.to_vec()),
            (Images, "mp4", ftyp(b"isom")),
            (Images, "", PNG.to_vec()),
        ];
        for (kind, ext, head) in &no {
            assert!(!accepts(*kind, ext, head), "{kind:?} .{ext} {head:?} should fail");
        }
        // The extension in the file name is matched in lower case by the importer; here it is exact.
        assert!(!accepts(Images, "PNG", PNG));
    }

    #[test]
    fn filters_list_the_types_of_the_kind() {
        for kind in [Kind::Images, Kind::Voice, Kind::Video] {
            let pattern = kind.filter().split('\0').nth(1).unwrap();
            let listed: Vec<&str> = pattern.split(';').map(|p| p.strip_prefix("*.").unwrap()).collect();
            assert_eq!(listed, kind.exts(), "{kind:?}");
            assert!(kind.filter().ends_with("\0\0"));
        }
    }

    #[test]
    fn set_names_come_from_the_field_then_the_folder_then_the_date() {
        let day = (2026, 10, 4);
        assert_eq!(set_name("Alice", Some("Recordings"), day), "Alice");
        assert_eq!(set_name("", Some("Holiday photos"), day), "Holiday photos");
        assert_eq!(set_name("   ", Some("Holiday photos"), day), "Holiday photos");
        // What is no use is passed over: a device name, only dots, only characters Windows forbids.
        assert_eq!(set_name("CON", Some("Holiday"), day), "Holiday");
        assert_eq!(set_name("..", Some("Holiday"), day), "Holiday");
        assert_eq!(set_name("", Some("NUL"), day), "set-2026-10-04");
        assert_eq!(set_name("", None, day), "set-2026-10-04");
        assert_eq!(set_name("<>:?*", Some("..."), (2026, 1, 31)), "set-2026-01-31");
        assert_eq!(set_name("com1", None, day), "set-2026-10-04");
        // A path typed into the field becomes one folder name.
        assert_eq!(set_name("..\\..\\Windows\\System32", None, day), "WindowsSystem32");
        assert_eq!(set_name("C:\\Users\\someone", None, day), "CUserssomeone");
        let (y, m, d) = today();
        assert!((2024..3000).contains(&y) && (1..=12).contains(&m) && (1..=31).contains(&d), "{y}-{m}-{d}");
    }

    #[test]
    fn the_result_line_reads_like_the_example() {
        let report = |kind, added, skipped, existing, failed| Report {
            kind,
            set: "Alice".into(),
            added,
            skipped,
            existing,
            taken: 0,
            first_taken: String::new(),
            failed,
            problem: "disk full".into(),
            cancelled: None,
            folder: None,
        };
        let r = |kind, added, skipped, existing, failed| report(kind, added, skipped, existing, failed).line();
        assert_eq!(r(Kind::Voice, 14, 2, 1, 0), "Voice set 'Alice': 14 added, 2 skipped (not audio), 1 already there.");
        assert_eq!(r(Kind::Images, 3, 0, 0, 0), "Image set 'Alice': 3 added.");
        assert_eq!(r(Kind::Video, 0, 1, 0, 2), "Video set 'Alice': 0 added, 1 skipped (not a video), 2 could not be copied (disk full).");
        assert_eq!(r(Kind::Images, 0, 4, 0, 0), "Image set 'Alice': 0 added, 4 skipped (not an image).");
        // A name taken by a different file is counted on its own, with the first such name.
        let taken = |n| Report { taken: n, first_taken: "Recording.m4a".into(), ..report(Kind::Voice, 14, 0, 0, 0) };
        assert_eq!(taken(1).line(), "Voice set 'Alice': 14 added, 1 not copied: the name is taken by a different file (Recording.m4a).");
        assert_eq!(taken(3).line(), "Voice set 'Alice': 14 added, 3 not copied: the name is taken by a different file (Recording.m4a and 2 more).");
        // A cancelled import says how far it got, then what is in the set.
        let cut = Report { cancelled: Some((3, 40)), ..report(Kind::Voice, 3, 0, 0, 0) };
        assert_eq!(cut.line(), "Cancelled after 3 of 40. Voice set 'Alice': 3 added.");
        // Green only when everything asked for was added.
        assert_eq!(report(Kind::Voice, 3, 0, 0, 0).code(), 2);
        assert_eq!(report(Kind::Voice, 3, 1, 1, 0).code(), 2);
        assert_eq!(report(Kind::Voice, 0, 4, 0, 0).code(), 1);
        assert_eq!(report(Kind::Voice, 3, 0, 0, 1).code(), 4);
        assert_eq!((taken(1).code(), cut.code()), (1, 1));
    }

    #[test]
    fn an_import_counts_skips_and_never_overwrites() {
        let root = scratch("import");
        let src = root.join("Holiday photos");
        fs::create_dir_all(&src).unwrap();
        let png = [PNG, &[7u8; 5000]].concat();
        fs::write(src.join("a.png"), &png).unwrap();
        fs::write(src.join("b.JPG"), [JPEG, &[1u8; 300]].concat()).unwrap();
        fs::write(src.join("fake.png"), "not a picture").unwrap();
        fs::write(src.join("notes.txt"), "hello").unwrap();
        fs::write(src.join("empty.png"), "").unwrap();
        let files: Vec<PathBuf> = ["a.png", "b.JPG", "fake.png", "notes.txt", "empty.png", "gone.png"].iter().map(|n| src.join(n)).collect();

        let training = root.join("training");
        let mut seen = Vec::new();
        let r = import(Kind::Images, &files, "", &training, &AtomicBool::new(false), |i, n| seen.push((i, n)));
        assert_eq!(seen, (1..=6).map(|i| (i, 6)).collect::<Vec<_>>());
        assert_eq!((r.added, r.skipped, r.existing, r.failed), (2, 3, 0, 1));
        // The file that is gone says so by name; the rest of the reason is Windows' own text.
        let gone = "1 could not be copied (gone.png: ";
        assert!(r.line().starts_with(&format!("Image set 'Holiday photos': 2 added, 3 skipped (not an image), {gone}")), "{}", r.line());
        let set = training.join("images").join("Holiday photos");
        assert_eq!(r.folder.as_deref(), Some(set.as_path()));
        assert_eq!(fs::read(set.join("a.png")).unwrap(), png);
        assert!(set.join("b.JPG").is_file());
        assert!(!set.join("fake.png").exists() && !set.join("notes.txt").exists() && !set.join("empty.png").exists());

        // The same files again: they are there (b.JPG), and what is in the set is left as it is. a.png is no longer
        // the same file, only a file of that name, which is not the same as being there.
        fs::write(set.join("a.png"), "kept").unwrap();
        let again = run(Kind::Images, &files, "", &training);
        assert_eq!((again.added, again.skipped, again.existing, again.taken, again.failed), (0, 3, 1, 1, 1));
        assert!(
            again.line().starts_with(&format!("Image set 'Holiday photos': 0 added, 3 skipped (not an image), 1 already there, 1 not copied: the name is taken by a different file (a.png), {gone}")),
            "{}",
            again.line()
        );
        assert_eq!(fs::read_to_string(set.join("a.png")).unwrap(), "kept");

        // Wrong files only: nothing is added and no folder is made.
        let none = run(Kind::Voice, &files, "Alice", &training);
        assert_eq!((none.added, none.skipped, none.failed), (0, 5, 1));
        assert!(none.line().starts_with(&format!("Voice set 'Alice': 0 added, 5 skipped (not audio), {gone}")), "{}", none.line());
        assert!(none.folder.is_none() && !training.join("voice").exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_folder_that_cannot_be_made_is_named() {
        let root = scratch("blocked");
        let src = root.join("clips");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("one.wav"), [WAV, &[0u8; 100]].concat()).unwrap();
        // A file is where the training folder should be.
        fs::write(root.join("training"), "in the way").unwrap();
        let r = run(Kind::Voice, &[src.join("one.wav")], "Alice", &root.join("training"));
        assert_eq!((r.added, r.failed), (0, 1));
        let line = r.line();
        assert!(line.starts_with("Voice set 'Alice': 0 added, 1 could not be copied (the folder ") && line.contains("cannot be made"), "{line}");
        assert!(r.folder.is_none());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_hostile_set_name_stays_inside_the_training_folder() {
        let root = scratch("inside");
        let src = root.join("clips");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("one.wav"), [WAV, &[0u8; 100]].concat()).unwrap();
        let files = [src.join("one.wav")];
        let training = root.join("training");
        for typed in ["..\\..\\evil", "../../evil", "C:\\evil", "\\\\host\\share\\evil", "evil\\..", "con", ".."] {
            let r = run(Kind::Voice, &files, typed, &training);
            let folder = r.folder.unwrap_or_else(|| panic!("{typed:?}: the clip was not added"));
            assert!(folder.starts_with(training.join("voice")) && folder.parent() == Some(training.join("voice").as_path()), "{typed:?} -> {}", folder.display());
            assert!(folder.join("one.wav").is_file());
        }
        // Nothing was made outside `training`.
        let mut outside: Vec<_> = fs::read_dir(&root).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        outside.sort();
        assert_eq!(outside, ["clips", "training"]);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_name_taken_by_a_different_file_is_told_apart_by_size() {
        let root = scratch("taken");
        let src = root.join("Recordings");
        fs::create_dir_all(&src).unwrap();
        let clip = [WAV, &[3u8; 400]].concat();
        fs::write(src.join("Recording.wav"), &clip).unwrap();
        fs::write(src.join("Take 2.wav"), &clip).unwrap();
        let files = [src.join("Recording.wav"), src.join("Take 2.wav")];
        let training = root.join("training");
        let set = training.join("voice").join("Alice");
        fs::create_dir_all(&set).unwrap();
        // Another file under the name of the first; the second is there already, whole.
        fs::write(set.join("Recording.wav"), "someone else's").unwrap();
        fs::write(set.join("Take 2.wav"), &clip).unwrap();
        let r = run(Kind::Voice, &files, "Alice", &training);
        assert_eq!((r.added, r.existing, r.taken, r.failed), (0, 1, 1, 0));
        assert_eq!(r.line(), "Voice set 'Alice': 0 added, 1 already there, 1 not copied: the name is taken by a different file (Recording.wav).");
        assert_eq!(r.code(), 1);
        assert_eq!(fs::read(set.join("Recording.wav")).unwrap(), b"someone else's");
        assert_eq!(fs::read(set.join("Take 2.wav")).unwrap(), clip);
        // A folder under that name is not the same file either.
        fs::remove_file(set.join("Take 2.wav")).unwrap();
        fs::create_dir(set.join("Take 2.wav")).unwrap();
        let r = run(Kind::Voice, &files[1..], "Alice", &training);
        assert_eq!((r.added, r.existing, r.taken), (0, 0, 1));
        assert!(set.join("Take 2.wav").is_dir());
        let mut left: Vec<_> = fs::read_dir(&set).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        left.sort();
        assert_eq!(left, ["Recording.wav", "Take 2.wav"], "no copy was left behind");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn unsafe_names_are_refused_before_anything_is_opened() {
        let root = scratch("names");
        let src = root.join("clips");
        fs::create_dir_all(&src).unwrap();
        // None of these may be opened: a device name opens the device (a read from CON waits for the keyboard).
        let files: Vec<PathBuf> = ["CON.wav", "nul.png", "a.wav:stream", "x.wav.", "x.wav "].iter().map(|n| src.join(n)).collect();
        let r = run(Kind::Voice, &files, "Alice", &root.join("training"));
        assert_eq!((r.added, r.skipped, r.failed), (0, 0, 5));
        assert!(r.line().ends_with("5 could not be copied (CON.wav: its name is a device name)."), "{}", r.line());
        assert!(r.folder.is_none());
        // A stream of another file holding a real clip: its name has a ':', so it does not go into the set.
        let stream = format!("{}:track.wav", src.join("host.txt").display());
        if fs::write(&stream, [WAV, &[0u8; 100]].concat()).is_ok() {
            let r = run(Kind::Voice, &[PathBuf::from(&stream)], "Alice", &root.join("training"));
            assert_eq!((r.added, r.failed), (0, 1));
            assert!(r.line().contains("host.txt:track.wav: its name has a ':'"), "{}", r.line());
            assert!(r.folder.is_none(), "nothing was made in the set");
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_name_with_an_unpaired_surrogate_still_copies() {
        use std::ffi::OsString;
        use std::os::windows::ffi::OsStringExt;
        let root = scratch("lone");
        let src = root.join("clips");
        fs::create_dir_all(&src).unwrap();
        let name = OsString::from_wide(&[0x61, 0xD800, u16::from(b'.'), u16::from(b'w'), u16::from(b'a'), u16::from(b'v')]);
        assert!(name.to_str().is_none());
        let clip = [WAV, &[5u8; 64]].concat();
        fs::write(src.join(&name), &clip).unwrap();
        let training = root.join("training");
        let r = run(Kind::Voice, &[src.join(&name)], "Alice", &training);
        assert_eq!((r.added, r.failed), (1, 0), "{}", r.line());
        assert_eq!(fs::read(training.join("voice").join("Alice").join(&name)).unwrap(), clip);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_file_that_cannot_be_read_is_a_failure_not_a_skip() {
        let root = scratch("unreadable");
        let src = root.join("clips");
        fs::create_dir_all(&src).unwrap();
        let file = src.join("locked.wav");
        fs::write(&file, [WAV, &[0u8; 100]].concat()).unwrap();
        // Another handle holds the whole file locked: it opens, but reading it fails.
        let holder = OpenOptions::new().read(true).write(true).open(&file).unwrap();
        holder.lock().unwrap();
        let r = run(Kind::Voice, &[file.clone()], "Alice", &root.join("training"));
        assert_eq!((r.added, r.skipped, r.failed), (0, 0, 1), "{}", r.line());
        // ERROR_LOCK_VIOLATION, from the read (Windows words the rest in the user's language).
        assert!(r.line().contains("could not be copied (locked.wav: ") && r.line().contains("(os error 33)"), "{}", r.line());
        assert!(r.folder.is_none());
        drop(holder);
        let r = run(Kind::Voice, &[file], "Alice", &root.join("training"));
        assert_eq!((r.added, r.failed), (1, 0));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn publishing_a_copy_never_replaces_a_file() {
        let root = scratch("publish");
        fs::write(root.join("a.wav.localai-part"), "new").unwrap();
        fs::write(root.join("a.wav"), "old").unwrap();
        let err = publish(&root.join("a.wav.localai-part"), &root.join("a.wav")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists, "{err}");
        assert_eq!(fs::read_to_string(root.join("a.wav")).unwrap(), "old");
        assert!(root.join("a.wav.localai-part").is_file(), "removing the copy is the caller's job");
        fs::remove_file(root.join("a.wav")).unwrap();
        publish(&root.join("a.wav.localai-part"), &root.join("a.wav")).unwrap();
        assert_eq!(fs::read_to_string(root.join("a.wav")).unwrap(), "new");
        assert!(!root.join("a.wav.localai-part").exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_copy_cut_short_leaves_no_file_and_old_leftovers_are_cleared() {
        let root = scratch("part");
        let src = root.join("clips");
        fs::create_dir_all(&src).unwrap();
        for n in ["1.wav", "2.wav", "3.wav"] {
            fs::write(src.join(n), [WAV, &vec![9u8; 3 << 20][..]].concat()).unwrap();
        }
        let files: Vec<PathBuf> = ["1.wav", "2.wav", "3.wav"].iter().map(|n| src.join(n)).collect();
        let training = root.join("training");
        let set = training.join("voice").join("Alice");
        let other = training.join("voice").join("Bob");
        // A leftover of an import that was cut short, a file of the set, a user's own .part file, and a leftover in another set.
        for dir in [&set, &other] {
            fs::create_dir_all(dir).unwrap();
        }
        fs::write(set.join("old.wav.localai-part"), "half").unwrap();
        fs::write(set.join("keep.wav"), "whole").unwrap();
        fs::write(set.join("notes.part"), "the user's").unwrap();
        fs::write(other.join("b.wav.localai-part"), "not this set's").unwrap();
        // Cancelled as the second file is about to be copied: the first is in, the second is there under no name.
        let cancel = AtomicBool::new(false);
        let r = import(Kind::Voice, &files, "Alice", &training, &cancel, |i, _| {
            if i == 2 {
                cancel.store(true, Ordering::Relaxed);
            }
        });
        assert_eq!(r.line(), "Cancelled after 1 of 3. Voice set 'Alice': 1 added.");
        assert_eq!(r.code(), 1);
        let names = |dir: &Path| {
            let mut v: Vec<_> = fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
            v.sort();
            v
        };
        assert_eq!(names(&set), ["1.wav", "keep.wav", "notes.part"]);
        assert_eq!(names(&other), ["b.wav.localai-part"]);
        assert_eq!(fs::metadata(set.join("1.wav")).unwrap().len() as usize, WAV.len() + (3 << 20));
        // A whole import leaves no copy behind either, and a cancel before the first file copies nothing.
        let r = run(Kind::Voice, &files, "Alice", &training);
        assert_eq!((r.added, r.existing, r.failed), (2, 1, 0));
        assert_eq!(names(&set), ["1.wav", "2.wav", "3.wav", "keep.wav", "notes.part"]);
        let r = import(Kind::Voice, &files, "Carol", &training, &AtomicBool::new(true), |_, _| panic!("nothing is started once cancelled"));
        assert_eq!(r.line(), "Cancelled after 0 of 3. Voice set 'Carol': 0 added.");
        assert!(r.folder.is_none());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn cancel_stops_a_running_import_and_reports_how_far_it_got() {
        let root = scratch("cancel");
        let src = root.join("Alice");
        fs::create_dir_all(&src).unwrap();
        // Many files: the worker is still busy when Cancel arrives right after the start.
        let files: Vec<PathBuf> = (0..1000)
            .map(|i| {
                let p = src.join(format!("{i:04}.wav"));
                fs::write(&p, [WAV, &[0u8; 64]].concat()).unwrap();
                p
            })
            .collect();
        let train: Shared = Arc::default();
        cancel(&train);
        assert!(train.lock().unwrap().text.is_empty(), "nothing runs, nothing to cancel");
        start(&train, Kind::Voice, files, "".into(), root.join("training"));
        cancel(&train);
        let end = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while train.lock().unwrap().busy {
            assert!(std::time::Instant::now() < end, "the import never ended");
            thread::sleep(std::time::Duration::from_millis(10));
        }
        let (text, code) = {
            let t = train.lock().unwrap();
            (t.text.clone(), t.code)
        };
        let done: usize = text.strip_prefix("Cancelled after ").and_then(|s| s.split(' ').next()).and_then(|n| n.parse().ok()).unwrap_or_else(|| panic!("{text}"));
        assert_eq!(text, format!("Cancelled after {done} of 1000. Voice set 'Alice': {done} added."));
        assert_eq!(code, 1);
        let set = root.join("training").join("voice").join("Alice");
        let on_disk: Vec<String> = fs::read_dir(&set).map(|d| d.map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect()).unwrap_or_default();
        assert_eq!(on_disk.len(), done, "what is reported is what is there");
        assert!(on_disk.iter().all(|n| !n.ends_with(".localai-part")));

        // While one runs, Cancel is a flag and a word on the screen.
        let busy: Shared = Arc::new(Mutex::new(Train { busy: true, text: "Importing 5 of 9…".into(), ..Default::default() }));
        cancel(&busy);
        let t = busy.lock().unwrap();
        assert_eq!(t.text, "Cancelling…");
        assert!(t.cancel.load(Ordering::Relaxed));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_worker_imports_and_reports_through_the_shared_state() {
        let root = scratch("worker");
        let src = root.join("Alice");
        fs::create_dir_all(&src).unwrap();
        for n in ["1.wav", "2.wav", "3.mp3"] {
            let head: &[u8] = if n.ends_with("mp3") { b"ID3\x04\0\0\0\0" } else { WAV };
            fs::write(src.join(n), [head, &[0u8; 2000]].concat()).unwrap();
        }
        fs::write(src.join("4.wav"), "text").unwrap();
        let files: Vec<PathBuf> = ["1.wav", "2.wav", "3.mp3", "4.wav"].iter().map(|n| src.join(n)).collect();
        let train: Shared = Arc::default();
        start(&train, Kind::Voice, files, "".into(), root.join("training"));
        let end = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while train.lock().unwrap().busy {
            assert!(std::time::Instant::now() < end, "the import never ended");
            thread::sleep(std::time::Duration::from_millis(10));
        }
        let t = train.lock().unwrap();
        assert_eq!(t.text, "Voice set 'Alice': 3 added, 1 skipped (not audio).");
        assert_eq!(t.code, 2);
        assert_eq!(t.folder.as_deref(), Some(root.join("training").join("voice").join("Alice").as_path()));
        drop(t);
        // Another start while idle works, and shows why a dialog failed.
        fail(&train, "Too many files".into());
        let t = train.lock().unwrap();
        assert_eq!((t.text.as_str(), t.code, t.busy), ("Too many files", 4, false));
        assert!(t.folder.is_some(), "the last folder stays for Open folder");
        let _ = fs::remove_dir_all(&root);
    }
}
