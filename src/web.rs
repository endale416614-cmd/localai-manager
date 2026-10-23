//! Odysseus inside the manager window: a WebView2 child window below the "Back" bar in app.slint.
//! It is created on first open and then only hidden, so the Odysseus session survives going back.
//! Only the Odysseus address stays inside; every other link or pop-up opens in the default browser.

use std::path::PathBuf;
use wry::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use wry::{MemoryUsageLevel, NewWindowResponse, WebViewBuilderExtWindows, WebViewExtWindows};

/// Height of the web bar in app.slint, in logical pixels.
const BAR: f32 = 48.0;
/// wry's default arguments (they are replaced, not extended), plus no GPU process: it would take
/// VRAM that the models need.
const BROWSER_ARGS: &str = "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection --disable-gpu";

pub struct Web {
    view: wry::WebView,
    _context: wry::WebContext,
}

impl Web {
    pub fn open(window: &slint::Window, url: &str) -> Result<Web, String> {
        let home = origin(url).ok_or("bad address")?;
        // Logins and cookies live here, not beside the exe, so a read-only install folder still works.
        let data = std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("LocalAI").join("WebView2"));
        let mut context = wry::WebContext::new(data);
        let handle = window.window_handle();
        let view = wry::WebViewBuilder::new_with_web_context(&mut context)
            .with_url(url)
            .with_bounds(bounds(window))
            .with_additional_browser_args(BROWSER_ARGS)
            .with_navigation_handler(move |to| {
                let stay = allowed(&home, &to);
                if !stay {
                    open_external(&to);
                }
                stay
            })
            .with_new_window_req_handler(|to, _| {
                open_external(&to);
                NewWindowResponse::Deny
            })
            .build_as_child(&handle)
            .map_err(|e| {
                drop_leftovers(&handle);
                e.to_string()
            })?;
        Ok(Web { view, _context: context })
    }

    pub fn show(&self, window: &slint::Window, on: bool) {
        if on {
            let _ = self.view.set_bounds(bounds(window));
        }
        let _ = self.view.set_visible(on);
        // Hidden, WebView2 may give memory back; shown, it runs at full speed again.
        let _ = self.view.set_memory_usage_level(if on { MemoryUsageLevel::Normal } else { MemoryUsageLevel::Low });
        if on {
            let _ = self.view.focus();
        } else {
            let _ = self.view.focus_parent();
        }
    }

    pub fn layout(&self, window: &slint::Window) {
        let _ = self.view.set_bounds(bounds(window));
    }

    pub fn reload(&self) {
        let _ = self.view.reload();
    }
}

fn bounds(window: &slint::Window) -> wry::Rect {
    let size = window.size();
    let bar = (BAR * window.scale_factor()).round() as u32;
    wry::Rect {
        position: wry::dpi::PhysicalPosition::new(0, bar as i32).into(),
        size: wry::dpi::PhysicalSize::new(size.width, size.height.saturating_sub(bar)).into(),
    }
}

/// Hands a web link to the default browser through explorer.exe, by full path. Other schemes (file:,
/// ms-msdt: and the like) would start programs, so they are dropped.
pub fn open_external(url: &str) {
    let l = url.to_ascii_lowercase();
    if l.starts_with("http://") || l.starts_with("https://") {
        let _ = std::process::Command::new(crate::setup::windows_dir().join("explorer.exe")).arg(url).spawn();
    }
}

/// `scheme://host[:port]` in lowercase, with localhost as 127.0.0.1 and port 80 dropped from http (the
/// way WebView2 reports it). A `user@` part stays in, so such an address never equals a plain one.
fn origin(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    let mut host = rest.split(['/', '?', '#', '\\']).next()?.to_ascii_lowercase();
    if let Some(port) = host.strip_prefix("localhost") {
        host = format!("127.0.0.1{port}");
    }
    if scheme == "http" {
        host = host.strip_suffix(":80").unwrap_or(&host).to_string();
    }
    Some(format!("{scheme}://{host}"))
}

/// Pages of the Odysseus origin, and the internal about:, data: and blob: ones, stay in the view.
fn allowed(home: &str, url: &str) -> bool {
    let l = url.to_ascii_lowercase();
    ["about:", "data:", "blob:"].iter().any(|s| l.starts_with(s)) || origin(url).is_some_and(|o| o == home)
}

/// wry creates its container window before it starts WebView2 and leaks it when that fails (a missing
/// Runtime, say), which leaves a blank child on top of the manager. Destroys every such child.
fn drop_leftovers(parent: &impl HasWindowHandle) {
    use windows_sys::Win32::Foundation::HWND;
    use windows_sys::Win32::UI::WindowsAndMessaging::{DestroyWindow, FindWindowExW};
    let Ok(RawWindowHandle::Win32(h)) = parent.window_handle().map(|h| h.as_raw()) else { return };
    let (parent, class) = (h.hwnd.get() as HWND, "WRY_WEBVIEW".encode_utf16().chain([0]).collect::<Vec<u16>>());
    loop {
        let child = unsafe { FindWindowExW(parent, std::ptr::null_mut(), class.as_ptr(), std::ptr::null()) };
        if child.is_null() || unsafe { DestroyWindow(child) } == 0 {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_odysseus_origin_stays_inside() {
        let home = origin("http://127.0.0.1:7000").unwrap();
        for stay in ["http://127.0.0.1:7000/", "HTTP://127.0.0.1:7000/chat?x=1#y", "http://localhost:7000/login", "about:blank", "data:text/html,hi", "blob:http://127.0.0.1:7000/1-2"] {
            assert!(allowed(&home, stay), "{stay}");
        }
        for out in [
            "http://127.0.0.1:7001/",
            "https://127.0.0.1:7000/",
            "http://example.com/",
            "http://127.0.0.1:7000@evil.com/",
            "http://localhost.evil.com:7000/",
            "http://evil.com/?u=http://127.0.0.1:7000/",
            "file:///C:/x.exe",
            "ms-msdt:/id",
            "javascript:alert(1)",
        ] {
            assert!(!allowed(&home, out), "{out}");
        }
        assert_eq!(origin("http://127.0.0.1:80/"), origin("http://127.0.0.1"));
    }
}
