//! Real isolated PostgreSQL + fake CE services, never production data/containers.
use axum::{
    Router,
    extract::{
        Request, State,
        ws::{Message, WebSocketUpgrade},
    },
    response::IntoResponse,
    routing::get,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use cbgpu_latex::{
    gateway::Gateway,
    store::{Store, now},
};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use std::{
    collections::HashMap,
    io::Write,
    os::unix::fs::PermissionsExt,
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
const USER: &str = "00000000-0000-0000-0000-000000000001";
const SESSION: &str = "00000000-0000-0000-0000-000000000011";
const OTHER: &str = "00000000-0000-0000-0000-000000000002";
const OTHER_SESSION: &str = "00000000-0000-0000-0000-000000000022";
const PROJECT: &str = "abcdef012345abcdef012345";
const OTHER_PROJECT: &str = "111111111111111111111111";
struct Fixture(String);
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = Command::new("docker")
            .args(["rm", "-f", "-v", &self.0])
            .output();
    }
}
impl Fixture {
    fn sql(&self, sql: &str) {
        let mut child = Command::new("docker")
            .args([
                "exec",
                "-i",
                &self.0,
                "psql",
                "-h",
                "/socket",
                "-U",
                "postgres",
                "-v",
                "ON_ERROR_STOP=1",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(sql.as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}
#[derive(Clone)]
struct Fake {
    headers: Arc<Mutex<Vec<axum::http::HeaderMap>>>,
    imports: Arc<AtomicUsize>,
}
async fn bridge(State(fake): State<Fake>, req: Request) -> axum::response::Response {
    assert_eq!(
        req.headers()["authorization"],
        "Bearer fixture-bridge-secret-key-long-enough"
    );
    let path = req.uri().path().to_owned();
    let data: Value =
        serde_json::from_slice(&axum::body::to_bytes(req.into_body(), 65536).await.unwrap())
            .unwrap();
    let mine = if data["user"]["id"] == USER {
        PROJECT
    } else {
        OTHER_PROJECT
    };
    match path.as_str() {
        "/projects" => {
            axum::Json(json!({"projects":[{"id":mine,"name":"Private project","role":"owner"}]}))
                .into_response()
        }
        "/session" => {
            axum::Json(json!({"cookie":"overleaf=signed-private-ce-cookie"})).into_response()
        }
        "/import" => {
            fake.imports.fetch_add(1, Ordering::SeqCst);
            axum::Json(json!({"id":mine,"name":"Imported"})).into_response()
        }
        "/trash" => {
            if data["project"] == mine {
                axum::Json(json!({"ok":true})).into_response()
            } else {
                axum::http::StatusCode::NOT_FOUND.into_response()
            }
        }
        _ => axum::http::StatusCode::NOT_FOUND.into_response(),
    }
}
async fn editor(State(fake): State<Fake>, req: Request) -> impl IntoResponse {
    fake.headers.lock().await.push(req.headers().clone());
    (
        [
            ("content-type", "text/html"),
            ("set-cookie", "browser-stolen=forbidden"),
            (
                "content-security-policy",
                "script-src 'nonce-fixture_nonce'; object-src 'none'",
            ),
        ],
        "<html><body>Private editor</body></html>",
    )
}
async fn websocket(State(fake): State<Fake>, req: Request) -> axum::response::Response {
    fake.headers.lock().await.push(req.headers().clone());
    use axum::extract::FromRequestParts;
    let (mut parts, _) = req.into_parts();
    let upgrade = WebSocketUpgrade::from_request_parts(&mut parts, &())
        .await
        .unwrap();
    upgrade
        .on_upgrade(|mut socket| async move {
            while let Some(Ok(message)) = socket.recv().await {
                if matches!(message, Message::Text(_) | Message::Binary(_))
                    && socket.send(message).await.is_err()
                {
                    break;
                }
            }
        })
        .into_response()
}
async fn serve(router: Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (format!("http://{address}"), handle)
}
fn identity(user: &str, session: &str) -> String {
    STANDARD.encode(
        serde_json::to_vec(&json!({"id":user,"session":session,"email":"fixture@example.invalid"}))
            .unwrap(),
    )
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "Docker network-none PostgreSQL and loopback fake editor only; no production credentials"]
async fn postgres_handoff_project_privacy_cookie_boundary_revalidation_and_import_continuity() {
    let socket = tempfile::tempdir().unwrap();
    std::fs::set_permissions(socket.path(), std::fs::Permissions::from_mode(0o777)).unwrap();
    let db = Fixture(format!(
        "cbgpu-latex-test-{}",
        uuid::Uuid::new_v4().simple()
    ));
    let out = Command::new("docker")
        .args([
            "run",
            "-d",
            "--name",
            &db.0,
            "--network",
            "none",
            "--memory",
            "384m",
            "--cpus",
            "1",
            "--tmpfs",
            "/var/lib/postgresql/data:rw,size=256m",
            "-v",
            &format!("{}:/socket", socket.path().display()),
            "-e",
            "POSTGRES_HOST_AUTH_METHOD=trust",
            "-e",
            "PGHOST=/socket",
            "postgres:17-alpine",
            "postgres",
            "-k",
            "/var/run/postgresql,/socket",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    let mut ready = false;
    for _ in 0..150 {
        let logs = Command::new("docker")
            .args(["logs", &db.0])
            .output()
            .unwrap();
        if String::from_utf8_lossy(&logs.stdout).contains("PostgreSQL init process complete")
            && Command::new("docker")
                .args([
                    "exec",
                    &db.0,
                    "pg_isready",
                    "-h",
                    "/socket",
                    "-U",
                    "postgres",
                ])
                .output()
                .is_ok_and(|o| o.status.success())
        {
            ready = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(ready);
    db.sql(&format!("CREATE ROLE anon;CREATE ROLE authenticated;CREATE ROLE service_role BYPASSRLS;CREATE SCHEMA auth;CREATE TABLE auth.users(id uuid primary key,deleted_at timestamptz,banned_until timestamptz);CREATE TABLE auth.sessions(id uuid primary key,user_id uuid,not_after timestamptz);CREATE TABLE public.profiles(id uuid primary key,is_approved bool,account_status text,compute bool);INSERT INTO auth.users(id)VALUES('{USER}'),('{OTHER}');INSERT INTO auth.sessions(id,user_id)VALUES('{SESSION}','{USER}'),('{OTHER_SESSION}','{OTHER}');INSERT INTO public.profiles VALUES('{USER}',true,'ACTIVE',true),('{OTHER}',false,'ACTIVE',false);CREATE FUNCTION public.cbgpu_service_can_use_resource(uuid,text,text)RETURNS bool LANGUAGE sql AS 'SELECT compute FROM public.profiles WHERE id=$1';"));
    db.sql(include_str!(
        "../migrations/20261116121608_latex_native_gateway_state.sql"
    ));
    db.sql(include_str!(
        "../migrations/20261116121608_latex_native_gateway_state.sql"
    ));
    db.sql("DO $$BEGIN IF has_schema_privilege('authenticated','latex_gateway','USAGE') OR has_function_privilege('authenticated','latex_gateway.authorized(uuid,uuid,boolean,boolean)','EXECUTE') OR has_table_privilege('authenticated','latex_gateway.state','SELECT') THEN RAISE EXCEPTION 'private gateway exposed'; END IF;END$$;");
    db.sql(&format!("SET ROLE cbgpu_latex;SELECT latex_gateway.authorized('{USER}','{SESSION}',true,false);INSERT INTO latex_gateway.state VALUES('challenges','role-check','{{}}',extract(epoch from now())+60);DELETE FROM latex_gateway.state WHERE id='role-check';RESET ROLE;"));
    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect_with(
            PgConnectOptions::new()
                .host(socket.path().to_str().unwrap())
                .username("postgres")
                .database("postgres"),
        )
        .await
        .unwrap();
    let store = Store {
        pool: pool.clone(),
        account_v2: true,
    };
    assert!(store.authorized(OTHER, OTHER_SESSION, false).await);
    assert!(!store.authorized(OTHER, OTHER_SESSION, true).await);
    let legacy = Store {
        pool: pool.clone(),
        account_v2: false,
    };
    assert!(!legacy.authorized(OTHER, OTHER_SESSION, false).await);
    assert!(legacy.authorized(USER, SESSION, false).await);
    store
        .put(
            "incoming",
            "suspended-owner-continuity",
            &json!({"user_id":USER,"name":"retained.zip","size":10,"received":5}),
            now() + 3600.,
        )
        .await
        .unwrap();
    db.sql(&format!(
        "UPDATE public.profiles SET account_status='SUSPENDED' WHERE id='{USER}';"
    ));
    assert!(!legacy.authorized(USER, SESSION, false).await);
    assert!(!store.authorized(USER, SESSION, false).await);
    assert!(
        store
            .get("incoming", "suspended-owner-continuity")
            .await
            .unwrap()
            .is_some()
    );
    db.sql(&format!(
        "UPDATE public.profiles SET account_status='ACTIVE' WHERE id='{USER}';"
    ));
    assert!(legacy.authorized(USER, SESSION, false).await);
    assert!(store.authorized(USER, SESSION, false).await);
    db.sql(&format!(
        "UPDATE auth.sessions SET not_after=now()-interval '1 second' WHERE id='{SESSION}';"
    ));
    assert!(!legacy.authorized(USER, SESSION, false).await);
    assert!(!store.authorized(USER, SESSION, false).await);
    db.sql(&format!("UPDATE auth.sessions SET not_after=NULL WHERE id='{SESSION}';UPDATE auth.users SET banned_until=now()+interval '1 hour' WHERE id='{USER}';"));
    assert!(!legacy.authorized(USER, SESSION, false).await);
    assert!(!store.authorized(USER, SESSION, false).await);
    db.sql(&format!(
        "UPDATE auth.users SET banned_until=NULL WHERE id='{USER}';"
    ));
    assert!(legacy.authorized(USER, SESSION, false).await);

    let private = tempfile::tempdir().unwrap();
    std::fs::create_dir(private.path().join("imports")).unwrap();
    let fake = Fake {
        headers: Arc::new(Mutex::new(vec![])),
        imports: Arc::new(AtomicUsize::new(0)),
    };
    let (bridge_url, bridge_task) =
        serve(Router::new().fallback(bridge).with_state(fake.clone())).await;
    let (editor_url, editor_task) = serve(
        Router::new()
            .route("/socket.io/", get(websocket))
            .fallback(editor)
            .with_state(fake.clone()),
    )
    .await;
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let gateway = Gateway {
        store: store.clone(),
        private: private.path().to_owned(),
        source: std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")),
        key: Arc::new("fixture-bridge-secret-key-long-enough".into()),
        http: http.clone(),
        bridge_url,
        editor_url,
        revocation_interval: Duration::from_millis(30),
        locks: Arc::new(Mutex::new(HashMap::new())),
    };
    let (url, task) = serve(gateway.router()).await;
    let connect = http
        .get(format!("{url}/cbgpu/connect?project={PROJECT}"))
        .send()
        .await
        .unwrap();
    assert_eq!(connect.status(), 303);
    let proof = connect.headers()["set-cookie"]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    let location = url::Url::parse(connect.headers()["location"].to_str().unwrap()).unwrap();
    let challenge = location
        .query_pairs()
        .find(|(k, _)| k == "connect")
        .unwrap()
        .1
        .into_owned();
    let open = http
        .post(format!("{url}/_api/open"))
        .header("x-cbgpu-key", "fixture-bridge-secret-key-long-enough")
        .header("x-cbgpu-identity", identity(USER, SESSION))
        .json(&json!({"challenge":challenge,"project":PROJECT}))
        .send()
        .await
        .unwrap();
    assert_eq!(open.status(), 200);
    let enter_url = open.json::<Value>().await.unwrap()["url"]
        .as_str()
        .unwrap()
        .replacen("https://latex.cbgpu.com", &url, 1);
    assert_eq!(http.get(&enter_url).send().await.unwrap().status(), 401);
    let enter = http
        .get(&enter_url)
        .header("cookie", &proof)
        .send()
        .await
        .unwrap();
    assert_eq!(enter.status(), 303);
    let browser = enter.headers()["set-cookie"]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    assert!(browser.starts_with("__Host-cbgpu-latex="));
    assert!(!browser.contains("overleaf"));
    assert_eq!(
        http.get(&enter_url)
            .header("cookie", &proof)
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    let page = http
        .get(format!("{url}/project/{PROJECT}"))
        .header(
            "cookie",
            format!("{browser}; session=browser-secret; overleaf=attacker"),
        )
        .header("authorization", "Bearer browser-secret")
        .header("x-cbgpu-key", "browser-secret")
        .send()
        .await
        .unwrap();
    assert_eq!(page.status(), 200);
    assert!(!page.headers().contains_key("set-cookie"));
    let html = page.text().await.unwrap();
    assert!(html.contains("nonce=\"fixture_nonce\""));
    let forwarded = fake.headers.lock().await.last().unwrap().clone();
    assert_eq!(forwarded["cookie"], "overleaf=signed-private-ce-cookie");
    assert!(!forwarded.contains_key("authorization"));
    assert!(!forwarded.contains_key("x-cbgpu-key"));
    assert_eq!(
        http.get(format!("{url}/project/{OTHER_PROJECT}/output/output.pdf"))
            .header("cookie", &browser)
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    assert_eq!(
        http.post(format!("{url}/project/{PROJECT}/settings"))
            .header("cookie", &browser)
            .header("origin", "https://evil.invalid")
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    let external_id = "external-test-session";
    store.put("sessions",external_id,&json!({"user_id":OTHER,"native_session":OTHER_SESSION,"ce_cookie":"overleaf=external"}),now()+3600.).await.unwrap();
    let external_cookie = format!("__Host-cbgpu-latex={external_id}");
    assert_eq!(
        http.get(format!("{url}/project/{OTHER_PROJECT}"))
            .header("cookie", &external_cookie)
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(
        http.post(format!("{url}/project/{OTHER_PROJECT}/compile"))
            .header("cookie", &external_cookie)
            .header("origin", "https://latex.cbgpu.com")
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    assert_eq!(
        http.post(format!("{url}/project/{OTHER_PROJECT}/settings"))
            .header("cookie", &external_cookie)
            .header("origin", "https://latex.cbgpu.com")
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    for path in [
        format!("/project/{OTHER_PROJECT}/compile/"),
        format!("/PROJECT/{OTHER_PROJECT}/COMPILE/"),
    ] {
        assert_eq!(
            http.post(format!("{url}{path}"))
                .header("cookie", &external_cookie)
                .header("origin", "https://latex.cbgpu.com")
                .send()
                .await
                .unwrap()
                .status(),
            403
        );
    }
    for path in [
        format!("/status/compiler/{OTHER_PROJECT}"),
        "/read/abcdefghijkl".into(),
        "/123456abcdef".into(),
        "/INTERNAL/project/111111111111111111111111/compile/pdf".into(),
    ] {
        assert_eq!(
            http.get(format!("{url}{path}"))
                .header("cookie", &external_cookie)
                .send()
                .await
                .unwrap()
                .status(),
            404
        );
    }
    for suffix in ["wordcount", "sync/code", "sync/pdf"] {
        assert_eq!(
            http.get(format!("{url}/project/{OTHER_PROJECT}/{suffix}"))
                .header("cookie", &external_cookie)
                .send()
                .await
                .unwrap()
                .status(),
            403
        );
    }
    let begin = http
        .post(format!("{url}/_api/upload-begin"))
        .header("x-cbgpu-key", "fixture-bridge-secret-key-long-enough")
        .header("x-cbgpu-identity", identity(USER, SESSION))
        .json(&json!({"name":"paper.zip","size":3}))
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    let upload = begin["upload"].as_str().unwrap();
    let body = format!("{}\nabc", json!({"upload":upload,"offset":0}));
    let chunk = http
        .post(format!("{url}/_api/upload-chunk"))
        .header("x-cbgpu-key", "fixture-bridge-secret-key-long-enough")
        .header("x-cbgpu-identity", identity(USER, SESSION))
        .body(body.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(chunk.status(), 200);
    assert_eq!(
        http.post(format!("{url}/_api/upload-chunk"))
            .header("x-cbgpu-key", "fixture-bridge-secret-key-long-enough")
            .header("x-cbgpu-identity", identity(USER, SESSION))
            .body(body)
            .send()
            .await
            .unwrap()
            .status(),
        409
    );
    assert_eq!(
        http.post(format!("{url}/_api/upload-cancel"))
            .header("x-cbgpu-key", "fixture-bridge-secret-key-long-enough")
            .header("x-cbgpu-identity", identity(OTHER, OTHER_SESSION))
            .json(&json!({"upload":upload}))
            .send()
            .await
            .unwrap()
            .status(),
        404
    );

    // Completed imports retry idempotently and remain bound to their original owner.
    let imported = uuid::Uuid::new_v4().to_string();
    store
        .put(
            "uploads",
            &imported,
            &json!({"user_id":USER,"metadata":{"files":["main.tex"],"name":"Paper"}}),
            now() + 1800.,
        )
        .await
        .unwrap();
    for _ in 0..2 {
        let response = http
            .post(format!("{url}/_api/import"))
            .header("x-cbgpu-key", "fixture-bridge-secret-key-long-enough")
            .header("x-cbgpu-identity", identity(USER, SESSION))
            .json(&json!({"upload":imported,"main":"main.tex","compiler":"pdflatex"}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 201);
        assert_eq!(response.json::<Value>().await.unwrap()["id"], PROJECT);
    }
    assert_eq!(fake.imports.load(Ordering::SeqCst), 1);
    // Read-only legacy migration preserves IDs/cookies and never overwrites new state.
    let legacy_file = private.path().join("legacy.sqlite");
    {
        let source = rusqlite::Connection::open(&legacy_file).unwrap();
        source.execute_batch("CREATE TABLE challenges(id TEXT PRIMARYKEY,proof TEXT,expires REAL);CREATE TABLE sessions(id TEXT PRIMARYKEY,user_id TEXT,native_session TEXT,ce_cookie TEXT,expires REAL);CREATE TABLE tickets(id TEXT PRIMARYKEY,session_id TEXT,proof TEXT,target TEXT,expires REAL);CREATE TABLE uploads(id TEXT PRIMARYKEY,user_id TEXT,metadata TEXT,result TEXT,expires REAL);CREATE TABLE incoming(id TEXT PRIMARYKEY,user_id TEXT,name TEXT,size INTEGER,received INTEGER,expires REAL);").unwrap();
        source
            .execute(
                "INSERT INTO sessions VALUES(?1,?2,?3,?4,?5)",
                rusqlite::params![
                    "preserved-old-session",
                    OTHER,
                    OTHER_SESSION,
                    "overleaf=legacy-same-cookie",
                    now() + 3600.
                ],
            )
            .unwrap();
    }
    let before = std::fs::read(&legacy_file).unwrap();
    assert_eq!(store.import_sqlite(&legacy_file).await.unwrap(), 1);
    assert_eq!(store.import_sqlite(&legacy_file).await.unwrap(), 0);
    assert_eq!(std::fs::read(&legacy_file).unwrap(), before);
    let old = store
        .get("sessions", "preserved-old-session")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(old["user_id"], OTHER);
    assert_eq!(old["ce_cookie"], "overleaf=legacy-same-cookie");
    sqlx::query(
        "UPDATE public.profiles SET account_status='REVERIFICATION_REQUIRED' WHERE id=$1::uuid",
    )
    .bind(OTHER)
    .execute(&pool)
    .await
    .unwrap();
    assert!(store.authorized(OTHER, OTHER_SESSION, false).await);
    assert!(!store.authorized(OTHER, OTHER_SESSION, true).await);
    // A rollback mirrors current native transient state, including new sessions,
    // consumed tickets and in-progress uploads; project data is never restored.
    {
        let legacy = rusqlite::Connection::open(&legacy_file).unwrap();
        legacy.execute("INSERT INTO tickets VALUES('consumed-before-cutover','old','proof','/',9999999999)",[]).unwrap();
    }
    store.put("sessions","native-created-session",&json!({"user_id":USER,"native_session":SESSION,"ce_cookie":"overleaf=native-preserved"}),now()+3600.).await.unwrap();
    let partial_id = uuid::Uuid::new_v4().to_string();
    store
        .put(
            "incoming",
            &partial_id,
            &json!({"user_id":USER,"name":"paper.zip","size":100,"received":42}),
            now() + 3600.,
        )
        .await
        .unwrap();
    let partial = private
        .path()
        .join("imports")
        .join(format!("{partial_id}.partial"));
    std::fs::write(&partial, [1u8; 42]).unwrap();
    let mirror = private.path().join("rollback.sqlite");
    std::fs::copy(&legacy_file, &mirror).unwrap();
    let mirrored = store.export_sqlite(&mirror).await.unwrap();
    assert!(mirrored >= 2);
    {
        let legacy = rusqlite::Connection::open(&mirror).unwrap();
        let cookie: String = legacy
            .query_row(
                "SELECT ce_cookie FROM sessions WHERE id='native-created-session'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(cookie, "overleaf=native-preserved");
        let stale: i64 = legacy
            .query_row(
                "SELECT count(*)FROM tickets WHERE id='consumed-before-cutover'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(stale, 0);
        let offset: i64 = legacy
            .query_row(
                "SELECT received FROM incoming WHERE id=?1",
                [&partial_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(offset, 42);
    }
    assert_eq!(std::fs::read(partial).unwrap(), vec![1; 42]);
    // Malformed native state causes an atomic rollback, not a partial mirror.
    store
        .put(
            "sessions",
            "invalid-rollback-session",
            &json!({"user_id":USER}),
            now() + 3600.,
        )
        .await
        .unwrap();
    assert!(store.export_sqlite(&mirror).await.is_err());
    {
        let legacy = rusqlite::Connection::open(&mirror).unwrap();
        let total: i64 = legacy
            .query_row(
                "SELECT count(*)FROM sessions WHERE id='native-created-session'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(total, 1);
    }
    store
        .delete("sessions", "invalid-rollback-session")
        .await
        .unwrap();
    let copy = private.path().join("gateway-state.copy");
    cbgpu_latex::operations::write_state_backup(&pool, &copy)
        .await
        .unwrap();
    assert_eq!(
        cbgpu_latex::operations::restore_state(&pool, &copy)
            .await
            .unwrap(),
        0
    );
    sqlx::query(
        "DELETE FROM latex_gateway.state WHERE kind='sessions' AND id='preserved-old-session'",
    )
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(
        cbgpu_latex::operations::restore_state(&pool, &copy)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        store
            .get("sessions", "preserved-old-session")
            .await
            .unwrap()
            .unwrap()["ce_cookie"],
        "overleaf=legacy-same-cookie"
    );
    let broken = private.path().join("broken.copy");
    std::fs::write(&broken, b"not a PostgreSQL COPY snapshot").unwrap();
    assert!(
        cbgpu_latex::operations::restore_state(&pool, &broken)
            .await
            .is_err()
    );
    assert_eq!(
        cbgpu_latex::operations::restore_state(&pool, &copy)
            .await
            .unwrap(),
        0
    );
    let mut request = url.replacen("http://", "ws://", 1);
    request.push_str("/socket.io/");
    let mut request = request.into_client_request().unwrap();
    request
        .headers_mut()
        .insert("cookie", browser.parse().unwrap());
    request
        .headers_mut()
        .insert("origin", "https://latex.cbgpu.com".parse().unwrap());
    let (mut ws, _) = tokio_tungstenite::connect_async(request).await.unwrap();
    ws.send(tokio_tungstenite::tungstenite::Message::Text(
        "hello".into(),
    ))
    .await
    .unwrap();
    assert_eq!(
        ws.next().await.unwrap().unwrap().into_text().unwrap(),
        "hello"
    );
    sqlx::query("UPDATE auth.users SET banned_until=now()+interval '1hour' WHERE id=$1::uuid")
        .bind(USER)
        .execute(&pool)
        .await
        .unwrap();
    let closed = tokio::time::timeout(Duration::from_secs(2), ws.next())
        .await
        .unwrap();
    assert!(
        closed.is_none() || closed.is_some_and(|v| v.is_err() || v.is_ok_and(|m| m.is_close()))
    );
    assert!(!store.authorized(USER, SESSION, false).await);
    task.abort();
    bridge_task.abort();
    editor_task.abort();
    pool.close().await;
}
