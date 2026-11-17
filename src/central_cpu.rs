use base64::{Engine, engine::general_purpose::STANDARD};
// Private central-CPU admission: durable SQL leases, signed one-use broker context.
use crate::Result;
use hmac::{Hmac, Mac};
use serde_json::{Value, json};
use sha2::Sha256;
use sqlx::PgPool;
use std::io::Read;
use std::{
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};
#[derive(Clone)]
pub struct CentralCPU {
    pub pool: PgPool,
    pub private: PathBuf,
}
pub fn enabled() -> Result<bool> {
    match std::env::var("CBGPU_CENTRAL_CPU_V1").as_deref() {
        Err(std::env::VarError::NotPresent) | Ok("") | Ok("0") | Ok("false") => Ok(false),
        Ok("1") | Ok("true") => Ok(true),
        _ => Err("Invalid central CPU configuration".into()),
    }
}
fn valid_project(p: &str) -> bool {
    p.len() == 24 && p.bytes().all(|b| b.is_ascii_hexdigit())
}
fn key(private: &Path) -> Result<Vec<u8>> {
    use std::os::unix::fs::MetadataExt;
    let p = private.join("central-cpu-admission-key");
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(p)
        .map_err(|_| "Private CPU admission key unavailable")?;
    let s = f.metadata().map_err(|_| "Invalid CPU admission key")?;
    if !s.is_file()
        || s.len() != 32
        || s.mode() & 0o077 != 0
        || s.uid() != unsafe { libc::getuid() }
    {
        return Err("Invalid private CPU admission key".into());
    }
    let mut b = vec![];
    f.read_to_end(&mut b)
        .map_err(|_| "Cannot read CPU admission key")?;
    Ok(b)
}
fn signed(private: &Path, payload: &Value) -> Result<Value> {
    let bytes = serde_json::to_vec(payload).map_err(|_| "Invalid CPU context")?;
    let mut mac = Hmac::<Sha256>::new_from_slice(&key(private)?).map_err(|_| "Invalid CPU key")?;
    mac.update(&bytes);
    Ok(
        json!({"version":1,"payload":STANDARD.encode(&bytes),"mac":hex::encode(mac.finalize().into_bytes())}),
    )
}
fn verified(private: &Path, envelope: &Value) -> Result<Value> {
    if envelope["version"] != 1 {
        return Err("Unsupported CPU context".into());
    }
    let bytes = STANDARD
        .decode(envelope["payload"].as_str().ok_or("Missing CPU context")?)
        .map_err(|_| "Invalid CPU context bytes")?;
    let tag = hex::decode(envelope["mac"].as_str().ok_or("Missing CPU signature")?)
        .map_err(|_| "Invalid CPU signature")?;
    let mut mac = Hmac::<Sha256>::new_from_slice(&key(private)?).map_err(|_| "Invalid CPU key")?;
    mac.update(&bytes);
    mac.verify_slice(&tag)
        .map_err(|_| "CPU context authentication failed")?;
    serde_json::from_slice(&bytes).map_err(|_| "Invalid authenticated CPU context".into())
}
impl CentralCPU {
    pub async fn call(&self, action: &str, payload: Value) -> Result<Value> {
        if !enabled()? {
            return Err("Central CPU not enabled".into());
        }
        let v = sqlx::query_scalar::<_, Value>("SELECT public.cbgpu_platform_cpu_latex($1,$2)")
            .bind(action)
            .bind(payload)
            .fetch_one(&self.pool)
            .await
            .map_err(|_| "Central CPU authority unavailable")?;
        if !v.is_object() {
            return Err("Invalid CPU authority response".into());
        }
        Ok(v)
    }
    pub fn bound(&self, project: &str) -> Result<bool> {
        Ok(self
            .mode(project)?
            .is_some_and(|m| m["execution_mode"] == "CENTRAL_V1"))
    }
    pub fn mode(&self, project: &str) -> Result<Option<Value>> {
        if !valid_project(project) {
            return Err("Invalid CPU project".into());
        }
        let p = self
            .private
            .join("central-cpu-modes")
            .join(format!("{project}.json"));
        if !p.exists() {
            return Ok(None);
        }
        let bytes = std::fs::read(p).map_err(|_| "Cannot inspect project mode")?;
        if bytes.len() > 65536 {
            return Err("Invalid project mode".into());
        };
        let v = verified(
            &self.private,
            &serde_json::from_slice(&bytes).map_err(|_| "Invalid project mode")?,
        )?;
        if v["project_id"] != project {
            return Err("Project mode mismatch".into());
        }
        Ok(Some(v))
    }
    fn write_mode(&self, project: &str, value: &Value) -> Result<()> {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        let dir = self.private.join("central-cpu-modes");
        std::fs::create_dir_all(&dir).map_err(|_| "Cannot persist project mode")?;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|_| "Cannot protect project mode")?;
        let temp = dir.join(format!("{project}.{}.tmp", uuid::Uuid::new_v4()));
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)
            .map_err(|_| "Cannot persist project mode")?;
        f.write_all(
            &serde_json::to_vec(&signed(&self.private, value)?)
                .map_err(|_| "Invalid project mode")?,
        )
        .map_err(|_| "Cannot persist project mode")?;
        f.sync_all().map_err(|_| "Cannot sync project mode")?;
        std::fs::rename(temp, dir.join(format!("{project}.json")))
            .map_err(|_| "Cannot publish project mode")?;
        Ok(())
    }
    pub fn register_unassigned(&self, project: &str, owner: &str) -> Result<()> {
        if !valid_project(project) || uuid::Uuid::parse_str(owner).is_err() {
            return Err("Invalid owned project".into());
        };
        if self.mode(project)?.is_some() || self.bound(project)? {
            return Err("Project already has allocation history".into());
        }
        self.write_mode(project,&json!({"project_id":project,"owner_id":owner,"execution_mode":"UNASSIGNED_V1","never_executed":true}))
    }
    pub async fn allocate(
        &self,
        actor: &str,
        session: &str,
        owner: &str,
        project: &str,
        school: &str,
    ) -> Result<Value> {
        let mode = self
            .mode(project)?
            .ok_or("Legacy project cannot silently change CPU allocation")?;
        if actor != owner
            || mode["owner_id"] != owner
            || mode["execution_mode"] != "UNASSIGNED_V1"
            || mode["never_executed"] != true
            || self.bound(project)?
        {
            return Err("Only a never-executed owned personal draft can be allocated".into());
        }
        let out=self.call("seal",json!({"project_kind":"latex","project_id":project,"owner_id":owner,"actor_id":actor,"actor_session_id":session,"school_id":school,"execution_mode":"UNASSIGNED_V1","never_executed":true})).await?;
        if out["allowed"] != true || out["school_id"] != school {
            return Err("CPU allocation denied".into());
        }
        let marker = json!({"project_id":project,"owner_id":owner,"school_id":school,"execution_mode":"CENTRAL_V1","never_executed":true});
        self.write_mode(project, &marker)?;
        Ok(
            json!({"ok":true,"execution_mode":"CENTRAL_V1","school_id":school,"allocation_school_id":school}),
        )
    }
    pub async fn prepare(
        &self,
        actor: &str,
        session: &str,
        owner: &str,
        project: &str,
    ) -> Result<()> {
        if !valid_project(project) {
            return Err("Invalid CPU project".into());
        }
        let mode = self
            .mode(project)?
            .ok_or("Explicit CPU allocation required before compile")?;
        if mode["execution_mode"] != "CENTRAL_V1"
            || mode["owner_id"] != owner
            || !self.bound(project)?
        {
            return Err("Explicit CPU allocation required before compile".into());
        }
        let identity = json!({"school_id":mode["school_id"],"project_kind":"latex","project_id":project,"actor_id":actor,"actor_session_id":session,"owner_id":owner});
        let seal = self.call("seal", identity.clone()).await?;
        if seal["allowed"] != true {
            return Err("This school has no central CPU allocation".into());
        }
        let school = seal["school_id"].as_str().ok_or("Missing sealed school")?;
        let directory = self.private.join("central-cpu-admissions");
        std::fs::create_dir_all(&directory).map_err(|_| "Cannot stage private CPU admission")?;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))
            .map_err(|_| "Cannot protect CPU admission")?;
        let p = directory.join(format!("{project}.json"));
        // Only an UNUSED pending admission can expire locally: SQL acquire occurs
        // after claim moves it to a separate immutable claimed file.
        let _pending_guard = ClaimLock::new(directory.join(format!("{project}.claim-lock")))?;
        if p.exists() {
            let mut f = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(&p)
                .map_err(|_| "Cannot inspect pending admission")?;
            if f.metadata().map_err(|_| "Invalid pending admission")?.len() > 65536 {
                return Err("Invalid pending admission size".into());
            }
            let mut bytes = Vec::new();
            f.read_to_end(&mut bytes)
                .map_err(|_| "Cannot read pending admission")?;
            let old = verified(
                &self.private,
                &serde_json::from_slice(&bytes).map_err(|_| "Invalid pending admission")?,
            )?;
            if old["project_id"] != project
                || old["owner_id"] != owner
                || old["expires_at"].as_f64().unwrap_or(f64::INFINITY) >= crate::store::now()
            {
                return Err("Compile admission already pending; retry after expiry".into());
            }
            std::fs::remove_file(&p).map_err(|_| "Cannot clear expired unused admission")?;
        }
        let mut random = [0u8; 32];
        getrandom::fill(&mut random).map_err(|_| "Private CPU randomness unavailable")?;
        let nonce = hex::encode(random);
        let job = uuid::Uuid::new_v4().to_string();
        let mut context = identity;
        context["school_id"] = json!(school);
        context["purpose"] = json!("latex_compile");
        context["job_id"] = json!(job);
        context["idempotency_key"] = json!(job);
        context["lease_nonce"] = json!(nonce);
        context["cpu_millis"] = json!(2000);
        context["memory_bytes"] = json!(2_u64 * 1024 * 1024 * 1024);
        context["storage_bytes"] = json!(0);
        context["expires_at"] = json!(crate::store::now() + 60.);
        // Persist nonce BEFORE network acquire. An unknown outcome keeps its budget and
        // exact context for operator reconciliation; never create a fresh retry token.
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&p)
            .map_err(|_| "Compile admission already pending; inspect previous request")?;
        use std::io::Write;
        let mut file = file;
        file.write_all(
            &serde_json::to_vec(&signed(&self.private, &context)?)
                .map_err(|_| "Invalid CPU context")?,
        )
        .map_err(|_| "Cannot persist CPU nonce")?;
        file.sync_all().map_err(|_| "Cannot sync CPU nonce")?;

        Ok(())
    }
    pub fn claim(&self, project: &str, job: &str) -> Result<Value> {
        if !valid_project(project) || uuid::Uuid::parse_str(job).is_err() {
            return Err("Invalid broker admission identity".into());
        }
        let dir = self.private.join("central-cpu-admissions");
        let from = dir.join(format!("{project}.json"));
        let to = dir.join(format!("{project}.claimed-{job}.json"));
        let lock = dir.join(format!("{project}.claim-lock"));
        let _guard = ClaimLock::new(lock)?;
        if to.exists() {
            return Err("Compile admission was already consumed".into());
        }
        std::fs::rename(&from, &to).map_err(|_| "No unused authenticated compile admission")?;
        let mut f = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(to)
            .map_err(|_| "Cannot inspect compile admission")?;
        if f.metadata().map_err(|_| "Invalid CPU admission")?.len() > 65536 {
            return Err("Invalid CPU admission size".into());
        }
        let mut bytes = vec![];
        f.read_to_end(&mut bytes)
            .map_err(|_| "Cannot read CPU admission")?;
        let v = verified(
            &self.private,
            &serde_json::from_slice(&bytes).map_err(|_| "Invalid CPU admission")?,
        )?;
        if v["project_id"] != project
            || v["purpose"] != "latex_compile"
            || v["expires_at"].as_f64().unwrap_or(0.) < crate::store::now()
        {
            return Err("Expired or mismatched CPU admission".into());
        };
        Ok(v)
    }
    pub async fn acquire(&self, context: &Value) -> Result<Value> {
        let mut payload = json!({});
        for key in [
            "school_id",
            "project_kind",
            "project_id",
            "actor_id",
            "actor_session_id",
            "owner_id",
            "purpose",
            "job_id",
            "idempotency_key",
            "lease_nonce",
            "cpu_millis",
            "memory_bytes",
            "storage_bytes",
        ] {
            payload[key] = context[key].clone();
        }
        let lease = self.call("acquire", payload).await?;
        if lease["allowed"] != true {
            return Err("Central CPU compile quota unavailable".into());
        }
        let mut out = context.clone();
        out["lease_id"] = lease["lease_id"].clone();
        Ok(out)
    }
    pub async fn start(&self, c: &Value, instance: &str) -> Result<()> {
        let v=self.call("start",json!({"lease_id":c["lease_id"],"lease_token":c["lease_nonce"],"actor_session_id":c["actor_session_id"],"instance_id":instance})).await?;
        if v["allowed"] != true {
            return Err("CPU permission revoked before compile".into());
        }
        Ok(())
    }
    pub async fn heartbeat(&self, c: &Value, instance: &str) -> Result<bool> {
        let v=self.call("heartbeat",json!({"lease_id":c["lease_id"],"lease_token":c["lease_nonce"],"instance_id":instance})).await?;
        Ok(v["allowed"] == true)
    }
    pub async fn release(&self, c: &Value, instance: &str) -> Result<()> {
        self.call("release",json!({"lease_id":c["lease_id"],"lease_token":c["lease_nonce"],"instance_id":instance,"stopped":true,"proof":"CONTAINER_ABSENT"})).await?;
        Ok(())
    }
}

