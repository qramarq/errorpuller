mod capture;
mod pdf;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    env, fs,
    io::Read,
    path::Path,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[derive(Debug, Serialize, Deserialize)]
struct Execution {
    program: String,
    args: Vec<String>,
    exit_code: Option<i32>,
    timed_out: bool,
    stdout: String,
    stderr: String,
}
fn execute(program: &str, args: &[String], cwd: &Path, timeout: u64) -> Result<Execution> {
    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command.spawn()?;
    let mut out = child.stdout.take().unwrap();
    let mut err = child.stderr.take().unwrap();
    let a = thread::spawn(move || {
        let mut v = Vec::new();
        out.read_to_end(&mut v).map(|_| v)
    });
    let b = thread::spawn(move || {
        let mut v = Vec::new();
        err.read_to_end(&mut v).map(|_| v)
    });
    let start = Instant::now();
    let mut timed_out = false;
    let status = loop {
        if let Some(s) = child.try_wait()? {
            break s;
        }
        if start.elapsed() > Duration::from_secs(timeout) {
            timed_out = true;
            // Kill descendants too: build tools otherwise keep the output pipes open.
            #[cfg(windows)]
            {
                let _ = Command::new("taskkill")
                    .args(["/PID", &child.id().to_string(), "/T", "/F"])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
            }
            #[cfg(unix)]
            {
                let _ = Command::new("kill")
                    .args(["-KILL", "--", &format!("-{}", child.id())])
                    .status();
            }
            child.kill().ok();
            break child.wait()?;
        }
        thread::sleep(Duration::from_millis(50));
    };
    Ok(Execution {
        program: program.into(),
        args: args.to_vec(),
        exit_code: status.code(),
        timed_out,
        stdout: String::from_utf8_lossy(&a.join().map_err(|_| "stdout reader panicked")??).into(),
        stderr: String::from_utf8_lossy(&b.join().map_err(|_| "stderr reader panicked")??).into(),
    })
}
fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|s| s.to_string()).collect()
}
fn save(path: &Path, value: &impl Serialize) -> Result<()> {
    fs::write(path, serde_json::to_vec_pretty(value)?)?;
    Ok(())
}
fn read(path: &Path) -> Result<Value> {
    Ok(serde_json::from_str(
        fs::read_to_string(path)?.trim_start_matches('\u{feff}'),
    )?)
}
fn api(repo: &str, endpoint: &str, cwd: &Path) -> Result<Value> {
    let x = execute(
        "gh",
        &strings(&[
            "api",
            "--method",
            "GET",
            "--paginate",
            "--slurp",
            &format!("repos/{repo}/{endpoint}"),
        ]),
        cwd,
        180,
    )?;
    if x.exit_code != Some(0) || x.timed_out {
        return Err(format!("GitHub {endpoint}: {}", x.stderr).into());
    }
    Ok(serde_json::from_str(&x.stdout)?)
}
fn flatten(v: &Value, field: Option<&str>) -> Vec<Value> {
    v.as_array()
        .into_iter()
        .flatten()
        .flat_map(|page| {
            let a = field.map_or(page, |f| &page[f]);
            a.as_array().cloned().unwrap_or_default()
        })
        .collect()
}
fn collect(repo: &str, root: &Path, output: &Path) -> Result<()> {
    let parts: Vec<_> = repo.split('/').collect();
    if parts.len() != 2
        || parts.iter().any(|s| {
            s.is_empty()
                || !s
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
        })
    {
        return Err("repository must be owner/name".into());
    }
    fs::create_dir_all(output)?;
    // Always refresh the inventory. Never present old cached inventories as a new scan.
    let pulls = api(repo, "pulls?state=all&per_page=100", root)?;
    save(&output.join("pulls-pages.json"), &pulls)?;
    let runs = api(repo, "actions/runs?per_page=100", root)?;
    save(&output.join("runs-pages.json"), &runs)?;
    let mut tasks: Vec<(String, String)> = Vec::new();
    for p in flatten(&pulls, None) {
        let n = p["number"].as_u64().ok_or("PR missing number")?;
        for (label, endpoint) in [
            ("detail", format!("pulls/{n}")),
            ("files", format!("pulls/{n}/files?per_page=100")),
            ("comments", format!("issues/{n}/comments?per_page=100")),
            (
                "review-comments",
                format!("pulls/{n}/comments?per_page=100"),
            ),
            ("reviews", format!("pulls/{n}/reviews?per_page=100")),
            ("commits", format!("pulls/{n}/commits?per_page=100")),
            (
                "checks",
                format!(
                    "commits/{}/check-runs?filter=all&per_page=100",
                    p["head"]["sha"].as_str().unwrap_or("")
                ),
            ),
            (
                "statuses",
                format!(
                    "commits/{}/statuses?per_page=100",
                    p["head"]["sha"].as_str().unwrap_or("")
                ),
            ),
        ] {
            tasks.push((format!("pr-{n}-{label}.json"), endpoint));
        }
    }
    let all_runs = flatten(&runs, Some("workflow_runs"));
    for r in &all_runs {
        if (r["conclusion"] != "success" && !r["conclusion"].is_null())
            || r["run_attempt"].as_u64().unwrap_or(1) > 1
        {
            let id = r["id"].as_u64().ok_or("run missing ID")?;
            for attempt in 1..=r["run_attempt"].as_u64().unwrap_or(1) {
                tasks.push((
                    format!("run-{id}-attempt-{attempt}-jobs.json"),
                    format!("actions/runs/{id}/attempts/{attempt}/jobs?per_page=100"),
                ));
            }
        }
    }
    let mut errors = Vec::new();
    for chunk in tasks.chunks(6) {
        let results = thread::scope(|s| {
            let handles: Vec<_> = chunk
                .iter()
                .map(|(file, endpoint)| s.spawn(move || (file, api(repo, endpoint, root))))
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().unwrap())
                .collect::<Vec<_>>()
        });
        for (file, result) in results {
            match result {
                Ok(v) => save(&output.join(file), &v)?,
                Err(e) => {
                    errors.push(format!("{file}: {e}"));
                    fs::remove_file(output.join(file)).ok();
                }
            }
        }
        eprintln!(
            "Collected {} endpoints; {} errors so far",
            chunk.last().unwrap().0,
            errors.len()
        );
    }
    for r in &all_runs {
        if r["conclusion"] != "failure"
            && r["conclusion"] != "timed_out"
            && r["run_attempt"].as_u64().unwrap_or(1) == 1
        {
            continue;
        }
        let id = r["id"].to_string();
        for attempt in 1..=r["run_attempt"].as_u64().unwrap_or(1) {
            let a = attempt.to_string();
            match execute(
                "gh",
                &strings(&[
                    "run",
                    "view",
                    &id,
                    "--repo",
                    repo,
                    "--attempt",
                    &a,
                    "--log-failed",
                ]),
                root,
                180,
            ) {
                Ok(x) if x.exit_code == Some(0) && !x.timed_out => {
                    fs::write(output.join(format!("run-{id}-attempt-{a}.log")), x.stdout)?;
                }
                x => {
                    errors.push(format!("run {id} attempt {a}: logs unavailable: {x:?}"));
                    fs::remove_file(output.join(format!("run-{id}-attempt-{a}.log"))).ok();
                }
            }
        }
    }
    let listed = all_runs.len();
    let total = runs[0]["total_count"].as_u64().unwrap_or(listed as u64);
    if listed as u64 != total {
        errors.push(format!("Workflow inventory incomplete: {listed} retrieved of {total}; partition by date if GitHub limits results."));
    }
    save(
        &output.join("collection.json"),
        &json!({"repository":repo,"collected_unix":SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),"pull_requests":flatten(&pulls,None).len(),"workflow_runs":listed,"errors":errors}),
    )?;
    if !errors.is_empty() {
        return Err(format!("{} collection gaps; see collection.json", errors.len()).into());
    }
    Ok(())
}

