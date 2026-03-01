use anyhow::Result;
use async_imap::extensions::idle::IdleResponse;
use async_imap::{Client, Session};
use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Json, Response},
    routing::{delete, get, post},
    Router,
};
use lettre::message::header::ContentType;
use lettre::transport::smtp::authentication::Credentials;
use lettre::{Message, SmtpTransport, Transport};
use native_tls::TlsConnector;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::net::TcpStream;
use tokio_native_tls::TlsStream;
use tracing::{error, info, warn};

// ─── Config ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct Config {
    api: ApiConfig,
    accounts: HashMap<String, AccountConfig>,
    webhook_url: Option<String>,
    webhook_token: Option<String>,
}

#[derive(Debug, Clone)]
struct ApiConfig {
    port: u16,
    token: String,
}

#[derive(Debug, Clone)]
struct AccountConfig {
    imap_host: String,
    imap_port: u16,
    smtp_host: String,
    smtp_port: u16,
    login: String,
    password: String,
}

/// Charge la config depuis les variables d'environnement.
///
/// Variables globales :
///   API_PORT        (défaut: 8090)
///   API_TOKEN       (obligatoire)
///   WEBHOOK_URL     (optionnel)
///
/// Par compte (préfixe ACCOUNT_<NOM>_) :
///   ACCOUNT_PERSO_IMAP_HOST, ACCOUNT_PERSO_IMAP_PORT
///   ACCOUNT_PERSO_SMTP_HOST, ACCOUNT_PERSO_SMTP_PORT
///   ACCOUNT_PERSO_LOGIN, ACCOUNT_PERSO_PASSWORD
///
/// Le nom du compte est déduit automatiquement en scannant les variables ACCOUNT_*_LOGIN.
fn load_config() -> Result<Config> {
    let api_port: u16 = std::env::var("API_PORT")
        .unwrap_or_else(|_| "8090".to_string())
        .parse()?;

    let api_token = std::env::var("API_TOKEN")
        .map_err(|_| anyhow::anyhow!("API_TOKEN environment variable is required"))?;

    let webhook_url = std::env::var("WEBHOOK_URL").ok();
    let webhook_token = std::env::var("WEBHOOK_TOKEN").ok();

    // Scan toutes les variables ACCOUNT_*_LOGIN pour découvrir les comptes
    let mut accounts: HashMap<String, AccountConfig> = HashMap::new();

    for (key, value) in std::env::vars() {
        if let Some(rest) = key.strip_prefix("ACCOUNT_") {
            if let Some(name) = rest.strip_suffix("_LOGIN") {
                let name = name.to_lowercase();
                let prefix = format!("ACCOUNT_{}", name.to_uppercase());

                let imap_host = std::env::var(format!("{}_IMAP_HOST", prefix))
                    .map_err(|_| anyhow::anyhow!("Missing {}_IMAP_HOST", prefix))?;
                let imap_port: u16 = std::env::var(format!("{}_IMAP_PORT", prefix))
                    .unwrap_or_else(|_| "993".to_string())
                    .parse()?;
                let smtp_host = std::env::var(format!("{}_SMTP_HOST", prefix))
                    .map_err(|_| anyhow::anyhow!("Missing {}_SMTP_HOST", prefix))?;
                let smtp_port: u16 = std::env::var(format!("{}_SMTP_PORT", prefix))
                    .unwrap_or_else(|_| "587".to_string())
                    .parse()?;
                let password = std::env::var(format!("{}_PASSWORD", prefix))
                    .map_err(|_| anyhow::anyhow!("Missing {}_PASSWORD", prefix))?;

                info!("Loaded account '{}'", name);
                accounts.insert(name, AccountConfig {
                    imap_host,
                    imap_port,
                    smtp_host,
                    smtp_port,
                    login: value,
                    password,
                });
            }
        }
    }

    if accounts.is_empty() {
        warn!("No accounts configured. Add ACCOUNT_<NAME>_LOGIN etc. to your environment.");
    }

    Ok(Config {
        api: ApiConfig { port: api_port, token: api_token },
        accounts,
        webhook_url,
        webhook_token,
    })
}

