//! Read-only activity guard and immutable unit staging. Activation is deliberately
//! coordinated by the operator; this module never stops services or containers.
use crate::{Result, operations};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    process::Command,
};
#[derive(Debug, Serialize, Deserialize)]
pub struct Activity {
    pub gateway_pid: u32,
    pub compiler_pid: u32,
    pub gateway_connections: usize,
    pub compiler_requests: usize,
    pub compiler_backlog: usize,
    pub compile_containers: usize,
    pub gateway_port_free: bool,
    pub safe_to_stop_compiler: bool,
}
fn pid(unit: &str) -> Result<u32> {
    let out = Command::new("systemctl")
        .args(["--user", "show", unit, "--property=MainPID", "--value"])
        .output()
        .map_err(|_| "Cannot inspect LaTeX service ownership")?;
    if !out.status.success() {
        return Err("Cannot verify LaTeX service ownership".into());
    }
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .map_err(|_| "Invalid LaTeX service PID".into())
}
pub fn tcp_connections(text: &str, port: u16) -> usize {
    text.lines()
        .skip(1)
        .filter(|line| {
            let f: Vec<_> = line.split_whitespace().collect();
            f.len() > 4
                && f[3] != "0A"
                && f[3] != "06"
                && f[1]
                    .rsplit(':')
                    .next()
                    .and_then(|p| u16::from_str_radix(p, 16).ok())
                    == Some(port)
        })
        .count()
}
pub fn unix_connections(text: &str, path: &Path) -> usize {
    text.lines()
        .skip(1)
        .filter(|line| {
            let f: Vec<_> = line.split_whitespace().collect();
            f.len() >= 8
                && f[3] != "00010000"
                && ["02", "03"].contains(&f[5])
                && f[7..].join(" ") == path.to_string_lossy()
        })
        .count()
}
pub fn unix_backlog(text: &str, path: &Path) -> Result<usize> {
    let wanted = path.to_string_lossy();
    let mut found = None;
    for line in text.lines() {
        let f: Vec<_> = line.split_whitespace().collect();
        if f.iter().any(|s| *s == wanted) {
            let index = f
                .iter()
                .position(|s| *s == "LISTEN")
                .ok_or("Unexpected compiler listener state")?;
            if f.len() <= index + 1 {
                return Err("Cannot inspect compiler listener queue".into());
            }
            let queue = f[index + 1]
                .parse::<usize>()
                .map_err(|_| "Invalid compiler listener queue")?;
            if found.replace(queue).is_some() {
                return Err("Ambiguous compiler listener queue".into());
            }
        }
    }
    Ok(found.unwrap_or(0))
}
pub fn activity(private: &Path) -> Result<Activity> {
    let gateway_pid = pid("cbgpu-latex-gateway.service")?;
    let compiler_pid = pid("cbgpu-latex-compiler.service")?;
    let tcp = std::fs::read_to_string("/proc/net/tcp")
        .map_err(|_| "Cannot inspect gateway connections")?;
    let tcp6 = std::fs::read_to_string("/proc/net/tcp6")
        .map_err(|_| "Cannot inspect gateway IPv6 connections")?;
    let unix =
        std::fs::read_to_string("/proc/net/unix").map_err(|_| "Cannot inspect compile requests")?;
    let socket = private
        .canonicalize()
        .map_err(|_| "Invalid private state directory")?
        .join("run/compile.sock");
    let compiler_requests = unix_connections(&unix, &socket);
    let ss = Command::new("ss")
        .args(["-xlnH"])
        .output()
        .map_err(|_| "Cannot inspect queued compile requests")?;
    if !ss.status.success() {
        return Err("Cannot inspect queued compile requests".into());
    }
    let compiler_backlog = unix_backlog(&String::from_utf8_lossy(&ss.stdout), &socket)?;
    let containers = Command::new("docker")
        .args(["ps", "-aq", "--filter", "name=^cbgpu-latex-job-"])
        .output()
        .map_err(|_| "Cannot inspect compile sandboxes")?;
    if !containers.status.success() {
        return Err("Cannot inspect compile sandbox ownership".into());
    }
    let compile_containers = String::from_utf8_lossy(&containers.stdout)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .count();
    let gateway_connections = tcp_connections(&tcp, 3520) + tcp_connections(&tcp6, 3520);
    let gateway_port_free =
        std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 3520)).is_ok();
    Ok(Activity {
        gateway_pid,
        compiler_pid,
        gateway_connections,
        compiler_requests,
        compiler_backlog,
        compile_containers,
        gateway_port_free,
        safe_to_stop_compiler: compiler_requests == 0
            && compiler_backlog == 0
            && compile_containers == 0,
    })
}
pub fn require_gateway_stopped(private: &Path) -> Result<()> {
    let state = activity(private)?;
    if state.gateway_pid != 0 || !state.gateway_port_free || state.gateway_connections != 0 {
        return Err("Stop and drain the gateway before importing/exporting transient state; existing users were preserved".into());
    }
    Ok(())
}
fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .and_then(|mut f| f.write_all(bytes))
        .map_err(|_| "Cannot stage private handoff artifact".into())
}
/// Does not install units, migrate, import, start/stop or edit the CE stack.
pub fn stage(source: &Path, private: &Path) -> Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    for p in [source, private] {
        if !p.is_absolute()
            || p.to_string_lossy()
                .chars()
                .any(|c| c.is_whitespace() || c == '\"')
        {
            return Err("Unit paths must be absolute and contain no whitespace/quotes".into());
        }
    }
    let executable = std::env::current_exe().map_err(|_| "Cannot inspect native executable")?;
    let binary = std::fs::read(executable).map_err(|_| "Cannot stage native binary")?;
    if !binary.starts_with(b"\x7fELF") {
        return Err("Expected a native ELF binary".into());
    }
    let offered = operations::source_offer(source)?;
    let mut hash = Sha256::new();
    hash.update(&binary);
    hash.update(&offered);
    let ident = hex::encode(hash.finalize());
    let root = private.join("native-releases");
    std::fs::create_dir_all(&root).map_err(|_| "Cannot stage release directory")?;
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))
        .map_err(|_| "Cannot protect release directory")?;
    let final_release = root.join(&ident);
    let release = root.join(format!("{ident}.partial"));
    std::fs::create_dir(&release).map_err(
        |_| "Release already staged; verify its existing manifest instead of overwriting",
    )?;
    struct Partial(PathBuf);
    impl Drop for Partial {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _partial = Partial(release.clone());
    write_new(&release.join("cbgpu-latex"), &binary)?;
    std::fs::set_permissions(
        release.join("cbgpu-latex"),
        std::fs::Permissions::from_mode(0o500),
    )
    .map_err(|_| "Cannot protect staged binary")?;
    write_new(&release.join("source.zip"), &offered)?;
    let snapshot = release.join("source");
    std::fs::create_dir(&snapshot).map_err(|_| "Cannot stage source snapshot")?;
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(offered))
        .map_err(|_| "Cannot read source offer")?;
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i).map_err(|_| "Cannot inspect source offer")?;
        let name = entry.enclosed_name().ok_or("Unsafe source snapshot path")?;
        let rel = name
            .strip_prefix("cbgpu-latex")
            .map_err(|_| "Invalid source prefix")?;
        let target = snapshot.join(rel);
        if entry.is_dir() {
            std::fs::create_dir_all(&target).map_err(|_| "Cannot stage source folder")?;
            continue;
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|_| "Cannot stage source folder")?;
        }
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut entry, &mut bytes)
            .map_err(|_| "Cannot read source file")?;
        write_new(&target, &bytes)?;
    }
    std::fs::create_dir(snapshot.join("dist")).map_err(|_| "Cannot stage editor assets")?;
    let asset = source.join("dist/editor.js");
    if !std::fs::symlink_metadata(&asset).is_ok_and(|m| m.file_type().is_file()) {
        return Err("Build and review the editor asset before staging".into());
    }
    write_new(
        &snapshot.join("dist/editor.js"),
        &std::fs::read(asset).map_err(|_| "Cannot read editor asset")?,
    )?;
    let units = release.join("units");
    let rollback = release.join("rollback-units");
    std::fs::create_dir(&units).map_err(|_| "Cannot stage units")?;
    std::fs::create_dir(&rollback).map_err(|_| "Cannot stage rollback units")?;
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or("Home directory unavailable")?;
    let allowed: HashSet<&str> = [
        "cbgpu-latex-gateway.service",
        "cbgpu-latex-compiler.service",
        "cbgpu-latex-backup.service",
        "cbgpu-latex-backup.timer",
    ]
    .into_iter()
    .collect();
    for name in allowed {
        let template = std::fs::read_to_string(source.join("systemd").join(name))
            .map_err(|_| "Native unit template missing")?;
        let content = template
            .replace(
                "@LATEX_SOURCE@",
                &final_release.join("source").display().to_string(),
            )
            .replace("@LATEX_PRIVATE@", &private.display().to_string())
            .replace(
                "@LATEX_BINARY@",
                &final_release.join("cbgpu-latex").display().to_string(),
            )
            .replace("EnvironmentFile=-", "EnvironmentFile=");
        write_new(&units.join(name), content.as_bytes())?;
        let installed = home.join(".config/systemd/user").join(name);
        if installed.exists() {
            if !std::fs::symlink_metadata(&installed).is_ok_and(|m| m.file_type().is_file()) {
                return Err("Refusing non-regular installed unit snapshot".into());
            }
            write_new(
                &rollback.join(name),
                &std::fs::read(installed).map_err(|_| "Cannot snapshot installed unit")?,
            )?;
        }
    }
    write_new(&release.join("manifest.json"),serde_json::to_vec_pretty(&serde_json::json!({"binary_sha256":hex::encode(Sha256::digest(&binary)),"source":final_release.join("source"),"private":private,"units":final_release.join("units"),"rollback_units":final_release.join("rollback-units"),"activated":false,"requires_private_env":"~/.config/cbgpu/latex-rust.env","account_v2_default":false,"preserves_ce_containers":true})).map_err(|_|"Cannot create handoff manifest")?.as_slice())?;
    std::fs::rename(&release, &final_release).map_err(|_| "Cannot publish staged release")?;
    Ok(final_release)
}
