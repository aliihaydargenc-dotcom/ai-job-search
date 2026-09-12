use anyhow::{Context, Result};
use chrono::{Duration as ChronoDuration, Utc};
use lettre::{
    message::header::ContentType,
    transport::smtp::authentication::Credentials,
    Message, SmtpTransport, Transport,
};
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{env, fs, path::Path, time::Duration};

const SETTINGS_PATH: &str = "settings.json";
const REPORT_PATH: &str = "data/latest_report.html";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Job {
    source: String,
    source_id: Option<String>,
    title: String,
    company: String,
    location: String,
    description: String,
    url: String,
    updated: String,
    score: i32,
    reasons: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct Settings {
    #[serde(default)]
    roles: Vec<String>,
    #[serde(default)]
    skills: Vec<String>,
    #[serde(default)]
    penalty_terms: Vec<String>,
    #[serde(default)]
    jooble_queries: Vec<String>,
    #[serde(default = "default_location")]
    location: String,
    #[serde(default = "default_radius")]
    radius: String,
    #[serde(default = "default_result_on_page")]
    result_on_page: usize,
}

fn default_location() -> String {
    "Antalya".into()
}

fn default_radius() -> String {
    "0".into()
}

fn default_result_on_page() -> usize {
    100
}

#[derive(Debug, Deserialize)]
struct JoobleResponse {
    #[serde(rename = "totalCount", default)]
    total_count: usize,
    #[serde(default)]
    jobs: Vec<JoobleJob>,
}

#[derive(Debug, Deserialize)]
struct JoobleJob {
    id: Option<Value>,
    title: Option<String>,
    location: Option<String>,
    snippet: Option<String>,
    source: Option<String>,
    link: Option<String>,
    company: Option<String>,
    updated: Option<String>,
}

fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    let settings = load_settings()?;
    let client = Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent("AliJobRadar/0.3")
        .build()
        .context("HTTP istemcisi oluşturulamadı")?;

    let yesterday = yesterday_istanbul();
    let mut warnings = Vec::new();

    let api_key = env::var("JOOBLE_API_KEY").unwrap_or_default();
    let mut jobs = if api_key.trim().is_empty() {
        warnings.push("JOOBLE_API_KEY tanımlı değil".to_string());
        Vec::new()
    } else {
        match fetch_jooble(&client, api_key.trim(), &settings, &yesterday) {
            Ok(found) => found,
            Err(err) => {
                warnings.push(format!("Jooble: {err:#}"));
                Vec::new()
            }
        }
    };

    jobs = deduplicate(jobs);
    for job in &mut jobs {
        let (score, reasons) = score_job(job, &settings);
        job.score = score;
        job.reasons = reasons;
    }
    jobs.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.title.cmp(&b.title)));

    let subject = format!(
        "Antalya İş Radarı — {} — {} ilan",
        format_date_tr(&yesterday),
        jobs.len()
    );
    let html = build_email_html(&jobs, &yesterday, &warnings, &settings);
    save_report(&html)?;

    let mail_ready = ["MAIL_TO", "SMTP_USERNAME", "SMTP_PASSWORD"]
        .iter()
        .all(|key| env::var(key).map(|v| !v.trim().is_empty()).unwrap_or(false));

    if mail_ready {
        send_email(&subject, &html)?;
        println!("E-posta gönderildi.");
    } else {
        println!("SMTP ayarları eksik; rapor {} olarak üretildi.", REPORT_PATH);
    }

    println!(
        "Tamamlandı. Tarih: {}, konum: {}, dün tarihli ilan: {}, kaynak uyarısı: {}",
        yesterday,
        settings.location,
        jobs.len(),
        warnings.len()
    );
    Ok(())
}

fn load_settings() -> Result<Settings> {
    let raw = fs::read_to_string(SETTINGS_PATH)
        .with_context(|| format!("{} okunamadı", SETTINGS_PATH))?;
    serde_json::from_str(&raw).context("settings.json geçerli JSON değil")
}