// ─── IMAP helpers ──────────────────────────────────────────────────────────

async fn imap_session(acc: &AccountConfig) -> Result<Session<TlsStream<TcpStream>>> {
    let tcp = TcpStream::connect(format!("{}:{}", acc.imap_host, acc.imap_port)).await?;
    let tls_connector = TlsConnector::builder().build()?;
    let tls_connector = tokio_native_tls::TlsConnector::from(tls_connector);
    let tls = tls_connector.connect(&acc.imap_host, tcp).await?;
    let client = Client::new(tls);
    let session = client
        .login(&acc.login, &acc.password)
        .await
        .map_err(|(e, _)| anyhow::anyhow!("IMAP login failed: {}", e))?;
    Ok(session)
}

fn get_account<'a>(cfg: &'a Config, name: &str) -> Result<&'a AccountConfig, (StatusCode, Json<Value>)> {
    cfg.accounts.get(name).ok_or_else(|| {
        (StatusCode::NOT_FOUND, Json(json!({ "error": format!("Account '{}' not found", name) })))
    })
}

// ─── Request / Response types ──────────────────────────────────────────────

#[derive(Serialize)]
struct EmailSummary {
    uid: u32,
    from: String,
    subject: String,
    date: String,
    seen: bool,
}

#[derive(Deserialize)]
struct SendRequest {
    to: String,
    subject: String,
    body: String,
    #[serde(default)]
    html: bool,
}

#[derive(Deserialize)]
struct MoveRequest { folder: String }

#[derive(Deserialize)]
struct LabelRequest { labels: Vec<String> }

// ─── Auth middleware ────────────────────────────────────────────────────────

async fn auth_middleware(
    State(cfg): State<Arc<Config>>,
    headers: HeaderMap,
    request: axum::extract::Request,
    next: Next,
) -> Response {
    let token = headers
        .get("Authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));

    match token {
        Some(t) if t == cfg.api.token => next.run(request).await,
        _ => (StatusCode::UNAUTHORIZED, Json(json!({"error": "Unauthorized"}))).into_response(),
    }
}

// ─── Handlers ──────────────────────────────────────────────────────────────

async fn list_accounts(State(cfg): State<Arc<Config>>) -> Json<Value> {
    let names: Vec<&String> = cfg.accounts.keys().collect();
    Json(json!({ "accounts": names }))
}

async fn list_folders(
    State(cfg): State<Arc<Config>>,
    Path(account): Path<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let acc = get_account(&cfg, &account)?;
    let mut session = imap_session(acc).await.map_err(imap_err)?;
    let folders = session.list(Some(""), Some("*")).await.map_err(imap_err)?;
    let names: Vec<String> = folders.iter().map(|f| f.name().to_string()).collect();
    session.logout().await.ok();
    Ok(Json(json!({ "account": account, "folders": names })))
}

async fn fetch_emails(
    State(cfg): State<Arc<Config>>,
    Path(account): Path<String>,
    axum::extract::Query(params): axum::extract::Query<HashMap<String, String>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let acc = get_account(&cfg, &account)?;
    let folder = params.get("folder").cloned().unwrap_or_else(|| "INBOX".to_string());
    let limit: u32 = params.get("limit").and_then(|v| v.parse().ok()).unwrap_or(20);

    let mut session = imap_session(acc).await.map_err(imap_err)?;
    session.select(&folder).await.map_err(imap_err)?;

    let messages = session
        .fetch(format!("1:{}", limit), "(UID FLAGS ENVELOPE)")
        .await
        .map_err(imap_err)?;

    let emails: Vec<EmailSummary> = messages.iter().filter_map(|m| {
        let uid = m.uid?;
        let envelope = m.envelope()?;
        let seen = m.flags().iter().any(|f| matches!(f, async_imap::types::Flag::Seen));

        let from = envelope.from.as_ref()?.first().map(|a| {
            let name = a.name.as_ref().and_then(|n| std::str::from_utf8(n).ok()).unwrap_or("");
            let mailbox = a.mailbox.as_ref().and_then(|m| std::str::from_utf8(m).ok()).unwrap_or("");
            let host = a.host.as_ref().and_then(|h| std::str::from_utf8(h).ok()).unwrap_or("");
            if name.is_empty() { format!("{}@{}", mailbox, host) }
            else { format!("{} <{}@{}>", name, mailbox, host) }
        }).unwrap_or_default();

        let subject = envelope.subject.as_ref()
            .and_then(|s| std::str::from_utf8(s).ok())
            .unwrap_or("(no subject)").to_string();

        let date = envelope.date.as_ref()
            .and_then(|d| std::str::from_utf8(d).ok())
            .unwrap_or("").to_string();

        Some(EmailSummary { uid, from, subject, date, seen })
    }).collect();

    session.logout().await.ok();
    Ok(Json(json!({ "account": account, "folder": folder, "emails": emails })))
}

async fn send_email(
    State(cfg): State<Arc<Config>>,
    Path(account): Path<String>,
    Json(req): Json<SendRequest>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let acc = get_account(&cfg, &account)?;
    let content_type = if req.html { ContentType::TEXT_HTML } else { ContentType::TEXT_PLAIN };

    let email = Message::builder()
        .from(acc.login.parse().map_err(|_| bad_request("Invalid from address"))?)
        .to(req.to.parse().map_err(|_| bad_request("Invalid to address"))?)
        .subject(&req.subject)
        .header(content_type)
        .body(req.body)
        .map_err(|e| bad_request(&e.to_string()))?;

    let creds = Credentials::new(acc.login.clone(), acc.password.clone());
    let mailer = SmtpTransport::relay(&acc.smtp_host)
        .map_err(|e| bad_request(&e.to_string()))?
        .port(acc.smtp_port)
        .credentials(creds)
        .build();

    mailer.send(&email).map_err(|e| {
        error!("SMTP error: {}", e);
        (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()})))
    })?;

    Ok(Json(json!({ "status": "sent", "account": account })))
}

