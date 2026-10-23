//! Chat tab backend (PLAN.md, "send" / "stop"): streams replies from llama-server's
//! OpenAI-compatible /v1/chat/completions on a worker thread.

use serde_json::{Value, json};
use std::io::{BufRead, BufReader};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

#[derive(Clone, Default)]
pub struct Msg {
    pub user: bool,
    pub text: String,
    /// Model's reasoning (Qwen3.6 thinks first); shown, never sent back.
    pub thinking: String,
    pub error: String,
}

#[derive(Default)]
pub struct Chat {
    pub msgs: Vec<Msg>,
    pub busy: bool,
    /// Bumped on every change so the UI only rebuilds rows when something moved.
    pub version: u64,
    cancel: Arc<AtomicBool>,
}

pub type Shared = Arc<Mutex<Chat>>;

impl Chat {
    /// Returns at once; the worker notices on its next chunk, drops the connection, and llama-server
    /// stops generating.
    pub fn stop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        if self.busy {
            self.busy = false;
            self.version += 1;
        }
    }

    pub fn clear(&mut self) {
        if !self.busy {
            self.msgs.clear();
            self.version += 1;
        }
    }
}

pub fn send(chat: &Shared, port: u16, text: String) {
    let mut c = chat.lock().unwrap();
    if c.busy || text.trim().is_empty() {
        return;
    }
    c.msgs.push(Msg { user: true, text, ..Default::default() });
    let history: Vec<Value> = c
        .msgs
        .iter()
        .filter(|m| m.error.is_empty() || m.user)
        .map(|m| json!({ "role": if m.user { "user" } else { "assistant" }, "content": m.text }))
        .collect();
    c.msgs.push(Msg::default());
    c.busy = true;
    c.version += 1;
    let cancel = Arc::new(AtomicBool::new(false));
    c.cancel = cancel.clone();
    drop(c);

    let chat = chat.clone();
    thread::spawn(move || {
        // Every write checks this request's own flag: after Stop the user may already be on a new reply.
        let edit = |f: &dyn Fn(&mut Msg)| {
            let mut c = chat.lock().unwrap();
            if cancel.load(Ordering::Relaxed) {
                return false;
            }
            if let Some(m) = c.msgs.last_mut() {
                f(m);
            }
            c.version += 1;
            true
        };
        let r = stream(port, &history, |ev| match ev {
            Event::Delta { thinking, text } => edit(&|m| {
                m.thinking.push_str(&thinking);
                m.text.push_str(&text);
            }),
            _ => true,
        });
        if let Err(e) = r {
            edit(&|m| m.error = e.clone());
        }
        let mut c = chat.lock().unwrap();
        if !cancel.load(Ordering::Relaxed) {
            c.busy = false;
            c.version += 1;
        }
    });
}

#[derive(Debug, PartialEq)]
enum Event {
    Delta { thinking: String, text: String },
    Done,
}

/// Calls `on` per streamed event until [DONE]; stops early when `on` returns false.
fn stream(port: u16, history: &[Value], mut on: impl FnMut(Event) -> bool) -> Result<(), String> {
    let agent: ureq::Agent = ureq::Agent::config_builder().http_status_as_error(false).build().into();
    let url = format!("http://127.0.0.1:{port}/v1/chat/completions");
    let mut resp = agent.post(&url).send_json(json!({ "messages": history, "stream": true })).map_err(|e| e.to_string())?;
    if resp.status() != 200 {
        let body = resp.body_mut().read_to_string().unwrap_or_default();
        return Err(format!("HTTP {}: {}", resp.status(), error_message(&body)));
    }
    for line in BufReader::new(resp.into_body().into_reader()).lines() {
        let line = line.map_err(|e| format!("connection lost: {e}"))?;
        match parse_sse(&line)? {
            Some(Event::Done) => return Ok(()),
            Some(ev) => {
                if !on(ev) {
                    return Ok(());
                }
            }
            None => {}
        }
    }
    Ok(())
}

fn error_message(body: &str) -> String {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v["error"]["message"].as_str().map(str::to_string))
        .unwrap_or_else(|| body.trim().to_string())
}

/// One SSE line. Blank lines, comments and non-data fields are None; an `error` object is Err.
fn parse_sse(line: &str) -> Result<Option<Event>, String> {
    let Some(data) = line.strip_prefix("data:").map(str::trim) else { return Ok(None) };
    if data == "[DONE]" {
        return Ok(Some(Event::Done));
    }
    let v: Value = serde_json::from_str(data).map_err(|e| format!("bad stream data: {e}"))?;
    if !v["error"].is_null() {
        return Err(error_message(data));
    }
    let d = &v["choices"][0]["delta"];
    let s = |k: &str| d[k].as_str().unwrap_or_default().to_string();
    Ok(Some(Event::Delta { thinking: s("reasoning_content"), text: s("content") }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_stream_lines() {
        let d = |t: &str, x: &str| Ok(Some(Event::Delta { thinking: t.into(), text: x.into() }));
        assert_eq!(parse_sse(r#"data: {"choices":[{"delta":{"content":"Hi"}}]}"#), d("", "Hi"));
        assert_eq!(parse_sse(r#"data: {"choices":[{"delta":{"reasoning_content":"hmm"}}]}"#), d("hmm", ""));
        assert_eq!(parse_sse("data: [DONE]"), Ok(Some(Event::Done)));
        assert_eq!(parse_sse(""), Ok(None));
        assert_eq!(parse_sse(": ping"), Ok(None));
        assert_eq!(parse_sse(r#"data: {"error":{"message":"context full"}}"#), Err("context full".into()));
    }

    #[test]
    fn stop_frees_the_chat_at_once() {
        let chat: Shared = Arc::default();
        // Port 9 (discard) refuses connections, so the worker can only fail; Stop must not wait for it.
        send(&chat, 9, "hello".into());
        let mut c = chat.lock().unwrap();
        assert!(c.busy);
        c.stop();
        assert!(!c.busy);
        assert_eq!(c.msgs.len(), 2);
        c.clear();
        assert!(c.msgs.is_empty());
    }
}
