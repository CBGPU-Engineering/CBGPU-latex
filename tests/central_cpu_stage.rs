//! Explicit isolated School HTTP stage + NEW network-none TeX sandbox only.
use cbgpu_latex::{central_cpu::CentralCPU, compiler::Compiler, operations::TEX_IMAGE};
use serde_json::{Value, json};
use std::{collections::HashMap, os::unix::fs::PermissionsExt, path::PathBuf, sync::Arc};
use tokio::sync::{Mutex, Semaphore};
use tower::ServiceExt;
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "Requires explicit private synthetic stage context/DSN and reviewed cached TeX image; never production"]
async fn actual_stage_quota_seal_signed_admission_broker_pdf_and_released_lease() {
    assert_eq!(std::env::var("CBGPU_CENTRAL_CPU_V1").as_deref(), Ok("1"));
    let context_file = PathBuf::from(
        std::env::var("CBGPU_CENTRAL_CPU_TEST_CONTEXT").expect("explicit synthetic context"),
    );
    let dsn_file = PathBuf::from(
        std::env::var("CBGPU_CENTRAL_CPU_TEST_DSN_FILE").expect("explicit narrow stage DSN"),
    );
    assert!(
        context_file
            .to_string_lossy()
            .contains("central-cpu-runtime-fixture")
    );
    let mut context: Value = serde_json::from_slice(&std::fs::read(context_file).unwrap()).unwrap();
    assert_eq!(context["production_changes"], false);
    context["project_id"] = json!(uuid::Uuid::new_v4().simple().to_string()[..24].to_owned());
    let dsn = std::fs::read_to_string(dsn_file).unwrap();
    let parsed = url::Url::parse(dsn.trim()).unwrap();
    assert_eq!(parsed.host_str(), Some("127.0.0.1"));
    assert_eq!(parsed.port(), Some(55485));
    assert!(parsed.username().starts_with("cbgpu_latex_"));
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect(dsn.trim())
        .await
        .unwrap();
    let dir = tempfile::Builder::new()
        .prefix("cbgpu-central-latex-")
        .tempdir()
        .unwrap();
    std::fs::write(dir.path().join("central-cpu-admission-key"), [9u8; 32]).unwrap();
    std::fs::set_permissions(
        dir.path().join("central-cpu-admission-key"),
        std::fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    let authority = CentralCPU {
        pool,
        private: dir.path().to_owned(),
    };
    let project = context["project_id"].as_str().unwrap();
    let actor = context["actor_id"].as_str().unwrap();
    let session = context["actor_session_id"].as_str().unwrap();
    let owner = context["owner_id"].as_str().unwrap();
    let school = context["school_id"].as_str().unwrap();
    authority.register_unassigned(project, owner).unwrap();
    assert!(!authority.bound(project).unwrap());
    authority
        .allocate(actor, session, owner, project, school)
        .await
        .unwrap();
    std::fs::create_dir_all(dir.path().join("overleaf/data/compiles")).unwrap();
    std::fs::create_dir(dir.path().join("jobs")).unwrap();
    let folder = format!("{project}-isolated-central-fixture");
    let source = dir.path().join("overleaf/data/compiles").join(&folder);
    std::fs::create_dir(&source).unwrap();
    std::fs::write(
        source.join("main.tex"),
        r"\documentclass{article}\begin{document}Synthetic central CPU quota proof\end{document}",
    )
    .unwrap();
    let worker_id = uuid::Uuid::new_v4().to_string();
    let compiler = Compiler {
        private: dir.path().to_owned(),
        image: TEX_IMAGE.into(),
        docker: "/usr/bin/docker".into(),
        jobs: Arc::new(Mutex::new(HashMap::new())),
        limit: Arc::new(Semaphore::new(1)),
        central_cpu: Some(authority.clone()),
    };
    let jobs = compiler.jobs.clone();
    let slots = compiler.limit.clone();
    let router = compiler.router();
    let body = json!({"id":worker_id,"projectId":project,"directory":format!("/var/lib/overleaf/data/compiles/{folder}"),"command":["latexmk","-pdf","-jobname=output","-outdir=$COMPILE_DIR","$COMPILE_DIR/main.tex"],"timeout":30000});
    for claim in [project, "ffffffffffffffffffffffff", ""] {
        let mut forged = body.clone();
        forged["projectId"] = json!(claim);
        let denied = router
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/run")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(forged.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(denied.status(), 403);
        assert_eq!(slots.available_permits(), 1);
        assert!(jobs.lock().await.is_empty());
    }
    authority
        .prepare(actor, session, owner, project)
        .await
        .unwrap();
    let response = router
        .clone()
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
    let status = response.status();
    let value: Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), 5_000_000)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(status, 200, "private fixture broker failed: {value}");
    assert_eq!(value["exitCode"], 0);
    let replay_denied = router
        .clone()
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
    assert_eq!(replay_denied.status(), 403);
    assert_eq!(slots.available_permits(), 1);
    assert!(jobs.lock().await.is_empty());
    assert!(
        std::fs::read(source.join("output.pdf"))
            .unwrap()
            .starts_with(b"%PDF-")
    );
    assert!(
        !std::process::Command::new("docker")
            .args(["inspect", &format!("cbgpu-latex-job-{worker_id}")])
            .output()
            .unwrap()
            .status
            .success()
    );
    // Idempotent recovery returns the same already released lease, never another budget allocation.
    let file = dir
        .path()
        .join("central-cpu-admissions")
        .join(format!("{project}.claimed-{worker_id}.json"));
    let envelope: Value = serde_json::from_slice(&std::fs::read(file).unwrap()).unwrap();
    use base64::Engine;
    let saved: Value = serde_json::from_slice(
        &base64::engine::general_purpose::STANDARD
            .decode(envelope["payload"].as_str().unwrap())
            .unwrap(),
    )
    .unwrap();
    let mut payload = json!({});
    for k in [
        "school_id",
        "project_kind",
        "project_id",
        "actor_id",
        "actor_session_id",
        "owner_id",
        "purpose",
        "job_id",
        "idempotency_key",
        "lease_nonce",
        "cpu_millis",
        "memory_bytes",
        "storage_bytes",
    ] {
        payload[k] = saved[k].clone()
    }
    let replay = authority.call("acquire", payload).await.unwrap();
    assert_eq!(replay["state"], "RELEASED");
    assert_eq!(replay["allowed"], false);
}