async fn move_email(
    State(cfg): State<Arc<Config>>,
    Path((account, uid)): Path<(String, u32)>,
    axum::extract::Query(params): axum::extract::Query<HashMap<String, String>>,
    Json(req): Json<MoveRequest>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let acc = get_account(&cfg, &account)?;
    let from_folder = params.get("folder").cloned().unwrap_or_else(|| "INBOX".to_string());

    let mut session = imap_session(acc).await.map_err(imap_err)?;
    session.select(&from_folder).await.map_err(imap_err)?;
    session.uid_mv(uid.to_string(), &req.folder).await.map_err(imap_err)?;

    session.logout().await.ok();
    Ok(Json(json!({ "status": "moved", "to": req.folder })))
}

async fn add_labels(
    State(cfg): State<Arc<Config>>,
    Path((account, uid)): Path<(String, u32)>,
    axum::extract::Query(params): axum::extract::Query<HashMap<String, String>>,
    Json(req): Json<LabelRequest>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let acc = get_account(&cfg, &account)?;
    let folder = params.get("folder").cloned().unwrap_or_else(|| "INBOX".to_string());
    let flags_str = req.labels.iter().map(|l| format!("\\{}", l)).collect::<Vec<_>>().join(" ");

    let mut session = imap_session(acc).await.map_err(imap_err)?;
    session.select(&folder).await.map_err(imap_err)?;
    session.uid_store(uid.to_string(), format!("+FLAGS ({})", flags_str)).await.map_err(imap_err)?;

    session.logout().await.ok();
    Ok(Json(json!({ "status": "labels_added", "labels": req.labels })))
}

async fn delete_email(
    State(cfg): State<Arc<Config>>,
    Path((account, uid)): Path<(String, u32)>,
    axum::extract::Query(params): axum::extract::Query<HashMap<String, String>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let acc = get_account(&cfg, &account)?;
    let folder = params.get("folder").cloned().unwrap_or_else(|| "INBOX".to_string());

    let mut session = imap_session(acc).await.map_err(imap_err)?;
    session.select(&folder).await.map_err(imap_err)?;
    session.uid_store(uid.to_string(), "+FLAGS (\\Deleted)").await.map_err(imap_err)?;
    session.expunge().await.map_err(imap_err)?;

    session.logout().await.ok();
    Ok(Json(json!({ "status": "deleted" })))
}

