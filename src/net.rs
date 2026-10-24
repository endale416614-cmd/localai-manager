//! HTTP to the apps on this PC.

use std::time::Duration;

/// An agent for the local apps (llama-server, ComfyUI). It never uses a proxy: the default one takes a proxy from
/// the environment (HTTP_PROXY, ALL_PROXY and the like), and would send this loopback traffic through it. An HTTP
/// error status comes back as a response, so that its body can be read.
pub fn local_agent(timeout: Option<Duration>) -> ureq::Agent {
    ureq::Agent::config_builder().http_status_as_error(false).proxy(None).timeout_global(timeout).build().into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    #[test]
    fn a_loopback_request_ignores_the_proxy_in_the_environment() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let _ = s.read(&mut [0u8; 1024]);
            s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nhi").unwrap();
        });

        // A proxy that nobody listens on: a request that went through it could not arrive. The agents read
        // the environment when they are made, so it is only set for that moment.
        let vars = ["ALL_PROXY", "HTTPS_PROXY", "HTTP_PROXY", "all_proxy", "https_proxy", "http_proxy"];
        // SAFETY: test code; the other tests do not read these variables, and their agents ignore them.
        vars.iter().for_each(|v| unsafe { std::env::set_var(v, "http://127.0.0.1:9") });
        let (ours, default) = (local_agent(None), ureq::Agent::new_with_defaults());
        vars.iter().for_each(|v| unsafe { std::env::remove_var(v) });

        assert!(default.config().proxy().is_some(), "the default agent does pick the proxy up");
        assert!(ours.config().proxy().is_none());
        assert_eq!(ours.get(format!("http://127.0.0.1:{port}/")).call().unwrap().body_mut().read_to_string().unwrap(), "hi");
        server.join().unwrap();
    }

    #[test]
    fn an_error_status_is_a_response() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let _ = s.read(&mut [0u8; 1024]);
            s.write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 4\r\nConnection: close\r\n\r\nnope").unwrap();
        });
        let mut r = local_agent(Some(Duration::from_secs(10))).get(format!("http://127.0.0.1:{port}/")).call().unwrap();
        assert_eq!((r.status().as_u16(), r.body_mut().read_to_string().unwrap().as_str()), (400, "nope"));
        server.join().unwrap();
    }
}
