//! Optional real isolated GoTrue/PostgREST stage identity contract. Read-only;
//! never queries project state, source files, auth rows, or production endpoints.
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use cbgpu_latex::store::Store;
use serde_json::Value;
use sqlx::postgres::PgPoolOptions;
#[tokio::test]
#[ignore = "Requires explicitly prepared private isolated stage actor/DSN files, never production"]
async fn actual_school_identity_is_separate_from_cbsh_compute() {
    let dsn_file =
        std::env::var("CBGPU_LATEX_TEST_STAGE_DSN_FILE").expect("explicit stage DSN file");
    let actors_file = std::env::var("CBGPU_LATEX_TEST_STAGE_ACTORS_FILE")
        .expect("explicit synthetic stage actors");
    let dsn = std::fs::read_to_string(dsn_file).expect("private stage credential");
    let uri = reqwest::Url::parse(dsn.trim()).expect("valid stage URL");
    assert_eq!(uri.host_str(), Some("127.0.0.1"));
    assert_eq!(uri.port(), Some(55485));
    assert!(uri.username().starts_with("cbgpu_latex_runtime_"));
    let actors: Value =
        serde_json::from_slice(&std::fs::read(actors_file).expect("stage actor fixture")).unwrap();
    let actor = &actors["student"];
    let user = actor["id"].as_str().unwrap();
    let claims: Value = serde_json::from_slice(
        &URL_SAFE_NO_PAD
            .decode(actor["token"].as_str().unwrap().split('.').nth(1).unwrap())
            .unwrap(),
    )
    .unwrap();
    let session = claims["session_id"].as_str().unwrap();
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(dsn.trim())
        .await
        .unwrap_or_else(|error| {
            panic!(
                "Isolated stage role connection unavailable: {}",
                error
                    .as_database_error()
                    .and_then(|e| e.code())
                    .map(|s| s.into_owned())
                    .unwrap_or_else(|| "transport".into())
            )
        });
    let restricted:bool=sqlx::query_scalar("SELECT NOT rolsuper AND NOT rolbypassrls AND pg_has_role(current_user,'cbgpu_latex','member') AND NOT has_schema_privilege(current_user,'auth','USAGE') AND NOT has_table_privilege(current_user,'public.profiles','SELECT') FROM pg_roles WHERE rolname=current_user").fetch_one(&pool).await.unwrap();
    assert!(
        restricted,
        "stage uses narrow gateway role, not a service-role reader"
    );
    let scoped = Store {
        pool: pool.clone(),
        account_v2: true,
    };
    assert!(
        scoped.authorized(user, session, false).await,
        "fresh SchoolB identity may retain owned projects"
    );
    assert!(
        !scoped.authorized(user, session, true).await,
        "SchoolB membership never grants CBsh compute"
    );
    let legacy = Store {
        pool: pool.clone(),
        account_v2: false,
    };
    assert!(
        !legacy.authorized(user, session, false).await,
        "legacy global approval mode rejects this external global identity"
    );
    assert!(
        !scoped
            .authorized(user, "00000000-0000-4000-8000-000000000099", false)
            .await
    );
    assert!(
        !scoped
            .authorized("00000000-0000-4000-8000-000000000098", session, false)
            .await
    );
    pool.close().await;
}
