use crate::{
    Result, archive,
    store::{Store, now},
};
use axum::{
    Router,
    body::{Body, to_bytes},
    extract::{
        DefaultBodyLimit, Request, State,
        ws::{Message, WebSocketUpgrade},
    },
    http::{HeaderMap, HeaderValue, Method, StatusCode},
    response::{IntoResponse, Response},
};
use base64::{
    Engine,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};
use subtle::ConstantTimeEq;
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::{Message as UpMessage, client::IntoClientRequest};
const PUBLIC: &str = "https://latex.cbgpu.com";
const PORTAL: &str = "https://researcher.cbgpu.com/latex";
const COOKIE: &str = "__Host-cbgpu-latex";
#[derive(Clone)]
pub struct Gateway {
    pub store: Store,
    pub private: PathBuf,
    pub source: PathBuf,
    pub key: Arc<String>,
    pub http: reqwest::Client,
    pub bridge_url: String,
    pub editor_url: String,
    pub revocation_interval: Duration,
    pub locks: Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>,
}
#[derive(Debug)]
pub struct Error(pub StatusCode, pub String);
impl IntoResponse for Error {
    fn into_response(self) -> Response {
        json_response(json!({"error":self.1}), self.0)
    }
}
impl From<String> for Error {
    fn from(v: String) -> Self {
        Self(StatusCode::BAD_GATEWAY, v)
    }
}
type WebResult<T> = std::result::Result<T, Error>;
fn error(status: StatusCode, text: &str) -> Error {
    Error(status, text.into())
}
pub fn json_response(value: Value, status: StatusCode) -> Response {
    let mut r = (status, axum::Json(value)).into_response();
    r.headers_mut()
        .insert("cache-control", HeaderValue::from_static("no-store"));
    r
}
fn redirect(target: &str) -> Response {
    let mut r = StatusCode::SEE_OTHER.into_response();
    if let Ok(v) = HeaderValue::from_str(target) {
        r.headers_mut().insert("location", v);
    }
    r
}
pub fn token() -> String {
    let mut bytes = Vec::new();
    bytes.extend(uuid::Uuid::new_v4().as_bytes());
    bytes.extend(uuid::Uuid::new_v4().as_bytes());
    URL_SAFE_NO_PAD.encode(bytes)
}
pub fn hash(value: &str) -> String {
    hex::encode(Sha256::digest(value.as_bytes()))
}
fn cookie(headers: &HeaderMap, name: &str) -> String {
    headers
        .get("cookie")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .split(';')
        .find_map(|p| {
            p.trim()
                .split_once('=')
                .filter(|(k, _)| *k == name)
                .map(|(_, v)| v.to_owned())
        })
        .unwrap_or_default()
}
fn set_cookie(response: &mut Response, name: &str, value: &str, seconds: u32) {
    if let Ok(v) = HeaderValue::from_str(&format!(
        "{name}={value}; Path=/; Max-Age={seconds}; Secure; HttpOnly; SameSite=Lax"
    )) {
        response.headers_mut().append("set-cookie", v);
    }
}
fn project_id(value: &str) -> bool {
    value.len() == 24
        && value
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}
pub fn canonical_path(raw: &str) -> Result<String> {
    let mut decoded = raw.to_owned();
    for _ in 0..8 {
        let next = percent_encoding::percent_decode_str(&decoded)
            .decode_utf8()
            .map_err(|_| "Invalid resource encoding")?
            .into_owned();
        if next == decoded {
            break;
        }
        decoded = next;
    }
    if percent_encoding::percent_decode_str(&decoded).decode_utf8_lossy() != decoded
        || decoded.contains(['\\', '\0'])
        || decoded.contains("//")
        || decoded.split('/').any(|s| s == "." || s == "..")
    {
        return Err("Invalid resource path".into());
    }
    Ok(decoded)
}
pub fn filtered_headers(input: &HeaderMap, request: bool) -> HeaderMap {
    let mut drop: HashSet<String> = [
        "connection",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
        "content-length",
        "content-encoding",
        "set-cookie",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    if let Some(connection) = input.get("connection").and_then(|v| v.to_str().ok()) {
        drop.extend(connection.split(',').map(|v| v.trim().to_ascii_lowercase()));
    }
    if request {
        drop.extend(
            [
                "host",
                "cookie",
                "authorization",
                "x-cbgpu-key",
                "x-cbgpu-identity",
                "x-forwarded-for",
                "x-forwarded-host",
                "x-forwarded-proto",
                "sec-websocket-extensions",
            ]
            .into_iter()
            .map(str::to_owned),
        );
    }
    input
        .iter()
        .filter(|(k, _)| !drop.contains(k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}
impl Gateway {
    pub fn router(self) -> Router {
        Router::new()
            .fallback(handler)
            .layer(DefaultBodyLimit::disable())
            .with_state(self)
    }
    pub async fn bridge(&self, path: &str, body: &Value) -> WebResult<Value> {
        let response = self
            .http
            .post(format!("{}/{path}", self.bridge_url))
            .bearer_auth(self.key.as_str())
            .json(body)
            .timeout(Duration::from_secs(180))
            .send()
            .await
            .map_err(|_| error(StatusCode::BAD_GATEWAY, "Editor adapter unavailable"))?;
        if response.status() == StatusCode::NOT_FOUND {
            return Err(error(StatusCode::NOT_FOUND, "Project not found"));
        }
        if !response.status().is_success() {
            return Err(error(
                StatusCode::BAD_GATEWAY,
                "Editor could not complete this request",
            ));
        }
        response
            .json()
            .await
            .map_err(|_| error(StatusCode::BAD_GATEWAY, "Invalid editor response"))
    }
    async fn identity(&self, headers: &HeaderMap) -> WebResult<Value> {
        let key = headers
            .get("x-cbgpu-key")
            .map(|v| v.as_bytes())
            .unwrap_or_default();
        if !bool::from(key.ct_eq(self.key.as_bytes())) {
            return Err(error(StatusCode::FORBIDDEN, "Forbidden"));
        }
        let user: Value = headers
            .get("x-cbgpu-identity")
            .and_then(|v| STANDARD.decode(v.as_bytes()).ok())
            .and_then(|v| serde_json::from_slice(&v).ok())
            .ok_or(error(StatusCode::UNAUTHORIZED, "Invalid identity"))?;
        if !self
            .store
            .authorized(
                user["id"].as_str().unwrap_or(""),
                user["session"].as_str().unwrap_or(""),
                false,
            )
            .await
        {
            return Err(error(StatusCode::UNAUTHORIZED, "CBGPU session expired"));
        }
        Ok(user)
    }
    async fn session(&self, headers: &HeaderMap) -> WebResult<Option<Value>> {
        let row = self.store.get("sessions", &cookie(headers, COOKIE)).await?;
        if let Some(row) = row
            && self
                .store
                .authorized(
                    row["user_id"].as_str().unwrap_or(""),
                    row["native_session"].as_str().unwrap_or(""),
                    false,
                )
                .await
        {
            return Ok(Some(row));
        }
        Ok(None)
    }
    async fn lock(&self, id: &str) -> Arc<Mutex<()>> {
        self.locks
            .lock()
            .await
            .entry(id.into())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }
    async fn incoming(&self, user: &Value, id: &str) -> WebResult<Value> {
        if uuid::Uuid::parse_str(id).is_err() {
            return Err(error(StatusCode::BAD_REQUEST, "Invalid upload"));
        }
        self.store
            .get("incoming", id)
            .await?
            .filter(|r| r["user_id"] == user["id"])
            .ok_or(error(StatusCode::NOT_FOUND, "Upload not found"))
    }
    async fn finish(&self, user: &Value, id: &str, path: PathBuf, name: &str) -> WebResult<Value> {
        let normalized = self
            .private
            .join("imports")
            .join(format!("{id}.normalized"));
        let out = normalized.clone();
        let source = path.clone();
        let inspected = tokio::task::spawn_blocking(move || archive::inspect(&source, &out))
            .await
            .map_err(|_| error(StatusCode::UNPROCESSABLE_ENTITY, "ZIP validator failed"))?;
        let result = async {
            let inspected = inspected.map_err(|v| error(StatusCode::UNPROCESSABLE_ENTITY, &v))?;
            tokio::fs::rename(
                &normalized,
                self.private.join("imports").join(format!("{id}.zip")),
            )
            .await
            .map_err(|_| {
                error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Cannot publish normalized ZIP",
                )
            })?;
            let mut metadata = serde_json::to_value(inspected).unwrap();
            metadata["name"] = json!(
                std::path::Path::new(name)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("Untitled")
                    .chars()
                    .take(100)
                    .collect::<String>()
            );
            self.store
                .put(
                    "uploads",
                    id,
                    &json!({"user_id":user["id"],"metadata":metadata}),
                    now() + 1800.,
                )
                .await?;
            metadata["upload"] = json!(id);
            Ok(metadata)
        }
        .await;
        let _ = tokio::fs::remove_file(normalized).await;
        let _ = tokio::fs::remove_file(path).await;
        result
    }
    async fn api(&self, req: Request, action: &str) -> WebResult<Response> {
        let user = self.identity(req.headers()).await?;
        if action == "projects" && req.method() == Method::GET {
            return Ok(json_response(
                self.bridge("projects", &json!({"user":user})).await?,
                StatusCode::OK,
            ));
        }
        if req.method() != Method::POST {
            return Err(error(StatusCode::METHOD_NOT_ALLOWED, "POST required"));
        }
        if action == "upload-chunk" {
            use tokio::io::{AsyncSeekExt, AsyncWriteExt};
            let bytes = to_bytes(req.into_body(), 8 * 1024 * 1024 + 1025)
                .await
                .map_err(|_| error(StatusCode::PAYLOAD_TOO_LARGE, "Upload chunk too large"))?;
            let split = bytes
                .iter()
                .position(|b| *b == b'\n')
                .filter(|v| *v <= 1024)
                .ok_or(error(StatusCode::BAD_REQUEST, "Invalid chunk header"))?;
            let data: Value = serde_json::from_slice(&bytes[..split])
                .map_err(|_| error(StatusCode::BAD_REQUEST, "Invalid chunk header"))?;
            let id = data["upload"].as_str().unwrap_or("");
            let lock = self.lock(id).await;
            let _guard = lock.lock().await;
            let row = self.incoming(&user, id).await?;
            let offset = data["offset"]
                .as_u64()
                .ok_or(error(StatusCode::BAD_REQUEST, "Invalid offset"))?;
            let chunk = &bytes[split + 1..];
            let path = self.private.join("imports").join(format!("{id}.partial"));
            let mut file = tokio::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .await
                .map_err(|_| error(StatusCode::CONFLICT, "Upload unavailable"))?;
            if row["received"].as_u64() != Some(offset)
                || file
                    .metadata()
                    .await
                    .map_err(|_| error(StatusCode::CONFLICT, "Upload unavailable"))?
                    .len()
                    != offset
            {
                return Err(error(StatusCode::CONFLICT, "Upload offset mismatch"));
            }
            if chunk.is_empty()
                || chunk.len() > 8 * 1024 * 1024
                || offset
                    .checked_add(chunk.len() as u64)
                    .is_none_or(|v| v > row["size"].as_u64().unwrap_or(0))
            {
                return Err(error(StatusCode::BAD_REQUEST, "Invalid upload chunk"));
            }
            file.seek(std::io::SeekFrom::Start(offset))
                .await
                .map_err(|_| error(StatusCode::INTERNAL_SERVER_ERROR, "Cannot seek upload"))?;
            archive::ensure_space(&self.private, chunk.len() as u64)
                .map_err(|v| error(StatusCode::INSUFFICIENT_STORAGE, &v))?;
            let written = async {
                file.write_all(chunk).await?;
                file.sync_all().await
            }
            .await;
            if written.is_err() {
                let _ = file.set_len(offset).await;
                return Err(error(
                    StatusCode::INSUFFICIENT_STORAGE,
                    "Cannot write upload; previous offset preserved",
                ));
            }
            let mut updated = row;
            updated["received"] = json!(offset + chunk.len() as u64);
            if let Err(e) = self
                .store
                .put("incoming", id, &updated, now() + 86400.)
                .await
            {
                let _ = file.set_len(offset).await;
                return Err(e.into());
            }
            return Ok(json_response(
                json!({"offset":updated["received"]}),
                StatusCode::OK,
            ));
        }
        if action == "inspect" {
            if self
                .store
                .pending(user["id"].as_str().unwrap_or(""))
                .await?
                >= 5
            {
                return Err(error(
                    StatusCode::TOO_MANY_REQUESTS,
                    "Finish pending imports first",
                ));
            }
            use axum::extract::FromRequest;
            use tokio::io::AsyncWriteExt;
            let mut form = axum::extract::Multipart::from_request(req, &())
                .await
                .map_err(|_| error(StatusCode::BAD_REQUEST, "ZIP multipart required"))?;
            let mut field = form
                .next_field()
                .await
                .map_err(|_| error(StatusCode::BAD_REQUEST, "Invalid multipart"))?
                .ok_or(error(StatusCode::BAD_REQUEST, "ZIP file required"))?;
            if field.name() != Some("file") {
                return Err(error(StatusCode::BAD_REQUEST, "ZIP file required"));
            }
            let name = field.file_name().unwrap_or("Untitled.zip").to_owned();
            let id = uuid::Uuid::new_v4().to_string();
            let staged = tempfile::Builder::new()
                .prefix(&format!("{id}-"))
                .suffix(".partial")
                .tempfile_in(self.private.join("imports"))
                .map_err(|_| error(StatusCode::INSUFFICIENT_STORAGE, "Cannot stage upload"))?;
            let path = staged.path().to_owned();
            let mut file = tokio::fs::File::from_std(
                staged
                    .as_file()
                    .try_clone()
                    .map_err(|_| error(StatusCode::INTERNAL_SERVER_ERROR, "Cannot open upload"))?,
            );
            while let Some(chunk) = field
                .chunk()
                .await
                .map_err(|_| error(StatusCode::BAD_REQUEST, "Invalid multipart chunk"))?
            {
                archive::ensure_space(&self.private, chunk.len() as u64)
                    .map_err(|v| error(StatusCode::INSUFFICIENT_STORAGE, &v))?;
                file.write_all(&chunk).await.map_err(|_| {
                    error(
                        StatusCode::INSUFFICIENT_STORAGE,
                        "Upload storage unavailable",
                    )
                })?;
            }
            drop(file);
            return Ok(json_response(
                self.finish(&user, &id, path, &name).await?,
                StatusCode::OK,
            ));
        }
        let bytes = to_bytes(req.into_body(), 65536)
            .await
            .map_err(|_| error(StatusCode::PAYLOAD_TOO_LARGE, "Request too large"))?;
        let data: Value = serde_json::from_slice(&bytes)
            .map_err(|_| error(StatusCode::BAD_REQUEST, "JSON required"))?;
        let result = match action {
            "upload-begin" => {
                let size = data["size"]
                    .as_u64()
                    .filter(|v| *v > 0 && *v <= 9007199254740991)
                    .ok_or(error(StatusCode::BAD_REQUEST, "Invalid ZIP size"))?;
                let name = data["name"]
                    .as_str()
                    .filter(|n| n.to_ascii_lowercase().ends_with(".zip"))
                    .ok_or(error(StatusCode::BAD_REQUEST, "Select a ZIP"))?;
                if self
                    .store
                    .pending(user["id"].as_str().unwrap_or(""))
                    .await?
                    >= 5
                {
                    return Err(error(
                        StatusCode::TOO_MANY_REQUESTS,
                        "Finish pending imports first",
                    ));
                }
                archive::ensure_space(&self.private, size)
                    .map_err(|v| error(StatusCode::INSUFFICIENT_STORAGE, &v))?;
                let id = uuid::Uuid::new_v4().to_string();
                let path = self.private.join("imports").join(format!("{id}.partial"));
                tokio::fs::OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .mode(0o600)
                    .open(&path)
                    .await
                    .map_err(|_| {
                        error(
                            StatusCode::INSUFFICIENT_STORAGE,
                            "Upload storage unavailable",
                        )
                    })?;
                if let Err(e)=self.store.put("incoming",&id,&json!({"user_id":user["id"],"name":name.chars().take(255).collect::<String>(),"size":size,"received":0}),now()+86400.).await{let _=tokio::fs::remove_file(&path).await;return Err(e.into());}
                json!({"upload":id,"offset":0})
            }
            "upload-finish" | "upload-cancel" => {
                let id = data["upload"].as_str().unwrap_or("");
                let lock = self.lock(id).await;
                let _guard = lock.lock().await;
                let row = self.incoming(&user, id).await?;
                let path = self.private.join("imports").join(format!("{id}.partial"));
                if action == "upload-cancel" {
                    let _ = tokio::fs::remove_file(path).await;
                    self.store.delete("incoming", id).await?;
                    json!({"ok":true})
                } else {
                    if row["received"] != row["size"]
                        || tokio::fs::metadata(&path)
                            .await
                            .map_err(|_| error(StatusCode::CONFLICT, "Upload incomplete"))?
                            .len()
                            != row["size"].as_u64().unwrap_or(0)
                    {
                        return Err(error(StatusCode::CONFLICT, "Upload incomplete"));
                    }
                    let result = self
                        .finish(
                            &user,
                            id,
                            path,
                            row["name"].as_str().unwrap_or("Untitled.zip"),
                        )
                        .await;
                    self.store.delete("incoming", id).await?;
                    result?
                }
            }
            "trash" => {
                let project = data["project"]
                    .as_str()
                    .filter(|v| project_id(v))
                    .ok_or(error(StatusCode::NOT_FOUND, "Project not found"))?;
                self.bridge("trash", &json!({"user":user,"project":project}))
                    .await?
            }
            "new" => {
                let id = uuid::Uuid::new_v4().to_string();
                let path = self.private.join("imports").join(format!("{id}.zip"));
                archive::blank(&path)?;
                let result=self.bridge("import",&json!({"user":user,"upload":id,"main":"main.tex","compiler":"pdflatex","name":"Untitled project"})).await;
                let _ = tokio::fs::remove_file(path).await;
                result?
            }
            "import" => {
                let id = data["upload"]
                    .as_str()
                    .filter(|v| uuid::Uuid::parse_str(v).is_ok())
                    .ok_or(error(StatusCode::BAD_REQUEST, "Invalid upload"))?;
                let lock = self.lock(id).await;
                let _guard = lock.lock().await;
                let mut row = self
                    .store
                    .get("uploads", id)
                    .await?
                    .filter(|r| r["user_id"] == user["id"])
                    .ok_or(error(StatusCode::GONE, "Import expired"))?;
                if let Some(result) = row.get("result") {
                    result.clone()
                } else {
                    self.store.put("uploads", id, &row, now() + 1800.).await?;
                    let info = &row["metadata"];
                    let main = data["main"]
                        .as_str()
                        .filter(|m| {
                            m.to_ascii_lowercase().ends_with(".tex")
                                && info["files"]
                                    .as_array()
                                    .is_some_and(|a| a.iter().any(|v| v.as_str() == Some(*m)))
                        })
                        .ok_or(error(StatusCode::BAD_REQUEST, "Select a main TeX source"))?;
                    let compiler = data["compiler"].as_str().unwrap_or("pdflatex");
                    if !matches!(compiler, "pdflatex" | "xelatex" | "lualatex" | "latex") {
                        return Err(error(StatusCode::BAD_REQUEST, "Unsupported compiler"));
                    }
                    let name = data["name"]
                        .as_str()
                        .or(info["name"].as_str())
                        .unwrap_or("Untitled")
                        .chars()
                        .take(120)
                        .collect::<String>();
                    let result=self.bridge("import",&json!({"user":user,"upload":id,"main":main,"compiler":compiler,"name":name})).await?;
                    row["result"] = result.clone();
                    self.store.put("uploads", id, &row, now() + 300.).await?;
                    result
                }
            }
            "open" => {
                let project = data["project"].as_str().unwrap_or("");
                if !project.is_empty() {
                    if !project_id(project) {
                        return Err(error(StatusCode::BAD_REQUEST, "Invalid project"));
                    }
                    self.check_project(&user, project, false).await?;
                }
                let challenge = data["challenge"].as_str().unwrap_or("");
                let proof = self
                    .store
                    .consume("challenges", challenge)
                    .await?
                    .ok_or(error(StatusCode::GONE, "Restart editor connection"))?;
                let ce = self.bridge("session", &json!({"user":user})).await?;
                let id = token();
                let ticket = token();
                self.store.put("sessions",&id,&json!({"id":id,"user_id":user["id"],"native_session":user["session"],"ce_cookie":ce["cookie"]}),now()+43200.).await?;
                self.store.put("tickets",&hash(&ticket),&json!({"session_id":id,"proof":proof["proof"],"target":if project.is_empty(){"/project".into()}else{format!("/project/{project}")}}),now()+60.).await?;
                json!({"url":format!("{PUBLIC}/cbgpu/enter?ticket={ticket}")})
            }
            _ => return Err(error(StatusCode::NOT_FOUND, "Not found")),
        };
        Ok(json_response(
            result,
            if matches!(action, "new" | "import") {
                StatusCode::CREATED
            } else {
                StatusCode::OK
            },
        ))
    }
    async fn check_project(&self, user: &Value, project: &str, trashed: bool) -> WebResult<()> {
        let rows = self
            .bridge("projects", &json!({"user":user,"includeTrashed":trashed}))
            .await?;
        if !rows["projects"]
            .as_array()
            .is_some_and(|a| a.iter().any(|v| v["id"].as_str() == Some(project)))
        {
            return Err(error(StatusCode::NOT_FOUND, "Project not found"));
        }
        Ok(())
    }
}
async fn handler(State(state): State<Gateway>, mut req: Request) -> WebResult<Response> {
    let path = canonical_path(req.uri().path()).map_err(|v| error(StatusCode::BAD_REQUEST, &v))?;
    if let Some(action) = path.strip_prefix("/_api/") {
        return state.api(req, action).await;
    }
    let query: HashMap<String, String> =
        url::form_urlencoded::parse(req.uri().query().unwrap_or("").as_bytes())
            .into_owned()
            .collect();
    if path == "/cbgpu/connect" {
        let project = query.get("project").map(String::as_str).unwrap_or("");
        if !project.is_empty() && !project_id(project) {
            return Err(error(StatusCode::BAD_REQUEST, "Invalid project"));
        }
        let proof = token();
        let challenge = token();
        state
            .store
            .put(
                "challenges",
                &challenge,
                &json!({"proof":hash(&proof)}),
                now() + 300.,
            )
            .await?;
        let portal = if query.get("portal").is_some_and(|v| v == "app") {
            "https://app.cbgpu.com/latex"
        } else {
            PORTAL
        };
        let params = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("connect", &challenge)
            .append_pair("project", project)
            .append_pair("mode", "researcher")
            .finish();
        let mut response = redirect(&format!("{portal}?{params}"));
        set_cookie(&mut response, "__Host-cbgpu-latex-proof", &proof, 300);
        return Ok(response);
    }
    if path == "/cbgpu/enter" {
        let ticket = hash(query.get("ticket").map(String::as_str).unwrap_or(""));
        let row = state
            .store
            .get("tickets", &ticket)
            .await?
            .ok_or(error(StatusCode::UNAUTHORIZED, "Editor connection expired"))?;
        let proof = hash(&cookie(req.headers(), "__Host-cbgpu-latex-proof"));
        if !bool::from(
            proof
                .as_bytes()
                .ct_eq(row["proof"].as_str().unwrap_or("").as_bytes()),
        ) {
            return Err(error(
                StatusCode::UNAUTHORIZED,
                "Different browser or expired connection",
            ));
        }
        let session = state
            .store
            .get("sessions", row["session_id"].as_str().unwrap_or(""))
            .await?
            .ok_or(error(StatusCode::UNAUTHORIZED, "Session expired"))?;
        if !state
            .store
            .authorized(
                session["user_id"].as_str().unwrap_or(""),
                session["native_session"].as_str().unwrap_or(""),
                false,
            )
            .await
        {
            return Err(error(StatusCode::UNAUTHORIZED, "Session expired"));
        }
        if state.store.consume("tickets", &ticket).await?.is_none() {
            return Err(error(StatusCode::UNAUTHORIZED, "Ticket already used"));
        }
        let mut response = redirect(row["target"].as_str().unwrap_or("/project"));
        set_cookie(
            &mut response,
            COOKIE,
            row["session_id"].as_str().unwrap_or(""),
            43200,
        );
        return Ok(response);
    }
    if path == "/healthz" {
        return Ok(json_response(
            json!({"status":"ok","service":"cbgpu-latex"}),
            StatusCode::OK,
        ));
    }
    if matches!(
        path.as_str(),
        "/cbgpu/editor.js" | "/cbgpu/editor.css" | "/cbgpu/logo.png"
    ) {
        let file = if path.ends_with("editor.js") {
            state.source.join("dist/editor.js")
        } else if path.ends_with("editor.css") {
            state.source.join("editor.css")
        } else {
            state
                .source
                .join("../../FrontEnd/cbgpu/public/img/cbgpu-logo.png")
        };
        let bytes = tokio::fs::read(file)
            .await
            .map_err(|_| error(StatusCode::NOT_FOUND, "Asset unavailable"))?;
        let mime = if path.ends_with(".js") {
            "application/javascript"
        } else if path.ends_with(".css") {
            "text/css"
        } else {
            "image/png"
        };
        return Ok((
            [("content-type", mime), ("cache-control", "no-cache")],
            bytes,
        )
            .into_response());
    }
    if matches!(path.as_str(), "/cbgpu/source" | "/cbgpu/source.zip") {
        let source = state.source.clone();
        let data = tokio::task::spawn_blocking(move || crate::operations::source_offer(&source))
            .await
            .map_err(|_| error(StatusCode::INTERNAL_SERVER_ERROR, "Source unavailable"))??;
        return Ok((
            [
                ("content-type", "application/zip"),
                (
                    "content-disposition",
                    "attachment; filename=\"cbgpu-latex-source.zip\"",
                ),
            ],
            data,
        )
            .into_response());
    }
    let route = path.trim_end_matches('/').to_ascii_lowercase();
    let Some(session) = state.session(req.headers()).await? else {
        return if req.method() == Method::GET
            && !req
                .headers()
                .get("upgrade")
                .is_some_and(|v| v == "websocket")
        {
            Ok(redirect(PORTAL))
        } else {
            Err(error(
                StatusCode::UNAUTHORIZED,
                "Reopen the editor from CBGPU",
            ))
        };
    };
    if ["/internal", "/status", "/health_check"]
        .iter()
        .any(|prefix| route.starts_with(prefix))
        || regex::Regex::new(r"^/(?:read/[a-z]{12}|[0-9]+[a-z]{6,12})(?:/grant)?$")
            .unwrap()
            .is_match(&route)
    {
        return Err(error(StatusCode::NOT_FOUND, "Not found"));
    }
    if ["/login", "/register", "/launchpad"].contains(&path.as_str())
        || ["/admin", "/user/email", "/user/password", "/user/delete"]
            .iter()
            .any(|v| path.starts_with(v))
    {
        return Ok(redirect(PORTAL));
    }
    if !matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS)
        && req.headers().get("origin").and_then(|v| v.to_str().ok()) != Some(PUBLIC)
    {
        return Err(error(StatusCode::FORBIDDEN, "Invalid origin"));
    }
    if route == "/project/new/upload" {
        return Err(error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Use CBGPU ZIP import",
        ));
    }
    let pattern =
        regex::Regex::new(r"(?i)^/(?:project|download/project)/([a-f0-9]{24})(?:/|$)").unwrap();
    if let Some(found) = pattern.captures(&path) {
        let id = found[1].to_ascii_lowercase();
        let trash = req.method() == Method::DELETE
            && (route == format!("/project/{id}") || route == format!("/project/{id}/trash"));
        state
            .check_project(
                &json!({"id":session["user_id"],"email":"session@cbgpu.com"}),
                &id,
                trash,
            )
            .await?;
        if ((req.method() == Method::POST && route == format!("/project/{id}/compile"))
            || (req.method() == Method::GET
                && [
                    format!("/project/{id}/wordcount"),
                    format!("/project/{id}/sync/code"),
                    format!("/project/{id}/sync/pdf"),
                ]
                .contains(&route)))
            && !state
                .store
                .authorized(
                    session["user_id"].as_str().unwrap_or(""),
                    session["native_session"].as_str().unwrap_or(""),
                    true,
                )
                .await
        {
            return Err(error(
                StatusCode::FORBIDDEN,
                "Your projects remain available; school compute permission is required to compile",
            ));
        }
    }
    let mut headers = filtered_headers(req.headers(), true);
    headers.insert("host", HeaderValue::from_static("latex.cbgpu.com"));
    headers.insert("x-forwarded-proto", HeaderValue::from_static("https"));
    headers.insert(
        "x-forwarded-host",
        HeaderValue::from_static("latex.cbgpu.com"),
    );
    headers.insert(
        "cookie",
        HeaderValue::from_str(session["ce_cookie"].as_str().unwrap_or(""))
            .map_err(|_| error(StatusCode::UNAUTHORIZED, "Invalid editor session"))?,
    );
    let url = format!(
        "{}{}",
        state.editor_url,
        req.uri()
            .path_and_query()
            .map(|v| v.as_str())
            .unwrap_or("/")
    );
    if req
        .headers()
        .get("upgrade")
        .is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"websocket"))
    {
        if req.headers().get("origin").and_then(|v| v.to_str().ok()) != Some(PUBLIC) {
            return Err(error(StatusCode::FORBIDDEN, "Invalid websocket origin"));
        }
        let mut up = url
            .replacen("http://", "ws://", 1)
            .into_client_request()
            .map_err(|_| error(StatusCode::BAD_GATEWAY, "Invalid editor websocket"))?;
        for (k, v) in &headers {
            if k != "sec-websocket-key" && k != "sec-websocket-version" {
                up.headers_mut().insert(k, v.clone());
            }
        }
        let (upstream, _) = tokio_tungstenite::connect_async(up)
            .await
            .map_err(|_| error(StatusCode::BAD_GATEWAY, "Editor realtime unavailable"))?;
        use axum::extract::FromRequestParts;
        let (mut parts, _) = std::mem::replace(&mut req, Request::new(Body::empty())).into_parts();
        let upgrade = WebSocketUpgrade::from_request_parts(&mut parts, &())
            .await
            .map_err(|_| error(StatusCode::BAD_REQUEST, "Invalid websocket"))?;
        return Ok(upgrade.on_upgrade(move|mut downstream|async move {let(mut tx,mut rx)=upstream.split();let mut tick=tokio::time::interval(state.revocation_interval);loop{tokio::select!{
   message=downstream.recv()=>match message{Some(Ok(Message::Text(v)))=>{if tx.send(UpMessage::Text(v.to_string().into())).await.is_err(){break;}},Some(Ok(Message::Binary(v)))=>{if tx.send(UpMessage::Binary(v)).await.is_err(){break;}},Some(Ok(Message::Ping(v)))=>{let _=tx.send(UpMessage::Ping(v)).await;},Some(Ok(Message::Pong(v)))=>{let _=tx.send(UpMessage::Pong(v)).await;},_=>break},
   message=rx.next()=>match message{Some(Ok(UpMessage::Text(v)))=>{if downstream.send(Message::Text(v.to_string().into())).await.is_err(){break;}},Some(Ok(UpMessage::Binary(v)))=>{if downstream.send(Message::Binary(v)).await.is_err(){break;}},Some(Ok(UpMessage::Ping(v)))=>{let _=downstream.send(Message::Ping(v)).await;},Some(Ok(UpMessage::Pong(v)))=>{let _=downstream.send(Message::Pong(v)).await;},_=>break},
   _=tick.tick()=>{if !state.store.authorized(session["user_id"].as_str().unwrap_or(""),session["native_session"].as_str().unwrap_or(""),false).await{break;}}
  }}let _=tx.close().await;let _=downstream.close().await;}).into_response());
    }
    let method = req.method().clone();
    let response = state
        .http
        .request(method, url)
        .headers(headers)
        .body(reqwest::Body::wrap_stream(
            req.into_body().into_data_stream(),
        ))
        .timeout(Duration::from_secs(360))
        .send()
        .await
        .map_err(|_| error(StatusCode::BAD_GATEWAY, "Editor unavailable"))?;
    let status = response.status();
    let mut headers = filtered_headers(response.headers(), false);
    headers.insert("cache-control", HeaderValue::from_static("no-store"));
    headers.insert(
        "x-robots-tag",
        HeaderValue::from_static("noindex, nofollow"),
    );
    headers.insert("referrer-policy", HeaderValue::from_static("same-origin"));
    headers.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    let body = if headers
        .get("content-type")
        .is_some_and(|v| v.to_str().unwrap_or("").contains("text/html"))
    {
        let mut inject="<link rel=\"stylesheet\" href=\"/cbgpu/editor.css?v=20261029-green2\"><script src=\"/cbgpu/editor.js?v=20261029-green2\" defer></script><a class=\"cbgpu-return\" href=\"https://researcher.cbgpu.com/latex\">CBGPU &middot; Projects</a>".to_owned();
        if let Some(csp) = headers
            .get("content-security-policy")
            .and_then(|v| v.to_str().ok())
            && let Some(nonce) = regex::Regex::new(r"'nonce-([A-Za-z0-9+/=_-]+)'")
                .unwrap()
                .captures(csp)
        {
            inject = inject.replace("<script ", &format!("<script nonce=\"{}\" ", &nonce[1]));
        }
        let bytes = response
            .bytes()
            .await
            .map_err(|_| error(StatusCode::BAD_GATEWAY, "Invalid editor HTML"))?;
        Body::from(String::from_utf8_lossy(&bytes).replace("</body>", &format!("{inject}</body>")))
    } else {
        Body::from_stream(response.bytes_stream())
    };
    let mut result = Response::new(body);
    *result.status_mut() = status;
    *result.headers_mut() = headers;
    Ok(result)
}
