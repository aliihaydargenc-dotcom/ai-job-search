use anyhow::{Context, Result};
use chrono::Local;
use lettre::{
    message::header::ContentType,
    transport::smtp::authentication::Credentials,
    Message, SmtpTransport, Transport,
};
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    env, fs,
    path::Path,
    time::Duration,
};

const SETTINGS_PATH: &str = "settings.json";
const STATE_PATH: &str = "data/seen_jobs.json";
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
    remote: bool,
    score: i32,
    reasons: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct Settings {
    min_score: i32,
    max_email_jobs: usize,
    roles: Vec<String>,
    skills: Vec<String>,
    locations: Vec<String>,
    penalty_terms: Vec<String>,
    jooble_queries: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct RemotiveResponse {
    #[serde(default)]
    jobs: Vec<RemotiveJob>,
}

#[derive(Debug, Deserialize)]
struct RemotiveJob {
    id: Option<i64>,
    url: Option<String>,
    title: Option<String>,
    company_name: Option<String>,
    candidate_required_location: Option<String>,
    description: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ArbeitnowResponse {
    #[serde(default)]
    data: Vec<ArbeitnowJob>,
}

#[derive(Debug, Deserialize)]
struct ArbeitnowJob {
    slug: Option<String>,
    company_name: Option<String>,
    title: Option<String>,
    description: Option<String>,
    remote: Option<bool>,
    url: Option<String>,
    location: Option<String>,
}

#[derive(Debug, Deserialize)]
struct JoobleResponse {
    #[serde(default)]
    jobs: Vec<JoobleJob>,
}

#[derive(Debug, Deserialize)]
struct JoobleJob {
    id: Option<String>,
    title: Option<String>,
    location: Option<String>,
    snippet: Option<String>,
    source: Option<String>,
    link: Option<String>,
    company: Option<String>,
}

fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    let settings = load_settings()?;
    let client = Client::builder()
        .timeout(Duration::from_secs(25))
        .user_agent("AliJobRadar/0.1")
        .build()
        .context("HTTP istemcisi oluşturulamadı")?;

    let mut jobs = Vec::new();
    let mut warnings = Vec::new();

    match fetch_remotive(&client) {
        Ok(mut found) => jobs.append(&mut found),
        Err(err) => warnings.push(format!("Remotive: {err:#}")),
    }
    match fetch_arbeitnow(&client) {
        Ok(mut found) => jobs.append(&mut found),
        Err(err) => warnings.push(format!("Arbeitnow: {err:#}")),
    }
    if let Ok(api_key) = env::var("JOOBLE_API_KEY") {
        if !api_key.trim().is_empty() {
            match fetch_jooble(&client, api_key.trim(), &settings) {
                Ok(mut found) => jobs.append(&mut found),
                Err(err) => warnings.push(format!("Jooble: {err:#}")),
            }
        }
    }

    let mut jobs = deduplicate(jobs);
    for job in &mut jobs {
        let (score, reasons) = score_job(job, &settings);
        job.score = score;
        job.reasons = reasons;
    }
    jobs.sort_by(|a, b| b.score.cmp(&a.score));

    let mut seen = load_seen()?;
    let fetched_count = jobs.len();
    let mut fresh = Vec::new();
    for job in jobs {
        let id = stable_job_id(&job);
        if !seen.contains(&id) && job.score >= settings.min_score {
            fresh.push(job.clone());
        }
        seen.insert(id);
    }
    fresh.sort_by(|a, b| b.score.cmp(&a.score));
    fresh.truncate(settings.max_email_jobs);

    let subject = format!(
        "İş Radarı — {} yeni eşleşme ({})",
        fresh.len(),
        Local::now().format("%d.%m.%Y")
    );
    let html = build_email_html(&fresh, fetched_count, &warnings, &settings);
    save_report(&html)?;

    let mail_ready = ["MAIL_TO", "SMTP_USERNAME", "SMTP_PASSWORD"]
        .iter()
        .all(|key| env::var(key).map(|v| !v.trim().is_empty()).unwrap_or(false));

    if mail_ready {
        send_email(&subject, &html)?;
        save_seen(&seen)?;
        println!("E-posta gönderildi.");
    } else {
        println!("SMTP ayarları yok; rapor {} olarak üretildi.", REPORT_PATH);
    }

    println!(
        "Tamamlandı. Taranan: {}, güçlü yeni eşleşme: {}, kaynak uyarısı: {}",
        fetched_count,
        fresh.len(),
        warnings.len()
    );
    Ok(())
}

fn load_settings() -> Result<Settings> {
    let raw = fs::read_to_string(SETTINGS_PATH)
        .with_context(|| format!("{} okunamadı", SETTINGS_PATH))?;
    serde_json::from_str(&raw).context("settings.json geçerli JSON değil")
}

fn fetch_remotive(client: &Client) -> Result<Vec<Job>> {
    let response: RemotiveResponse = client
        .get("https://remotive.com/api/remote-jobs")
        .send()
        .context("Remotive isteği başarısız")?
        .error_for_status()
        .context("Remotive HTTP hatası")?
        .json()
        .context("Remotive JSON çözümlenemedi")?;

    Ok(response
        .jobs
        .into_iter()
        .filter_map(|j| {
            let title = j.title.unwrap_or_default();
            let url = j.url.unwrap_or_default();
            if title.is_empty() || url.is_empty() {
                return None;
            }
            Some(Job {
                source: "Remotive".into(),
                source_id: j.id.map(|x| x.to_string()),
                title,
                company: j.company_name.unwrap_or_else(|| "Bilinmiyor".into()),
                location: j
                    .candidate_required_location
                    .unwrap_or_else(|| "Remote".into()),
                description: strip_html(&j.description.unwrap_or_default()),
                url,
                remote: true,
                score: 0,
                reasons: vec![],
            })
        })
        .collect())
}

fn fetch_arbeitnow(client: &Client) -> Result<Vec<Job>> {
    let response: ArbeitnowResponse = client
        .get("https://www.arbeitnow.com/api/job-board-api")
        .send()
        .context("Arbeitnow isteği başarısız")?
        .error_for_status()
        .context("Arbeitnow HTTP hatası")?
        .json()
        .context("Arbeitnow JSON çözümlenemedi")?;

    Ok(response
        .data
        .into_iter()
        .filter_map(|j| {
            let title = j.title.unwrap_or_default();
            let url = j.url.unwrap_or_default();
            if title.is_empty() || url.is_empty() {
                return None;
            }
            Some(Job {
                source: "Arbeitnow".into(),
                source_id: j.slug,
                title,
                company: j.company_name.unwrap_or_else(|| "Bilinmiyor".into()),
                location: j.location.unwrap_or_else(|| "Belirtilmemiş".into()),
                description: strip_html(&j.description.unwrap_or_default()),
                url,
                remote: j.remote.unwrap_or(false),
                score: 0,
                reasons: vec![],
            })
        })
        .collect())
}

fn fetch_jooble(client: &Client, api_key: &str, settings: &Settings) -> Result<Vec<Job>> {
    let endpoint = format!("https://tr.jooble.org/api/{api_key}");
    let mut result = Vec::new();
    for query in &settings.jooble_queries {
        let response: JoobleResponse = client
            .post(&endpoint)
            .json(&json!({"keywords": query, "location": "Antalya", "page": 1}))
            .send()
            .with_context(|| format!("Jooble isteği başarısız: {query}"))?
            .error_for_status()
            .with_context(|| format!("Jooble HTTP hatası: {query}"))?
            .json()
            .with_context(|| format!("Jooble JSON çözümlenemedi: {query}"))?;

        for j in response.jobs {
            let title = j.title.unwrap_or_default();
            let url = j.link.unwrap_or_default();
            if title.is_empty() || url.is_empty() {
                continue;
            }
            result.push(Job {
                source: j.source.unwrap_or_else(|| "Jooble".into()),
                source_id: j.id,
                title,
                company: j.company.unwrap_or_else(|| "Bilinmiyor".into()),
                location: j.location.unwrap_or_else(|| "Antalya".into()),
                description: strip_html(&j.snippet.unwrap_or_default()),
                url,
                remote: false,
                score: 0,
                reasons: vec![],
            });
        }
    }
    Ok(result)
}

fn score_job(job: &Job, settings: &Settings) -> (i32, Vec<String>) {
    let title = normalize(&job.title);
    let description = normalize(&job.description);
    let location = normalize(&job.location);
    let all = format!("{title} {description} {location}");
    let mut score: i32 = 0;
    let mut reasons = Vec::new();

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

    if location.contains("antalya") {
        score += 22;
        reasons.push("Antalya".into());
    } else if location.contains("turkey") || location.contains("turkiye") {
        score += 14;
        reasons.push("Türkiye".into());
    }

    if job.remote || all.contains("remote") || all.contains("uzaktan") {
        score += 12;
        reasons.push("Remote".into());
    }
    if all.contains("hybrid") || all.contains("hibrit") {
        score += 7;
        reasons.push("Hybrid".into());
    }
    if ["worldwide", "anywhere", "global", "europe", "emea"]
        .iter()
        .any(|x| location.contains(x))
    {
        score += 8;
        reasons.push("Geniş başvuru bölgesi".into());
    }
    if ["united states only", "usa only", "u.s. only", "canada only"]
        .iter()
        .any(|x| location.contains(x))
    {
        score -= 30;
        reasons.push("Lokasyon kısıtı".into());
    }
    if let Some(term) = settings
        .penalty_terms
        .iter()
        .find(|term| title.contains(&normalize(term)))
    {
        score -= 18;
        reasons.push(format!("Üst seviye rol: {term}"));
    }
    if settings
        .locations
        .iter()
        .any(|loc| all.contains(&normalize(loc)))
    {
        score += 4;
    }
    (score.clamp(0, 100), reasons)
}

fn deduplicate(jobs: Vec<Job>) -> Vec<Job> {
    let mut map: HashMap<String, Job> = HashMap::new();
    for job in jobs {
        map.entry(normalize_url(&job.url)).or_insert(job);
    }
    map.into_values().collect()
}

fn stable_job_id(job: &Job) -> String {
    if let Some(id) = &job.source_id {
        return format!("{}:{}", normalize(&job.source), id);
    }
    let mut hasher = Sha256::new();
    hasher.update(normalize_url(&job.url).as_bytes());
    format!("{:x}", hasher.finalize())
}

fn load_seen() -> Result<HashSet<String>> {
    if !Path::new(STATE_PATH).exists() {
        return Ok(HashSet::new());
    }
    let raw = fs::read_to_string(STATE_PATH).context("Geçmiş ilan dosyası okunamadı")?;
    if raw.trim().is_empty() {
        return Ok(HashSet::new());
    }
    serde_json::from_str(&raw).context("Geçmiş ilan dosyası bozuk")
}

fn save_seen(seen: &HashSet<String>) -> Result<()> {
    if let Some(parent) = Path::new(STATE_PATH).parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(STATE_PATH, serde_json::to_string_pretty(seen)?)
        .context("Geçmiş ilan dosyası kaydedilemedi")
}

fn save_report(html: &str) -> Result<()> {
    if let Some(parent) = Path::new(REPORT_PATH).parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(REPORT_PATH, html).context("HTML raporu kaydedilemedi")
}

fn build_email_html(jobs: &[Job], fetched: usize, warnings: &[String], settings: &Settings) -> String {
    let mut cards = String::new();
    if jobs.is_empty() {
        cards.push_str("<p>Bugün eşik üzerinde yeni ilan bulunmadı.</p>");
    } else {
        for job in jobs {
            cards.push_str(&format!(
                "<div style='padding:16px;margin:12px 0;border:1px solid #ddd;border-radius:12px'><b>{}% — {}</b><br>{} — {}<br><small>{} • {}</small><br><a href='{}'>İlanı aç</a></div>",
                job.score,
                escape_html(&job.title),
                escape_html(&job.company),
                escape_html(&job.location),
                escape_html(&job.source),
                escape_html(&job.reasons.join(" • ")),
                escape_html(&job.url)
            ));
        }
    }
    let warning_html = if warnings.is_empty() {
        String::new()
    } else {
        format!("<p><small>Kaynak uyarıları: {}</small></p>", escape_html(&warnings.join(" | ")))
    };
    format!(
        "<!doctype html><html lang='tr'><body style='font-family:Arial;max-width:760px;margin:auto;padding:24px'><h2>Günlük İş Radarı</h2><p>{} ilan tarandı. Eşik: {}. Yeni güçlü eşleşme: {}.</p>{}{}</body></html>",
        fetched,
        settings.min_score,
        jobs.len(),
        cards,
        warning_html
    )
}

fn send_email(subject: &str, html: &str) -> Result<()> {
    let to = env::var("MAIL_TO").context("MAIL_TO tanımlı değil")?;
    let username = env::var("SMTP_USERNAME").context("SMTP_USERNAME tanımlı değil")?;
    let password = env::var("SMTP_PASSWORD").context("SMTP_PASSWORD tanımlı değil")?;
    let host = env::var("SMTP_HOST").unwrap_or_else(|_| "smtp.gmail.com".into());

    let email = Message::builder()
        .from(username.parse().context("SMTP_USERNAME geçerli e-posta değil")?)
        .to(to.parse().context("MAIL_TO geçerli e-posta değil")?)
        .subject(subject)
        .header(ContentType::TEXT_HTML)
        .body(html.to_owned())
        .context("E-posta oluşturulamadı")?;

    let mailer = SmtpTransport::relay(&host)
        .with_context(|| format!("SMTP bağlantısı kurulamadı: {host}"))?
        .credentials(Credentials::new(username, password))
        .build();
    mailer.send(&email).context("E-posta gönderilemedi")?;
    Ok(())
}

fn strip_html(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut in_tag = false;
    for ch in input.chars() {
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
    out.replace("&nbsp;", " ").replace("&amp;", "&")
}

fn normalize(input: &str) -> String {
    input
        .to_lowercase()
        .replace('ı', "i")
        .replace('ş', "s")
        .replace('ğ', "g")
        .replace('ü', "u")
        .replace('ö', "o")
        .replace('ç', "c")
}

fn normalize_url(input: &str) -> String {
    input
        .trim()
        .split('#')
        .next()
        .unwrap_or(input)
        .trim_end_matches('/')
        .to_lowercase()
}

fn escape_html(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> Settings {
        Settings {
            min_score: 48,
            max_email_jobs: 15,
            roles: vec!["data analyst".into(), "veri analisti".into()],
            skills: vec!["sql".into(), "excel".into(), "qlik sense".into()],
            locations: vec!["antalya".into(), "remote".into()],
            penalty_terms: vec!["senior".into()],
            jooble_queries: vec!["data analyst".into()],
        }
    }

    fn job(title: &str) -> Job {
        Job {
            source: "test".into(),
            source_id: None,
            title: title.into(),
            company: "Test".into(),
            location: "Antalya".into(),
            description: "SQL Excel Qlik Sense".into(),
            url: "https://example.com/1".into(),
            remote: false,
            score: 0,
            reasons: vec![],
        }
    }

    #[test]
    fn strong_local_match_scores_high() {
        let (score, _) = score_job(&job("Data Analyst"), &settings());
        assert!(score >= 70);
    }

    #[test]
    fn senior_role_is_penalized() {
        let (normal, _) = score_job(&job("Data Analyst"), &settings());
        let (senior, _) = score_job(&job("Senior Data Analyst"), &settings());
        assert!(senior < normal);
    }
}
