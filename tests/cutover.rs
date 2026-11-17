use cbgpu_latex::cutover;
use std::path::Path;
#[test]
fn active_and_queued_requests_are_not_mistaken_for_idle() {
    let path = Path::new("/private/run/compile.sock");
    let unix = "Num RefCount Protocol Flags Type St Inode Path\nA: 2 0 00010000 0001 01 11 /private/run/compile.sock\nB: 3 0 00000000 0001 03 12 /private/run/compile.sock\n";
    assert_eq!(cutover::unix_connections(unix, path), 1);
    assert_eq!(
        cutover::unix_backlog("u_str LISTEN 2 128 /private/run/compile.sock 11 * 0", path).unwrap(),
        2
    );
    assert!(
        cutover::unix_backlog(
            "u_str LISTEN unknown 128 /private/run/compile.sock 11 * 0",
            path
        )
        .is_err()
    );
    let tcp = "sl local_address rem_address st tx_queue rx_queue\n0: 0100007F:0DC0 0100007F:1234 01 0:0\n1: 0100007F:0DC0 00000000:0000 0A 0:0\n";
    assert_eq!(cutover::tcp_connections(tcp, 3520), 1);
}
#[test]
fn actual_accepted_unix_stream_is_visible_to_guard() {
    use std::os::unix::net::{UnixListener, UnixStream};
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("compile.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let client = UnixStream::connect(&path).unwrap();
    let (server, _) = listener.accept().unwrap();
    let text = std::fs::read_to_string("/proc/net/unix").unwrap();
    assert!(cutover::unix_connections(&text, &path) >= 1);
    drop(server);
    drop(client);
}
#[test]
fn staging_copies_immutable_binary_source_and_units_without_activation() {
    let tmp = tempfile::tempdir().unwrap();
    let source = tmp.path().join("source");
    let private = tmp.path().join("private");
    std::fs::create_dir_all(source.join("dist")).unwrap();
    std::fs::create_dir(source.join("systemd")).unwrap();
    std::fs::create_dir(&private).unwrap();
    std::fs::write(source.join("editor.css"), "body{}").unwrap();
    std::fs::write(source.join("dist/editor.js"), "console.log('public')").unwrap();
    for name in [
        "cbgpu-latex-gateway.service",
        "cbgpu-latex-compiler.service",
        "cbgpu-latex-backup.service",
        "cbgpu-latex-backup.timer",
    ] {
        std::fs::write(source.join("systemd").join(name),"[Service]\nExecStart=@LATEX_BINARY@ gateway\nEnvironment=CBGPU_LATEX_SOURCE=@LATEX_SOURCE@\nEnvironmentFile=-%h/.config/cbgpu/latex-rust.env\n").unwrap();
    }
    let stage = cutover::stage(&source, &private).unwrap();
    assert!(stage.join("cbgpu-latex").is_file());
    let unit = std::fs::read_to_string(stage.join("units/cbgpu-latex-gateway.service")).unwrap();
    assert!(!unit.contains(".partial"));
    assert!(unit.contains("EnvironmentFile=%h/"));
    assert!(stage.join("source/dist/editor.js").is_file());
    let metadata: serde_json::Value =
        serde_json::from_slice(&std::fs::read(stage.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(metadata["activated"], false);
    assert!(!metadata["source"].as_str().unwrap().contains(".partial"));
    assert!(cutover::stage(&source, &private).is_err());
}
#[test]
fn actual_unaccepted_unix_connection_is_visible_in_backlog() {
    use std::os::unix::net::{UnixListener, UnixStream};
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("compile.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let client = UnixStream::connect(&path).unwrap();
    let out = std::process::Command::new("ss")
        .args(["-xlnH"])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(cutover::unix_backlog(&String::from_utf8_lossy(&out.stdout), &path).unwrap() >= 1);
    drop(client);
    drop(listener);
}
