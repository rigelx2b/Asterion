//! Loopback-only HTTP server for the run status dashboard.

use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::thread;
use std::time::Duration;

use asterion_data::progress;
use chrono::Utc;

const PAGE: &str = include_str!("monitor.html");
const MAX_REQUEST_BYTES: usize = 8 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, PartialEq, Eq)]
struct Response {
    status: &'static str,
    content_type: &'static str,
    body: Vec<u8>,
}

impl Response {
    fn empty(status: &'static str) -> Self {
        Response {
            status,
            content_type: "text/plain",
            body: Vec::new(),
        }
    }
}

pub fn bind(port: u16) -> io::Result<TcpListener> {
    TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, port)))
}

// Serves until the process is stopped; each connection gets its own thread so an
// idle browser connection cannot stall the polling requests.
pub fn serve(listener: TcpListener, status_dir: &Path) -> io::Result<()> {
    let port = listener.local_addr()?.port();
    for stream in listener.incoming() {
        let Ok(stream) = stream else {
            continue;
        };
        let status_dir = status_dir.to_owned();
        thread::spawn(move || {
            let _ = handle(stream, port, &status_dir);
        });
    }
    Ok(())
}

fn handle(mut stream: TcpStream, port: u16, status_dir: &Path) -> io::Result<()> {
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;

    let mut request = Vec::new();
    let mut chunk = [0u8; 1024];
    while !request.windows(4).any(|w| w == b"\r\n\r\n") && request.len() < MAX_REQUEST_BYTES {
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        request.extend_from_slice(&chunk[..read]);
    }

    let response = route(&String::from_utf8_lossy(&request), port, status_dir);
    let head = format!(
        "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n",
        response.status,
        response.content_type,
        response.body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(&response.body)?;
    stream.flush()
}

fn route(request: &str, port: u16, status_dir: &Path) -> Response {
    let mut lines = request.split("\r\n");
    let mut request_line = lines.next().unwrap_or("").split(' ');
    let (Some(method), Some(target)) = (request_line.next(), request_line.next()) else {
        return Response::empty("400 Bad Request");
    };
    if method != "GET" {
        return Response::empty("405 Method Not Allowed");
    }

    // Rejecting foreign Host values stops DNS-rebinding pages from reading the status API.
    let host = lines
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("host"))
        .map(|(_, value)| value.trim());
    let allowed = [format!("127.0.0.1:{port}"), format!("localhost:{port}")];
    if !host.is_some_and(|h| allowed.iter().any(|a| a.eq_ignore_ascii_case(h))) {
        return Response::empty("403 Forbidden");
    }

    match target.split('?').next().unwrap_or("") {
        "/" | "/index.html" => Response {
            status: "200 OK",
            content_type: "text/html; charset=utf-8",
            body: PAGE.as_bytes().to_vec(),
        },
        "/api/status" => {
            let payload = serde_json::json!({
                "now": Utc::now().to_rfc3339(),
                "runs": progress::load_all(status_dir),
            });
            Response {
                status: "200 OK",
                content_type: "application/json",
                body: payload.to_string().into_bytes(),
            }
        }
        _ => Response::empty("404 Not Found"),
    }
}

#[cfg(test)]
mod tests {
    use asterion_data::ProgressTracker;

    use super::*;

    fn get(target: &str, host: &str) -> String {
        format!("GET {target} HTTP/1.1\r\nHost: {host}\r\nAccept: */*\r\n\r\n")
    }

    #[test]
    fn routes_page_status_and_unknown_paths() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut tracker = ProgressTracker::start(dir.path(), "fetch");
        tracker.stage("fetching", 4);
        tracker.finish(None);

        let page = route(&get("/", "127.0.0.1:8787"), 8787, dir.path());
        assert_eq!(page.status, "200 OK");
        assert!(page.content_type.starts_with("text/html"));

        let status = route(&get("/api/status?t=1", "localhost:8787"), 8787, dir.path());
        let json: serde_json::Value = serde_json::from_slice(&status.body).expect("json");
        assert_eq!(json["runs"][0]["kind"], "fetch");
        assert_eq!(json["runs"][0]["state"], "completed");
        assert_eq!(json["runs"][0]["total"], 4);

        let missing = route(&get("/data/raw", "127.0.0.1:8787"), 8787, dir.path());
        assert_eq!(missing, Response::empty("404 Not Found"));
    }

    #[test]
    fn rejects_foreign_hosts_and_other_methods() {
        let dir = tempfile::tempdir().expect("tempdir");
        let foreign = route(&get("/api/status", "evil.example:8787"), 8787, dir.path());
        assert_eq!(foreign.status, "403 Forbidden");
        let no_host = route("GET / HTTP/1.1\r\n\r\n", 8787, dir.path());
        assert_eq!(no_host.status, "403 Forbidden");
        let post = route(
            "POST / HTTP/1.1\r\nHost: 127.0.0.1:8787\r\n\r\n",
            8787,
            dir.path(),
        );
        assert_eq!(post.status, "405 Method Not Allowed");
        assert_eq!(route("", 8787, dir.path()).status, "400 Bad Request");
    }

    #[test]
    fn serves_over_a_real_socket() {
        let dir = tempfile::tempdir().expect("tempdir");
        let listener = bind(0).expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let status_dir = dir.path().to_owned();
        thread::spawn(move || serve(listener, &status_dir));

        let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).expect("connect");
        stream
            .write_all(get("/api/status", &format!("127.0.0.1:{port}")).as_bytes())
            .expect("write");
        let mut response = String::new();
        stream.read_to_string(&mut response).expect("read");
        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(response.ends_with("\"runs\":[]}"));
    }
}