fn yesterday_istanbul() -> String {
    let istanbul_now = Utc::now() + ChronoDuration::hours(3);
    (istanbul_now.date_naive() - ChronoDuration::days(1))
        .format("%Y-%m-%d")
        .to_string()
}

fn format_date_tr(iso_date: &str) -> String {
    let mut parts = iso_date.split('-');
    let y = parts.next().unwrap_or("");
    let m = parts.next().unwrap_or("");
    let d = parts.next().unwrap_or("");
    if y.len() == 4 && m.len() == 2 && d.len() == 2 {
        format!("{d}.{m}.{y}")
    } else {
        iso_date.to_string()
    }
}

fn fetch_jooble(
    client: &Client,
    api_key: &str,
    settings: &Settings,
    target_date: &str,
) -> Result<Vec<Job>> {
    let endpoint = format!("https://tr.jooble.org/api/{api_key}");
    let query = settings.jooble_queries.join(", ");
    if query.trim().is_empty() {
        anyhow::bail!("jooble_queries boş");
    }

    let mut result = Vec::new();
    let mut page = 1usize;
    let per_page = settings.result_on_page.clamp(1, 100);

    loop {
        let response: JoobleResponse = client
            .post(&endpoint)
            .json(&json!({
                "keywords": query,
                "location": settings.location,
                "radius": settings.radius,
                "page": page,
                "ResultOnPage": per_page,
                "companysearch": false
            }))
            .send()
            .with_context(|| format!("Jooble isteği başarısız: sayfa {page}"))?
            .error_for_status()
            .with_context(|| format!("Jooble HTTP hatası: sayfa {page}"))?
            .json()
            .with_context(|| format!("Jooble JSON çözümlenemedi: sayfa {page}"))?;

        let total_count = response.total_count;
        let received = response.jobs.len();

        for j in response.jobs {
            let title = j.title.unwrap_or_default();
            let url = j.link.unwrap_or_default();
            let location = j.location.unwrap_or_default();
            let updated = j.updated.unwrap_or_default();

            if title.is_empty() || url.is_empty() || updated.is_empty() {
                continue;
            }

            if !is_antalya(&location) || !same_iso_day(&updated, target_date) {
                continue;
            }

            result.push(Job {
                source: j.source.unwrap_or_else(|| "Jooble".into()),
                source_id: j.id.map(json_id_to_string),
                title,
                company: j.company.unwrap_or_else(|| "Bilinmiyor".into()),
                location,
                description: strip_html(&j.snippet.unwrap_or_default()),
                url,
                updated,
                score: 0,
                reasons: vec![],
            });
        }

        if received == 0 || page * per_page >= total_count || page >= 10 {
            break;
        }
        page += 1;
    }

    Ok(result)
}

fn same_iso_day(value: &str, target_date: &str) -> bool {
    value.get(0..10).map(|d| d == target_date).unwrap_or(false)
}

fn is_antalya(value: &str) -> bool {
    normalize(value).contains("antalya")
}

fn json_id_to_string(value: Value) -> String {
    match value {
        Value::String(s) => s,
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        other => other.to_string(),
    }
}

fn score_job(job: &Job, settings: &Settings) -> (i32, Vec<String>) {
    let title = normalize(&job.title);
    let description = normalize(&job.description);
    let all = format!("{title} {description}");
    let mut score = 22i32;
    let mut reasons = vec!["Antalya".to_string()];

    if let Some(role) = settings.roles.iter().find(|r| title.contains(&normalize(r))) {
        score += 38;
        reasons.push(format!("Pozisyon: {role}"));
    } else if settings.roles.iter().any(|r| all.contains(&normalize(r))) {
        score += 16;
        reasons.push("İlan içeriğinde hedef rol".into());
    }

    let skill_hits = settings
        .skills
        .iter()
        .filter(|s| all.contains(&normalize(s)))
        .count() as i32;
    if skill_hits > 0 {
        score += (skill_hits * 5).min(30);
        reasons.push(format!("{skill_hits} yetkinlik eşleşmesi"));
    }

    if let Some(term) = settings
        .penalty_terms
        .iter()
        .find(|term| title.contains(&normalize(term)))
    {
        score -= 18;
        reasons.push(format!("Üst seviye rol: {term}"));
    }

    (score.clamp(0, 100), reasons)
}