#[derive(Serialize, Deserialize, Debug)]
struct TestCase {
    suite: String,
    name: String,
    status: String,
    evidence: String,
}
fn parse_go(text: &str) -> Vec<TestCase> {
    let mut logs = BTreeMap::<(String, String), String>::new();
    let mut cases = Vec::new();
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let package = v["Package"].as_str().unwrap_or("").to_string();
        let name = v["Test"].as_str().unwrap_or("[package]").to_string();
        let key = (package.clone(), name.clone());
        if let Some(o) = v["Output"].as_str() {
            logs.entry(key.clone()).or_default().push_str(o);
        }
        let action = v["Action"].as_str().unwrap_or("");
        if matches!(action, "pass" | "fail" | "skip") {
            cases.push(TestCase {
                suite: package,
                name,
                status: action.into(),
                evidence: logs.remove(&key).unwrap_or_default(),
            });
        }
    }
    cases
}
fn parse_tap(text: &str) -> Vec<TestCase> {
    let mut cases: Vec<TestCase> = Vec::new();
    for line in text.lines() {
        // Node's top-level test points start at column zero; diagnostics attach to the preceding point.
        let (status, rest) = if let Some(x) = line.strip_prefix("not ok ") {
            ("fail", x)
        } else if let Some(x) = line.strip_prefix("ok ") {
            ("pass", x)
        } else {
            if line.starts_with("  ") {
                if let Some(c) = cases.last_mut() {
                    c.evidence.push_str(line);
                    c.evidence.push('\n');
                }
            }
            continue;
        };
        let name = rest.split_once(" - ").map_or(rest, |(_, n)| n);
        let status = if name.contains("# SKIP") || name.contains("# TODO") {
            "skip"
        } else {
            status
        };
        cases.push(TestCase {
            suite: "node".into(),
            name: name.into(),
            status: status.into(),
            evidence: String::new(),
        });
    }
    cases
}
fn parse_rust(suite: &str, text: &str) -> Vec<TestCase> {
    text.lines()
        .filter_map(|line| {
            let (name, status) = line.strip_prefix("test ")?.rsplit_once(" ... ")?;
            let status = match status {
                "ok" => "pass",
                "FAILED" => "fail",
                s if s.starts_with("ignored") => "skip",
                _ => return None,
            };
            Some(TestCase {
                suite: suite.into(),
                name: name.into(),
                status: status.into(),
                evidence: if status == "fail" {
                    text.into()
                } else {
                    String::new()
                },
            })
        })
        .collect()
}
#[derive(Deserialize)]
struct TestCommand {
    name: String,
    program: String,
    args: Vec<String>,
    parser: String,
    #[serde(default = "default_timeout")]
    timeout_seconds: u64,
}
fn default_timeout() -> u64 {
    900
}
fn test(root: &Path, config: &Path, output: &Path) -> Result<()> {
    fs::create_dir_all(output)?;
    let commands: Vec<TestCommand> = serde_json::from_value(read(config)?)?;
    let mut cases = Vec::new();
    let mut executions = Vec::new();
    let mut errors = Vec::new();
    let head = execute("git", &strings(&["rev-parse", "HEAD"]), root, 30)?;
    let status = execute("git", &strings(&["status", "--short"]), root, 30)?;
    for c in commands {
        if !safe_id(&c.name) || !matches!(c.parser.as_str(), "go" | "tap" | "rust" | "exit") {
            return Err("invalid test name or parser".into());
        }
        eprintln!("Running {}", c.name);
        match execute(&c.program, &c.args, root, c.timeout_seconds) {
            Ok(x) => {
                let text = format!("{}\n{}", x.stdout, x.stderr);
                fs::write(output.join(format!("{}.log", c.name)), &text)?;
                let parsed = match c.parser.as_str() {
                    "go" => parse_go(&x.stdout),
                    "tap" => parse_tap(&x.stdout),
                    "rust" => parse_rust(&c.name, &x.stdout),
                    _ => Vec::new(),
                };
                if x.exit_code != Some(0) || x.timed_out {
                    errors.push(format!(
                        "{}: exit {:?}, timeout {}",
                        c.name, x.exit_code, x.timed_out
                    ));
                }
                if c.parser != "exit" && parsed.is_empty() {
                    errors.push(format!("{}: no test results parsed", c.name));
                }
                cases.extend(parsed);
                executions.push(json!({"name":c.name,"result":x}));
            }
            Err(e) => {
                errors.push(format!("{}: {e}", c.name));
                executions.push(json!({"name":c.name,"unavailable":e.to_string()}));
            }
        }
    }
    save(
        &output.join("tests.json"),
        &json!({"captured_unix":SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),"head":head.stdout.trim(),"working_tree":status.stdout,"cases":cases,"commands":executions,"gaps_or_failures":errors}),
    )?;
    if !errors.is_empty() {
        return Err(format!(
            "{} failed/unavailable commands; see tests.json",
            errors.len()
        )
        .into());
    }
    Ok(())
}
#[derive(Debug, Serialize, Deserialize)]
struct Finding {
    id: String,
    title: String,
    status: String,
    category: String,
    confidence: String,
    cause: String,
    evidence: Vec<String>,
    impact: String,
    correction: String,
    prevention: String,
    #[serde(default)]
    trigger: String,
    #[serde(default)]
    visuals: Vec<capture::Visual>,
}
fn candidate(id: String, title: String, evidence: Vec<String>, category: &str) -> Finding {
    Finding {
        trigger: String::new(),
        visuals: Vec::new(),
        id,
        title,
        status: "needs-review".into(),
        category: category.into(),
        confidence: "unknown".into(),
        cause: "Not diagnosed. Inspect source and evidence before assigning a root cause.".into(),
        evidence,
        impact: "Not established; distinguish a test harness error from product behavior.".into(),
        correction:
            "Review the evidence, reproduce in isolation, and record a supported correction.".into(),
        prevention: "Add a regression for the supported cause and retain before/after results."
            .into(),
    }
}
fn ci_lines(log: &str) -> String {
    log.lines()
        .map(|l| {
            let content = l.splitn(3, '\t').nth(2).unwrap_or(l);
            if content.as_bytes().get(4) == Some(&b'-') {
                content.split_once(' ').map_or(content, |(_, s)| s)
            } else {
                content
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}
fn triage(input: &Path, out: &Path) -> Result<()> {
    let mut findings = Vec::new();
    let mut coverage = Vec::new();
    let tests = input.join("tests.json");
    if tests.exists() {
        let v = read(&tests)?;
        let cases: Vec<TestCase> = serde_json::from_value(v["cases"].clone())?;
        coverage.push(format!(
            "{} parsed local test/package outcomes; snapshot {}",
            cases.len(),
            v["head"]
        ));
        for c in cases.iter().filter(|c| c.status == "fail") {
            findings.push(candidate(
                format!("LOCAL-{:03}", findings.len() + 1),
                format!("{}: {}", c.suite, c.name),
                vec![tests.display().to_string(), c.evidence.clone()],
                "test-failure",
            ));
        }
        for e in v["gaps_or_failures"].as_array().into_iter().flatten() {
            coverage.push(e.as_str().unwrap_or("").to_string());
        }
        for c in v["commands"].as_array().into_iter().flatten() {
            if !c["unavailable"].is_null()
                || c["result"]["exit_code"] != 0
                || c["result"]["timed_out"] == true
            {
                findings.push(candidate(
                    format!("COMMAND-{:03}", findings.len() + 1),
                    format!("Command {} failed or unavailable", c["name"]),
                    vec![c.to_string()],
                    "command-or-environment",
                ));
            }
        }
    }
    let mut paths = fs::read_dir(input)?
        .map(|e| e.map(|e| e.path()))
        .collect::<std::io::Result<Vec<_>>>()?;
    paths.sort();
    for path in paths {
        let name = path.file_name().unwrap().to_string_lossy();
        if !name.starts_with("run-") || !name.ends_with(".log") {
            continue;
        }
        let log = fs::read_to_string(&path)?;
        if log.trim().is_empty() {
            continue;
        }
        let clean = ci_lines(&log);
        let failed: Vec<_> = parse_tap(&clean)
            .into_iter()
            .filter(|c| c.status == "fail")
            .collect();
        let stem = name.trim_end_matches(".log");
        for (i, c) in failed.iter().enumerate() {
            findings.push(candidate(
                format!("{stem}-test-{}", i + 1),
                c.name.clone(),
                vec![path.display().to_string(), c.evidence.clone()],
                "historical-ci-test",
            ));
        }
        // Keep a run-level candidate too: one log may contain multiple failing jobs,
        // compiler failures or non-TAP tests. A reviewer deduplicates, never silently drops it.
        findings.push(candidate(
            format!("{stem}-review"),
            format!("Review failed steps in {stem}"),
            vec![
                path.display().to_string(),
                clean
                    .lines()
                    .filter(|l| {
                        l.contains("error")
                            || l.contains("FAIL")
                            || l.contains("not ok")
                            || l.contains("not found")
                    })
                    .take(60)
                    .collect::<Vec<_>>()
                    .join("\n"),
            ],
            "historical-ci-run",
        ));
    }
    let collection = input.join("collection.json");
    if collection.exists() {
        coverage.push(read(&collection)?.to_string());
    }
    if findings.is_empty() {
        coverage.push("No candidates extracted. This is not proof of a clean audit; verify collection and test coverage.".into());
    }
    save(
        out,
        &json!({"scope":"UNREVIEWED candidates: verify and deduplicate before treating these as diagnosed errors.","coverage":coverage,"findings":findings,"pr_review":[]}),
    )?;
    println!("Wrote {} candidates to {}", findings.len(), out.display());
    Ok(())
}
fn safe_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 100
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}
fn render_finding(doc: &mut pdf::Document, f: &Finding, base: &Path) -> Result<()> {
    doc.bookmark(&format!("{} - {}", f.id, f.title));
    doc.text(&f.id, 11.0, true);
    doc.title(&f.title);
    doc.text(
        &format!("{} | Cause confidence: {}", f.status, f.confidence),
        10.0,
        false,
    );
    if !f.trigger.is_empty() {
        doc.section("When this happens", &f.trigger);
    }
    doc.section("Why it happens", &f.cause);
    doc.section("What it affects", &f.impact);
    doc.section("How to correct it", &f.correction);
    doc.section("How to prevent it", &f.prevention);
    doc.text("Evidence references", 12.0, true);
    doc.text(&f.evidence.join("\n"), 9.0, false);
    if f.visuals.is_empty() {
        doc.text("No screenshot attached. Consult the evidence references; this finding has no captured visual reproduction.",10.0,false);
    }
    for visual in &f.visuals {
        doc.page();
        doc.bookmark(&format!("{} - visual evidence", f.id));
        doc.text(&format!("{} / {}", f.id, visual.kind), 12.0, true);
        doc.image(&base.join(&visual.path), &visual.caption)?;
    }
    Ok(())
}
fn report(input: &Path, out: &Path) -> Result<()> {
    let v = read(input)?;
    let findings: Vec<Finding> = serde_json::from_value(v["findings"].clone())?;
    let mut ids = std::collections::BTreeSet::new();
    let base = input.parent().unwrap_or(Path::new("."));
    for f in &findings {
        if !safe_id(&f.id) || !ids.insert(&f.id) {
            return Err("unsafe or duplicate finding ID".into());
        }
        for visual in &f.visuals {
            if visual.caption.trim().is_empty() || visual.kind.trim().is_empty() {
                return Err("visual evidence needs a caption and provenance kind".into());
            }
            let p = base.join(&visual.path);
            let (w, h) = image::image_dimensions(&p)?;
            if w == 0 || h == 0 || u64::from(w) * u64::from(h) > 20_000_000 {
                return Err("invalid screenshot dimensions".into());
            }
        }
    }
    fs::create_dir_all(out)?;
    let mut index = pdf::Document::new();
    index.bookmark("Start here");
    index.title(&format!(
        "{}\nError review",
        v["project"].as_str().unwrap_or("Errorpuller")
    ));
    index.text(
        v["scope"].as_str().unwrap_or("Evidence review"),
        12.0,
        false,
    );
    index.section("Read the status first","LOCAL entries describe failures observed in the recorded local checkout. CI entries explain historical failing builds. HISTORY entries describe documented changes. GAP entries identify missing coverage. A historical fix is not a claim about current production.");
    index.section("What the pictures mean","Screenshots capture local evidence documents containing source and log excerpts. Their captions identify a recorded local failure, historical CI output, documented change, or coverage gap. They are not photographs of live production failures. Use the PDF bookmarks to jump to a finding.");
    index.section("Coverage",&format!("{} reviewed entries. The complete report includes each explanation and its available visual evidence. The individual PDFs contain the same finding-specific material.",findings.len()));
    for s in v["coverage"].as_array().into_iter().flatten() {
        if let Some(t) = s.as_str() {
            index.text(t, 11.5, false);
        }
    }
    index.page();
    index.bookmark("Finding directory");
    index.title("Finding directory");
    for f in &findings {
        index.text(&format!("{} / {}", f.id, f.title), 12.0, true);
        index.text(&f.status, 10.0, false);
    }
    index.save(&out.join("errorpuller-index.pdf"))?;
    let mut full = index;
    for f in &findings {
        let mut individual = pdf::Document::new();
        render_finding(&mut individual, f, base)?;
        individual.save(&out.join(format!("{}.pdf", f.id)))?;
        full.page();
        render_finding(&mut full, f, base)?;
    }
    for (field, title) in [
        ("pr_review", "Pull request register"),
        ("test_review", "Local test register"),
    ] {
        if let Some(entries) = v[field].as_array() {
            full.page();
            full.bookmark(title);
            full.title(title);
            for entry in entries {
                full.text(entry.as_str().unwrap_or(""), 10.0, false);
            }
        }
    }
    full.save(&out.join("errorpuller-review.pdf"))?;
    save(&out.join("findings.json"), &v)?;
    println!(
        "Wrote {} individual reports, overview and complete illustrated review to {}",
        findings.len(),
        out.display()
    );
    Ok(())
}
fn main() {
    if let Err(e) = run() {
        eprintln!("errorpuller: {e}");
        std::process::exit(1);
    }
}
fn run() -> Result<()> {
    let a: Vec<String> = env::args().skip(1).collect();
    match a.first().map(String::as_str) {
        Some("collect") if a.len() == 4 => collect(&a[1], Path::new(&a[2]), Path::new(&a[3])),
        Some("test") if a.len() == 4 => test(Path::new(&a[1]), Path::new(&a[2]), Path::new(&a[3])),
        Some("report") if a.len() == 3 => report(Path::new(&a[1]), Path::new(&a[2])),
        Some("triage") if a.len() == 3 => triage(Path::new(&a[1]), Path::new(&a[2])),
        Some("capture") if a.len() == 4 => {
            capture::run(Path::new(&a[1]), Path::new(&a[2]), Path::new(&a[3]))
        }
        None | Some("--help") | Some("-h") => {
            println!("errorpuller {}\n  collect OWNER/REPO ROOT EVIDENCE_DIR\n  test ROOT COMMANDS.json EVIDENCE_DIR\n  triage EVIDENCE_DIR CANDIDATES.json\n  capture CAPTURES.json IMAGE_DIR CHROMIUM_PATH\n  report REVIEWED_FINDINGS.json PDF_DIR\n\nRead-only GitHub collection through authenticated gh. Test configs execute trusted local commands.\nReports require evidence-reviewed causes; automation must not invent diagnoses.", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        _ => Err("invalid arguments; use --help".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn go_failures_keep_test_output() {
        let s="{\"Action\":\"output\",\"Package\":\"p\",\"Test\":\"T\",\"Output\":\"bad assertion\"}\n{\"Action\":\"fail\",\"Package\":\"p\",\"Test\":\"T\"}\n{\"Action\":\"fail\",\"Package\":\"p\"}";
        let p = parse_go(s);
        assert_eq!(p.len(), 2);
        assert_eq!(p[0].evidence, "bad assertion");
        assert_eq!(p[1].name, "[package]");
    }
    #[test]
    fn tap_diagnostics_and_skips() {
        let p = parse_tap("not ok 1 - bad\n  error: nope\nok 2 - later # SKIP\n# fail 1");
        assert_eq!(p.len(), 2);
        assert_eq!(p[0].status, "fail");
        assert!(p[0].evidence.contains("nope"));
        assert_eq!(p[1].status, "skip");
    }
    #[test]
    fn rust_ignored_not_counted_as_pass() {
        let p=parse_rust("core","test a ... ok\ntest b ... FAILED\ntest c ... ignored, device required\ntest result: FAILED");
        assert_eq!(p.len(), 3);
        assert_eq!(p[2].status, "skip");
    }
    #[test]
    fn pagination_flattens_every_page() {
        assert_eq!(flatten(&json!([[1, 2], [3]]), None).len(), 3);
        assert_eq!(
            flatten(&json!([{"jobs":[1]},{"jobs":[2]}]), Some("jobs")).len(),
            2
        );
    }
    #[test]
    fn ci_prefix_removed_before_tap_parse() {
        let s=ci_lines("test\tstep\t2026-10-04T01:00:00Z not ok 1 - bad\ntest\tstep\t2026-10-04T01:00:00Z   error: nope");
        let p = parse_tap(&s);
        assert_eq!(p.len(), 1);
        assert!(p[0].evidence.contains("nope"));
    }
    #[test]
    fn unsafe_report_paths_rejected() {
        for id in ["../bad", "x/y", "", "a.pdf", "a\\b"] {
            assert!(!safe_id(id));
        }
        assert!(safe_id("HMNP-001"));
    }
}
