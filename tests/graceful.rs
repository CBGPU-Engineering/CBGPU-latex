#![cfg(unix)]
use std::{
    path::PathBuf,
    process::{Command, Stdio},
    time::{Duration, Instant},
};
#[test]
fn signal_child() {
    let Ok(dir) = std::env::var("CBGPU_LATEX_DRAIN_TEST_DIR") else {
        return;
    };
    let root = PathBuf::from(dir);
    assert!(
        root.file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("cbgpu-latex-drain-")
    );
    tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(async move {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            std::fs::write(
                root.join("port"),
                listener.local_addr().unwrap().port().to_string(),
            )
            .unwrap();
            let handler_root = root.clone();
            let app = axum::Router::new().route(
                "/",
                axum::routing::get(move || {
                    let root = handler_root.clone();
                    async move {
                        std::fs::write(root.join("entered"), b"1").unwrap();
                        while !root.join("release").exists() {
                            tokio::time::sleep(Duration::from_millis(10)).await;
                        }
                        "accepted request completed"
                    }
                }),
            );
            cbgpu_latex::shutdown::serve(listener, app).await.unwrap();
        });
}
async fn wait_file(p: &std::path::Path) {
    let start = Instant::now();
    while !p.exists() {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "test child readiness"
        );
        tokio::time::sleep(Duration::from_millis(10)).await
    }
}
#[tokio::test]
async fn actual_sigterm_stops_listener_and_drains_accepted_http() {
    for inflight in [false, true] {
        let dir = tempfile::Builder::new()
            .prefix("cbgpu-latex-drain-")
            .tempdir()
            .unwrap();
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "signal_child", "--nocapture"])
            .env("CBGPU_LATEX_DRAIN_TEST_DIR", dir.path())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        wait_file(&dir.path().join("port")).await;
        let port: u16 = std::fs::read_to_string(dir.path().join("port"))
            .unwrap()
            .parse()
            .unwrap();
        let url = format!("http://127.0.0.1:{port}/");
        let pending = if inflight {
            let url = url.clone();
            let read =
                tokio::spawn(async move { reqwest::get(url).await.unwrap().text().await.unwrap() });
            wait_file(&dir.path().join("entered")).await;
            Some(read)
        } else {
            None
        };
        // Allow the real signal listener to register before delivering SIGTERM.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGTERM) }, 0);
        tokio::time::sleep(Duration::from_millis(150)).await;
        if inflight {
            assert!(
                child.try_wait().unwrap().is_none(),
                "accepted request must not be killed"
            );
            assert!(
                tokio::net::TcpStream::connect(("127.0.0.1", port))
                    .await
                    .is_err(),
                "new admissions must stop"
            );
            std::fs::write(dir.path().join("release"), b"1").unwrap();
            assert_eq!(
                pending.unwrap().await.unwrap(),
                "accepted request completed"
            )
        }
        let started = Instant::now();
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success());
                break;
            }
            if started.elapsed() > Duration::from_secs(10) {
                let _ = child.kill();
                panic!("test child failed to drain")
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}
