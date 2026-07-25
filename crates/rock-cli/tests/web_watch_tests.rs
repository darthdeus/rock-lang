use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn rock_bin() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_rock"))
}

fn reserve_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn http_get(port: u16, path: &str) -> std::io::Result<String> {
    let mut stream = TcpStream::connect(("127.0.0.1", port))?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n"
    )?;
    let mut response = String::new();
    stream.read_to_string(&mut response)?;
    let (head, body) = response.split_once("\r\n\r\n").unwrap_or((&response, ""));
    if !head.starts_with("HTTP/1.1 200") {
        return Err(std::io::Error::other(format!("GET {path}: {head}")));
    }
    Ok(body.to_string())
}

fn wait_http(port: u16, path: &str, needle: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Ok(body) = http_get(port, path) {
            if body.contains(needle) {
                return body;
            }
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {path} to contain {needle:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn subscribe(port: u16) -> BufReader<TcpStream> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    write!(
        stream,
        "GET /__rock_reload HTTP/1.1\r\nHost: 127.0.0.1\r\nAccept: text/event-stream\r\n\r\n"
    )
    .unwrap();
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    loop {
        line.clear();
        reader.read_line(&mut line).unwrap();
        if line == "\r\n" {
            break;
        }
    }
    reader
}

fn next_reload(reader: &mut BufReader<TcpStream>) -> std::io::Result<String> {
    loop {
        let mut line = String::new();
        reader.read_line(&mut line)?;
        if line.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "reload stream closed",
            ));
        }
        if let Some(data) = line.strip_prefix("data: ") {
            return Ok(data.trim().to_string());
        }
    }
}

fn wait_file_contains(path: &Path, needle: &str) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if std::fs::read_to_string(path).is_ok_and(|s| s.contains(needle)) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {} to contain {needle:?}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

struct ChildGuard {
    child: Child,
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn watch_serves_sources_reloads_on_green_and_keeps_last_green_on_error() {
    let td = tempfile::tempdir().unwrap();
    let project = td.path().join("proj");
    let out = td.path().join("out");
    let stderr_path = td.path().join("watch.stderr");
    std::fs::create_dir_all(project.join("assets")).unwrap();
    std::fs::write(
        project.join("stick.toml"),
        "[package]\nname = \"proj\"\n[release]\nassets = [\"assets\"]\n",
    )
    .unwrap();
    std::fs::write(
        project.join("main.rock"),
        "const VALUE: int = 1\n\
         @nomangle fn hot() { print(\"version-one\"); print(VALUE) }\n\
         fn main() { hot() }\n",
    )
    .unwrap();
    std::fs::write(project.join("assets/data.txt"), "asset-one").unwrap();

    let port = reserve_port();
    let stderr = std::fs::File::create(&stderr_path).unwrap();
    let child = Command::new(rock_bin())
        .args([
            "web",
            project.to_str().unwrap(),
            "--out",
            out.to_str().unwrap(),
            "--watch",
            "--port",
            &port.to_string(),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr))
        .spawn()
        .unwrap();
    let _guard = ChildGuard { child };

    wait_http(port, "/game.js", "version-one");
    assert_eq!(
        http_get(port, "/proj/assets/data.txt").unwrap(),
        "asset-one"
    );
    assert!(
        !out.join("proj/assets/data.txt").exists(),
        "watch mode must serve release assets from source"
    );

    let mut reload = subscribe(port);
    std::fs::write(
        project.join("main.rock"),
        "const VALUE: int = 1\n\
         @nomangle fn hot() { print(\"version-two\"); print(VALUE) }\n\
         fn main() { hot() }\n",
    )
    .unwrap();
    let event = next_reload(&mut reload).unwrap();
    assert!(event.contains("\"kind\":\"overlay\""), "{event}");
    wait_http(port, "/incremental.js", "version-two");
    wait_http(port, "/game.js", "version-two");

    let mut reload = subscribe(port);
    std::fs::write(
        project.join("main.rock"),
        "const VALUE: int = 2\n\
         @nomangle fn hot() { print(\"version-two\"); print(VALUE) }\n\
         fn main() { hot() }\n",
    )
    .unwrap();
    let event = next_reload(&mut reload).unwrap();
    assert!(event.contains("\"kind\":\"overlay\""), "{event}");

    let mut failed_reload = subscribe(port);
    std::fs::write(
        project.join("main.rock"),
        "fn main() { this is not valid rock",
    )
    .unwrap();
    wait_file_contains(&stderr_path, "serving last green browser build");
    failed_reload
        .get_ref()
        .set_read_timeout(Some(Duration::from_millis(700)))
        .unwrap();
    assert!(
        next_reload(&mut failed_reload).is_err(),
        "a failed build emitted a browser reload"
    );
    let game = http_get(port, "/game.js").unwrap();
    assert!(
        game.contains("version-two"),
        "compile error replaced the last green bundle"
    );
    assert!(
        http_get(port, "/incremental.js")
            .unwrap()
            .contains("version-two"),
        "compile error replaced the last green overlay"
    );

    let mut reload = subscribe(port);
    std::fs::write(
        project.join("main.rock"),
        "@nomangle fn hot(message: string) { print(message) }\n\
         fn main() { hot(\"version-three\") }\n",
    )
    .unwrap();
    let event = next_reload(&mut reload).unwrap();
    assert!(event.contains("\"kind\":\"reload\""), "{event}");
    wait_http(port, "/game.js", "version-three");

    let mut reload = subscribe(port);
    std::fs::write(project.join("assets/data.txt"), "asset-two").unwrap();
    let event = next_reload(&mut reload).unwrap();
    assert!(event.contains("\"kind\":\"reload\""), "{event}");
    assert_eq!(
        http_get(port, "/proj/assets/data.txt").unwrap(),
        "asset-two"
    );

    reload
        .get_ref()
        .set_read_timeout(Some(Duration::from_millis(700)))
        .unwrap();
    std::fs::write(out.join("probe.txt"), "ignored output write").unwrap();
    assert!(
        next_reload(&mut reload).is_err(),
        "a write under --out triggered another reload"
    );
}