struct ClaimLock(PathBuf);
impl ClaimLock {
    fn new(path: PathBuf) -> Result<Self> {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .map_err(|_| "CPU admission claim already in progress")?;
        Ok(Self(path))
    }
}
impl Drop for ClaimLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    fn instance() -> (tempfile::TempDir, CentralCPU) {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("central-cpu-admission-key"), [7u8; 32]).unwrap();
        std::fs::set_permissions(
            d.path().join("central-cpu-admission-key"),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://unused:unused@127.0.0.1:1/unused")
            .unwrap();
        let a = CentralCPU {
            pool,
            private: d.path().to_owned(),
        };
        (d, a)
    }
    #[tokio::test]
    async fn personal_metadata_has_no_allocation_and_signature_prevents_owner_spoof() {
        let (d, a) = instance();
        let p = "abcdefabcdefabcdefabcdef";
        a.register_unassigned(p, "11111111-1111-4111-8111-111111111111")
            .unwrap();
        assert!(!a.bound(p).unwrap());
        assert_eq!(
            a.mode(p).unwrap().unwrap()["execution_mode"],
            "UNASSIGNED_V1"
        );
        let file = d.path().join("central-cpu-modes").join(format!("{p}.json"));
        let mut v: Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
        v["payload"] =
            json!(STANDARD.encode(br#"{"execution_mode":"CENTRAL_V1","owner_id":"forged"}"#));
        std::fs::write(file, serde_json::to_vec(&v).unwrap()).unwrap();
        assert!(a.mode(p).is_err());
    }
    #[tokio::test]
    async fn admission_is_project_bound_expiring_and_atomically_single_use() {
        let (d, a) = instance();
        let p = "abcdefabcdefabcdefabcdef";
        let dir = d.path().join("central-cpu-admissions");
        std::fs::create_dir(&dir).unwrap();
        let payload = json!({"project_id":p,"purpose":"latex_compile","expires_at":crate::store::now()+60.,"lease_nonce":"a".repeat(64)});
        std::fs::write(
            dir.join(format!("{p}.json")),
            serde_json::to_vec(&signed(d.path(), &payload).unwrap()).unwrap(),
        )
        .unwrap();
        let job = uuid::Uuid::new_v4().to_string();
        assert_eq!(a.claim(p, &job).unwrap()["project_id"], p);
        assert!(a.claim(p, &uuid::Uuid::new_v4().to_string()).is_err());
        assert!(!dir.join(format!("{p}.json")).exists());
        let mut bad = payload.clone();
        bad["project_id"] = json!("ffffffffffffffffffffffff");
        std::fs::write(
            dir.join(format!("{p}.json")),
            serde_json::to_vec(&signed(d.path(), &bad).unwrap()).unwrap(),
        )
        .unwrap();
        assert!(a.claim(p, &uuid::Uuid::new_v4().to_string()).is_err());
        let mut old = payload;
        old["expires_at"] = json!(1);
        std::fs::write(
            dir.join(format!("{p}.json")),
            serde_json::to_vec(&signed(d.path(), &old).unwrap()).unwrap(),
        )
        .unwrap();
        assert!(a.claim(p, &uuid::Uuid::new_v4().to_string()).is_err());
    }
}