// ─── IMAP IDLE ─────────────────────────────────────────────────────────────

async fn start_idle_watchers(cfg: Arc<Config>) {
    let Some(ref webhook_url) = cfg.webhook_url else {
        info!("No WEBHOOK_URL configured, IDLE watchers disabled");
        return;
    };

    for (name, acc) in &cfg.accounts {
        let acc = acc.clone();
        let name = name.clone();
        let url = webhook_url.clone();
        let token = cfg.webhook_token.clone();
        tokio::spawn(async move {
            info!("IDLE watcher started for account '{}'", name);
            loop {
                match run_idle(&acc, &name, &url, token.as_deref()).await {
                    Ok(_) => info!("IDLE session ended for '{}', reconnecting...", name),
                    Err(e) => {
                        error!("IDLE error for '{}': {}, reconnecting in 5s...", name, e);
                        tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                    }
                }
            }
        });
    }
}

async fn run_idle(acc: &AccountConfig, account_name: &str, webhook_url: &str, webhook_token: Option<&str>) -> Result<()> {
    let mut session = imap_session(acc).await?;
    session.select("INBOX").await?;

    let idle = session.idle();
    let (idle_response, mut session) = idle
        .wait_with_timeout(std::time::Duration::from_secs(480))
        .await?;

    if matches!(idle_response, IdleResponse::NewData(_)) {
        info!("New email on '{}', triggering webhook", account_name);
        let client = reqwest::Client::new();
        let mut req = client
            .post(webhook_url)
            .json(&json!({ "event": "new_email", "account": account_name, "folder": "INBOX" }));

        if let Some(token) = webhook_token {
            req = req.header("Authorization", format!("Bearer {}", token));
        }

        let _ = req.send().await;
    }

    session.logout().await.ok();
    Ok(())
}

// ─── Search handler ────────────────────────────────────────────────────────
//
// GET /accounts/:account/emails/search?from=addr@example.com&since=2026-01-15&folder=INBOX
//
// `from` et `since` sont optionnels mais au moins un doit être fourni.
// `since` attend le format YYYY-MM-DD.

async fn search_emails(
    State(cfg): State<Arc<Config>>,
    Path(account): Path<String>,
    axum::extract::Query(params): axum::extract::Query<HashMap<String, String>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let acc = get_account(&cfg, &account)?;
    let folder = params.get("folder").cloned().unwrap_or_else(|| "INBOX".to_string());
    let from_addr = params.get("from").cloned();
    let since_date = params.get("since").cloned();

    if from_addr.is_none() && since_date.is_none() {
        return Err(bad_request("At least one of 'from' or 'since' query params is required"));
    }

    // Construit la requête IMAP SEARCH
    // ex: SINCE 15-Jan-2026 FROM addr@example.com
    let mut criteria = String::new();

    if let Some(ref since) = since_date {
        // Convertit YYYY-MM-DD → DD-Mon-YYYY (format IMAP)
        let imap_date = parse_imap_date(since)
            .map_err(|_| bad_request("Invalid 'since' format, expected YYYY-MM-DD"))?;
        criteria.push_str(&format!("SINCE {}", imap_date));
    }

    if let Some(ref from) = from_addr {
        if !criteria.is_empty() { criteria.push(' '); }
        criteria.push_str(&format!("FROM \"{}\"", from));
    }

    let mut session = imap_session(acc).await.map_err(imap_err)?;
    session.select(&folder).await.map_err(imap_err)?;

    let uids = session.uid_search(&criteria).await.map_err(imap_err)?;

    if uids.is_empty() {
        session.logout().await.ok();
        return Ok(Json(json!({
            "account": account,
            "folder": folder,
            "criteria": criteria,
            "emails": []
        })));
    }

    // Fetch les enveloppes des UIDs trouvés
    let uid_list = uids.iter().map(|u| u.to_string()).collect::<Vec<_>>().join(",");
    let messages = session
        .uid_fetch(&uid_list, "(UID FLAGS ENVELOPE)")
        .await
        .map_err(imap_err)?;

    let emails: Vec<EmailSummary> = messages.iter().filter_map(|m| {
        let uid = m.uid?;
        let envelope = m.envelope()?;
        let seen = m.flags().iter().any(|f| matches!(f, async_imap::types::Flag::Seen));

        let from = envelope.from.as_ref()?.first().map(|a| {
            let name = a.name.as_ref().and_then(|n| std::str::from_utf8(n).ok()).unwrap_or("");
            let mailbox = a.mailbox.as_ref().and_then(|m| std::str::from_utf8(m).ok()).unwrap_or("");
            let host = a.host.as_ref().and_then(|h| std::str::from_utf8(h).ok()).unwrap_or("");
            if name.is_empty() { format!("{}@{}", mailbox, host) }
            else { format!("{} <{}@{}>", name, mailbox, host) }
        }).unwrap_or_default();

        let subject = envelope.subject.as_ref()
            .and_then(|s| std::str::from_utf8(s).ok())
            .unwrap_or("(no subject)").to_string();

        let date = envelope.date.as_ref()
            .and_then(|d| std::str::from_utf8(d).ok())
            .unwrap_or("").to_string();

        Some(EmailSummary { uid, from, subject, date, seen })
    }).collect();

    session.logout().await.ok();
    Ok(Json(json!({
        "account": account,
        "folder": folder,
        "criteria": criteria,
        "count": emails.len(),
        "emails": emails
    })))
}

