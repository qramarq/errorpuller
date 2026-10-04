use serde_json::{json, Value};
use std::{
    fs,
    path::PathBuf,
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        let id = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = std::env::temp_dir().join(format!("errorpuller-test-{}-{id}", std::process::id()));
        fs::create_dir(&p).unwrap();
        Self(p)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_errorpuller")
}

#[test]
fn failing_and_missing_commands_are_recorded_and_do_not_stop_later_commands() {
    let s = Scratch::new();
    let config = s.0.join("commands.json");
    fs::write(&config,serde_json::to_vec(&json!([
        {"name":"failure","program":binary(),"args":["invalid"],"parser":"exit"},
        {"name":"missing","program":"errorpuller-nonexistent-tool-179112","args":[],"parser":"exit"},
        {"name":"success","program":binary(),"args":["--help"],"parser":"exit"}
    ])).unwrap()).unwrap();
    let out = s.0.join("evidence");
    let result = Command::new(binary())
        .arg("test")
        .arg(env!("CARGO_MANIFEST_DIR"))
        .arg(&config)
        .arg(&out)
        .output()
        .unwrap();
    assert!(!result.status.success());
    let v: Value = serde_json::from_slice(&fs::read(out.join("tests.json")).unwrap()).unwrap();
    assert_eq!(v["commands"].as_array().unwrap().len(), 3);
    assert_eq!(v["commands"][2]["result"]["exit_code"], 0);
    assert_eq!(v["gaps_or_failures"].as_array().unwrap().len(), 2);
}

#[test]
fn empty_test_discovery_is_not_a_pass() {
    let s = Scratch::new();
    let config = s.0.join("commands.json");
    fs::write(
        &config,
        serde_json::to_vec(&json!([
            {"name":"empty","program":binary(),"args":["--help"],"parser":"tap"}
        ]))
        .unwrap(),
    )
    .unwrap();
    let out = s.0.join("evidence");
    let result = Command::new(binary())
        .arg("test")
        .arg(env!("CARGO_MANIFEST_DIR"))
        .arg(config)
        .arg(&out)
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(fs::read_to_string(out.join("tests.json"))
        .unwrap()
        .contains("no test results parsed"));
}

#[test]
fn report_rejects_path_traversal_before_creating_any_outputs() {
    let s = Scratch::new();
    let input = s.0.join("findings.json");
    let out = s.0.join("pdf");
    fs::write(
        &input,
        serde_json::to_vec(&json!({"project":"Portable demo","findings":[{
            "id":"../escape","title":"x","status":"x","category":"x","confidence":"unknown",
            "cause":"x","evidence":[],"impact":"x","correction":"x","prevention":"x"
        }]}))
        .unwrap(),
    )
    .unwrap();
    let result = Command::new(binary())
        .arg("report")
        .arg(input)
        .arg(&out)
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(!out.exists());
    assert!(!s.0.join("escape.pdf").exists());
}

#[test]
fn illustrated_report_requires_image_then_embeds_it_with_bookmarks() {
    let s = Scratch::new();
    let input = s.0.join("findings.json");
    let out = s.0.join("pdf");
    fs::write(&input, serde_json::to_vec(&json!({"project":"Portable demo","findings":[{
        "id":"LOCAL-001","title":"Storage failure","status":"recorded local",
        "category":"storage","confidence":"confirmed","cause":"Injected rejection",
        "evidence":["recorded test"],"impact":"Save fails","correction":"Handle rejection",
        "prevention":"Regression test","trigger":"Submit settings",
        "visuals":[{"path":"evidence.png","caption":"Recorded source/log evidence","kind":"local test"}]
    }]})).unwrap()).unwrap();
    let run = || {
        Command::new(binary())
            .arg("report")
            .arg(&input)
            .arg(&out)
            .output()
            .unwrap()
    };
    assert!(!run().status.success());
    assert!(
        !out.exists(),
        "missing image must fail before outputs are created"
    );
    image::RgbImage::from_pixel(800, 900, image::Rgb([240, 245, 250]))
        .save(s.0.join("evidence.png"))
        .unwrap();
    let result = run();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let bytes = fs::read(out.join("errorpuller-review.pdf")).unwrap();
    let pdf = String::from_utf8_lossy(&bytes);
    assert!(pdf.contains("/Subtype /Image"));
    assert!(pdf.contains("/Outlines"));
    assert!(pdf.contains("Submit settings"));
    assert!(!pdf.contains("HMNPblock"));
    assert!(pdf.contains("Portable demo"));
    assert!(out.join("LOCAL-001.pdf").exists());
    assert!(out.join("errorpuller-index.pdf").exists());
}
