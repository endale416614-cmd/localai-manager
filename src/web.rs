//! The web pages of the apps inside the manager window: one WebView2 child window per app, below the
//! "Back" bar in app.slint. A view is created on the first open and then only hidden, so each page
//! (logins, open chats) survives going back, until its app stops: then the view is dropped, because its
//! page belongs to the old process. Only the app's own address stays inside; every other link or pop-up
//! opens in the default browser.

use std::collections::HashMap;
use std::path::PathBuf;
use windows_sys::Win32::Foundation::HWND;
use wry::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use wry::{MemoryUsageLevel, NewWindowResponse, WebViewBuilderExtWindows, WebViewExtWindows};

/// Height of the web bar in app.slint, in logical pixels.
const BAR: f32 = 48.0;
/// wry's default arguments (they are replaced, not extended), plus no GPU process: it would take
/// VRAM that the models need. Every view gets the same ones: WebView2 shares its browser process
/// between views of one data folder only when their settings match.
const BROWSER_ARGS: &str = "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection --disable-gpu";

pub struct Web {
    view: wry::WebView,
    _context: wry::WebContext,
}

/// The views by app id, and which one is on screen.
#[derive(Default)]
pub struct Views {
    pages: HashMap<String, Web>,
    shown: Option<String>,
}

impl Views {
    /// Shows the page of app `id`, creating its view on first use.
    pub fn show(&mut self, window: &slint::Window, id: &str, url: &str) -> Result<(), String> {
        if !self.pages.contains_key(id) {
            self.pages.insert(id.to_string(), Web::open(window, url)?);
        }
        self.hide(window);
        self.pages[id].show(window, true);
        self.shown = Some(id.to_string());
        Ok(())
    }

    pub fn hide(&mut self, window: &slint::Window) {
        if let Some(w) = self.shown.take().and_then(|id| self.pages.get(&id)) {
            w.show(window, false);
        }
    }

    /// Drops the views of the apps that `stopped` names, each with its page and window, so that the next open builds
    /// a fresh one (the cookies stay in the shared profile). True when the view on screen was one of them.
    pub fn forget(&mut self, mut stopped: impl FnMut(&str) -> bool) -> bool {
        let ids: Vec<String> = self.pages.keys().filter(|id| stopped(id)).cloned().collect();
        let on_screen = ids.iter().any(|id| self.shown.as_ref() == Some(id));
        for id in &ids {
            self.pages.remove(id);
        }
        if on_screen {
            self.shown = None;
        }
        on_screen
    }

    fn current(&self) -> Option<&Web> {
        self.shown.as_ref().and_then(|id| self.pages.get(id))
    }

    pub fn layout(&self, window: &slint::Window) {
        if let Some(w) = self.current() {
            w.layout(window);
        }
    }

    pub fn reload(&self) {
        if let Some(w) = self.current() {
            w.reload();
        }
    }
}

impl Web {
    fn open(window: &slint::Window, url: &str) -> Result<Web, String> {
        let home = origin(url).ok_or("bad address")?;
        // Logins and cookies live here, not beside the exe, so a read-only install folder still works.
        let data = std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("LocalAI").join("WebView2"));
        let mut context = wry::WebContext::new(data);
        let handle = window.window_handle();
        let before = containers(&handle);
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
                drop_leftovers(&handle, &before);
                e.to_string()
            })?;
        Ok(Web { view, _context: context })
    }

    fn show(&self, window: &slint::Window, on: bool) {
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

    fn layout(&self, window: &slint::Window) {
        let _ = self.view.set_bounds(bounds(window));
    }

    fn reload(&self) {
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

/// Pages of the app's own origin, and the internal about:, data: and blob: ones, stay in the view.
fn allowed(home: &str, url: &str) -> bool {
    let l = url.to_ascii_lowercase();
    ["about:", "data:", "blob:"].iter().any(|s| l.starts_with(s)) || origin(url).is_some_and(|o| o == home)
}

/// The container windows wry has made under the manager window, one per view.
fn containers(parent: &impl HasWindowHandle) -> Vec<HWND> {
    use windows_sys::Win32::UI::WindowsAndMessaging::FindWindowExW;
    let Ok(RawWindowHandle::Win32(h)) = parent.window_handle().map(|h| h.as_raw()) else { return Vec::new() };
    let (parent, class) = (h.hwnd.get() as HWND, "WRY_WEBVIEW".encode_utf16().chain([0]).collect::<Vec<u16>>());
    let (mut found, mut child) = (Vec::new(), std::ptr::null_mut());
    loop {
        child = unsafe { FindWindowExW(parent, child, class.as_ptr(), std::ptr::null()) };
        if child.is_null() {
            return found;
        }
        found.push(child);
    }
}

/// wry creates its container window before it starts WebView2 and leaks it when that fails (a missing
/// Runtime, say), which leaves a blank child on top of the manager. Destroys the ones made since `before`,
/// not the views that work.
fn drop_leftovers(parent: &impl HasWindowHandle, before: &[HWND]) {
    use windows_sys::Win32::UI::WindowsAndMessaging::DestroyWindow;
    for child in containers(parent).into_iter().filter(|c| !before.contains(c)) {
        unsafe { DestroyWindow(child) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_apps_own_origin_stays_inside() {
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

    #[test]
    fn each_app_keeps_to_its_own_port() {
        // ComfyUI's view must not wander into the model chat page, nor the wiki into ComfyUI.
        let comfy = origin("http://127.0.0.1:8188").unwrap();
        let wiki = origin("http://localhost:8080").unwrap();
        assert!(allowed(&comfy, "http://127.0.0.1:8188/api/prompt") && !allowed(&comfy, "http://127.0.0.1:8080/"));
        assert!(allowed(&wiki, "http://127.0.0.1:8080/wikipedia_en_all/A/Rust") && !allowed(&wiki, "http://127.0.0.1:8188/"));
    }
}
