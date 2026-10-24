//! Names that are safe to create on Windows: no device names, no stream or path syntax, nothing that could
//! leave the folder it is made in. Used for result files that a server names, for training set folders, and for
//! the files copied into them.

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;

/// What Windows does not allow in a file name, and the two characters that would make it a path.
const BAD: &str = "<>:\"/\\|?*";

/// `CON`, `PRN`, `AUX`, `NUL`, `COM0`..`COM9` and `LPT0`..`LPT9` (the superscript digits too), with or without
/// an extension: Windows opens the device instead of making a file.
pub fn is_device(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or_default().trim_end().to_uppercase();
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL") || (stem.chars().count() == 4 && (stem.starts_with("COM") || stem.starts_with("LPT")) && stem.ends_with(|c| "0123456789¹²³".contains(c)))
}

/// A single plain file name that Windows would create as it stands. A name that a server hands out is
/// not trusted further than that: no folders, no `:` (an alternate data stream), no trailing dot or space.
pub fn is_plain(name: &str) -> bool {
    let one_part = std::path::Path::new(name).file_name().is_some_and(|f| f == name);
    one_part && !name.ends_with(['.', ' ']) && !name.chars().any(|c| c.is_control() || BAD.contains(c)) && !is_device(name)
}

/// Why a file that exists cannot be copied under its own name, or None when it can. The name comes from the file
/// system and may hold an unpaired surrogate, which no `&str` can, so it is read as UTF-16 units. A `:` makes the
/// name a stream of another file, Windows cuts a trailing dot or space off, and opening a device name opens the
/// device instead of the file.
pub fn copy_problem(name: &OsStr) -> Option<&'static str> {
    let units: Vec<u16> = name.encode_wide().collect();
    if units.contains(&u16::from(b':')) {
        Some("its name has a ':'")
    } else if units.last().is_some_and(|&u| u == u16::from(b'.') || u == u16::from(b' ')) {
        Some("its name ends with a dot or a space")
    } else if is_device(&name.to_string_lossy()) {
        Some("its name is a device name")
    } else {
        None
    }
}

/// A folder name made of the usable part of `raw`: what Windows forbids and control characters are dropped, spaces
/// and dots are trimmed off both ends, 64 characters are kept at most. None when nothing is left, or the rest is a
/// device name. The result is never `..` and has no separator, so it cannot leave the folder it is made in.
pub fn sanitize(raw: &str) -> Option<String> {
    let kept: String = raw.chars().filter(|c| !c.is_control() && !BAD.contains(*c)).collect();
    let name: String = kept.trim_matches([' ', '.']).chars().take(64).collect();
    let name = name.trim_end_matches([' ', '.']);
    (!name.is_empty() && !is_device(name)).then(|| name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_names_with_or_without_an_extension() {
        for dev in ["CON", "con", "Prn", "AUX", "nul", "COM1", "com9", "COM0", "LPT1", "lpt9", "COM¹", "nul.txt", "CON.tar.gz", "aux .x", "COM1 "] {
            assert!(is_device(dev), "{dev}");
        }
        for fine in ["CONSOLE", "COM10", "COM", "LPT", "CONX", "xcon", "a.con", "NULL", "COM1x", "auxiliary", ""] {
            assert!(!is_device(fine), "{fine}");
        }
    }

    #[test]
    fn only_plain_names_pass() {
        for ok in ["fake_00001_.png", "localai-1792807697_00001.mp4", "a b.png", "ünï.png", ".hidden", "a..b"] {
            assert!(is_plain(ok), "{ok}");
        }
        for bad in ["", ".", "..", "a/b.png", "..\\x.png", "../x.png", "C:\\x.png", "x.png:stream", "C:x.png", "x.png.", "x.png ", "CON", "nul.png", "COM3.png", "a\0b.png", "a\nb.png", "a|b", "a?b", "a*b", "a<b", "\\\\server\\share\\x.png"] {
            assert!(!is_plain(bad), "{bad:?}");
        }
    }

    #[test]
    fn a_file_name_from_the_file_system_is_read_as_utf_16() {
        use std::ffi::OsString;
        use std::os::windows::ffi::OsStringExt;
        let os = |s: &str| OsString::from(s);
        for ok in ["clip.wav", "a b.wav", "ünï.wav", ".hidden.wav", "a..b.wav", "COM10.wav", "CONSOLE.wav", "声音.flac"] {
            assert_eq!(copy_problem(&os(ok)), None, "{ok}");
        }
        for bad in ["a:b.wav", "x.wav:Zone.Identifier", "C:x.wav", "x.wav.", "x.wav ", "CON.wav", "nul", "COM1.wav", "lpt3.mp3", "Aux .wav"] {
            assert!(copy_problem(&os(bad)).is_some(), "{bad}");
        }
        // An unpaired surrogate is no reason to refuse a name (a &str could not even hold it); the rules still apply around it.
        let with_lone = |tail: &str| OsString::from_wide(&[&[0x61, 0xD800][..], &tail.encode_utf16().collect::<Vec<_>>()].concat());
        assert!(with_lone(".wav").to_str().is_none());
        assert_eq!(copy_problem(&with_lone(".wav")), None);
        assert!(copy_problem(&with_lone(":stream.wav")).is_some());
        assert!(copy_problem(&with_lone(".wav.")).is_some());
        assert!(copy_problem(&OsString::from_wide(&[0xD800, b':' as u16])).is_some());
    }

    #[test]
    fn set_names_are_cleaned_and_stay_inside() {
        let some = |s: &str| Some(s.to_string());
        assert_eq!(sanitize("Alice"), some("Alice"));
        assert_eq!(sanitize("  .Alice. "), some("Alice"));
        assert_eq!(sanitize("a/b\\c:d*e?f\"g<h>i|j"), some("abcdefghij"));
        assert_eq!(sanitize("..\\..\\evil"), some("evil"));
        assert_eq!(sanitize("../../Windows/System32"), some("WindowsSystem32"));
        assert_eq!(sanitize("tab\there\u{7f}\u{0}"), some("tabhere"));
        assert_eq!(sanitize("Café 声音"), some("Café 声音"));
        assert_eq!(sanitize("a..b"), some("a..b"));
        for none in ["", " ", "..", "...", ". .", "\\", "/..", "CON", "con", "Nul.txt", "COM1", "lpt9", "  PRN  ", ".aux"] {
            assert_eq!(sanitize(none), None, "{none:?}");
        }
        assert_eq!(sanitize("COM10"), some("COM10"));
        // Long names are cut, and the cut leaves no trailing space or dot.
        let long = format!("{}. {}", "x".repeat(63), "tail");
        assert_eq!(sanitize(&long), Some("x".repeat(63)));
        assert_eq!(sanitize(&"y".repeat(200)).map(|s| s.len()), Some(64));
        // Whatever comes out is one folder-name component.
        for raw in ["..", "..\\..\\x", "a/../b", "C:\\Windows", "\\\\host\\share", "x:y", "....//"] {
            if let Some(name) = sanitize(raw) {
                let parts: Vec<_> = std::path::Path::new(&name).components().collect();
                assert!(parts.len() == 1 && matches!(parts[0], std::path::Component::Normal(_)), "{raw:?} -> {name:?}");
            }
        }
    }
}
