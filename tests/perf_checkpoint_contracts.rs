//! The process-wide trace in its own test binary: no other test can add marks to it.
use industrious::perf;

fn names(path: &std::path::Path) -> Vec<String> {
    let trace: serde_json::Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    trace["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|event| event["name"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn a_mark_soon_after_a_checkpoint_is_written_within_the_second() {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join("PUBLIC-checkpoint.json");
    perf::start(Some(path.to_string_lossy().into_owned()), &[], home.path());
    perf::pre_action("root");
    assert!(names(&path).contains(&"pre-action".to_owned()));
    perf::mark("PUBLIC-late-mark", None);
    assert!(!names(&path).contains(&"PUBLIC-late-mark".to_owned()));
    std::thread::sleep(std::time::Duration::from_millis(1500));
    assert!(names(&path).contains(&"PUBLIC-late-mark".to_owned()));
    perf::flush(0);
    perf::mark("PUBLIC-after-exit", None);
    std::thread::sleep(std::time::Duration::from_millis(1200));
    let trace: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(trace["complete"], true);
    assert!(!names(&path).contains(&"PUBLIC-after-exit".to_owned()));
}
