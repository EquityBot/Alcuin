use alcuin::perf;
use serde_json::{Value, json};
use std::{fs, path::PathBuf, process::Command};
fn events(duration: f64) -> Vec<Value> {
    perf::PHASES.iter().map(|p|json!({"name":format!("consultation.{p}"),"data":{"durationMs":duration,"turnIndex":0,"ok":1}})).collect()
}
fn write(root: &std::path::Path, index: usize, trace: Value) -> PathBuf {
    let path = root.join(format!("PUBLIC-{index}.json"));
    fs::write(&path, serde_json::to_vec(&trace).unwrap()).unwrap();
    path
}
#[test]
fn summaries_require_completed_ordered_traces_and_keep_independent_detail_totals() {
    let root = tempfile::tempdir().unwrap();
    let mut detailed = events(10.0);
    for (phase, duration, ok) in [
        ("send-acknowledgement", 2.0, 1),
        ("send-acknowledgement", 3.0, 1),
        ("answer-waiting", 5.0, 0),
    ] {
        detailed.push(json!({"name":format!("consultation-detail.{phase}"),"data":{"detail":1,"durationMs":duration,"turnIndex":0,"ok":ok}}));
    }
    let mut incomplete = events(999.0);
    incomplete.push(
        json!({"name":"consultation.generation","data":{"durationMs":1,"turnIndex":1,"ok":1}}),
    );
    let files = vec![
        write(
            root.path(),
            0,
            json!({"events":detailed,"private":"SYNTHETIC_PRIVATE_SENTINEL"}),
        ),
        write(root.path(), 1, json!({"events":events(20.0)})),
        write(
            root.path(),
            2,
            json!({"events":events(100.0),"complete":false}),
        ),
        write(root.path(), 3, json!({"events":incomplete})),
    ];
    let summary = perf::summarise(&files).unwrap();
    assert_eq!(summary.completed_files, 2);
    assert_eq!(summary.incomplete_files, 2);
    assert_eq!(summary.phases[0].median_ms, Some(15.0));
    assert_eq!(summary.phases[0].p95_ms, Some(20.0));
    assert!(summary.p95_low_sample);
    let acknowledgement = summary
        .details
        .iter()
        .find(|r| r.phase == "send-acknowledgement")
        .unwrap();
    assert_eq!(acknowledgement.n, 1);
    assert_eq!(acknowledgement.median_ms, Some(5.0));
    assert_eq!(
        summary
            .details
            .iter()
            .find(|r| r.phase == "answer-waiting")
            .unwrap()
            .n,
        0
    );
    let rendered = perf::format_summary(&summary);
    assert!(rendered.contains("must not be added"));
    assert!(!rendered.contains("SYNTHETIC_PRIVATE_SENTINEL"));
    assert!(
        !serde_json::to_string(&summary)
            .unwrap()
            .contains("SYNTHETIC_PRIVATE_SENTINEL")
    );
}
#[test]
fn trace_failure_errors_do_not_expose_paths_or_untrusted_metadata() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("SYNTHETIC_PRIVATE_SENTINEL.json");
    for raw in [
        "{SYNTHETIC_PRIVATE_SENTINEL",
        r#"{"events":[]}"#,
        r#"{"events":[{"name":"consultation.SYNTHETIC_PRIVATE_SENTINEL"}]}"#,
        r#"{"events":[{"name":"consultation.queue","data":{"durationMs":true,"turnIndex":0,"ok":1}}]}"#,
    ] {
        fs::write(&path, raw).unwrap();
        let error = perf::summarise(std::slice::from_ref(&path))
            .unwrap_err()
            .to_string();
        assert!(error.starts_with("perf-summary input 1: "));
        assert!(!error.contains("SYNTHETIC_PRIVATE_SENTINEL"));
    }
    fs::write(&path, vec![b' '; 2 * 1024 * 1024 + 1]).unwrap();
    assert_eq!(
        perf::summarise(std::slice::from_ref(&path))
            .unwrap_err()
            .to_string(),
        "perf-summary input 1: file exceeds 2 MiB."
    );
    assert!(perf::summarise(&[]).is_err());
}
#[test]
fn percentile_uses_nearest_rank_and_rejects_duplicate_or_reordered_input() {
    let root = tempfile::tempdir().unwrap();
    let files = (0..20)
        .map(|i| write(root.path(), i, json!({"events":events(i as f64+1.0)})))
        .collect::<Vec<_>>();
    let summary = perf::summarise(&files).unwrap();
    assert_eq!(summary.phases[0].median_ms, Some(10.5));
    assert_eq!(summary.phases[0].p95_ms, Some(19.0));
    assert!(!summary.p95_low_sample);
    assert_eq!(
        perf::summarise(&[files[0].clone(), files[0].clone()])
            .unwrap_err()
            .to_string(),
        "perf-summary input 2: duplicate file."
    );
    let mut reordered = events(10.0);
    reordered.swap(0, 5);
    let path = write(root.path(), 21, json!({"events":reordered}));
    assert_eq!(perf::summarise(&[path]).unwrap().completed_files, 0);
}
#[test]
fn numeric_evidence_and_overflow_fail_the_whole_invocation() {
    let root = tempfile::tempdir().unwrap();
    for invalid in [
        json!({"durationMs":-1}),
        json!({"turnIndex":0.5}),
        json!({"turnIndex":9007199254740992u64}),
        json!({"ok":true}),
        json!({"ok":2}),
    ] {
        let mut records = events(1.0);
        for (key, value) in invalid.as_object().unwrap() {
            records[0]["data"][key] = value.clone();
        }
        let path = write(root.path(), 1, json!({"events":records}));
        assert_eq!(
            perf::summarise(&[path]).unwrap_err().to_string(),
            "perf-summary input 1: invalid phase timing."
        );
    }
    let mut records = events(f64::MAX);
    records.extend(events(f64::MAX));
    let path = write(root.path(), 2, json!({"events":records}));
    assert_eq!(
        perf::summarise(&[path]).unwrap_err().to_string(),
        "perf-summary input 1: phase total overflow."
    );
}
#[test]
fn actual_cli_perf_summary_json_and_failure_output_match_the_read_contract() {
    let root = tempfile::tempdir().unwrap();
    let path = write(
        root.path(),
        1,
        json!({"events":events(12.0),"private":"SYNTHETIC_PRIVATE_SENTINEL"}),
    );
    let run = |path: &std::path::Path| {
        Command::new(env!("CARGO_BIN_EXE_alcuin"))
            .env_clear()
            .env("ORACLE_DISABLE_NOTIFICATIONS", "1")
            .env("HOME", root.path())
            .env("USERPROFILE", root.path())
            .env("ORACLE_HOME_DIR", root.path())
            .current_dir(root.path())
            .arg("perf-summary")
            .arg(path)
            .arg("--json")
            .output()
            .unwrap()
    };
    let output = run(&path);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let summary: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(summary["completedFiles"], 1);
    assert!(output.stderr.is_empty());
    assert!(!String::from_utf8_lossy(&output.stdout).contains("SYNTHETIC_PRIVATE_SENTINEL"));
    let output = run(&root.path().join("SYNTHETIC_PRIVATE_SENTINEL-missing"));
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(!String::from_utf8_lossy(&output.stderr).contains("SYNTHETIC_PRIVATE_SENTINEL"));
}

