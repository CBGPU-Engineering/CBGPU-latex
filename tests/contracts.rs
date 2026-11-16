use cbgpu_latex::{archive, compiler, gateway, operations};
use std::{
    io::{Read, Write},
    path::Path,
};
fn zip(path: &Path, files: &[(&str, &[u8])]) {
    let mut z = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
    for (name, body) in files {
        z.start_file(
            *name,
            zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated),
        )
        .unwrap();
        z.write_all(body).unwrap();
    }
    z.finish().unwrap();
}
#[test]
fn journal_zip_preserves_tree_main_choice_unicode_and_compiler() {
    let tmp = tempfile::tempdir().unwrap();
    let input = tmp.path().join("in.zip");
    let out = tmp.path().join("out.zip");
    zip(
        &input,
        &[
            (
                "paper/main.tex",
                b"\\documentclass{article}\\input{sections/intro}",
            ),
            ("paper/supplement.tex", b"\\documentclass{article}"),
            ("paper/sections/intro.tex", b"Research"),
            ("paper/figures/a.png", b"\x89PNG"),
            ("paper/refs.bib", b"@article{x}"),
            ("paper/journal.cls", b"%class"),
        ],
    );
    let info = archive::inspect(&input, &out).unwrap();
    assert_eq!(info.suggested_main, "main.tex");
    assert_eq!(info.candidates, vec!["main.tex", "supplement.tex"]);
    assert!(info.files.contains(&"sections/intro.tex".into()));
    let mut normalized = zip::ZipArchive::new(std::fs::File::open(&out).unwrap()).unwrap();
    let mut bytes = Vec::new();
    normalized
        .by_name("figures/a.png")
        .unwrap()
        .read_to_end(&mut bytes)
        .unwrap();
    assert_eq!(bytes, b"\x89PNG");
    zip(
        &input,
        &[
            (
                "paper/source/manuscript.tex",
                "\\documentclass{article}\\usepackage{kotex} 안녕".as_bytes(),
            ),
            ("paper/figures/a.pdf", b"%PDF"),
        ],
    );
    let info = archive::inspect(&input, &out).unwrap();
    assert_eq!(info.suggested_main, "source/manuscript.tex");
    assert_eq!(info.compiler, "xelatex");
    zip(
        &input,
        &[(
            "main.tex",
            b"%!TeX program = LuaLaTeX\n\\documentclass{article}",
        )],
    );
    assert_eq!(archive::inspect(&input, &out).unwrap().compiler, "lualatex");
}
#[test]
fn zip_rejects_traversal_collision_links_bombs_non_utf8_and_non_source() {
    let tmp = tempfile::tempdir().unwrap();
    let input = tmp.path().join("in.zip");
    let out = tmp.path().join("out.zip");
    for name in [
        "../evil.tex",
        "a/../../evil.tex",
        "/root.tex",
        "C:/main.tex",
        "a\\main.tex",
        "a/./main.tex",
        "a\nmain.tex",
    ] {
        zip(&input, &[(name, b"source")]);
        assert!(archive::inspect(&input, &out).is_err(), "{name}");
    }
    for files in [
        vec![("main.tex", &b"x"[..]), ("Main.tex", &b"y"[..])],
        vec![("é.tex", &b"x"[..]), ("e\u{301}.tex", &b"y"[..])],
        vec![("STRASSE.tex", &b"x"[..]), ("Straße.tex", &b"y"[..])],
    ] {
        zip(&input, &files);
        assert!(archive::inspect(&input, &out).is_err());
    }
    zip(
        &input,
        &[("main.tex", b"x"), ("bomb.bin", &vec![0; 3_000_000])],
    );
    assert!(archive::inspect(&input, &out).is_err());
    zip(&input, &[("paper.pdf", b"%PDF")]);
    assert!(archive::inspect(&input, &out).is_err());
    zip(&input, &[("main.tex", b"\xff")]);
    assert!(archive::inspect(&input, &out).is_err());
    let mut writer = zip::ZipWriter::new(std::fs::File::create(&input).unwrap());
    writer
        .add_symlink(
            "main.tex",
            "/etc/passwd",
            zip::write::SimpleFileOptions::default(),
        )
        .unwrap();
    writer.finish().unwrap();
    assert!(archive::inspect(&input, &out).is_err());
    std::fs::write(&input, b"notZIP").unwrap();
    assert!(archive::inspect(&input, &out).is_err());
}
#[test]
fn imports_have_no_50mb_archive_20mb_member_or_150mb_expanded_cap() {
    let tmp = tempfile::tempdir().unwrap();
    let source = tmp.path().join("large.zip");
    let out = tmp.path().join("normalized.zip");
    let mut z = zip::ZipWriter::new(std::fs::File::create(&source).unwrap());
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Stored)
        .large_file(true);
    z.start_file("main.tex", options).unwrap();
    z.write_all(b"\\documentclass{article}").unwrap();
    let block = vec![42u8; 1024 * 1024];
    for i in 0..3 {
        z.start_file(format!("asset-{i}.bin"), options).unwrap();
        for _ in 0..51 {
            z.write_all(&block).unwrap();
        }
    }
    z.finish().unwrap();
    assert!(std::fs::metadata(&source).unwrap().len() > 50 * 1024 * 1024);
    let info = archive::inspect(&source, &out).unwrap();
    assert!(info.size > 150 * 1024 * 1024);
    assert_eq!(info.count, 4);
    let mut normalized = zip::ZipArchive::new(std::fs::File::open(out).unwrap()).unwrap();
    assert_eq!(
        normalized.by_name("asset-2.bin").unwrap().size(),
        51 * 1024 * 1024
    );
}
#[test]
fn compiler_validates_paths_commands_snapshot_and_isolation() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join("overleaf/data/compiles");
    std::fs::create_dir_all(&base).unwrap();
    let id = "a".repeat(32);
    let dir = base.join(&id);
    std::fs::create_dir(&dir).unwrap();
    std::fs::write(dir.join("main.tex"), "source").unwrap();
    let mut data = serde_json::json!({"id":uuid::Uuid::new_v4().to_string(),"directory":format!("/var/lib/overleaf/data/compiles/{id}"),"command":["/usr/bin/latexmk","-outdir=$COMPILE_DIR","$COMPILE_DIR/main.tex"],"timeout":99999999});
    let job = compiler::validate(tmp.path(), &data).unwrap();
    assert_eq!(job.command[0], "latexmk");
    assert_eq!(job.command[1], "-outdir=/compile");
    assert_eq!(job.timeout.as_secs(), 300);
    let stage = tempfile::tempdir().unwrap();
    compiler::snapshot(&dir, stage.path()).unwrap();
    assert_eq!(
        std::fs::read(stage.path().join("main.tex")).unwrap(),
        b"source"
    );
    let args = compiler::sandbox_args(&job, stage.path(), operations::TEX_IMAGE, 1000, 1000);
    for value in [
        "none",
        "--read-only",
        "ALL",
        "no-new-privileges",
        "2g",
        "128",
        "openin_any=p",
        "openout_any=p",
    ] {
        assert!(args.iter().any(|s| s == value));
    }
    assert!(!args.join(" ").contains("docker.sock"));
    assert!(!args.join(" ").contains("bridge-key"));
    data["cwd"] = serde_json::json!("../../..");
    assert!(compiler::validate(tmp.path(), &data).is_err());
    data.as_object_mut().unwrap().remove("cwd");
    data["command"] = serde_json::json!(["sh", "-c", "evil"]);
    assert!(compiler::validate(tmp.path(), &data).is_err());
    data["directory"] = serde_json::json!("/etc");
    assert!(compiler::validate(tmp.path(), &data).is_err());
    std::os::unix::fs::symlink("/etc/passwd", dir.join("evil")).unwrap();
    assert!(compiler::snapshot(&dir, tempfile::tempdir().unwrap().path()).is_err());
}
#[test]
fn output_publisher_only_accepts_flat_regular_output_files() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("result.tar");
    let output = tmp.path().join("published");
    std::fs::create_dir(&output).unwrap();
    let mut tar = tar::Builder::new(std::fs::File::create(&path).unwrap());
    for (name, kind, bytes) in [
        ("output.pdf", tar::EntryType::Regular, &b"%PDF-fixture"[..]),
        ("output.link", tar::EntryType::Symlink, &b""[..]),
        (
            "nested/output.secret",
            tar::EntryType::Regular,
            &b"forbidden"[..],
        ),
        ("main.tex", tar::EntryType::Regular, &b"source"[..]),
    ] {
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o600);
        header.set_entry_type(kind);
        if kind.is_symlink() {
            header.set_link_name("/etc/passwd").unwrap();
        }
        header.set_cksum();
        tar.append_data(&mut header, name, bytes).unwrap();
    }
    tar.finish().unwrap();
    assert_eq!(compiler::publish(&path, &output).unwrap(), 1);
    assert_eq!(
        std::fs::read(output.join("output.pdf")).unwrap(),
        b"%PDF-fixture"
    );
    assert!(!output.join("output.link").exists());
    assert!(!output.join("main.tex").exists());
}
#[tokio::test]
async fn compiler_output_is_bounded_and_retains_the_final_diagnostics() {
    let bytes = vec![b'a'; 2_100_000];
    assert_eq!(
        compiler::capture(&bytes[..]).await.unwrap().len(),
        2_000_000
    );
    assert!(compiler::capture(&vec![0; 8_000_001][..]).await.is_err());
}
#[test]
fn proxy_cookie_boundary_and_encoded_paths_are_strict() {
    use axum::http::{HeaderMap, HeaderValue};
    let mut h = HeaderMap::new();
    for (k, v) in [
        ("cookie", "browser=secret"),
        ("authorization", "Bearer secret"),
        ("x-cbgpu-key", "secret"),
        ("x-cbgpu-identity", "secret"),
        ("x-forwarded-for", "attacker"),
        ("set-cookie", "steal=secret"),
        ("connection", "x-private"),
        ("x-private", "secret"),
        ("content-type", "text/html"),
    ] {
        h.insert(
            axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
            HeaderValue::from_str(v).unwrap(),
        );
    }
    let out = gateway::filtered_headers(&h, true);
    for name in [
        "cookie",
        "authorization",
        "x-cbgpu-key",
        "x-cbgpu-identity",
        "x-forwarded-for",
        "set-cookie",
        "x-private",
    ] {
        assert!(!out.contains_key(name));
    }
    assert_eq!(out["content-type"], "text/html");
    assert!(!gateway::filtered_headers(&h, false).contains_key("set-cookie"));
    for path in [
        "/project/%2e%2e/output",
        "/project/%252e%252e/output",
        "/project//id",
        "/project/%5csecret",
        "/project/%00",
    ] {
        assert!(gateway::canonical_path(path).is_err());
    }
    assert_eq!(
        gateway::canonical_path("/project/abcdef012345abcdef012345/output/output.pdf").unwrap(),
        "/project/abcdef012345abcdef012345/output/output.pdf"
    );
}
#[test]
fn initialize_preserves_secrets_and_source_offer_excludes_runtime_data() {
    let tmp = tempfile::tempdir().unwrap();
    let private = tmp.path().join("private");
    operations::initialize(&private).unwrap();
    let key = std::fs::read(private.join("bridge-key")).unwrap();
    operations::initialize(&private).unwrap();
    assert_eq!(std::fs::read(private.join("bridge-key")).unwrap(), key);
    assert!(
        std::fs::read_to_string(private.join("overleaf.env"))
            .unwrap()
            .contains("OVERLEAF_ALLOW_ANONYMOUS_READ_AND_WRITE_SHARING=false")
    );
    std::fs::write(tmp.path().join(".env"), "SECRET").unwrap();
    std::fs::write(tmp.path().join("main.rs"), "native source").unwrap();
    let bytes = operations::source_offer(tmp.path()).unwrap();
    let mut z = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
    assert!(z.by_name("cbgpu-latex/main.rs").is_ok());
    assert!(z.by_name("cbgpu-latex/.env").is_err());
    assert!(z.by_name("cbgpu-latex/private/bridge-key").is_err());
}

#[test]
fn maintenance_defaults_to_a_plan_without_creating_state_or_contacting_services() {
    let tmp = tempfile::tempdir().unwrap();
    let private = tmp.path().join("must-not-exist");
    for action in [
        "install",
        "backup",
        "migrate",
        "restore-state",
        "import-legacy",
        "verify-restore",
        "acl",
    ] {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_cbgpu-latex"))
            .arg(action)
            .env("CBGPU_LATEX_PRIVATE", &private)
            .env_remove("CBGPU_LATEX_DATABASE_URL")
            .output()
            .unwrap();
        assert!(output.status.success());
        let plan: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(plan["apply"], false);
        assert!(!private.exists());
    }
}
