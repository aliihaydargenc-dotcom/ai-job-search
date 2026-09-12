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
use std::{collections::HashMap, env, fs, path::Path, time::Duration};

const SETTINGS_PATH: &str = "settings.json";
const REPORT_PATH: &str = "data/latest_report.html";
const CACHE_PATH: &str = "data/job_cache.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
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
    #[serde(default = "default_max_api_pages")]
    max_api_pages: usize,
    #[serde(default = "default_bootstrap_api_pages")]
    bootstrap_api_pages: usize,
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

fn default_max_api_pages() -> usize {
    2
}

fn default_bootstrap_api_pages() -> usize {
    3
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
        .user_agent("AliJobRadar/0.5")
        .build()
        .context("HTTP istemcisi oluşturulamadı")?;

    let (window_start, window_end) = last_30_days_istanbul();
    let mut warnings = Vec::new();
    let cached_jobs = load_cache()?;
    let cache_was_empty = cached_jobs.is_empty();

    let api_key = env::var("JOOBLE_API_KEY").unwrap_or_default();
    let page_limit = if cache_was_empty {
        settings.bootstrap_api_pages.max(1)
    } else {
        settings.max_api_pages.max(1)
    };

    let (fresh_jobs, api_calls) = if api_key.trim().is_empty() {
        warnings.push("JOOBLE_API_KEY tanımlı değil".to_string());
        (Vec::new(), 0usize)
    } else {
        match fetch_jooble(
            &client,
            api_key.trim(),
            &settings,
            &window_start,
            &window_end,
            page_limit,
        ) {
            Ok(found) => found,
            Err(err) => {
                warnings.push(format!("Jooble: {err:#}"));
                (Vec::new(), 0usize)
            }
        }
    };

    let fresh_count = fresh_jobs.len();
    let mut jobs = merge_cache(cached_jobs, fresh_jobs, &window_start, &window_end);
    jobs.sort_by(|a, b| {
        b.updated
            .cmp(&a.updated)
            .then_with(|| a.title.cmp(&b.title))
    });
    save_cache(&jobs)?;

    let subject = format!(
        "Antalya İş İlanları — Son 30 Gün — {} ilan",
        jobs.len()
    );
    let html = build_email_html(
        &jobs,
        &window_start,
        &window_end,
        &warnings,
        &settings,
        fresh_count,
        api_calls,
        cache_was_empty,
    );
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
        "Tamamlandı. Dönem: {} - {}, konum: {}, cache ilanı: {}, bu çalışmada API'den gelen: {}, API çağrısı: {}, kaynak uyarısı: {}",
        window_start,
        window_end,
        settings.location,
        jobs.len(),
        fresh_count,
        api_calls,
        warnings.len()
    );
    Ok(())
}

fn load_settings() -> Result<Settings> {
    let raw = fs::read_to_string(SETTINGS_PATH)
        .with_context(|| format!("{} okunamadı", SETTINGS_PATH))?;
    serde_json::from_str(&raw).context("settings.json geçerli JSON değil")
}

fn load_cache() -> Result<Vec<Job>> {
    if !Path::new(CACHE_PATH).exists() {
        return Ok(Vec::new());
    }
    let raw = fs::read_to_string(CACHE_PATH).context("İlan cache dosyası okunamadı")?;
    if raw.trim().is_empty() {
        return Ok(Vec::new());
    }
    serde_json::from_str(&raw).context("İlan cache dosyası geçerli JSON değil")
}