fn deduplicate(jobs: Vec<Job>) -> Vec<Job> {
    let mut seen = std::collections::HashSet::new();
    let mut result = Vec::new();
    for job in jobs {
        let key = normalize_url(&job.url);
        if seen.insert(key) {
            result.push(job);
        }
    }
    result
}

fn save_report(html: &str) -> Result<()> {
    if let Some(parent) = Path::new(REPORT_PATH).parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(REPORT_PATH, html).context("HTML raporu kaydedilemedi")
}

fn build_email_html(
    jobs: &[Job],
    target_date: &str,
    warnings: &[String],
    settings: &Settings,
) -> String {
    let mut cards = String::new();
    if jobs.is_empty() {
        cards.push_str("<p>Bu tarihte hedef roller için Antalya ilanı bulunamadı.</p>");
    } else {
        for job in jobs {
            cards.push_str(&format!(
                "<div style='padding:16px;margin:12px 0;border:1px solid #ddd;border-radius:12px'>\
                 <b>{}% — {}</b><br>\
                 {} — {}<br>\
                 <small>{} • Güncelleme: {} • {}</small><br>\
                 <a href='{}'>İlanı aç</a></div>",
                job.score,
                escape_html(&job.title),
                escape_html(&job.company),
                escape_html(&job.location),
                escape_html(&job.source),
                escape_html(&job.updated),
                escape_html(&job.reasons.join(" • ")),
                escape_html(&job.url)
            ));
        }
    }

    let warning_html = if warnings.is_empty() {
        String::new()
    } else {
        format!(
            "<p><small>Kaynak uyarıları: {}</small></p>",
            escape_html(&warnings.join(" | "))
        )
    };

    format!(
        "<!doctype html><html lang='tr'><body style='font-family:Arial;max-width:780px;margin:auto;padding:24px'>\
         <h2>Antalya Günlük İş Radarı</h2>\
         <p><b>Tarih:</b> {} &nbsp; <b>Konum:</b> {} &nbsp; <b>İlan:</b> {}</p>\
         <p>Bu raporda puan eşiği uygulanmaz; dün tarihli tüm hedef ilanlar gösterilir. Puan yalnızca uygunluk sıralamasıdır.</p>\
         {cards}{warning_html}</body></html>",
        format_date_tr(target_date),
        escape_html(&settings.location),
        jobs.len()
    )
}

fn send_email(subject: &str, html: &str) -> Result<()> {
    let recipients_raw = env::var("MAIL_TO").context("MAIL_TO yok")?;
    let username = env::var("SMTP_USERNAME").context("SMTP_USERNAME yok")?;
    let password = env::var("SMTP_PASSWORD").context("SMTP_PASSWORD yok")?;
    let host = env::var("SMTP_HOST").unwrap_or_else(|_| "smtp.gmail.com".into());

    let recipients: Vec<&str> = recipients_raw
        .split(|c| c == ',' || c == ';')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .collect();

    if recipients.is_empty() {
        anyhow::bail!("MAIL_TO içinde geçerli alıcı yok");
    }

    let mut builder = Message::builder().from(username.parse()?);
    for recipient in recipients {
        builder = builder.to(recipient.parse()?);
    }

    let email = builder
        .subject(subject)
        .header(ContentType::TEXT_HTML)
        .body(html.to_string())?;

    let mailer = SmtpTransport::relay(&host)?
        .credentials(Credentials::new(username, password))
        .build();
    mailer.send(&email).context("SMTP gönderimi başarısız")?;
    Ok(())
}

fn normalize(value: &str) -> String {
    value.to_lowercase().replace('ı', "i")
}

fn normalize_url(value: &str) -> String {
    value
        .split('?')
        .next()
        .unwrap_or(value)
        .trim_end_matches('/')
        .to_string()
}

fn strip_html(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut in_tag = false;
    for ch in value.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => {
                in_tag = false;
                out.push(' ');
            }
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}
