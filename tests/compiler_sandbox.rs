//! Own temporary sources and disposable containers; never mounts an existing CE volume.
use cbgpu_latex::{compiler::Compiler, operations::TEX_IMAGE};
use serde_json::{Value, json};
use std::{collections::HashMap, sync::Arc};
use tokio::sync::{Mutex, Semaphore};
use tower::ServiceExt;
async fn compile(
    router: axum::Router,
    private: &std::path::Path,
    files: &[(&str, &str)],
    timeout: u64,
) -> (std::path::PathBuf, Value) {
    let id = uuid::Uuid::new_v4().to_string();
    let folder = uuid::Uuid::new_v4().simple().to_string()
        + "-"
        + &uuid::Uuid::new_v4().simple().to_string();
    let path = private.join("overleaf/data/compiles").join(&folder);
    std::fs::create_dir(&path).unwrap();
    for (name, text) in files {
        std::fs::write(path.join(name), text).unwrap();
    }
    let body = json!({"id":id,"directory":format!("/var/lib/overleaf/data/compiles/{folder}"),"command":["latexmk","-pdf","-jobname=output","-outdir=$COMPILE_DIR","$COMPILE_DIR/main.tex"],"timeout":timeout});
    let response = router
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/run")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let result = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), 5_000_000)
            .await
            .unwrap(),
    )
    .unwrap();
    assert!(
        !std::process::Command::new("docker")
            .args(["inspect", &format!("cbgpu-latex-job-{id}")])
            .output()
            .unwrap()
            .status
            .success(),
        "Fixture sandbox leaked"
    );
    (path, result)
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires pinned TeX Live Docker image; only disposable network-none fixture containers"]
async fn real_tex_pdf_startup_script_isolation_symlink_and_timeout() {
    assert!(
        std::process::Command::new("docker")
            .args(["image", "inspect", TEX_IMAGE])
            .output()
            .unwrap()
            .status
            .success(),
        "Pull the reviewed pinned TeX image separately before this test"
    );
    let private = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(private.path().join("overleaf/data/compiles")).unwrap();
    std::fs::create_dir(private.path().join("jobs")).unwrap();
    let compiler = Compiler {
        private: private.path().to_owned(),
        image: TEX_IMAGE.into(),
        docker: "/usr/bin/docker".into(),
        jobs: Arc::new(Mutex::new(HashMap::new())),
        limit: Arc::new(Semaphore::new(1)),
        central_cpu: None,
    };
    let jobs = compiler.jobs.clone();
    let limit = compiler.limit.clone();
    let router = compiler.router();
    let (path, result) = compile(
        router.clone(),
        private.path(),
        &[(
            "main.tex",
            r"\documentclass{article}\begin{document}Native Rust sandbox PDF\end{document}",
        )],
        30000,
    )
    .await;
    assert_eq!(result["exitCode"], 0, "{result}");
    assert!(
        std::fs::read(path.join("output.pdf"))
            .unwrap()
            .starts_with(b"%PDF-")
    );
    let startup = r#"open(my $f, '>', 'output.probe'); print $f ((exists $ENV{'SUPABASE_SERVICE_ROLE_KEY'} || -e '/var/run/docker.sock' || -e '/var/lib/overleaf/data') ? 'LEAK' : 'ISOLATED'); close($f); system('ln -s /etc/passwd output.pdf'); exit(0);"#;
    let (path, _) = compile(
        router.clone(),
        private.path(),
        &[("main.tex", "ignored"), (".latexmkrc", startup)],
        30000,
    )
    .await;
    assert_eq!(
        std::fs::read(path.join("output.probe")).unwrap(),
        b"ISOLATED"
    );
    assert!(!path.join("output.pdf").exists());
    let (_, result) = compile(
        router.clone(),
        private.path(),
        &[
            ("main.tex", "ignored"),
            (".latexmkrc", "sleep 30; exit(0);"),
        ],
        1500,
    )
    .await;
    assert_eq!(result["exitCode"], 124);
    assert_eq!(result["terminated"], true);
    assert_eq!(result["timedout"], true);
    // A queued cancellation cannot spring back to life when a permit frees up.
    let folder = "queued-cancellation-project-00000000000";
    let path = private.path().join("overleaf/data/compiles").join(folder);
    std::fs::create_dir(&path).unwrap();
    std::fs::write(path.join("main.tex"), "ignored").unwrap();
    std::fs::write(path.join(".latexmkrc"), "sleep 30; exit(0);").unwrap();
    let first = uuid::Uuid::new_v4().to_string();
    let second = uuid::Uuid::new_v4().to_string();
    let request = |id: &str, timeout: u64| {
        axum::http::Request::builder().method("POST").uri("/run").header("content-type","application/json").body(axum::body::Body::from(json!({"id":id,"directory":format!("/var/lib/overleaf/data/compiles/{folder}"),"command":["latexmk","$COMPILE_DIR/main.tex"],"timeout":timeout}).to_string())).unwrap()
    };
    let task = tokio::spawn(router.clone().oneshot(request(&first, 1500)));
    for _ in 0..100 {
        if limit.available_permits() == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(limit.available_permits(), 0);
    let queued = tokio::spawn(router.clone().oneshot(request(&second, 30000)));
    for _ in 0..100 {
        if jobs.lock().await.contains_key(&second) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(jobs.lock().await.contains_key(&second));
    let response = router
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/kill")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(json!({"id":second}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let _ = task.await.unwrap().unwrap();
    let response = queued.await.unwrap().unwrap();
    let result: Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), 5_000_000)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(result["terminated"], true);
    assert_eq!(result["timedout"], false);
    assert!(
        !std::process::Command::new("docker")
            .args(["inspect", &format!("cbgpu-latex-job-{second}")])
            .output()
            .unwrap()
            .status
            .success()
    );
}