fn save_cache(jobs: &[Job]) -> Result<()> {
    if let Some(parent) = Path::new(CACHE_PATH).parent() {
        fs::create_dir_all(parent)?;
    }
    let raw = serde_json::to_string_pretty(jobs).context("İlan cache JSON üretilemedi")?;
    fs::write(CACHE_PATH, raw).context("İlan cache dosyası kaydedilemedi")
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
    page_limit: usize,
) -> Result<(Vec<Job>, usize)> {
    let endpoint = format!("https://tr.jooble.org/api/{api_key}");
    let query = settings.jooble_queries.join(", ");
    if query.trim().is_empty() {
        anyhow::bail!("jooble_queries boş");
    }

    let mut result = Vec::new();
    let mut page = 1usize;
    let mut api_calls = 0usize;
    let per_page = settings.result_on_page.clamp(1, 100);
    let page_limit = page_limit.clamp(1, 10);

    loop {
        api_calls += 1;
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

        if received == 0
            || page * per_page >= total_count
            || page >= page_limit
        {
            break;
        }
        page += 1;
    }

    Ok((deduplicate(result), api_calls))
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

fn job_key(job: &Job) -> String {
    if let Some(id) = &job.source_id {
        format!("{}:{}", normalize(&job.source), id)
    } else {
        normalize_url(&job.url)
    }
}

fn deduplicate(jobs: Vec<Job>) -> Vec<Job> {
    let mut by_key: HashMap<String, Job> = HashMap::new();
    for job in jobs {
        let key = job_key(&job);
        match by_key.get(&key) {
            Some(existing) if existing.updated >= job.updated => {}
            _ => {
                by_key.insert(key, job);
            }
        }
    }
    by_key.into_values().collect()
}

fn merge_cache(
    cached: Vec<Job>,
    fresh: Vec<Job>,
    start_date: &str,
    end_date: &str,
) -> Vec<Job> {
    let mut by_key: HashMap<String, Job> = HashMap::new();

    for job in cached.into_iter().chain(fresh) {
        if !is_antalya(&job.location) || !in_iso_day_range(&job.updated, start_date, end_date) {
            continue;
        }
        let key = job_key(&job);
        match by_key.get(&key) {
            Some(existing) if existing.updated >= job.updated => {}
            _ => {
                by_key.insert(key, job);
            }
        }
    }

    by_key.into_values().collect()
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
    fresh_count: usize,
    api_calls: usize,
    cache_was_empty: bool,
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

    let quota_note = if cache_was_empty {
        format!(
            "İlk cache kurulumu: en fazla {} API sayfası. Sonraki günlük çalışmalarda en fazla {} sayfa kullanılacak.",
            settings.bootstrap_api_pages.max(1),
            settings.max_api_pages.max(1)
        )
    } else {
        format!(
            "API kota koruması aktif: bu çalışmada {} çağrı yapıldı; günlük üst sınır {} sayfa. 30 günlük geçmiş ilanlar yerel cache'de korunur.",
            api_calls,
            settings.max_api_pages.max(1)
        )
    };

    format!(
        "<!doctype html><html lang='tr'><body style='font-family:Arial,sans-serif;max-width:980px;margin:auto;padding:24px;color:#222'>\
         <h2>Antalya İş İlanları — Son 30 Gün</h2>\
         <p><b>Dönem:</b> {} - {} &nbsp; <b>Konum:</b> {} &nbsp; <b>Toplam:</b> {}</p>\
         <p><b>Bu çalışmada API'den alınan:</b> {} &nbsp; <b>API çağrısı:</b> {}</p>\
         <p>{}</p>\
         <p>Rol veya uygunluk puanı nedeniyle ilan elenmez. Jooble API anahtar kelime alanını zorunlu tuttuğu için geniş bir kelime kümesiyle tarama yapılır; cache önceki günlerde görülen ilanları 30 günlük pencere boyunca korur.</p>\
         <table style='width:100%;border-collapse:collapse;font-size:13px'>\
         <thead><tr style='text-align:left;background:#f5f5f5'>\
         <th style='padding:8px'>Pozisyon</th><th style='padding:8px'>Şirket</th><th style='padding:8px'>Konum</th><th style='padding:8px'>Kaynak</th><th style='padding:8px'>Güncelleme</th><th style='padding:8px'>Link</th>\
         </tr></thead><tbody>{rows}</tbody></table>{warning_html}</body></html>",
        format_date_tr(start_date),
        format_date_tr(end_date),
        escape_html(&settings.location),
        jobs.len(),
        fresh_count,
        api_calls,
        escape_html(&quota_note)
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
