use cbgpu_latex::{Result, compiler::Compiler, gateway::Gateway, operations, store::Store};
use std::{collections::HashMap, path::PathBuf, sync::Arc, time::Duration};
use tokio::sync::{Mutex, Semaphore};
#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("{error}");
        std::process::exit(1)
    }
}
async fn run() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let action = args.first().map(String::as_str).unwrap_or("--help");
    if action == "--help" {
        println!(
            "cbgpu-latex gateway|compiler|migrate|import-legacy|backup|restore-state|install|verify-restore|acl\nMaintenance defaults to a read-only plan; append --apply. Configure CBGPU_LATEX_PRIVATE, CBGPU_LATEX_SOURCE, CBGPU_LATEX_DATABASE_URL and CBGPU_ACCOUNT_V2. No project database/volume is reset."
        );
        return Ok(());
    }
    let source = std::env::var_os("CBGPU_LATEX_SOURCE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")));
    let private = std::env::var_os("CBGPU_LATEX_PRIVATE")
        .map(PathBuf::from)
        .unwrap_or_else(|| source.join("../private/latex"));
    if !matches!(action, "gateway" | "compiler") && !args.iter().any(|v| v == "--apply") {
        println!(
            "{}",
            serde_json::json!({"action":action,"apply":false,"private_state":private,"preserves_projects":true,"message":"Explicit --apply required; candidate migrations and service activation require reviewed backup and approval"})
        );
        return Ok(());
    }
    unsafe {
        libc::umask(0o077);
    }
    if action == "install" {
        return operations::install(&source, &private).await;
    }
    if action == "verify-restore" {
        return operations::verify_restore(&private).await;
    }
    if action == "acl" {
        let uid = args
            .iter()
            .find_map(|v| v.strip_prefix("--uid="))
            .and_then(|v| v.parse::<u32>().ok())
            .ok_or("acl requires --uid=HOST_BROKER_UID")?;
        return operations::apply_acl(
            &private.join("overleaf/data/compiles"),
            &format!("u::rwx,u:{uid}:rwx,g::r-x,m::rwx,o::r-x"),
        );
    }
    if action == "compiler" {
        let image = std::fs::read_to_string(private.join("compiler-image"))
            .map_err(|_| "Compiler image configuration missing")?;
        let image = image.trim().to_owned();
        if !image
            .strip_prefix("texlive/texlive@sha256:")
            .is_some_and(|v| v.len() == 64 && v.bytes().all(|b| b.is_ascii_hexdigit()))
        {
            return Err("Compiler image must use reviewed immutable TeX Live digest".into());
        }
        std::fs::create_dir_all(private.join("jobs"))
            .map_err(|_| "Cannot initialize sandbox staging")?;
        let socket = private.join("run/compile.sock");
        if let Ok(meta) = std::fs::symlink_metadata(&socket) {
            use std::os::unix::fs::{FileTypeExt, MetadataExt};
            if !meta.file_type().is_socket() || meta.uid() != unsafe { libc::getuid() } {
                return Err("Refusing to replace non-owned compile socket".into());
            }
            if tokio::net::UnixStream::connect(&socket).await.is_ok() {
                return Err(
                    "Compile socket is already active; existing broker was preserved".into(),
                );
            }
            std::fs::remove_file(&socket).map_err(|_| "Cannot remove stale socket")?;
        }
        let listener = tokio::net::UnixListener::bind(&socket)
            .map_err(|_| "Cannot bind private compile socket")?;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o666))
            .map_err(|_| "Cannot configure CE socket access")?;
        let state = Compiler {
            private,
            image,
            docker: PathBuf::from("/usr/bin/docker"),
            jobs: Arc::new(Mutex::new(HashMap::new())),
            limit: Arc::new(Semaphore::new(3)),
        };
        return axum::serve(listener, state.router())
            .await
            .map_err(|_| "Compile broker failed".into());
    }
    let database = std::env::var("CBGPU_LATEX_DATABASE_URL")
        .map_err(|_| "CBGPU_LATEX_DATABASE_URL must point to reviewed private PostgreSQL state")?;
    if action == "backup" {
        operations::backup(&private, &database).await?;
        println!("Private LaTeX backup completed");
        return Ok(());
    }
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(8)
        .acquire_timeout(Duration::from_secs(5))
        .connect(&database)
        .await
        .map_err(|_| "LaTeX PostgreSQL state unavailable")?;
    if action == "restore-state" {
        let path = args
            .iter()
            .find_map(|v| v.strip_prefix("--backup="))
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .ok_or("restore-state requires --backup=ABSOLUTE_COPY_FILE")?;
        let count = operations::restore_state(&pool, &path).await?;
        println!(
            "Restored {count} transient gateway records; existing records and projects unchanged"
        );
        return Ok(());
    }
    if action == "migrate" {
        sqlx::raw_sql(include_str!(
            "../migrations/20261116121608_latex_native_gateway_state.sql"
        ))
        .execute(&pool)
        .await
        .map_err(|_| "Candidate migration failed")?;
        return Ok(());
    }
    let store = Store {
        pool,
        account_v2: std::env::var("CBGPU_ACCOUNT_V2").as_deref() == Ok("1"),
    };
    if action == "import-legacy" {
        let count = store.import_sqlite(&private.join("gateway.sqlite")).await?;
        println!("Imported {count} legacy transient records; project data and IDs unchanged");
        return Ok(());
    }
    if action != "gateway" {
        return Err("Unknown native LaTeX command".into());
    }
    let key = std::fs::read_to_string(private.join("bridge-key"))
        .map_err(|_| "Private CE bridge key missing")?
        .trim()
        .to_owned();
    if key.len() < 32 {
        return Err("Invalid private bridge key".into());
    }
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| "Cannot initialize native HTTP client")?;
    let state = Gateway {
        store,
        private: private.clone(),
        source,
        key: Arc::new(key),
        http: client,
        bridge_url: "http://127.0.0.1:3523".into(),
        editor_url: "http://127.0.0.1:3521".into(),
        revocation_interval: Duration::from_secs(20),
        locks: Arc::new(Mutex::new(HashMap::new())),
    };
    let cleanup = state.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(300)).await;
            let _ = cleanup
                .store
                .cleanup(&cleanup.private.join("imports"))
                .await;
            cleanup
                .locks
                .lock()
                .await
                .retain(|_, v| Arc::strong_count(v) > 1);
        }
    });
    let port = std::env::var("CBGPU_LATEX_PORT")
        .unwrap_or_else(|_| "3520".into())
        .parse::<u16>()
        .map_err(|_| "Invalid loopback port")?;
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port))
        .await
        .map_err(|_| "LaTeX gateway port unavailable; existing service was preserved")?;
    axum::serve(listener, state.router())
        .await
        .map_err(|_| "Native LaTeX gateway failed".into())
}
