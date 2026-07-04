//! Regression test for the remote-DoS crash observed in production:
//!
//! ```text
//! thread '<unnamed>' panicked at tiny_http-0.12.0/src/util/equal_reader.rs:70:27:
//! capacity overflow
//! ```
//!
//! A request declaring `Content-Length: 18446744073709551615` whose body is
//! never read made tiny_http's `EqualReader::drop` allocate the entire
//! remaining length in one `Vec`, panicking — and `panic = "abort"` took the
//! whole process down. Fixed in the vendored crate (see `[patch.crates-io]`
//! in Cargo.toml); this test fails if the patch ever stops being applied.

use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream};

#[test]
fn huge_content_length_does_not_panic_server() {
    let server = tiny_http::Server::http("127.0.0.1:0").expect("bind test server");
    let port = server
        .server_addr()
        .to_ip()
        .expect("test server has an IP listen address")
        .port();

    let handle = std::thread::spawn(move || {
        // Mirror production: respond without reading the body. Dropping the
        // request afterwards is what used to panic.
        let req = server.recv().expect("recv request");
        let _ = req.respond(tiny_http::Response::empty(404u16));
    });

    let mut sock = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    sock.write_all(
        b"POST /yt/cb/x HTTP/1.1\r\nHost: test\r\nContent-Length: 18446744073709551615\r\n\r\n",
    )
    .expect("send request");
    // EOF the body so the (now bounded) drain finishes immediately.
    sock.shutdown(Shutdown::Write).expect("shutdown write half");

    handle
        .join()
        .expect("server must survive a request with an absurd Content-Length");

    let mut response = Vec::new();
    let _ = sock.read_to_end(&mut response);
}
