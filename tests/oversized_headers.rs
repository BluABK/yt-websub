//! Regression test for the H1 header-buffering DoS: an unauthenticated client
//! that sends an over-long header line must not be able to make the server buffer
//! unbounded memory. The vendored tiny_http now caps a single request/header line
//! (see vendor/tiny_http/src/client.rs), so the server closes that connection and
//! stays alive to serve the next request instead of OOM-aborting.

use std::io::{Read, Write};
use std::net::TcpStream;

#[test]
fn oversized_header_line_is_rejected_and_server_survives() {
    let server = tiny_http::Server::http("127.0.0.1:0").expect("bind test server");
    let port = server
        .server_addr()
        .to_ip()
        .expect("test server has an IP address")
        .port();

    // The server thread only ever needs to serve ONE good request. If the
    // oversized-header connection had hung or crashed the server, recv() for the
    // good request below would never return and the test would hang/fail.
    let handle = std::thread::spawn(move || {
        let req = server.recv().expect("recv the well-formed request");
        let _ = req.respond(tiny_http::Response::from_string("ok"));
    });

    // 1) Oversized header line: a single header value far exceeding the per-line
    //    cap, with no CRLF for ~1 MB. The server must bail and close, not buffer
    //    it all. The oversized connection never yields a Request, so it is not
    //    counted by the single recv() above.
    {
        let mut sock = TcpStream::connect(("127.0.0.1", port)).expect("connect #1");
        let _ = sock.write_all(b"GET / HTTP/1.1\r\nX-Huge: ");
        let chunk = vec![b'A'; 64 * 1024];
        for _ in 0..16 {
            if sock.write_all(&chunk).is_err() {
                break; // server closed on us after hitting the line cap — expected
            }
        }
        let _ = sock.write_all(b"\r\n\r\n");
        let mut sink = Vec::new();
        let _ = sock.read_to_end(&mut sink); // drain until the server closes
    }

    // 2) A normal request must still succeed — proving the server survived.
    let mut sock = TcpStream::connect(("127.0.0.1", port)).expect("connect #2");
    sock.write_all(b"GET / HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n")
        .expect("send good request");
    let mut resp = String::new();
    sock.read_to_string(&mut resp).expect("read response");
    assert!(
        resp.contains("200"),
        "server should still serve requests after an oversized header; got: {:?}",
        resp
    );

    handle.join().expect("server thread panicked");
}
