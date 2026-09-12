use anyhow::{Context, Result};
use chrono::{Duration as ChronoDuration, Utc};
use lettre::{
    message::header::ContentType,
    transport::smtp::authentication::Credentials,
    Message, SmtpTransport, Transport,
};
use reqwest::blocking::Client;
use serde::Deserialize;
use serde_json::{json, Value};
use std::{env, fs, path::Path, time::Duration};

const SETTINGS_PATH: &str = "settings.json";
const REPORT_PATH: &str = "data/latest_report.html";

#[derive(Debug, Clone)]
struct Job {
    source: String,
    source_id: Option<String>,
    title: String,
    company: String,
    location: String,
    url: String,
    updated: String,
}

#[derive(Debug, Clone, Deserialize)]
struct Settings {
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
        .user_agent("AliJobRadar/0.4")
        .build()
        .context("HTTP istemcisi oluşturulamadı")?;

    let (window_start, window_end) = last_30_days_istanbul();
    let mut warnings = Vec::new();

    let api_key = env::var("JOOBLE_API_KEY").unwrap_or_default();
    let mut jobs = if api_key.trim().is_empty() {
        warnings.push("JOOBLE_API_KEY tanımlı değil".to_string());
        Vec::new()
    } else {
        match fetch_jooble(
            &client,
            api_key.trim(),
            &settings,
            &window_start,
            &window_end,
        ) {
            Ok(found) => found,
            Err(err) => {
                warnings.push(format!("Jooble: {err:#}"));
                Vec::new()
            }
        }
    };

    jobs = deduplicate(jobs);
    jobs.sort_by(|a, b| {
        b.updated
            .cmp(&a.updated)
            .then_with(|| a.title.cmp(&b.title))
    });

    let subject = format!(
        "Antalya İş İlanları — Son 30 Gün — {} ilan",
        jobs.len()
    );
    let html = build_email_html(&jobs, &window_start, &window_end, &warnings, &settings);
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
        "Tamamlandı. Dönem: {} - {}, konum: {}, ilan: {}, kaynak uyarısı: {}",
        window_start,
        window_end,
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

fn last_30_days_istanbul() -> (String, String) {
    let istanbul_now = Utc::now() + ChronoDuration::hours(3);
    let today = istanbul_now.date_naive();
    let start = today - ChronoDuration::days(29);
    (
        start.format("%Y-%m-%d").to_string(),
        today.format("%Y-%m-%d").to_string(),
    )
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
    start_date: &str,
    end_date: &str,
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

            if !is_antalya(&location) || !in_iso_day_range(&updated, start_date, end_date) {
                continue;
            }

            result.push(Job {
                source: j.source.unwrap_or_else(|| "Jooble".into()),
                source_id: j.id.map(json_id_to_string),
                title,
                company: j.company.unwrap_or_else(|| "Bilinmiyor".into()),
                location,
                url,
                updated,
            });
        }

        if received == 0 || page * per_page >= total_count || page >= 10 {
            break;
        }
        page += 1;
    }

    Ok(result)
}

fn in_iso_day_range(value: &str, start_date: &str, end_date: &str) -> bool {
    value
        .get(0..10)
        .map(|date| date >= start_date && date <= end_date)
        .unwrap_or(false)
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

fn deduplicate(jobs: Vec<Job>) -> Vec<Job> {
    let mut seen = std::collections::HashSet::new();
    let mut result = Vec::new();
    for job in jobs {
        let key = if let Some(id) = &job.source_id {
            format!("{}:{}", normalize(&job.source), id)
        } else {
            normalize_url(&job.url)
        };
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
    start_date: &str,
    end_date: &str,
    warnings: &[String],
    settings: &Settings,
) -> String {
    let mut rows = String::new();

    if jobs.is_empty() {
        rows.push_str("<tr><td colspan='6' style='padding:14px'>İlan bulunamadı.</td></tr>");
    } else {
        for job in jobs {
            rows.push_str(&format!(
                "<tr>\
                 <td style='padding:8px;border-bottom:1px solid #eee'>{}</td>\
                 <td style='padding:8px;border-bottom:1px solid #eee'>{}</td>\
                 <td style='padding:8px;border-bottom:1px solid #eee'>{}</td>\
                 <td style='padding:8px;border-bottom:1px solid #eee'>{}</td>\
                 <td style='padding:8px;border-bottom:1px solid #eee'>{}</td>\
                 <td style='padding:8px;border-bottom:1px solid #eee'><a href='{}'>Aç</a></td>\
                 </tr>",
                escape_html(&job.title),
                escape_html(&job.company),
                escape_html(&job.location),
                escape_html(&job.source),
                escape_html(job.updated.get(0..10).unwrap_or(&job.updated)),
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
        "<!doctype html><html lang='tr'><body style='font-family:Arial,sans-serif;max-width:980px;margin:auto;padding:24px;color:#222'>\
         <h2>Antalya İş İlanları — Son 30 Gün</h2>\
         <p><b>Dönem:</b> {} - {} &nbsp; <b>Konum:</b> {} &nbsp; <b>Toplam:</b> {}</p>\
         <p>Rol veya uygunluk puanı nedeniyle ilan elenmez. Jooble API anahtar kelime alanını zorunlu tuttuğu için geniş bir kelime kümesiyle tarama yapılır; bu nedenle rapor Jooble'daki tüm Antalya ilanlarının eksiksiz kopyası olduğunu iddia etmez.</p>\
         <table style='width:100%;border-collapse:collapse;font-size:13px'>\
         <thead><tr style='text-align:left;background:#f5f5f5'>\
         <th style='padding:8px'>Pozisyon</th><th style='padding:8px'>Şirket</th><th style='padding:8px'>Konum</th><th style='padding:8px'>Kaynak</th><th style='padding:8px'>Güncelleme</th><th style='padding:8px'>Link</th>\
         </tr></thead><tbody>{rows}</tbody></table>{warning_html}</body></html>",
        format_date_tr(start_date),
        format_date_tr(end_date),
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

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}
