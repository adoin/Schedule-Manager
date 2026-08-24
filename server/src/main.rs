use anyhow::{Context, Result, anyhow};
use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier, password_hash::SaltString};
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::post,
};
use chrono::{DateTime, Datelike, Duration, Utc};
use lettre::{Message, SmtpTransport, Transport, transport::smtp::authentication::Credentials};
use rand::Rng;
use rusqlite::{Connection, OptionalExtension, params};
#[allow(dead_code)]
#[path = "../../src/models.rs"]
mod shared_models;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use shared_models::{ApiEnvelope, AuthData, CalendarEvent, Holiday, SyncPullData, UserProfile};
use std::{
    env,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration as StdDuration,
};
use tracing::{error, info, warn};
use uuid::Uuid;

#[derive(Clone)]
struct AppState {
    db: Arc<Mutex<Connection>>,
    config: Arc<Config>,
    http: reqwest::Client,
}

#[derive(Clone)]
struct Config {
    bind: String,
    database_path: String,
    public_base_url: String,
    email_delivery: String,
    email_from: String,
    smtp_host: String,
    smtp_port: u16,
    smtp_username: String,
    smtp_password: String,
}

impl Config {
    fn from_env() -> Self {
        Self {
            bind: env::var("SCHEDULE_BIND").unwrap_or_else(|_| "127.0.0.1:8010".into()),
            database_path: env::var("SCHEDULE_DATABASE_PATH")
                .unwrap_or_else(|_| "./schedule.db".into()),
            public_base_url: env::var("SCHEDULE_PUBLIC_BASE_URL")
                .unwrap_or_else(|_| "https://novel.emssion.com/schedule".into()),
            email_delivery: env::var("EMAIL_DELIVERY").unwrap_or_else(|_| "console".into()),
            email_from: env::var("EMAIL_FROM")
                .unwrap_or_else(|_| "Schedule Manager <noreply@mail.emssion.com>".into()),
            smtp_host: env::var("SMTP_HOST").unwrap_or_else(|_| "smtpdm.aliyun.com".into()),
            smtp_port: env::var("SMTP_PORT")
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(465),
            smtp_username: env::var("SMTP_USERNAME").unwrap_or_default(),
            smtp_password: env::var("SMTP_PASSWORD").unwrap_or_default(),
        }
    }
}

type ApiResult<T> = Result<Json<ApiEnvelope<T>>, ApiError>;

struct ApiError(StatusCode, String);

impl ApiError {
    fn bad_request(message: impl Into<String>) -> Self {
        Self(StatusCode::BAD_REQUEST, message.into())
    }
    fn unauthorized(message: impl Into<String>) -> Self {
        Self(StatusCode::UNAUTHORIZED, message.into())
    }
    fn conflict(message: impl Into<String>) -> Self {
        Self(StatusCode::CONFLICT, message.into())
    }
    fn internal(error: impl std::fmt::Display) -> Self {
        error!(%error, "schedule api error");
        Self(StatusCode::INTERNAL_SERVER_ERROR, "服务暂时不可用".into())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        let body = ApiEnvelope {
            code: self.0.as_u16() as i32,
            message: self.1,
            data: json!({}),
        };
        (self.0, Json(body)).into_response()
    }
}

