//! The Win32 "open file" dialog: one picture for the Create tab, or several files to import as training data.

use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::path::PathBuf;
use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::Controls::Dialogs::{CommDlgExtendedError, FNERR_BUFFERTOOSMALL, GetOpenFileNameW, OFN_ALLOWMULTISELECT, OFN_EXPLORER, OFN_FILEMUSTEXIST, OFN_HIDEREADONLY, OFN_NOCHANGEDIR, OFN_PATHMUSTEXIST, OPENFILENAMEW};
use wry::raw_window_handle::{HasWindowHandle, RawWindowHandle};

/// Room for the names of some 25,000 files, in UTF-16 units; the dialog refuses to close beyond that.
const MANY: usize = 1 << 20;

/// A png, jpg or webp file; None when the dialog is cancelled. Modal over the manager window.
pub fn picture(window: &slint::Window) -> Result<Option<PathBuf>, String> {
    let buf = dialog(window, "Choose a picture", "Pictures (png, jpg, webp)\0*.png;*.jpg;*.jpeg;*.webp\0\0", false)?;
    Ok(buf.and_then(|buf| selection(&buf).into_iter().next()))
}

/// Any number of files, none when the dialog is cancelled. `filter` is pairs of "name\0pattern\0", ended by one more NUL.
pub fn files(window: &slint::Window, title: &str, filter: &str) -> Result<Vec<PathBuf>, String> {
    Ok(dialog(window, title, filter, true)?.map(|buf| selection(&buf)).unwrap_or_default())
}

/// Runs the dialog and returns its result buffer (see `selection`); None when it was cancelled.
fn dialog(window: &slint::Window, title: &str, filter: &str, many: bool) -> Result<Option<Vec<u16>>, String> {
    let owner = match window.window_handle().window_handle().map(|h| h.as_raw()) {
        Ok(RawWindowHandle::Win32(h)) => h.hwnd.get() as HWND,
        _ => std::ptr::null_mut(),
    };
    let filter: Vec<u16> = filter.encode_utf16().collect();
    let title: Vec<u16> = title.encode_utf16().chain([0]).collect();
    let mut file = vec![0u16; if many { MANY } else { 32768 }];
    let mut ofn: OPENFILENAMEW = unsafe { std::mem::zeroed() };
    ofn.lStructSize = std::mem::size_of::<OPENFILENAMEW>() as u32;
    ofn.hwndOwner = owner;
    ofn.lpstrFilter = filter.as_ptr();
    ofn.lpstrFile = file.as_mut_ptr();
    ofn.nMaxFile = file.len() as u32;
    ofn.lpstrTitle = title.as_ptr();
    // NOCHANGEDIR: the dialog must not move the working folder, where a dev run finds apps.toml.
    ofn.Flags = OFN_EXPLORER | OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST | OFN_HIDEREADONLY | OFN_NOCHANGEDIR | if many { OFN_ALLOWMULTISELECT } else { 0 };
    if unsafe { GetOpenFileNameW(&mut ofn) } != 0 {
        return Ok(Some(file));
    }
    match unsafe { CommDlgExtendedError() } {
        0 => Ok(None),
        FNERR_BUFFERTOOSMALL => Err("Too many files at once. Choose fewer, or import them in two or more steps.".into()),
        code => Err(format!("The file dialog failed (error {code:#x}).")),
    }
}

/// The files in a result buffer. One file is its full path. Several files are the folder, then each file name.
/// Every string ends with a NUL, and the list with one more.
fn selection(buf: &[u16]) -> Vec<PathBuf> {
    let mut parts = buf.split(|&c| c == 0).take_while(|s| !s.is_empty()).map(OsString::from_wide);
    let Some(first) = parts.next() else { return Vec::new() };
    let names: Vec<OsString> = parts.collect();
    if names.is_empty() { vec![PathBuf::from(first)] } else { names.iter().map(|n| PathBuf::from(&first).join(n)).collect() }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    #[test]
    fn one_file_is_its_full_path() {
        assert_eq!(selection(&w("C:\\Pictures\\a.png\0\0")), [PathBuf::from("C:\\Pictures\\a.png")]);
    }

    #[test]
    fn several_files_are_a_folder_and_names() {
        let all = selection(&w("C:\\Pictures\\Holiday trip\0a.png\0b b.JPG\0ünï.webp\0\0"));
        assert_eq!(all, ["C:\\Pictures\\Holiday trip\\a.png", "C:\\Pictures\\Holiday trip\\b b.JPG", "C:\\Pictures\\Holiday trip\\ünï.webp"].map(PathBuf::from));
        // A drive root keeps its backslash.
        assert_eq!(selection(&w("D:\\\0a.wav\0b.wav\0\0")), ["D:\\a.wav", "D:\\b.wav"].map(PathBuf::from));
    }

    #[test]
    fn an_empty_or_cancelled_buffer_selects_nothing() {
        assert!(selection(&[]).is_empty());
        assert!(selection(&[0, 0, 0]).is_empty());
        assert!(selection(&vec![0; MANY]).is_empty());
    }

    #[test]
    fn what_follows_the_final_double_nul_is_ignored() {
        // The buffer is as long as the dialog was given; old text may sit behind the list.
        let mut buf = w("C:\\d\0a.png\0b.png\0\0old leftovers\0\0");
        buf.resize(MANY, 0);
        assert_eq!(selection(&buf), ["C:\\d\\a.png", "C:\\d\\b.png"].map(PathBuf::from));
        let mut one = w("C:\\d\\only.png\0\0stale\0b.png\0\0");
        one.resize(MANY, 0);
        assert_eq!(selection(&one), [PathBuf::from("C:\\d\\only.png")]);
    }

    #[test]
    fn hundreds_of_files_fit() {
        let names: Vec<String> = (0..800).map(|i| format!("take {i:04} of the long recording.wav")).collect();
        let mut buf = w("C:\\Recordings\\Alice\0");
        for n in &names {
            buf.extend(w(n));
            buf.push(0);
        }
        buf.push(0);
        assert!(buf.len() < MANY);
        let all = selection(&buf);
        assert_eq!(all.len(), 800);
        assert_eq!(all[799], PathBuf::from(format!("C:\\Recordings\\Alice\\{}", names[799])));
    }
}
