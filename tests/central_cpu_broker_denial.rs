//! Private synthetic broker traffic only: no database, Docker, project, or school mutation.
use cbgpu_latex::{central_cpu::CentralCPU, compiler::Compiler};
use serde_json::json;
use std::{collections::HashMap, os::unix::fs::PermissionsExt, sync::Arc};
use tokio::sync::{Mutex, Semaphore};
use tower::ServiceExt;
#[tokio::test]
async fn central_unsigned_wrong_project_directory_and_replay_never_take_slot_or_spawn() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("central-cpu-admission-key"), [5u8; 32]).unwrap();
    std::fs::set_permissions(
        dir.path().join("central-cpu-admission-key"),
        std::fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    let project = "abcdefabcdefabcdefabcdef";
    let pool = sqlx::postgres::PgPoolOptions::new()
        .connect_lazy("postgres://unused:unused@127.0.0.1:1/unused")
        .unwrap();
    let authority = CentralCPU {
        pool,
        private: dir.path().to_owned(),
    };
    authority
        .register_unassigned(project, "11111111-1111-4111-8111-111111111111")
        .unwrap();
    let folder = format!("{project}-private-fixture");
    let source = dir.path().join("overleaf/data/compiles").join(&folder);
    std::fs::create_dir_all(&source).unwrap();
    std::fs::write(source.join("main.tex"), "synthetic").unwrap();
    let jobs = Arc::new(Mutex::new(HashMap::new()));
    let limit = Arc::new(Semaphore::new(3));
    let state = Compiler {
        private: dir.path().to_owned(),
        image: "never-used".into(),
        docker: "/nonexistent-must-not-spawn-docker".into(),
        jobs: jobs.clone(),
        limit: limit.clone(),
        central_cpu: Some(authority),
    };
    let router = state.router();
    for claimed in [project, "ffffffffffffffffffffffff", ""] {
        let body = json!({"id":uuid::Uuid::new_v4(),"projectId":claimed,"directory":format!("/var/lib/overleaf/data/compiles/{folder}"),"command":["latexmk","-pdf","$COMPILE_DIR/main.tex"],"timeout":30000});
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
        assert_eq!(response.status(), 403);
        assert_eq!(limit.available_permits(), 3);
        assert!(jobs.lock().await.is_empty());
    }
    // A forged unsigned "legacy" context cannot replace the signed private mode.
    let admission = dir.path().join("central-cpu-admissions");
    std::fs::create_dir(&admission).unwrap();
    std::fs::write(
        admission.join(format!("{project}.json")),
        json!({"version":1,"payload":"e30=","mac":"00"}).to_string(),
    )
    .unwrap();
    let body = json!({"id":uuid::Uuid::new_v4(),"projectId":project,"directory":format!("/var/lib/overleaf/data/compiles/{folder}"),"command":["latexmk","-pdf","$COMPILE_DIR/main.tex"]});
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
    assert_eq!(response.status(), 403);
    assert_eq!(limit.available_permits(), 3);
    assert!(jobs.lock().await.is_empty());
}
