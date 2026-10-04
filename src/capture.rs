//! Screenshot local evidence documents without executing their contents.
use crate::{execute, read, safe_id, save, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
};

#[derive(Deserialize)]
pub struct Capture {
    pub id: String,
    pub title: String,
    pub kind: String,
    pub when: String,
    pub source: Slice,
    pub trigger: Slice,
}
#[derive(Deserialize)]
pub struct Slice {
    pub path: PathBuf,
    pub label: String,
    #[serde(default = "one")]
    pub start_line: usize,
    pub line_count: usize,
}
fn one() -> usize {
    1
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Visual {
    pub path: String,
    pub caption: String,
    pub kind: String,
}
pub fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}
fn excerpt(slice: &Slice, base: &Path) -> Result<(String, String)> {
    if slice.start_line == 0 || slice.line_count == 0 || slice.line_count > 12 {
        return Err("evidence slices need a 1-based start and 1..12 lines".into());
    }
    let bytes = fs::read(base.join(&slice.path))?;
    let text = std::str::from_utf8(&bytes)?;
    let all: Vec<_> = text.lines().collect();
    if slice.start_line > all.len() {
        return Err("evidence start line is beyond the source".into());
    }
    let selected = all
        .iter()
        .skip(slice.start_line - 1)
        .take(slice.line_count)
        .copied()
        .collect::<Vec<_>>()
        .join("\n");
    // Explicit bounded excerpts keep screenshots legible at print size. Never crop silently.
    if selected.chars().count() > 1600 || selected.lines().any(|l| l.chars().count() > 160) {
        return Err("excerpt too wide/long; choose a smaller real source excerpt".into());
    }
    Ok((selected, format!("{:x}", Sha256::digest(bytes))))
}
pub fn html(c: &Capture, source: &str, trigger: &str) -> String {
    format!(
        r##"<!doctype html><html><head><meta charset="utf-8">
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; style-src 'unsafe-inline'">
<style>
*{{box-sizing:border-box}} body{{margin:0;background:#edf2f5;color:#172a39;font:20px/1.45 Arial,sans-serif}}
main{{margin:30px;max-width:1040px}} .eyebrow{{font-size:15px;letter-spacing:1.2px;color:#456377;text-transform:uppercase}}
h1{{font-size:29px;line-height:1.2;margin:12px 0}} .when{{font-size:19px;margin:15px 0 20px}}
section{{background:white;border:1px solid #cdd8df;margin:16px 0;padding:20px;border-radius:7px}}
h2{{font-size:20px;margin:0 0 8px}} .label{{font-size:15px;color:#456377;margin-bottom:14px;overflow-wrap:anywhere}}
pre{{font:18px/1.4 Consolas,'Courier New',monospace;white-space:pre-wrap;overflow-wrap:anywhere;margin:0;color:#243848}}
.trigger{{border-left:5px solid #a34d29}} footer{{font-size:14px;color:#456377;margin-top:16px}}
</style></head><body><main><div class="eyebrow">{} / {}</div><h1>{}</h1>
<div class="when"><strong>When it is triggered:</strong> {}</div>
<section><h2>Where to look</h2><div class="label">{}</div><pre>{}</pre></section>
<section class="trigger"><h2>What the evidence shows</h2><div class="label">{}</div><pre>{}</pre></section>
<footer>Screenshot of a local evidence document. Source/log excerpts are escaped text. This is not a screenshot of a live production incident.</footer>
</main></body></html>"##,
        escape(&c.id),
        escape(&c.kind),
        escape(&c.title),
        escape(&c.when),
        escape(&c.source.label),
        escape(source),
        escape(&c.trigger.label),
        escape(trigger)
    )
}
fn file_url(path: &Path) -> String {
    let path = path.to_string_lossy().replace('\\', "/");
    let path = path.strip_prefix("//?/").unwrap_or(&path);
    let escaped: String = path
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"/-._~:".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect();
    if escaped.starts_with('/') {
        format!("file://{escaped}")
    } else {
        format!("file:///{escaped}")
    }
}
pub fn run(config: &Path, output: &Path, browser: &Path) -> Result<()> {
    let specs: Vec<Capture> = serde_json::from_value(read(config)?)?;
    let mut ids = std::collections::BTreeSet::new();
    for c in &specs {
        if !safe_id(&c.id) || !ids.insert(&c.id) {
            return Err("unsafe or duplicate capture ID".into());
        }
    }
    fs::create_dir_all(output)?;
    let output = output.canonicalize()?;
    // Chromium expects ordinary Windows paths rather than verbatim \\?\ paths.
    let output = PathBuf::from(output.to_string_lossy().trim_start_matches(r"\\?\"));
    let profile = output.join(format!("browser-profile-{}", std::process::id()));
    let base = config.parent().unwrap_or(Path::new("."));
    let cwd = std::env::current_dir()?;
    let mut manifest = Vec::new();
    let mut errors = Vec::new();
    for c in specs {
        let png = output.join(format!("{}.png", c.id));
        if png.exists() {
            fs::remove_file(&png)?;
        }
        let result = (|| -> Result<serde_json::Value> {
            let (source, source_hash) = excerpt(&c.source, base)?;
            let (trigger, trigger_hash) = excerpt(&c.trigger, base)?;
            let page = output.join(format!("{}.html", c.id));
            fs::write(&page, html(&c, &source, &trigger))?;
            let args = vec![
                "--headless".into(),
                "--disable-gpu".into(),
                "--hide-scrollbars".into(),
                "--no-first-run".into(),
                "--no-default-browser-check".into(),
                "--disable-background-networking".into(),
                "--force-device-scale-factor=1".into(),
                "--window-size=1100,1250".into(),
                format!("--user-data-dir={}", profile.display()),
                format!("--screenshot={}", png.display()),
                file_url(&page),
            ];
            let mut attempts = Vec::new();
            let mut captured = false;
            // A fresh Chromium profile can fail during initialization. Retry once,
            // retaining the first failure so a successful retry does not erase it.
            for attempt in 1..=2 {
                if png.exists() {
                    fs::remove_file(&png)?;
                }
                let x = execute(&browser.to_string_lossy(), &args, &cwd, 60)?;
                captured = x.exit_code == Some(0) && !x.timed_out && png.exists();
                attempts.push(json!({"attempt":attempt,"exit_code":x.exit_code,"timed_out":x.timed_out,"stderr":x.stderr}));
                if captured {
                    break;
                }
            }
            if !captured {
                return Err(format!("browser capture failed: {attempts:?}").into());
            }
            let dimensions = image::image_dimensions(&png)?;
            if dimensions.0 < 800 || dimensions.1 < 800 {
                return Err("unexpectedly small screenshot".into());
            }
            Ok(
                json!({"id":c.id,"kind":c.kind,"image":png,"html":page,"source":c.source.path,"source_sha256":source_hash,"trigger":c.trigger.path,"trigger_sha256":trigger_hash,"screenshot_sha256":format!("{:x}",Sha256::digest(fs::read(&png)?)),"dimensions":dimensions,"browser_attempts":attempts,"requires_visual_review":true}),
            )
        })();
        match result {
            Ok(v) => {
                eprintln!("Captured {}", c.id);
                manifest.push(v)
            }
            Err(e) => errors.push(format!("{}: {e}", c.id)),
        }
    }
    save(
        &output.join("capture-manifest.json"),
        &json!({"captures":manifest,"errors":errors,"note":"Inspect every screenshot for overflow/cropping before distribution."}),
    )?;
    if !errors.is_empty() {
        return Err(format!(
            "{} capture failures; see capture-manifest.json",
            errors.len()
        )
        .into());
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn text_cannot_become_html() {
        assert_eq!(escape("<script>\"&"), "&lt;script&gt;&quot;&amp;");
    }
    #[test]
    fn file_url_encodes_spaces_and_fragment() {
        assert_eq!(
            file_url(Path::new("C:/a b/c#d.html")),
            "file:///C:/a%20b/c%23d.html"
        );
    }
}
