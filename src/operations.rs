//! Explicit maintenance commands. No installer, backup, restore or migration runs on startup.
use crate::Result;
use std::{
    io::Write,
    path::{Path, PathBuf},
    process::Stdio,
};
pub const TEX_IMAGE: &str =
    "texlive/texlive@sha256:540d736c78e2803cdd7b6a196bd21339c30185948db55e04e75a415959102ca8";
pub const MONGO_IMAGE: &str =
    "mongo@sha256:b9924c330a8bbc5800fe9c69f04a84e221eca7b28fd049a0354147d9908fa142";
pub fn source_offer(root: &Path) -> Result<Vec<u8>> {
    fn walk(
        base: &Path,
        dir: &Path,
        writer: &mut zip::ZipWriter<std::io::Cursor<Vec<u8>>>,
    ) -> Result<()> {
        for item in std::fs::read_dir(dir).map_err(|_| "Cannot enumerate source")? {
            let item = item.map_err(|_| "Cannot inspect source")?;
            let name = item.file_name();
            let name = name.to_string_lossy();
            if [
                ".git",
                "node_modules",
                ".venv",
                "__pycache__",
                "private",
                "dist",
                "target",
            ]
            .contains(&name.as_ref())
                || name.starts_with(".env")
            {
                continue;
            }
            let kind = item.file_type().map_err(|_| "Cannot inspect source type")?;
            if kind.is_symlink() {
                continue;
            }
            if kind.is_dir() {
                walk(base, &item.path(), writer)?;
            } else if kind.is_file() {
                let path = item.path();
                let rel = path.strip_prefix(base).map_err(|_| "Invalid source path")?;
                let extension = path.extension().and_then(|s| s.to_str()).unwrap_or("");
                if [
                    "rs", "toml", "lock", "mts", "ts", "cts", "sh", "css", "yml", "yaml", "txt",
                    "md", "service", "timer", "json", "sql",
                ]
                .contains(&extension)
                    || ["LICENSE", "NOTICE", "bridge-run", ".gitignore"].contains(&name.as_ref())
                {
                    writer
                        .start_file(
                            format!("cbgpu-latex/{}", rel.display()),
                            zip::write::SimpleFileOptions::default()
                                .compression_method(zip::CompressionMethod::Deflated),
                        )
                        .map_err(|_| "Cannot archive source")?;
                    std::io::copy(
                        &mut std::fs::File::open(path).map_err(|_| "Cannot read source")?,
                        writer,
                    )
                    .map_err(|_| "Cannot copy source")?;
                }
            }
        }
        Ok(())
    }
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    walk(root, root, &mut writer)?;
    Ok(writer
        .finish()
        .map_err(|_| "Cannot finish source offer")?
        .into_inner())
}
async fn capture_to(args: &[&str], path: &Path, database: Option<&str>) -> Result<()> {
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|_| "Cannot create private backup file")?;
    let mut command = tokio::process::Command::new(args[0]);
    command
        .args(&args[1..])
        .stdout(Stdio::from(file))
        .stderr(Stdio::null())
        .kill_on_drop(true);
    if let Some(database) = database {
        command.env("PGDATABASE", database);
    }
    let status = tokio::time::timeout(std::time::Duration::from_secs(600), command.status())
        .await
        .map_err(|_| "Backup command timed out")?
        .map_err(|_| "Cannot start backup command")?;
    if !status.success()
        || std::fs::metadata(path)
            .map_err(|_| "Cannot verify backup file")?
            .len()
            == 0
    {
        return Err("Empty or failed backup".into());
    }
    Ok(())
}
pub async fn backup(private: &Path, database: &str) -> Result<PathBuf> {
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::fs::PermissionsExt;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(private.join("backup.lock"))
        .map_err(|_| "Cannot lock private backup")?;
    fs2::FileExt::try_lock_exclusive(&lock)
        .map_err(|_| "Another LaTeX backup is already running")?;
    let backups = private.join("backups");
    std::fs::create_dir_all(&backups).map_err(|_| "Cannot create private backup directory")?;
    std::fs::set_permissions(&backups, std::fs::Permissions::from_mode(0o700))
        .map_err(|_| "Cannot protect backups")?;
    if fs2::available_space(private).map_err(|_| "Cannot measure backup disk space")?
        < 20 * 1024 * 1024 * 1024
    {
        return Err("At least20GiB free disk space is required for backup".into());
    }
    let ident = format!(
        "{}-{}",
        chrono::Utc::now().format("%Y%m%dT%H%M%SZ"),
        uuid::Uuid::new_v4()
    );
    let partial = backups.join(format!("{ident}.partial"));
    std::fs::create_dir(&partial).map_err(|_| "Cannot stage backup")?;
    let result=async{
  capture_to(&["docker","exec","cbgpu-latex-mongo","mongodump","--archive","--gzip","--oplog"],&partial.join("mongo.archive.gz"),None).await?;
  capture_to(&["docker","exec","cbgpu-latex-overleaf","tar","--exclude=data/compiles","--exclude=data/cache","-czf","-","-C","/var/lib/overleaf","data"],&partial.join("files.tar.gz"),None).await?;
  capture_to(&["docker","exec","cbgpu-latex-redis","sh","-c","redis-cli SAVE >/dev/null && cat /data/dump.rdb"],&partial.join("redis.rdb"),None).await?;
  let pool=sqlx::postgres::PgPoolOptions::new().max_connections(1).connect(database).await.map_err(|_|"Cannot connect private gateway backup state")?;
  write_state_backup(&pool,&partial.join("gateway-postgres.copy")).await?;pool.close().await;
  std::fs::write(partial.join("gateway-schema.sql"),include_str!("../migrations/20261116121608_latex_native_gateway_state.sql")).map_err(|_|"Cannot save gateway schema")?;
  let archive=std::fs::File::open(partial.join("files.tar.gz")).map_err(|_|"Cannot verify files backup")?;let mut tar=tar::Archive::new(flate2::read::GzDecoder::new(archive));for entry in tar.entries().map_err(|_|"Invalid files backup")?{let mut entry=entry.map_err(|_|"Corrupt files backup")?;std::io::copy(&mut entry,&mut std::io::sink()).map_err(|_|"Corrupt files backup data")?;}
  for name in ["bridge-key","overleaf.env","compiler-image"]{std::fs::copy(private.join(name),partial.join(name)).map_err(|_|"Cannot back up private configuration")?;}
  if private.join("gateway.sqlite").exists(){let source=rusqlite::Connection::open_with_flags(private.join("gateway.sqlite"),rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(|_|"Cannot read legacy gateway backup")?;let mut target=rusqlite::Connection::open(partial.join("gateway.sqlite")).map_err(|_|"Cannot create legacy gateway backup")?;rusqlite::backup::Backup::new(&source,&mut target).map_err(|_|"Cannot start legacy backup")?.run_to_completion(100,std::time::Duration::from_millis(10),None).map_err(|_|"Cannot complete legacy gateway backup")?;}
  std::fs::write(partial.join("RESTORE.txt"),"Stop CE before restoration. Restore Mongo archive+oplog to a separate fresh replica set, files.tar.gz to /var/lib/overleaf and Redis RDB before Redis startup. Keep original bridge-key/overleaf.env/compiler-image unchanged. Apply gateway-schema.sql through the backend migration workflow into a separate restored PostgreSQL database first, then use native restore-state --backup=ABSOLUTE_GATEWAY_POSTGRES_COPY --apply. Existing records are never overwritten. gateway.sqlite is a legacy snapshot only; never reset project IDs/owners/memberships. Native auth sessions are always revalidated. Keep the full backup private.\n").map_err(|_|"Cannot write restore guide")?;
  for entry in std::fs::read_dir(&partial).map_err(|_|"Cannot protect backup")?{std::fs::set_permissions(entry.map_err(|_|"Cannot inspect backup")?.path(),std::fs::Permissions::from_mode(0o600)).map_err(|_|"Cannot protect backup file")?;}
  let complete=backups.join(&ident);std::fs::rename(&partial,&complete).map_err(|_|"Cannot publish backup")?;
  let mut old=std::fs::read_dir(&backups).map_err(|_|"Cannot list backups")?.filter_map(|v|v.ok()).filter(|e|e.file_type().is_ok_and(|t|t.is_dir())&&!e.file_name().to_string_lossy().ends_with(".partial")).map(|e|e.path()).collect::<Vec<_>>();old.sort_by(|a,b|b.file_name().cmp(&a.file_name()));for path in old.into_iter().skip(7){std::fs::remove_dir_all(path).map_err(|_|"Cannot prune old backup")?;}Ok(complete)
 }.await;
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&partial);
    }
    result
}
fn private_file(path: &Path, value: &str) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    match std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(path)
    {
        Ok(mut f) => f
            .write_all(value.as_bytes())
            .map_err(|_| "Cannot initialize private configuration".into()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(_) => Err("Cannot create private configuration".into()),
    }
}
pub fn initialize(private: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(private).map_err(|_| "Cannot create private directory")?;
    std::fs::set_permissions(private, std::fs::Permissions::from_mode(0o700))
        .map_err(|_| "Cannot protect private directory")?;
    for name in ["run", "imports", "jobs", "overleaf", "mongo", "redis"] {
        std::fs::create_dir_all(private.join(name))
            .map_err(|_| "Cannot initialize private directory")?;
    }
    private_file(
        &private.join("bridge-key"),
        &format!("{}\n", crate::gateway::token()),
    )?;
    private_file(&private.join("compiler-image"), &format!("{TEX_IMAGE}\n"))?;
    private_file(
        &private.join("overleaf.env"),
        &format!(
            "OVERLEAF_APP_NAME=CBGPU LaTeX\nOVERLEAF_NAV_TITLE=CBGPU LaTeX\nOVERLEAF_SITE_URL=https://latex.cbgpu.com\nOVERLEAF_MONGO_URL=mongodb://mongo:27017/sharelatex?replicaSet=cbgpu-latex\nOVERLEAF_REDIS_HOST=redis\nOVERLEAF_SESSION_SECRET={}{}\nOVERLEAF_INVITE_TOKEN_SECRET={}\nOVERLEAF_ADMIN_EMAIL=info@cbgpu.com\nOVERLEAF_ALLOW_ANONYMOUS_READ_AND_WRITE_SHARING=false\nOVERLEAF_RESTRICT_INVITES_TO_EXISTING_ACCOUNTS=true\nOVERLEAF_SECURE_COOKIE=true\nCOMPILE_TIMEOUT=300\n",
            crate::gateway::token(),
            crate::gateway::token(),
            crate::gateway::token()
        ),
    )?;
    Ok(())
}
pub async fn install(source: &Path, private: &Path) -> Result<()> {
    initialize(private)?;
    async fn run(args: &[&str]) -> Result<()> {
        let status = tokio::process::Command::new(args[0])
            .args(&args[1..])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .map_err(|_| "Cannot run setup command")?;
        if !status.success() {
            return Err("Setup command failed; existing projects and secrets preserved".into());
        }
        Ok(())
    }
    let compose = source.join("compose.yml");
    let file = compose.to_str().ok_or("Invalid source directory")?;
    run(&[
        "docker", "compose", "-f", file, "up", "-d", "mongo", "redis",
    ])
    .await?;
    let mut ready = false;
    for _ in 0..30 {
        if run(&["docker","exec","cbgpu-latex-mongo","mongosh","--quiet","--eval","try {rs.status().ok} catch(e) {rs.initiate({_id:'cbgpu-latex',members:[{_id:0,host:'mongo:27017'}]})}"]).await.is_ok(){ready=true;break;}
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
    if !ready {
        return Err("Mongo replica set did not become ready".into());
    }
    run(&["docker", "compose", "-f", file, "up", "-d", "overleaf"]).await?;
    // ACL setup is an explicit privileged native command, never a Python container.
    let compiles = private.join("overleaf/data/compiles");
    for _ in 0..60 {
        if compiles.is_dir() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
    if !compiles.is_dir() {
        return Err("CE compile directory missing".into());
    }
    let uid = unsafe { libc::getuid() };
    let text = format!("u::rwx,u:{uid}:rwx,g::r-x,m::rwx,o::r-x");
    apply_acl(&compiles, &text)?;
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or("HOME unavailable")?;
    let unit = home.join(".config/systemd/user");
    std::fs::create_dir_all(&unit).map_err(|_| "Cannot create user units")?;
    let binary = std::env::current_exe().map_err(|_| "Cannot locate native executable")?;
    for entry in std::fs::read_dir(source.join("systemd")).map_err(|_| "Unit templates missing")? {
        let entry = entry.map_err(|_| "Cannot inspect unit")?;
        let content = std::fs::read_to_string(entry.path())
            .map_err(|_| "Cannot read unit")?
            .replace("@LATEX_SOURCE@", &source.display().to_string())
            .replace("@LATEX_PRIVATE@", &private.display().to_string())
            .replace("@LATEX_BINARY@", &binary.display().to_string());
        std::fs::write(unit.join(entry.file_name()), content).map_err(|_| "Cannot write unit")?;
    }
    run(&["systemctl", "--user", "daemon-reload"]).await?;
    run(&[
        "systemctl",
        "--user",
        "enable",
        "--now",
        "cbgpu-latex-compiler.service",
        "cbgpu-latex-gateway.service",
        "cbgpu-latex-backup.timer",
    ])
    .await?;
    Ok(())
}
pub fn apply_acl(path: &Path, text: &str) -> Result<()> {
    // libacl's stable C ABI; no interpreter and no world-readable private directory.
    unsafe {
        let library =
            libloading::Library::new("libacl.so.1").map_err(|_| "Native libacl unavailable")?;
        let from: libloading::Symbol<
            unsafe extern "C" fn(*const libc::c_char) -> *mut libc::c_void,
        > = library
            .get(b"acl_from_text")
            .map_err(|_| "Native ACL API unavailable")?;
        let set: libloading::Symbol<
            unsafe extern "C" fn(
                *const libc::c_char,
                libc::c_int,
                *mut libc::c_void,
            ) -> libc::c_int,
        > = library
            .get(b"acl_set_file")
            .map_err(|_| "Native ACL API unavailable")?;
        let free: libloading::Symbol<unsafe extern "C" fn(*mut libc::c_void) -> libc::c_int> =
            library
                .get(b"acl_free")
                .map_err(|_| "Native ACL API unavailable")?;
        let path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())
            .map_err(|_| "Invalid ACL path")?;
        let text = std::ffi::CString::new(text).map_err(|_| "Invalid ACL")?;
        let acl = from(text.as_ptr());
        if acl.is_null() {
            return Err("Invalid ACL entries".into());
        }
        let result = set(path.as_ptr(), 0x8000, acl) == 0 && set(path.as_ptr(), 0x4000, acl) == 0;
        free(acl);
        if !result {
            return Err("Compile directory ACL requires its owner/root: run the explicit native acl command with appropriate privileges, then rerun install".into());
        }
    }
    Ok(())
}
pub async fn verify_restore(private: &Path) -> Result<()> {
    let mut backups = std::fs::read_dir(private.join("backups"))
        .map_err(|_| "No private backups")?
        .filter_map(|v| v.ok())
        .filter(|v| {
            v.file_type().is_ok_and(|t| t.is_dir())
                && !v.file_name().to_string_lossy().ends_with(".partial")
        })
        .map(|v| v.path())
        .collect::<Vec<_>>();
    backups.sort();
    let latest = backups.last().ok_or("No complete backup")?;
    let name = format!("cbgpu-latex-restore-test-{}", uuid::Uuid::new_v4().simple());
    let result = async {
        let output = tokio::process::Command::new("docker")
            .args([
                "run",
                "-d",
                "--name",
                &name,
                "--network",
                "none",
                "--memory",
                "2g",
                "-e",
                "GLIBC_TUNABLES=glibc.pthread.rseq=1",
                "--tmpfs",
                "/data/db:size=1g",
                "--tmpfs",
                "/data/configdb:size=64m",
                MONGO_IMAGE,
                "mongod",
                "--bind_ip",
                "127.0.0.1",
                "--wiredTigerCacheSizeGB",
                "0.25",
            ])
            .output()
            .await
            .map_err(|_| "Cannot create isolated restore instance")?;
        if !output.status.success() {
            return Err("Cannot start isolated restore instance".into());
        }
        let mut ready = false;
        for _ in 0..30 {
            let out = tokio::process::Command::new("docker")
                .args([
                    "exec",
                    &name,
                    "mongosh",
                    "--quiet",
                    "--eval",
                    "db.adminCommand({ping:1}).ok",
                ])
                .output()
                .await
                .map_err(|_| "Cannot check isolated restore instance")?;
            if out.status.success() {
                ready = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
        if !ready {
            return Err("Isolated restore instance did not become ready".into());
        }
        let file = std::fs::File::open(latest.join("mongo.archive.gz"))
            .map_err(|_| "Backup archive missing")?;
        let out = tokio::time::timeout(
            std::time::Duration::from_secs(60),
            tokio::process::Command::new("docker")
                .args([
                    "exec",
                    "-i",
                    &name,
                    "mongorestore",
                    "--archive",
                    "--gzip",
                    "--oplogReplay",
                ])
                .stdin(Stdio::from(file))
                .kill_on_drop(true)
                .output(),
        )
        .await
        .map_err(|_| "Restore validation timed out")?
        .map_err(|_| "Cannot validate restore")?;
        if !out.status.success() {
            return Err("Mongo archive/oplog validation failed".into());
        }
        let out = tokio::process::Command::new("docker")
            .args([
                "exec",
                &name,
                "mongosh",
                "--quiet",
                "sharelatex",
                "--eval",
                "db.getCollectionNames().length",
            ])
            .output()
            .await
            .map_err(|_| "Cannot inspect isolated restored schema")?;
        if !out.status.success()
            || String::from_utf8_lossy(&out.stdout)
                .trim()
                .parse::<usize>()
                .unwrap_or(0)
                <= 10
        {
            return Err("Restored Mongo schema incomplete".into());
        }
        Ok(())
    }
    .await;
    let _ = tokio::process::Command::new("docker")
        .args(["rm", "-f", &name])
        .output()
        .await;
    result
}

/// COPY uses a single PostgreSQL MVCC snapshot; no host pg_dump/interpreter required.
pub async fn write_state_backup(pool: &sqlx::PgPool, path: &Path) -> Result<()> {
    use futures_util::StreamExt;
    use tokio::io::AsyncWriteExt;
    let mut connection = pool
        .acquire()
        .await
        .map_err(|_| "Cannot acquire backup connection")?;
    let mut stream = connection
        .copy_out_raw(
            "COPY latex_gateway.state(kind,id,data,expires) TO STDOUT WITH (FORMAT binary)",
        )
        .await
        .map_err(|_| "Cannot snapshot gateway state")?;
    let mut file = tokio::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(path)
        .await
        .map_err(|_| "Cannot create gateway backup")?;
    while let Some(bytes) = stream.next().await {
        file.write_all(&bytes.map_err(|_| "Gateway snapshot failed")?)
            .await
            .map_err(|_| "Cannot write gateway snapshot")?;
    }
    file.sync_all()
        .await
        .map_err(|_| "Cannot sync gateway snapshot")?;
    Ok(())
}
pub async fn restore_state(pool: &sqlx::PgPool, path: &Path) -> Result<u64> {
    use tokio::io::AsyncReadExt;
    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(|_| "Cannot open private gateway snapshot")?;
    let mut tx = pool
        .begin()
        .await
        .map_err(|_| "Cannot start gateway restore")?;
    sqlx::query("CREATE TEMP TABLE cbgpu_latex_restore (LIKE latex_gateway.state INCLUDING CONSTRAINTS) ON COMMIT DROP").execute(&mut *tx).await.map_err(|_|"Cannot stage gateway restore")?;
    let mut copy = tx
        .copy_in_raw(
            "COPY cbgpu_latex_restore(kind,id,data,expires) FROM STDIN WITH (FORMAT binary)",
        )
        .await
        .map_err(|_| "Cannot initialize gateway restore")?;
    let mut buffer = [0u8; 65536];
    loop {
        let n = file
            .read(&mut buffer)
            .await
            .map_err(|_| "Cannot read gateway snapshot")?;
        if n == 0 {
            break;
        }
        copy.send(&buffer[..n])
            .await
            .map_err(|_| "Invalid gateway snapshot")?;
    }
    copy.finish()
        .await
        .map_err(|_| "Incomplete gateway snapshot")?;
    let count=sqlx::query("INSERT INTO latex_gateway.state(kind,id,data,expires)SELECT kind,id,data,expires FROM cbgpu_latex_restore ON CONFLICT DO NOTHING").execute(&mut *tx).await.map_err(|_|"Cannot merge restored gateway state")?.rows_affected();
    tx.commit()
        .await
        .map_err(|_| "Cannot commit gateway restore")?;
    Ok(count)
}
