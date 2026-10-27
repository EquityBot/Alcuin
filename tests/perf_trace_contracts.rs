//! The process-wide trace in its own test binary: no other test can add marks to it.
use industrious::perf;

#[test]
fn browser_phase_marks_produce_a_trace_that_perf_summary_completes() {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join("PUBLIC-trace.json");
    perf::start(
        Some(path.to_string_lossy().into_owned()),
        &[
            "-p".into(),
            "PUBLIC secret".into(),
            "--engine".into(),
            "browser".into(),
        ],
        home.path(),
    );
    perf::pre_action("root");
    perf::mark("root-command-start", None);
    perf::phase("setup", Some(0));
    perf::phase("queue", Some(0));
    perf::phase("setup", None);
    perf::detail("composer-readiness", Some(0));
    for turn in 0..2u64 {
        perf::phase("mode-selection", Some(turn));
        perf::phase("upload", None);
        perf::detail("composer-population", Some(turn));
        perf::phase("generation", None);
        perf::detail("send-readiness-click", Some(turn));
        perf::detail("send-acknowledgement", Some(turn));
        perf::detail("answer-waiting", Some(turn));
        perf::phase("capture", None);
    }
    perf::detail("output-metadata-persistence", None);
    perf::phase_end(1);
    perf::flush(0);
    let trace: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(trace["complete"], true);
    assert_eq!(
        trace["argv"],
        serde_json::json!(["-p", "[redacted]", "--engine", "browser"])
    );
    let names: Vec<_> = trace["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|event| event["name"].as_str().unwrap().to_owned())
        .collect();
    for expected in [
        "cli-module-ready",
        "pre-action",
        "root-command-start",
        "exit",
    ] {
        assert!(names.contains(&expected.to_owned()), "{names:?}");
    }
    let summary = perf::summarise(&[path]).unwrap();
    assert_eq!(summary.completed_files, 1, "{names:?}");
    assert!(summary.phases.iter().all(|row| row.n == 1));
    assert!(summary.details.iter().filter(|row| row.n == 1).count() >= 6);
    assert!(
        !String::from_utf8_lossy(&std::fs::read(home.path().join("PUBLIC-trace.json")).unwrap())
            .contains("PUBLIC secret")
    );
}
