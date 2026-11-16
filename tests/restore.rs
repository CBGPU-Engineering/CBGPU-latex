//! Restore only a synthetic replica-set archive to a second network-none fixture.
use cbgpu_latex::operations::{self, MONGO_IMAGE};
use std::{process::Command, time::Duration};
struct Source(String);
impl Drop for Source {
    fn drop(&mut self) {
        let _ = Command::new("docker")
            .args(["rm", "-f", "-v", &self.0])
            .output();
    }
}
#[tokio::test]
#[ignore = "requires pinned Mongo image; two temporary disconnected instances, no live archives"]
async fn synthetic_mongo_oplog_archive_restores_without_touching_existing_stack() {
    assert!(
        Command::new("docker")
            .args(["image", "inspect", MONGO_IMAGE])
            .output()
            .unwrap()
            .status
            .success()
    );
    let source = Source(format!(
        "cbgpu-latex-source-fixture-{}",
        uuid::Uuid::new_v4().simple()
    ));
    let output = Command::new("docker")
        .args([
            "run",
            "-d",
            "--name",
            &source.0,
            "--network",
            "none",
            "--memory",
            "1g",
            "--cpus",
            "1",
            "-e",
            "GLIBC_TUNABLES=glibc.pthread.rseq=1",
            "--tmpfs",
            "/data/db:size=256m",
            "--tmpfs",
            "/data/configdb:size=64m",
            MONGO_IMAGE,
            "mongod",
            "--bind_ip",
            "127.0.0.1",
            "--replSet",
            "fixture",
            "--wiredTigerCacheSizeGB",
            "0.25",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let mut ready = false;
    for _ in 0..60 {
        if Command::new("docker")
            .args([
                "exec",
                &source.0,
                "mongosh",
                "--quiet",
                "--eval",
                "db.adminCommand({ping:1}).ok",
            ])
            .output()
            .is_ok_and(|o| o.status.success())
        {
            ready = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(ready);
    assert!(
        Command::new("docker")
            .args([
                "exec",
                &source.0,
                "mongosh",
                "--quiet",
                "--eval",
                "rs.initiate({_id:'fixture',members:[{_id:0,host:'localhost:27017'}]})"
            ])
            .output()
            .unwrap()
            .status
            .success()
    );
    let mut ready = false;
    for _ in 0..60 {
        let out = Command::new("docker")
            .args([
                "exec",
                &source.0,
                "mongosh",
                "--quiet",
                "--eval",
                "db.hello().isWritablePrimary",
            ])
            .output()
            .unwrap();
        if out.status.success() && String::from_utf8_lossy(&out.stdout).trim() == "true" {
            ready = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(ready);
    assert!(Command::new("docker").args(["exec",&source.0,"mongosh","--quiet","--eval","for(let i=0;i<11;i++)db.getSiblingDB('sharelatex').getCollection('fixture_'+i).insertOne({fixture:true})"]).output().unwrap().status.success());
    let private = tempfile::tempdir().unwrap();
    let backup = private.path().join("backups/20261116T000000Z");
    std::fs::create_dir_all(&backup).unwrap();
    let archive = std::fs::File::create(backup.join("mongo.archive.gz")).unwrap();
    assert!(
        Command::new("docker")
            .args([
                "exec",
                &source.0,
                "mongodump",
                "--archive",
                "--gzip",
                "--oplog"
            ])
            .stdout(archive)
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
            .success()
    );
    operations::verify_restore(private.path()).await.unwrap();
}
