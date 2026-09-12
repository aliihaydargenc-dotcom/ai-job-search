# Ali Job Radar 🦀📧

Rust ile çalışan kişisel iş ilanı radarı.

## Varsayılan arama profili

- Data Analyst / Veri Analisti
- BI Analyst / Business Intelligence
- Reporting Analyst / Raporlama Uzmanı
- SQL Analyst / Data Reporting / Revenue Analyst
- Antalya önceliği
- Türkiye, Remote, Hybrid, Europe ve EMEA
- SQL, Excel, Qlik Sense, Power BI, Python, dashboard ve veri analizi
- Senior / Lead / Director gibi üst seviye rollere puan cezası

## Kaynaklar

- Remotive public API
- Arbeitnow public API
- Jooble Türkiye API (JOOBLE_API_KEY varsa)

## Çalışma biçimi

GitHub Actions her gün 09:15 Europe/Istanbul saatinde Rust programını çalıştırır. SMTP bilgileri GitHub Secrets içine eklenmişse HTML e-posta gönderilir. SMTP bilgileri yoksa çalışma hata vermez; `data/latest_report.html` üretilir ve Actions artifact olarak saklanır.

Mail için gereken repository secrets:

- `MAIL_TO`
- `SMTP_USERNAME`
- `SMTP_PASSWORD`
- `JOOBLE_API_KEY` (opsiyonel)

Gerçek Gmail parolasını kaynak koda yazmayın. Gmail kullanılıyorsa uygulama parolası/OAuth benzeri güvenli kimlik doğrulama tercih edilmelidir.

## Yerel kullanım

```bash
cargo run --release
```

Ayarlar `settings.json` içinden değiştirilebilir.