fn ok<T>(data: T) -> Json<ApiEnvelope<T>> {
    Json(ApiEnvelope {
        code: 0,
        message: "ok".into(),
        data,
    })
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "schedule_api=info".into()),
        )
        .init();
    let config = Arc::new(Config::from_env());
    if let Some(parent) = std::path::Path::new(&config.database_path).parent() {
        std::fs::create_dir_all(parent)?;
    }
    let connection = Connection::open(&config.database_path)?;
    connection.pragma_update(None, "journal_mode", "WAL")?;
    connection.pragma_update(None, "foreign_keys", "ON")?;
    migrate(&connection)?;
    let state = AppState {
        db: Arc::new(Mutex::new(connection)),
        config: config.clone(),
        http: reqwest::Client::builder()
            .timeout(StdDuration::from_secs(20))
            .build()?,
    };

    let refresh_state = state.clone();
    tokio::spawn(async move {
        loop {
            if let Err(error) = refresh_holidays(&refresh_state).await {
                warn!(%error, "holiday refresh failed; cached data remains available");
            }
            tokio::time::sleep(StdDuration::from_secs(24 * 60 * 60)).await;
        }
    });

    let app = Router::new()
        .route("/health", post(health))
        .route("/auth/request-code", post(request_code))
        .route("/auth/register", post(register))
        .route("/auth/login", post(login))
        .route("/sync/pull", post(sync_pull))
        .route("/events/upsert", post(upsert_event))
        .route("/events/delete", post(delete_event))
        .route("/holidays/list", post(list_holidays))
        .route("/holidays/refresh", post(refresh_holidays_endpoint))
        .with_state(state);
    let address: SocketAddr = config.bind.parse()?;
    let listener = tokio::net::TcpListener::bind(address).await?;
    info!(%address, database=%config.database_path, base_url=%config.public_base_url, "schedule api listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

fn migrate(db: &Connection) -> Result<()> {
    db.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS users (
            id TEXT PRIMARY KEY,
            email TEXT NOT NULL UNIQUE,
            display_name TEXT NOT NULL,
            password_hash TEXT NOT NULL,
            created_at TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS verification_codes (
            email TEXT PRIMARY KEY,
            code_hash TEXT NOT NULL,
            expires_at TEXT NOT NULL,
            attempts INTEGER NOT NULL DEFAULT 0,
            last_sent_at TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS sessions (
            token_hash TEXT PRIMARY KEY,
            user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
            expires_at TEXT NOT NULL,
            created_at TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS events (
            id TEXT PRIMARY KEY,
            user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
            payload TEXT NOT NULL,
            version INTEGER NOT NULL,
            updated_seq INTEGER NOT NULL,
            updated_at TEXT NOT NULL,
            deleted INTEGER NOT NULL DEFAULT 0
        );
        CREATE INDEX IF NOT EXISTS idx_events_user_sync ON events(user_id, updated_seq);
        CREATE TABLE IF NOT EXISTS holidays (
            date TEXT PRIMARY KEY,
            year INTEGER NOT NULL,
            payload TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );
        "#,
    )?;
    Ok(())
}

async fn health(State(state): State<AppState>) -> ApiResult<Value> {
    let db = state.db.lock().map_err(ApiError::internal)?;
    db.query_row("SELECT 1", [], |_| Ok(()))
        .map_err(ApiError::internal)?;
    Ok(ok(json!({"status":"ok","time":Utc::now()})))
}

#[derive(Deserialize)]
struct EmailRequest {
    email: String,
}

async fn request_code(
    State(state): State<AppState>,
    Json(payload): Json<EmailRequest>,
) -> ApiResult<Value> {
    let email = normalize_email(&payload.email)?;
    let code = format!("{:06}", rand::rng().random_range(0..1_000_000));
    {
        let db = state.db.lock().map_err(ApiError::internal)?;
        let last_sent: Option<String> = db
            .query_row(
                "SELECT last_sent_at FROM verification_codes WHERE email=?1",
                [&email],
                |row| row.get(0),
            )
            .optional()
            .map_err(ApiError::internal)?;
        if let Some(value) = last_sent {
            if parse_utc(&value).map_err(ApiError::internal)? > Utc::now() - Duration::seconds(60) {
                return Err(ApiError(
                    StatusCode::TOO_MANY_REQUESTS,
                    "请 60 秒后再获取验证码".into(),
                ));
            }
        }
        let now = Utc::now();
        db.execute(
            "INSERT INTO verification_codes(email,code_hash,expires_at,attempts,last_sent_at) VALUES(?1,?2,?3,0,?4) ON CONFLICT(email) DO UPDATE SET code_hash=excluded.code_hash,expires_at=excluded.expires_at,attempts=0,last_sent_at=excluded.last_sent_at",
            params![email, token_hash(&code), (now + Duration::minutes(10)).to_rfc3339(), now.to_rfc3339()],
        ).map_err(ApiError::internal)?;
    }
    send_verification_email(&state.config, &email, &code).map_err(ApiError::internal)?;
    Ok(ok(json!({"expiresInSeconds":600})))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RegisterRequest {
    email: String,
    password: String,
    display_name: String,
    code: String,
}

async fn register(
    State(state): State<AppState>,
    Json(payload): Json<RegisterRequest>,
) -> ApiResult<AuthData> {
    let email = normalize_email(&payload.email)?;
    validate_password(&payload.password)?;
    let display_name = payload.display_name.trim();
    if display_name.is_empty() || display_name.chars().count() > 80 {
        return Err(ApiError::bad_request("昵称长度应为 1 到 80 个字符"));
    }
    let (user, token) = {
        let db = state.db.lock().map_err(ApiError::internal)?;
        let code_row: Option<(String, String, i64)> = db
            .query_row(
                "SELECT code_hash,expires_at,attempts FROM verification_codes WHERE email=?1",
                [&email],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .map_err(ApiError::internal)?;
        let Some((expected, expires_at, attempts)) = code_row else {
            return Err(ApiError::bad_request("请先获取邮箱验证码"));
        };
        if attempts >= 6 || parse_utc(&expires_at).map_err(ApiError::internal)? < Utc::now() {
            return Err(ApiError::bad_request("验证码已失效，请重新获取"));
        }
        if expected != token_hash(payload.code.trim()) {
            db.execute(
                "UPDATE verification_codes SET attempts=attempts+1 WHERE email=?1",
                [&email],
            )
            .map_err(ApiError::internal)?;
            return Err(ApiError::bad_request("邮箱验证码不正确"));
        }
        let exists: bool = db
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM users WHERE email=?1)",
                [&email],
                |row| row.get(0),
            )
            .map_err(ApiError::internal)?;
        if exists {
            return Err(ApiError::conflict("该邮箱已注册，请直接登录"));
        }
        let id = Uuid::new_v4().to_string();
        let password_hash = hash_password(&payload.password).map_err(ApiError::internal)?;
        db.execute(
            "INSERT INTO users(id,email,display_name,password_hash,created_at) VALUES(?1,?2,?3,?4,?5)",
            params![id, email, display_name, password_hash, Utc::now().to_rfc3339()],
        ).map_err(ApiError::internal)?;
        db.execute("DELETE FROM verification_codes WHERE email=?1", [&email])
            .map_err(ApiError::internal)?;
        let user = UserProfile {
            id,
            email: email.clone(),
            display_name: display_name.into(),
        };
        let token = create_session(&db, &user.id).map_err(ApiError::internal)?;
        (user, token)
    };
    Ok(ok(AuthData { token, user }))
}

#[derive(Deserialize)]
struct LoginRequest {
    email: String,
    password: String,
}

async fn login(
    State(state): State<AppState>,
    Json(payload): Json<LoginRequest>,
) -> ApiResult<AuthData> {
    let email = normalize_email(&payload.email)?;
    let db = state.db.lock().map_err(ApiError::internal)?;
    let row: Option<(String, String, String, String)> = db
        .query_row(
            "SELECT id,email,display_name,password_hash FROM users WHERE email=?1",
            [&email],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()
        .map_err(ApiError::internal)?;
    let Some((id, email, display_name, password_hash)) = row else {
        return Err(ApiError::unauthorized("邮箱或密码不正确"));
    };
    let parsed = PasswordHash::new(&password_hash).map_err(ApiError::internal)?;
    if Argon2::default()
        .verify_password(payload.password.as_bytes(), &parsed)
        .is_err()
    {
        return Err(ApiError::unauthorized("邮箱或密码不正确"));
    }
    let token = create_session(&db, &id).map_err(ApiError::internal)?;
    Ok(ok(AuthData {
        token,
        user: UserProfile {
            id,
            email,
            display_name,
        },
    }))
}

#[derive(Deserialize)]
struct PullRequest {
    #[serde(default)]
    cursor: i64,
}

async fn sync_pull(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<PullRequest>,
) -> ApiResult<SyncPullData> {
    let user = authenticated_user(&state, &headers)?;
    let db = state.db.lock().map_err(ApiError::internal)?;
    let mut statement = db
        .prepare(
            "SELECT payload FROM events WHERE user_id=?1 AND updated_seq>?2 ORDER BY updated_seq",
        )
        .map_err(ApiError::internal)?;
    let rows = statement
        .query_map(params![user.id, payload.cursor], |row| {
            row.get::<_, String>(0)
        })
        .map_err(ApiError::internal)?;
    let events: Vec<CalendarEvent> = rows
        .map(|row| {
            serde_json::from_str(&row.map_err(anyhow::Error::from)?).map_err(anyhow::Error::from)
        })
        .collect::<Result<_>>()
        .map_err(ApiError::internal)?;
    let cursor = events
        .iter()
        .map(|event| event.updated_seq)
        .max()
        .unwrap_or(payload.cursor);
    let year = Utc::now().year();
    let mut holidays_statement = db
        .prepare("SELECT payload FROM holidays WHERE year BETWEEN ?1 AND ?2 ORDER BY date")
        .map_err(ApiError::internal)?;
    let holiday_rows = holidays_statement
        .query_map(params![year - 1, year + 1], |row| row.get::<_, String>(0))
        .map_err(ApiError::internal)?;
    let holidays = holiday_rows
        .map(|row| {
            serde_json::from_str(&row.map_err(anyhow::Error::from)?).map_err(anyhow::Error::from)
        })
        .collect::<Result<_>>()
        .map_err(ApiError::internal)?;
    Ok(ok(SyncPullData {
        events,
        holidays,
        cursor,
    }))
}

async fn upsert_event(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(mut event): Json<CalendarEvent>,
) -> ApiResult<CalendarEvent> {
    let user = authenticated_user(&state, &headers)?;
    if event.title.trim().is_empty() {
        return Err(ApiError::bad_request("日程标题不能为空"));
    }
    if event.end_at <= event.start_at {
        return Err(ApiError::bad_request("结束时间必须晚于开始时间"));
    }
    if event
        .reminder_minutes
        .iter()
        .any(|value| *value < 0 || *value > 525_600)
    {
        return Err(ApiError::bad_request("提醒时间必须在事件前一年以内"));
    }
    let db = state.db.lock().map_err(ApiError::internal)?;
    let existing: Option<(String, i64)> = db
        .query_row(
            "SELECT user_id,version FROM events WHERE id=?1",
            [&event.id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(ApiError::internal)?;
    if let Some((owner, version)) = existing {
        if owner != user.id {
            return Err(ApiError(
                StatusCode::FORBIDDEN,
                "不能修改其他用户的日程".into(),
            ));
        }
        if event.version != version {
            return Err(ApiError::conflict("此日程已在其他设备更新，请先同步"));
        }
        event.version = version + 1;
    } else {
        event.version = 1;
    }
    event.updated_seq = next_sequence(&db, &user.id).map_err(ApiError::internal)?;
    event.updated_at = Utc::now();
    event.deleted = false;
    let payload = serde_json::to_string(&event).map_err(ApiError::internal)?;
    db.execute(
        "INSERT INTO events(id,user_id,payload,version,updated_seq,updated_at,deleted) VALUES(?1,?2,?3,?4,?5,?6,0) ON CONFLICT(id) DO UPDATE SET payload=excluded.payload,version=excluded.version,updated_seq=excluded.updated_seq,updated_at=excluded.updated_at,deleted=0",
        params![event.id, user.id, payload, event.version, event.updated_seq, event.updated_at.to_rfc3339()],
    ).map_err(ApiError::internal)?;
    Ok(ok(event))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeleteRequest {
    id: String,
    base_version: i64,
}

async fn delete_event(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<DeleteRequest>,
) -> ApiResult<CalendarEvent> {
    let user = authenticated_user(&state, &headers)?;
    let db = state.db.lock().map_err(ApiError::internal)?;
    let stored: Option<(String, String, i64)> = db
        .query_row(
            "SELECT user_id,payload,version FROM events WHERE id=?1",
            [&payload.id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .map_err(ApiError::internal)?;
    let Some((owner, json, version)) = stored else {
        return Err(ApiError(StatusCode::NOT_FOUND, "日程不存在".into()));
    };
    if owner != user.id {
        return Err(ApiError(
            StatusCode::FORBIDDEN,
            "不能删除其他用户的日程".into(),
        ));
    }
    if version != payload.base_version {
        return Err(ApiError::conflict("此日程已在其他设备更新，请先同步"));
    }
    let mut event: CalendarEvent = serde_json::from_str(&json).map_err(ApiError::internal)?;
    event.deleted = true;
    event.version += 1;
    event.updated_seq = next_sequence(&db, &user.id).map_err(ApiError::internal)?;
    event.updated_at = Utc::now();
    let json = serde_json::to_string(&event).map_err(ApiError::internal)?;
    db.execute(
        "UPDATE events SET payload=?2,version=?3,updated_seq=?4,updated_at=?5,deleted=1 WHERE id=?1",
        params![event.id, json, event.version, event.updated_seq, event.updated_at.to_rfc3339()],
    ).map_err(ApiError::internal)?;
    Ok(ok(event))
}

async fn refresh_holidays_endpoint(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(_): Json<Value>,
) -> ApiResult<Value> {
    let _ = authenticated_user(&state, &headers)?;
    let count = refresh_holidays(&state).await.map_err(ApiError::internal)?;
    Ok(ok(json!({"updated":count})))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct HolidayListRequest {
    start_year: i32,
    end_year: i32,
}

async fn list_holidays(
    State(state): State<AppState>,
    Json(payload): Json<HolidayListRequest>,
) -> ApiResult<Vec<Holiday>> {
    let current_year = Utc::now().year();
    if payload.start_year > payload.end_year
        || payload.start_year < current_year - 2
        || payload.end_year > current_year + 2
    {
        return Err(ApiError::bad_request("节假日年份范围无效"));
    }
    let db = state.db.lock().map_err(ApiError::internal)?;
    let mut statement = db
        .prepare("SELECT payload FROM holidays WHERE year BETWEEN ?1 AND ?2 ORDER BY date")
        .map_err(ApiError::internal)?;
    let rows = statement
        .query_map(params![payload.start_year, payload.end_year], |row| {
            row.get::<_, String>(0)
        })
        .map_err(ApiError::internal)?;
    let holidays = rows
        .map(|row| {
            serde_json::from_str(&row.map_err(anyhow::Error::from)?).map_err(anyhow::Error::from)
        })
        .collect::<Result<_>>()
        .map_err(ApiError::internal)?;
    Ok(ok(holidays))
}

#[derive(Deserialize)]
struct HolidaySource {
    year: i32,
    #[serde(default)]
    papers: Vec<String>,
    days: Vec<HolidayDay>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct HolidayDay {
    name: String,
    date: String,
    is_off_day: bool,
}

async fn refresh_holidays(state: &AppState) -> Result<usize> {
    let current_year = Utc::now().year();
    let mut all = Vec::new();
    for year in (current_year - 1)..=(current_year + 1) {
        let url =
            format!("https://raw.githubusercontent.com/NateScarlet/holiday-cn/master/{year}.json");
        let response = match state.http.get(&url).send().await {
            Ok(response) if response.status().is_success() => response,
            Ok(response) => {
                warn!(year, status=%response.status(), "holiday source unavailable");
                continue;
            }
            Err(error) => {
                warn!(year, %error, "holiday source request failed");
                continue;
            }
        };
        let source: HolidaySource = response.json().await.context("decode holiday source")?;
        if source.year != year {
            return Err(anyhow!(
                "holiday source year mismatch: expected {year}, got {}",
                source.year
            ));
        }
        let paper = source.papers.first().cloned().unwrap_or(url);
        for day in source.days {
            let parsed = chrono::NaiveDate::parse_from_str(&day.date, "%Y-%m-%d")?;
            if parsed.year() != year {
                return Err(anyhow!("holiday date outside source year: {}", day.date));
            }
            all.push((
                year,
                Holiday {
                    date: day.date,
                    name: day.name,
                    is_off_day: day.is_off_day,
                    source_url: paper.clone(),
                },
            ));
        }
    }
    let mut db = state
        .db
        .lock()
        .map_err(|_| anyhow!("database lock poisoned"))?;
    let transaction = db.transaction()?;
    let now = Utc::now().to_rfc3339();
    for (year, holiday) in &all {
        transaction.execute(
            "INSERT INTO holidays(date,year,payload,updated_at) VALUES(?1,?2,?3,?4) ON CONFLICT(date) DO UPDATE SET year=excluded.year,payload=excluded.payload,updated_at=excluded.updated_at",
            params![holiday.date, year, serde_json::to_string(holiday)?, now],
        )?;
    }
    transaction.commit()?;
    info!(count = all.len(), "holiday cache refreshed");
    Ok(all.len())
}

fn authenticated_user(state: &AppState, headers: &HeaderMap) -> Result<UserProfile, ApiError> {
    let header = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    let token = header
        .strip_prefix("Bearer ")
        .ok_or_else(|| ApiError::unauthorized("请先登录"))?;
    let db = state.db.lock().map_err(ApiError::internal)?;
    let row: Option<(String, String, String, String)> = db.query_row(
        "SELECT u.id,u.email,u.display_name,s.expires_at FROM sessions s JOIN users u ON u.id=s.user_id WHERE s.token_hash=?1",
        [token_hash(token)], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    ).optional().map_err(ApiError::internal)?;
    let Some((id, email, display_name, expires_at)) = row else {
        return Err(ApiError::unauthorized("登录已失效，请重新登录"));
    };
    if parse_utc(&expires_at).map_err(ApiError::internal)? < Utc::now() {
        return Err(ApiError::unauthorized("登录已过期，请重新登录"));
    }
    Ok(UserProfile {
        id,
        email,
        display_name,
    })
}

fn next_sequence(db: &Connection, user_id: &str) -> Result<i64> {
    Ok(db.query_row(
        "SELECT COALESCE(MAX(updated_seq),0)+1 FROM events WHERE user_id=?1",
        [user_id],
        |row| row.get(0),
    )?)
}

fn create_session(db: &Connection, user_id: &str) -> Result<String> {
    let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    let now = Utc::now();
    db.execute(
        "INSERT INTO sessions(token_hash,user_id,expires_at,created_at) VALUES(?1,?2,?3,?4)",
        params![
            token_hash(&token),
            user_id,
            (now + Duration::days(30)).to_rfc3339(),
            now.to_rfc3339()
        ],
    )?;
    Ok(token)
}

fn normalize_email(value: &str) -> Result<String, ApiError> {
    let email = value.trim().to_lowercase();
    let valid = email.len() <= 254
        && email.split_once('@').is_some_and(|(local, domain)| {
            !local.is_empty() && domain.contains('.') && !domain.ends_with('.')
        });
    if !valid {
        return Err(ApiError::bad_request("邮箱格式不正确"));
    }
    Ok(email)
}

fn validate_password(value: &str) -> Result<(), ApiError> {
    if value.chars().count() < 8 || value.chars().count() > 128 {
        return Err(ApiError::bad_request("密码长度应为 8 到 128 个字符"));
    }
    Ok(())
}

fn hash_password(value: &str) -> Result<String> {
    let mut salt_bytes = [0u8; 16];
    rand::rng().fill(&mut salt_bytes);
    let salt = SaltString::encode_b64(&salt_bytes).map_err(|error| anyhow!(error.to_string()))?;
    Ok(Argon2::default()
        .hash_password(value.as_bytes(), &salt)
        .map_err(|error| anyhow!(error.to_string()))?
        .to_string())
}

fn token_hash(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}

fn parse_utc(value: &str) -> Result<DateTime<Utc>> {
    Ok(DateTime::parse_from_rfc3339(value)?.with_timezone(&Utc))
}

fn send_verification_email(config: &Config, recipient: &str, code: &str) -> Result<()> {
    if config.email_delivery.eq_ignore_ascii_case("console") {
        info!(recipient, code, "verification email (console mode)");
        return Ok(());
    }
    let email = Message::builder()
        .from(config.email_from.parse()?)
        .to(recipient.parse()?)
        .subject("Schedule Manager 邮箱验证码")
        .body(format!(
            "你的验证码是：{code}\n\n验证码 10 分钟内有效。若非本人操作，请忽略此邮件。"
        ))?;
    let credentials = Credentials::new(config.smtp_username.clone(), config.smtp_password.clone());
    let mailer = SmtpTransport::relay(&config.smtp_host)?
        .port(config.smtp_port)
        .credentials(credentials)
        .timeout(Some(StdDuration::from_secs(20)))
        .build();
    mailer.send(&email)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_hash_is_stable_and_password_roundtrips() {
        assert_eq!(token_hash("abc"), token_hash("abc"));
        let hash = hash_password("correct horse").unwrap();
        let parsed = PasswordHash::new(&hash).unwrap();
        assert!(
            Argon2::default()
                .verify_password(b"correct horse", &parsed)
                .is_ok()
        );
    }
}