/// Convertit "2026-01-15" → "15-Jan-2026" (format attendu par IMAP SEARCH SINCE)
fn parse_imap_date(date: &str) -> Result<String> {
    let parts: Vec<&str> = date.split('-').collect();
    if parts.len() != 3 { anyhow::bail!("invalid date"); }

    let months = ["Jan","Feb","Mar","Apr","May","Jun","Jul","Aug","Sep","Oct","Nov","Dec"];
    let month_idx: usize = parts[1].parse::<usize>()
        .map_err(|_| anyhow::anyhow!("invalid month"))?
        .checked_sub(1)
        .ok_or_else(|| anyhow::anyhow!("month out of range"))?;
    let month = months.get(month_idx).ok_or_else(|| anyhow::anyhow!("month out of range"))?;

    Ok(format!("{}-{}-{}", parts[2].trim_start_matches('0').replace("", "").trim(), month, parts[0]))
}

// ─── Error helpers ─────────────────────────────────────────────────────────

fn imap_err<E: std::fmt::Display>(e: E) -> (StatusCode, Json<Value>) {
    error!("IMAP error: {}", e);
    (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() })))
}

fn bad_request(msg: &str) -> (StatusCode, Json<Value>) {
    (StatusCode::BAD_REQUEST, Json(json!({ "error": msg })))
}

// ─── Main ──────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let config = load_config()?;
    let port = config.api.port;
    let cfg = Arc::new(config);

    {
        let cfg_idle = Arc::clone(&cfg);
        tokio::spawn(async move { start_idle_watchers(cfg_idle).await; });
    }

    let protected = Router::new()
        .route("/accounts", get(list_accounts))
        .route("/accounts/:account/folders", get(list_folders))
        .route("/accounts/:account/emails", get(fetch_emails))
        .route("/accounts/:account/emails/search", get(search_emails))
        .route("/accounts/:account/emails/send", post(send_email))
        .route("/accounts/:account/emails/:uid/move", post(move_email))
        .route("/accounts/:account/emails/:uid/labels", post(add_labels))
        .route("/accounts/:account/emails/:uid", delete(delete_email))
        .layer(middleware::from_fn_with_state(Arc::clone(&cfg), auth_middleware))
        .with_state(Arc::clone(&cfg));

    let app = Router::new()
        .route("/health", get(|| async { Json(json!({ "status": "ok" })) }))
        .merge(protected);

    let addr = format!("0.0.0.0:{}", port);
    info!("mailbridge listening on {} ({} accounts)", addr, cfg.accounts.len());

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}