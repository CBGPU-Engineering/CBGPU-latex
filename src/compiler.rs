//! Private Unix-socket broker; TeX sees only a read-only source snapshot.
use crate::Result;
use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    io::Write,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
    sync::{Mutex, Semaphore},
};
#[derive(Clone)]
pub enum JobState {
    Active(String),
    Cancelled,
}
#[derive(Clone)]
pub struct Compiler {
    pub private: PathBuf,
    pub image: String,
    pub docker: PathBuf,
    pub jobs: Arc<Mutex<HashMap<String, JobState>>>,
    pub limit: Arc<Semaphore>,
}
#[derive(Debug)]
pub struct Validated {
    pub id: String,
    pub name: String,
    pub directory: PathBuf,
    pub command: Vec<String>,
    pub work: String,
    pub timeout: Duration,
}
pub fn validate(root: &Path, data: &Value) -> Result<Validated> {
    let id = uuid::Uuid::parse_str(data["id"].as_str().ok_or("Missing job ID")?)
        .map_err(|_| "Invalid job ID")?
        .to_string();
    let raw = data["directory"]
        .as_str()
        .ok_or("Missing compile directory")?;
    let relative = raw
        .strip_prefix("/var/lib/overleaf/data/compiles/")
        .filter(|v| {
            (20..=180).contains(&v.len())
                && v.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        })
        .ok_or("Invalid compile directory")?;
    let base = root
        .join("overleaf/data/compiles")
        .canonicalize()
        .map_err(|_| "Compile directory unavailable")?;
    let directory = base.join(relative);
    if std::fs::symlink_metadata(&directory)
        .map_err(|_| "Compile project unavailable")?
        .file_type()
        .is_symlink()
        || directory
            .canonicalize()
            .map_err(|_| "Invalid project directory")?
            .parent()
            != Some(base.as_path())
    {
        return Err("Invalid compile directory".into());
    }
    let array = data["command"]
        .as_array()
        .filter(|a| !a.is_empty() && a.len() <= 80)
        .ok_or("Invalid compile command")?;
    let mut command: Vec<String> = array
        .iter()
        .map(|v| match v {
            Value::String(s) => Ok(s.clone()),
            Value::Number(n) => Ok(n.to_string()),
            _ => Err("Invalid command argument"),
        })
        .collect::<std::result::Result<_, _>>()?;
    if command.iter().any(|s| s.len() > 8192 || s.contains('\0')) {
        return Err("Invalid command argument".into());
    }
    let executable = Path::new(&command[0])
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or("Invalid executable")?
        .to_owned();
    if ![
        "latexmk",
        "synctex",
        "texcount",
        "pdflatex",
        "xelatex",
        "lualatex",
        "latex",
        "bibtex",
        "biber",
        "convert",
        "inkscape",
        "epstopdf",
        "gs",
        "rsvg-convert",
        "makeindex",
    ]
    .contains(&executable.as_str())
    {
        return Err("Unsupported compile command".into());
    }
    for arg in &mut command {
        *arg = arg.replace("$COMPILE_DIR", raw).replace(raw, "/compile");
    }
    command[0] = executable;
    let cwd = data["cwd"].as_str().unwrap_or("");
    let resolved = directory
        .join(cwd)
        .canonicalize()
        .map_err(|_| "Invalid compile working directory")?;
    if !resolved.starts_with(&directory) {
        return Err("Invalid compile working directory".into());
    }
    let work = if cwd.is_empty() {
        "/compile".into()
    } else {
        format!("/compile/{cwd}")
    };
    let seconds = data["timeout"].as_f64().unwrap_or(300000.) / 1000.;
    if !seconds.is_finite() {
        return Err("Invalid compile timeout".into());
    }
    Ok(Validated {
        id: id.clone(),
        name: format!("cbgpu-latex-job-{id}"),
        directory,
        command,
        work,
        timeout: Duration::from_secs_f64(seconds.clamp(1., 300.)),
    })
}
pub fn snapshot(source: &Path, target: &Path) -> Result<()> {
    fn walk(source: &Path, target: &Path, total: &mut u64, count: &mut usize) -> Result<()> {
        for entry in std::fs::read_dir(source).map_err(|_| "Cannot inspect compile sources")? {
            let entry = entry.map_err(|_| "Cannot read source entry")?;
            let metadata = entry
                .file_type()
                .map_err(|_| "Cannot inspect source type")?;
            let dest = target.join(entry.file_name());
            if metadata.is_symlink() {
                return Err("Symbolic compile input refused".into());
            }
            if metadata.is_dir() {
                std::fs::create_dir(&dest).map_err(|_| "Cannot stage source folder")?;
                walk(&entry.path(), &dest, total, count)?;
            } else if metadata.is_file() {
                *count += 1;
                *total = total
                    .checked_add(
                        entry
                            .metadata()
                            .map_err(|_| "Cannot inspect source size")?
                            .len(),
                    )
                    .ok_or("Source size overflow")?;
                if *count > 5000 || *total > 400 * 1024 * 1024 {
                    return Err("Project compile cache exceeds sandbox limits; clear cached files in the editor".into());
                }
                std::fs::copy(entry.path(), dest).map_err(|_| "Cannot stage compile source")?;
            } else {
                return Err("Special compile input refused".into());
            }
        }
        Ok(())
    }
    walk(source, target, &mut 0, &mut 0)
}
pub fn sandbox_args(job: &Validated, stage: &Path, image: &str, uid: u32, gid: u32) -> Vec<String> {
    let args = vec![
        "run".into(),
        "-d".into(),
        "--name".into(),
        job.name.clone(),
        "--network".into(),
        "none".into(),
        "--read-only".into(),
        "--cap-drop".into(),
        "ALL".into(),
        "--security-opt".into(),
        "no-new-privileges".into(),
        "--pids-limit".into(),
        "128".into(),
        "--cpus".into(),
        "2".into(),
        "--memory".into(),
        "2g".into(),
        "--memory-swap".into(),
        "2g".into(),
        "--ulimit".into(),
        "fsize=268435456:268435456".into(),
        "--user".into(),
        format!("{uid}:{gid}"),
        "--tmpfs".into(),
        "/tmp:rw,nosuid,nodev,size=256m,mode=1777".into(),
        "--tmpfs".into(),
        format!("/compile:rw,nosuid,nodev,size=512m,uid={uid},gid={gid},mode=0700"),
        "--mount".into(),
        format!("type=bind,src={},dst=/source,readonly", stage.display()),
        "--workdir".into(),
        "/compile".into(),
        "-e".into(),
        "HOME=/tmp".into(),
        "-e".into(),
        "TEXMFVAR=/tmp/texmf-var".into(),
        "-e".into(),
        "TEXMFCONFIG=/tmp/texmf-config".into(),
        "-e".into(),
        "openin_any=p".into(),
        "-e".into(),
        "openout_any=p".into(),
        image.into(),
        "sleep".into(),
        "330".into(),
    ];
    args
}
pub async fn capture(mut input: impl AsyncRead + Unpin) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    let mut total = 0usize;
    let mut buffer = [0u8; 65536];
    loop {
        let n = input
            .read(&mut buffer)
            .await
            .map_err(|_| "Compiler output failed")?;
        if n == 0 {
            break;
        }
        total += n;
        if total > 8_000_000 {
            return Err("Compiler output exceeded 8 MB".into());
        }
        output.extend_from_slice(&buffer[..n]);
        if output.len() > 2_000_000 {
            output.drain(..output.len() - 2_000_000);
        }
    }
    Ok(output)
}
pub fn publish(archive: &Path, directory: &Path) -> Result<usize> {
    let mut archive =
        tar::Archive::new(std::fs::File::open(archive).map_err(|_| "Cannot open compile archive")?);
    let mut total = 0u64;
    let mut count = 0;
    for entry in archive.entries().map_err(|_| "Invalid compile archive")? {
        let mut entry = entry.map_err(|_| "Corrupt compile archive")?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let path = entry
            .path()
            .map_err(|_| "Invalid output path")?
            .into_owned();
        let raw = path
            .to_str()
            .unwrap_or("")
            .strip_prefix("./")
            .unwrap_or(path.to_str().unwrap_or(""));
        if raw.contains('/')
            || !raw.starts_with("output.")
            || raw.contains(['\\', '\0'])
            || raw == "output."
        {
            continue;
        }
        total = total
            .checked_add(entry.size())
            .ok_or("Output size overflow")?;
        if total > 300 * 1024 * 1024 {
            return Err("Compile output exceeds 300 MB".into());
        }
        let mut temp = tempfile::NamedTempFile::new_in(directory)
            .map_err(|_| "Cannot stage compile output")?;
        std::io::copy(&mut entry, &mut temp).map_err(|_| "Cannot read compile output")?;
        temp.flush().map_err(|_| "Cannot flush compile output")?;
        use std::os::unix::fs::PermissionsExt;
        temp.as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o666))
            .map_err(|_| "Cannot set output permissions")?;
        temp.persist(directory.join(raw))
            .map_err(|_| "Cannot publish compile output")?;
        count += 1;
    }
    Ok(count)
}
impl Compiler {
    async fn cancelled(&self, id: &str) -> bool {
        !matches!(self.jobs.lock().await.get(id), Some(JobState::Active(_)))
    }
    async fn docker(&self, args: &[String]) -> Result<std::process::Output> {
        Command::new(&self.docker)
            .args(args)
            .kill_on_drop(true)
            .output()
            .await
            .map_err(|_| "Cannot run sandbox engine".into())
    }
    async fn remove(&self, name: &str) {
        let _ = self.docker(&["rm".into(), "-f".into(), name.into()]).await;
    }
    async fn collect(&self, name: &str, stage: &Path, directory: &Path) -> Result<()> {
        // Static command only; source names are NUL-delimited and never interpolated.
        let mut child=Command::new(&self.docker).args(["exec","--workdir","/compile",name,"sh","-c","find . -maxdepth 1 -type f -name 'output.*' -print0 | tar --null --no-recursion -T - -cf -"]).stdout(Stdio::piped()).stderr(Stdio::null()).kill_on_drop(true).spawn().map_err(|_|"Cannot collect compile outputs")?;
        let mut output = child.stdout.take().ok_or("Missing compile output stream")?;
        let path = stage.join("result.tar");
        let mut file = tokio::fs::File::create(&path)
            .await
            .map_err(|_| "Cannot stage output archive")?;
        let mut total = 0usize;
        let mut buffer = [0u8; 65536];
        use tokio::io::AsyncWriteExt;
        loop {
            let n = tokio::time::timeout(Duration::from_secs(10), output.read(&mut buffer))
                .await
                .map_err(|_| "Compile output timeout")?
                .map_err(|_| "Compile output failed")?;
            if n == 0 {
                break;
            }
            total += n;
            if total > 320 * 1024 * 1024 {
                return Err("Output archive exceeds sandbox budget".into());
            }
            file.write_all(&buffer[..n])
                .await
                .map_err(|_| "Cannot store compile archive")?;
        }
        if !child
            .wait()
            .await
            .map_err(|_| "Cannot finish output collection")?
            .success()
        {
            return Err("Unable to collect compile outputs".into());
        }
        drop(file);
        let directory = directory.to_owned();
        tokio::task::spawn_blocking(move || publish(&path, &directory))
            .await
            .map_err(|_| "Output publisher failed")??;
        Ok(())
    }
    async fn run_job(&self, job: Validated) -> Result<Value> {
        let _permit = self
            .limit
            .acquire()
            .await
            .map_err(|_| "Compile broker closed")?;
        if self.cancelled(&job.id).await {
            return Ok(
                json!({"exitCode":124,"stdout":"","stderr":"Compilation cancelled","terminated":true,"timedout":false}),
            );
        }
        let stage = tempfile::Builder::new()
            .prefix("job-")
            .tempdir_in(self.private.join("jobs"))
            .map_err(|_| "Cannot stage compile sandbox")?;
        let source = job.directory.clone();
        let staged = stage.path().to_owned();
        tokio::task::spawn_blocking(move || snapshot(&source, &staged))
            .await
            .map_err(|_| "Source snapshot failed")??;
        let out = self
            .docker(&sandbox_args(
                &job,
                stage.path(),
                &self.image,
                unsafe { libc::getuid() },
                unsafe { libc::getgid() },
            ))
            .await?;
        if self.cancelled(&job.id).await {
            self.remove(&job.name).await;
            return Ok(
                json!({"exitCode":124,"stdout":"","stderr":"Compilation cancelled","terminated":true,"timedout":false}),
            );
        }
        if !out.status.success() {
            return Err("Unable to start isolated compile sandbox".into());
        }
        let result=async {
   let mut args=vec!["exec".to_owned(),"--workdir".into(),job.work.clone(),job.name.clone(),"sh".into(),"-c".into(),"cp -R /source/. /compile/ && exec \"$@\"".into(),"cbgpu".into()];args.extend(job.command.clone());
   let mut child=Command::new(&self.docker).args(args).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true).spawn().map_err(|_|"Cannot start TeX process")?;
   let stdout=child.stdout.take().ok_or("Missing compiler stdout")?;let stderr=child.stderr.take().ok_or("Missing compiler stderr")?;
   let timed=tokio::time::timeout(job.timeout,async{let (stdout,stderr)=tokio::try_join!(capture(stdout),capture(stderr))?;let status=child.wait().await.map_err(|_|"Cannot await TeX process")?;Ok::<_,String>((stdout,stderr,status))}).await;
   match timed {Ok(Ok((stdout,stderr,status)))=>{self.collect(&job.name,stage.path(),&job.directory).await?;Ok(json!({"stdout":String::from_utf8_lossy(&stdout),"stderr":String::from_utf8_lossy(&stderr),"exitCode":status.code().unwrap_or(1)}))},other=>{self.remove(&job.name).await;let _=child.kill().await;let _=child.wait().await;Ok(json!({"stdout":"","stderr":"CBGPU: compilation stopped by timeout or sandbox output budget","exitCode":124,"terminated":true,"timedout":other.is_err()}))}}
  }.await;
        self.remove(&job.name).await;
        if self.cancelled(&job.id).await {
            return Ok(
                json!({"exitCode":124,"stdout":"","stderr":"Compilation cancelled","terminated":true,"timedout":false}),
            );
        }
        result
    }
    pub fn router(self) -> Router {
        Router::new()
            .route("/run", post(run))
            .route("/kill", post(kill))
            .route("/healthz", get(health))
            .layer(axum::extract::DefaultBodyLimit::max(65536))
            .with_state(self)
    }
}
async fn health(State(state): State<Compiler>) -> Json<Value> {
    Json(
        json!({"status":"ok","service":"cbgpu-latex-compiler","queued_or_running":state.jobs.lock().await.len(),"occupied_slots":3usize.saturating_sub(state.limit.available_permits())}),
    )
}
async fn run(State(state): State<Compiler>, Json(data): Json<Value>) -> axum::response::Response {
    let job = match validate(&state.private, &data) {
        Ok(v) => v,
        Err(e) => return (StatusCode::BAD_REQUEST, Json(json!({"error":e}))).into_response(),
    };
    let id = job.id.clone();
    {
        let mut jobs = state.jobs.lock().await;
        if jobs.contains_key(&id) {
            return (
                StatusCode::CONFLICT,
                Json(json!({"error":"Compile job already exists"})),
            )
                .into_response();
        }
        jobs.insert(id.clone(), JobState::Active(job.name.clone()));
    }
    // Detached cleanup survives a browser disconnect; every sandbox is removed.
    let (tx, rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let result = state.run_job(job).await;
        state.jobs.lock().await.remove(&id);
        let _ = tx.send(result);
    });
    match rx.await {
        Ok(Ok(value)) => Json(value).into_response(),
        Ok(Err(e)) => (StatusCode::BAD_REQUEST, Json(json!({"error":e}))).into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error":"Compile task failed"})),
        )
            .into_response(),
    }
}
async fn kill(State(state): State<Compiler>, Json(data): Json<Value>) -> axum::response::Response {
    let Some(id) = data["id"]
        .as_str()
        .and_then(|s| uuid::Uuid::parse_str(s).ok())
        .map(|id| id.to_string())
    else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    // Keep a tombstone until the original task finishes: queued cancellations
    // cannot start later or allow another request to reuse the same job ID.
    let name = {
        let mut jobs = state.jobs.lock().await;
        jobs.get_mut(&id).and_then(
            |entry| match std::mem::replace(entry, JobState::Cancelled) {
                JobState::Active(name) => Some(name),
                JobState::Cancelled => None,
            },
        )
    };
    if let Some(name) = name {
        state.remove(&name).await;
    }
    Json(json!({"ok":true})).into_response()
}
