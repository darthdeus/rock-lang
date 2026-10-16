//! A panic in an LSP request handler must terminate the server process, not
//! wedge it. Before the fix, `catch_unwind` swallowed the panic and the loop
//! fell through to a blocking `io_threads.join()` waiting on stdin EOF -- so a
//! panicked server stayed alive holding its compiled session in RAM, invisible
//! to the client (no crash, requests silently hang). This drives the real
//! binary: handshake, fire the gated `$/rockDebugPanic` request, and assert the
//! process exits on its own within a few seconds rather than hanging.

use std::io::{BufReader, Read, Write};
use std::process::{Stdio};
use std::time::{Duration, Instant};

fn frame(body: &str) -> String {
    format!("Content-Length: {}\r\n\r\n{}", body.len(), body)
}

#[test]
fn panicking_handler_exits_process_not_wedge() {
    let mut child = crate::command::rock_bin()
        .args(["lsp", "serve"])
        .env("ROCK_LSP_ALLOW_TEST_PANIC", "1")
        .env("ROCK_LSP_BACKEND", "bone")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn rock lsp serve");

    let mut stdin = child.stdin.take().unwrap();
    // Read stdout on a thread so the server never blocks on a full pipe.
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        while let Ok(n) = stdout.read(&mut buf) {
            if n == 0 {
                break;
            }
        }
    });

    let init = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"processId":null,"capabilities":{}}}"#;
    stdin.write_all(frame(init).as_bytes()).unwrap();
    stdin
        .write_all(frame(r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#).as_bytes())
        .unwrap();
    // Give the handshake a beat, then trigger the panic in a request handler.
    std::thread::sleep(Duration::from_millis(300));
    let boom = r#"{"jsonrpc":"2.0","id":2,"method":"$/rockDebugPanic","params":{}}"#;
    stdin.write_all(frame(boom).as_bytes()).unwrap();
    stdin.flush().unwrap();

    // The process must exit on its own. Poll rather than block forever so a
    // wedge (the bug) fails as a timeout instead of hanging the suite.
    let deadline = Instant::now() + test_utils::HANG_BACKSTOP;
    loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            assert!(
                !status.success(),
                "server should exit non-zero after a handler panic, got {status:?}"
            );
            return;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("server did not exit within {:?} after a handler panic -- it wedged", test_utils::HANG_BACKSTOP);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}
