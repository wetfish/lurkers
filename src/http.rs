//! A small web page on lurker's domain that says whether lurker is connected
//! to IRC, and nothing about anyone in its rooms or channels. `/up` answers
//! 200 while lurker is connected and 503 while it isn't, for the container's
//! health check, which runs `lurker --health`.

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream as StdTcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time;

/// How long a visitor has to send their request and read the answer.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// The most of a request lurker reads. Only the first line matters.
const MAX_REQUEST: usize = 4096;

const PLAIN: &str = "text/plain; charset=utf-8";

/// Whether lurker is connected to IRC, shared with the web page.
#[derive(Clone, Default)]
pub struct Status(Arc<AtomicBool>);

impl Status {
    pub fn set_connected(&self, connected: bool) {
        self.0.store(connected, Ordering::Relaxed);
    }

    pub fn connected(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

/// Starts listening, before lurker connects to IRC, so a port that is
/// already taken stops lurker straight away.
pub async fn bind(address: &str) -> Result<TcpListener, String> {
    let listener = TcpListener::bind(address)
        .await
        .map_err(|err| format!("listening on {address}: {err}"))?;
    eprintln!("Serving the status page on http://{address}");
    Ok(listener)
}

/// Answers requests until lurker stops.
pub async fn serve(listener: TcpListener, status: Status) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let status = status.clone();
                tokio::spawn(async move {
                    let _ = time::timeout(REQUEST_TIMEOUT, answer(stream, status)).await;
                });
            }
            Err(err) => {
                eprintln!("warning: status page: {err}");
                time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
}

async fn answer(mut stream: TcpStream, status: Status) -> io::Result<()> {
    let mut request = Vec::new();
    let mut buffer = [0u8; 1024];
    while !request.windows(4).any(|window| window == b"\r\n\r\n") && request.len() < MAX_REQUEST {
        let read = stream.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        request.extend_from_slice(&buffer[..read]);
    }

    stream
        .write_all(&respond(&request, status.connected()))
        .await?;
    stream.shutdown().await
}

/// The whole response to a request.
fn respond(request: &[u8], connected: bool) -> Vec<u8> {
    let first_line = request.split(|&byte| byte == b'\n').next().unwrap_or(&[]);
    let first_line = String::from_utf8_lossy(first_line);
    let mut words = first_line.split_whitespace();
    let method = words.next().unwrap_or("");
    let path = words.next().unwrap_or("");
    let path = path.split('?').next().unwrap_or("");

    let (status, content_type, body) = match (method, path) {
        ("GET" | "HEAD", "/") => ("200 OK", "text/html; charset=utf-8", page(connected)),
        ("GET" | "HEAD", "/up") if connected => ("200 OK", PLAIN, "OK\n".to_owned()),
        ("GET" | "HEAD", "/up") => (
            "503 Service Unavailable",
            PLAIN,
            "Not connected to IRC\n".to_owned(),
        ),
        ("GET" | "HEAD", _) => ("404 Not Found", PLAIN, "Not found\n".to_owned()),
        _ => (
            "405 Method Not Allowed",
            PLAIN,
            "Method not allowed\n".to_owned(),
        ),
    };

    let allow = if status.starts_with("405") {
        "Allow: GET, HEAD\r\n"
    } else {
        ""
    };
    let mut response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\
         Cache-Control: no-store\r\n{allow}Connection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    if method != "HEAD" {
        response.extend_from_slice(body.as_bytes());
    }
    response
}

/// The page itself, which only says whether lurker is connected.
fn page(connected: bool) -> String {
    let state = if connected {
        "lurker is connected to IRC."
    } else {
        "lurker isn't connected to IRC right now."
    };
    format!(
        "<!doctype html>\n\
         <html lang=\"en\">\n\
         <head>\n\
         <meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <title>lurker</title>\n\
         <style>\n\
         :root {{ color-scheme: light dark; }}\n\
         body {{ font-family: system-ui, sans-serif; max-width: 32rem; margin: 4rem auto; \
         padding: 0 1rem; line-height: 1.5; }}\n\
         </style>\n\
         </head>\n\
         <body>\n\
         <h1>lurker</h1>\n\
         <p>{state}</p>\n\
         <p><a href=\"{}\">Source code</a></p>\n\
         </body>\n\
         </html>\n",
        env!("CARGO_PKG_REPOSITORY")
    )
}

/// `lurker --health <address>`: whether lurker at that address says it is
/// connected to IRC.
pub fn check(address: &str) -> Result<(), String> {
    let address: SocketAddr = address
        .parse()
        .map_err(|_| format!("{address} isn't an address and port, such as 127.0.0.1:8080"))?;
    let fail = |err: io::Error| format!("asking {address}: {err}");

    let mut stream = StdTcpStream::connect_timeout(&address, REQUEST_TIMEOUT).map_err(fail)?;
    stream
        .set_read_timeout(Some(REQUEST_TIMEOUT))
        .map_err(fail)?;
    stream
        .set_write_timeout(Some(REQUEST_TIMEOUT))
        .map_err(fail)?;
    stream
        .write_all(b"GET /up HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .map_err(fail)?;

    let mut response = Vec::new();
    stream
        .take(MAX_REQUEST as u64)
        .read_to_end(&mut response)
        .map_err(fail)?;
    let response = String::from_utf8_lossy(&response);
    let status = response.lines().next().unwrap_or("");
    if status.split_whitespace().nth(1) == Some("200") {
        Ok(())
    } else {
        Err(format!("lurker isn't connected to IRC ({status})"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(response: &[u8]) -> String {
        String::from_utf8(response.to_vec()).unwrap()
    }

    #[test]
    fn shows_whether_lurker_is_connected() {
        let response = text(&respond(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n", true));
        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
        assert!(
            response.contains("lurker is connected to IRC."),
            "{response}"
        );

        let response = text(&respond(b"GET /?x=1 HTTP/1.1\r\n\r\n", false));
        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
        assert!(response.contains("isn't connected"), "{response}");
    }

    #[test]
    fn answers_health_checks() {
        let up = text(&respond(b"GET /up HTTP/1.1\r\n\r\n", true));
        assert!(up.starts_with("HTTP/1.1 200 OK\r\n") && up.ends_with("\r\n\r\nOK\n"));

        let down = text(&respond(b"GET /up HTTP/1.1\r\n\r\n", false));
        assert!(
            down.starts_with("HTTP/1.1 503 Service Unavailable\r\n"),
            "{down}"
        );
    }

    #[test]
    fn answers_head_without_a_body() {
        let response = text(&respond(b"HEAD /up HTTP/1.1\r\n\r\n", true));
        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(response.contains("Content-Length: 3\r\n"));
        assert!(response.ends_with("\r\n\r\n"), "{response}");
    }

    #[test]
    fn refuses_everything_else() {
        let response = text(&respond(b"GET /lurkers HTTP/1.1\r\n\r\n", true));
        assert!(response.starts_with("HTTP/1.1 404 Not Found\r\n"));

        let response = text(&respond(b"POST /up HTTP/1.1\r\n\r\n", true));
        assert!(response.starts_with("HTTP/1.1 405 Method Not Allowed\r\n"));
        assert!(response.contains("Allow: GET, HEAD\r\n"));

        let response = text(&respond(b"", true));
        assert!(response.starts_with("HTTP/1.1 405"), "{response}");
    }

    #[tokio::test]
    async fn the_health_check_asks_the_page() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let status = Status::default();
        tokio::spawn(serve(listener, status.clone()));

        let ask = |address: String| tokio::task::spawn_blocking(move || check(&address));
        let err = ask(address.clone()).await.unwrap().unwrap_err();
        assert!(err.contains("503"), "{err}");

        status.set_connected(true);
        assert_eq!(ask(address).await.unwrap(), Ok(()));
    }

    #[test]
    fn the_health_check_explains_failures() {
        assert!(check("localhost").unwrap_err().contains("isn't an address"));
        // Nothing listens on port 1
        assert!(check("127.0.0.1:1").unwrap_err().starts_with("asking"));
    }
}