#[test]
fn valid_json_with_a_nonfinite_numeric_trace_is_rejected_as_timing_evidence() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("overflow.json");
    fs::write(&path,r#"{"events":[{"name":"consultation.queue","data":{"durationMs":1e400,"turnIndex":0,"ok":1}}]}"#).unwrap();
    assert_eq!(
        perf::summarise(&[path]).unwrap_err().to_string(),
        "perf-summary input 1: invalid phase timing."
    );
}

#[test]
fn cli_perf_trace_writes_sanitised_complete_traces_and_skips_perf_summary() {
    let root = tempfile::tempdir().unwrap();
    let run = |args: &[&str], env: &[(&str, &str)]| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_alcuin"));
        command
            .env_clear()
            .env("ORACLE_DISABLE_NOTIFICATIONS", "1")
            .env("HOME", root.path())
            .env("USERPROFILE", root.path())
            .env("ORACLE_HOME_DIR", root.path())
            .current_dir(root.path())
            .args(args);
        for (key, value) in env {
            command.env(key, value);
        }
        command.output().unwrap()
    };
    let read = |name: &str| -> Value {
        serde_json::from_slice(&fs::read(root.path().join(name)).unwrap()).unwrap()
    };
    let output = run(&["--perf-trace=PUBLIC-status.json", "status"], &[]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let trace = read("PUBLIC-status.json");
    assert_eq!(trace["complete"], true);
    let marks = trace["events"].as_array().unwrap();
    assert!(
        marks
            .iter()
            .any(|e| e["name"] == "pre-action" && e["data"]["command"] == "status")
    );
    assert!(
        marks
            .iter()
            .any(|e| e["name"] == "exit" && e["data"]["exitCode"] == 0)
    );
    let output = run(
        &[
            "-p",
            "PUBLIC private prompt",
            "--perf-trace-path",
            "PUBLIC-preview.json",
            "--dry-run",
        ],
        &[],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let trace = read("PUBLIC-preview.json");
    assert!(!trace.to_string().contains("PUBLIC private prompt"));
    assert!(
        trace["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["name"] == "root-command-start")
    );
    let output = run(&["status"], &[("ORACLE_PERF_TRACE", "PUBLIC-env.json")]);
    assert!(output.status.success());
    assert_eq!(read("PUBLIC-env.json")["complete"], true);
    let summary = write(root.path(), 9, json!({"events":events(1.0)}));
    let output = run(
        &[
            "--perf-trace=PUBLIC-suppressed.json",
            "perf-summary",
            summary.to_str().unwrap(),
        ],
        &[],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!root.path().join("PUBLIC-suppressed.json").exists());
    let output = run(&["--perf-trace-path", "--wait"], &[]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("error: option '--perf-trace-path <path>' argument missing")
    );
}
#[test]
fn summary_json_and_table_use_the_reference_order_and_number_formats() {
    let root = tempfile::tempdir().unwrap();
    let files = [
        write(root.path(), 0, json!({"events":events(1200.0)})),
        write(root.path(), 1, json!({"events":events(0.0625)})),
    ];
    let summary = perf::summarise(&files[..1]).unwrap();
    let text = serde_json::to_string(&summary).unwrap();
    assert!(
        text.starts_with(r#"{"selectedFiles":1,"completedFiles":1,"incompleteFiles":0,"p95LowSample":true,"details":["#),
        "{text}"
    );
    assert!(
        text.contains(r#""n":1,"medianMs":1200,"p95Ms":1200}"#),
        "{text}"
    );
    assert!(text.find(r#""details""#) < text.find(r#""phases""#));
    let rendered = perf::format_summary(&perf::summarise(&files[1..]).unwrap());
    assert!(rendered.contains("\t1\t0.063\t0.063"), "{rendered}");
}
