//! Explicit, bounded trace reads with summaries that omit private input metadata.
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeSet,
    fs,
    io::Read,
    path::{Path, PathBuf},
};
const MAX_BYTES: u64 = 2 * 1024 * 1024;
pub const PHASES: [&str; 6] = [
    "queue",
    "setup",
    "mode-selection",
    "upload",
    "generation",
    "capture",
];
pub const DETAILS: [&str; 7] = [
    "composer-readiness",
    "attachment-transfer",
    "composer-population",
    "send-readiness-click",
    "send-acknowledgement",
    "answer-waiting",
    "output-metadata-persistence",
];
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TimingSummary {
    pub phase: String,
    pub n: usize,
    #[serde(serialize_with = "js_number")]
    pub median_ms: Option<f64>,
    #[serde(serialize_with = "js_number")]
    pub p95_ms: Option<f64>,
}
/// JSON.stringify number output: integral values carry no fraction.
fn js_number<S: serde::Serializer>(value: &Option<f64>, serializer: S) -> Result<S::Ok, S::Error> {
    match value {
        Some(number) if number.fract() == 0.0 && number.abs() < 9_007_199_254_740_992.0 => {
            serializer.serialize_i64(*number as i64)
        }
        Some(number) => serializer.serialize_f64(*number),
        None => serializer.serialize_none(),
    }
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PerfSummary {
    pub selected_files: usize,
    pub completed_files: usize,
    pub incomplete_files: usize,
    pub p95_low_sample: bool,
    // Reference key order: details before phases.
    pub details: Vec<TimingSummary>,
    pub phases: Vec<TimingSummary>,
}
fn failure(index: usize, category: &str) -> anyhow::Error {
    anyhow::anyhow!("perf-summary input {index}: {category}.")
}
fn read_trace(path: &Path, index: usize, seen: &mut BTreeSet<PathBuf>) -> Result<Value> {
    let canonical = fs::canonicalize(path).map_err(|_| failure(index, "unreadable file"))?;
    #[cfg(windows)]
    let key = PathBuf::from(canonical.to_string_lossy().to_lowercase());
    #[cfg(not(windows))]
    let key = canonical.clone();
    if !seen.insert(key) {
        return Err(failure(index, "duplicate file"));
    }
    let metadata = fs::metadata(&canonical).map_err(|_| failure(index, "unreadable file"))?;
    if !metadata.is_file() {
        return Err(failure(index, "regular file required"));
    }
    let file = fs::File::open(canonical).map_err(|_| failure(index, "unreadable file"))?;
    let metadata = file
        .metadata()
        .map_err(|_| failure(index, "unreadable file"))?;
    if !metadata.is_file() {
        return Err(failure(index, "regular file required"));
    }
    if metadata.len() > MAX_BYTES {
        return Err(failure(index, "file exceeds 2 MiB"));
    }
    let mut bytes = vec![];
    file.take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| failure(index, "unreadable file"))?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(failure(index, "file exceeds 2 MiB"));
    }
    serde_json::from_slice(&bytes).map_err(|_| failure(index, "invalid JSON"))
}
fn turn(value: &Value) -> Option<u64> {
    value
        .as_f64()
        .filter(|n| n.is_finite() && *n >= 0.0 && *n <= 9007199254740991.0 && n.fract() == 0.0)
        .map(|n| n as u64)
}
fn duration(value: &Value) -> Option<f64> {
    value.as_f64().filter(|n| n.is_finite() && *n >= 0.0)
}
fn rows<const N: usize>(names: [&str; N], samples: [Vec<f64>; N]) -> Vec<TimingSummary> {
    names
        .into_iter()
        .zip(samples)
        .map(|(phase, mut values)| {
            values.sort_by(f64::total_cmp);
            let n = values.len();
            let (median_ms, p95_ms) = if n == 0 {
                (None, None)
            } else {
                let lower = values[(n - 1) / 2];
                let upper = values[n / 2];
                (
                    Some(lower + (upper - lower) / 2.0),
                    Some(values[(0.95 * n as f64).ceil() as usize - 1]),
                )
            };
            TimingSummary {
                phase: phase.into(),
                n,
                median_ms,
                p95_ms,
            }
        })
        .collect()
}
pub fn summarise(filenames: &[PathBuf]) -> Result<PerfSummary> {
    if filenames.is_empty() {
        bail!("perf-summary requires explicit trace files.");
    }
    let mut seen = BTreeSet::new();
    let mut completed_files = 0;
    let mut samples: [Vec<f64>; 6] = std::array::from_fn(|_| vec![]);
    let mut details: [Vec<f64>; 7] = std::array::from_fn(|_| vec![]);
    for (offset, path) in filenames.iter().enumerate() {
        let index = offset + 1;
        let trace = read_trace(path, index, &mut seen)?;
        let events = trace
            .as_object()
            .and_then(|_| trace["events"].as_array())
            .ok_or_else(|| failure(index, "events array required"))?;
        let mut totals: [Option<f64>; 6] = [None; 6];
        let mut detail_totals: [Option<f64>; 7] = [None; 7];
        let mut detail_failed = [false; 7];
        let (mut successful, mut ordered, mut turn_index, mut stage, mut last_phase) =
            (true, true, 0, 0, None);
        for event in events {
            let Some(name) = event.as_object().and_then(|_| event["name"].as_str()) else {
                continue;
            };
            if let Some(phase) = name.strip_prefix("consultation-detail.") {
                let p = DETAILS
                    .iter()
                    .position(|n| *n == phase)
                    .ok_or_else(|| failure(index, "unsupported consultation detail"))?;
                let data = &event["data"];
                let valid = data.is_object()
                    && data["detail"].as_f64() == Some(1.0)
                    && duration(&data["durationMs"]).is_some()
                    && matches!(data["ok"].as_f64(), Some(0.0 | 1.0))
                    && if p == 6 {
                        data.get("turnIndex").is_none()
                    } else {
                        turn(&data["turnIndex"]).is_some()
                    };
                if !valid {
                    return Err(failure(index, "invalid detail timing"));
                }
                detail_failed[p] |= data["ok"].as_f64() == Some(0.0);
                let total =
                    detail_totals[p].unwrap_or(0.0) + duration(&data["durationMs"]).unwrap();
                if !total.is_finite() {
                    return Err(failure(index, "detail total overflow"));
                }
                detail_totals[p] = Some(total);
                continue;
            }
            let Some(phase) = name.strip_prefix("consultation.") else {
                continue;
            };
            let p = PHASES
                .iter()
                .position(|n| *n == phase)
                .ok_or_else(|| failure(index, "unsupported consultation phase"))?;
            let data = &event["data"];
            let (Some(duration), Some(next_turn)) =
                (duration(&data["durationMs"]), turn(&data["turnIndex"]))
            else {
                return Err(failure(index, "invalid phase timing"));
            };
            if !data.is_object() || !matches!(data["ok"].as_f64(), Some(0.0 | 1.0)) {
                return Err(failure(index, "invalid phase timing"));
            }
            if next_turn != turn_index {
                if next_turn != turn_index + 1 || stage != 4 {
                    ordered = false;
                }
                turn_index = next_turn;
                stage = 0;
            }
            match p {
                0 | 1 => {
                    if turn_index != 0 || stage != 0 {
                        ordered = false;
                    }
                }
                2 => {
                    if stage == 4 {
                        ordered = false;
                    }
                    stage = 1;
                }
                3 => {
                    if stage < 1 || stage == 4 {
                        ordered = false;
                    }
                    stage = 2;
                }
                4 => {
                    if stage < 2 || stage == 4 {
                        ordered = false;
                    }
                    stage = 3;
                }
                _ => {
                    if stage != 3 && stage != 4 {
                        ordered = false;
                    }
                    stage = 4;
                }
            }
            let total = totals[p].unwrap_or(0.0) + duration;
            if !total.is_finite() {
                return Err(failure(index, "phase total overflow"));
            }
            totals[p] = Some(total);
            successful &= data["ok"].as_f64() == Some(1.0);
            last_phase = Some(p);
        }
        if totals.iter().all(Option::is_none) {
            return Err(failure(index, "no consultation phases"));
        }
        if trace["complete"] != false
            && ordered
            && successful
            && totals.iter().all(Option::is_some)
            && last_phase == Some(5)
        {
            completed_files += 1;
            for (p, total) in totals.into_iter().enumerate() {
                samples[p].push(total.unwrap());
            }
            for (p, total) in detail_totals.into_iter().enumerate() {
                if let Some(total) = total.filter(|_| !detail_failed[p]) {
                    details[p].push(total);
                }
            }
        }
    }
    Ok(PerfSummary {
        selected_files: filenames.len(),
        completed_files,
        incomplete_files: filenames.len() - completed_files,
        p95_low_sample: completed_files < 20,
        details: rows(DETAILS, details),
        phases: rows(PHASES, samples),
    })
}
pub fn format_summary(summary: &PerfSummary) -> String {
    let mut lines = vec![
        format!(
            "Selected files: {}; completed: {}; incomplete: {}.",
            summary.selected_files, summary.completed_files, summary.incomplete_files
        ),
        "Phase totals across each completed trace file (ms):".into(),
        "phase\tn\tmedian\tp95".into(),
    ];
    let timing = |v: Option<f64>| {
        v.map(|v| crate::cli_execution::to_fixed(v, 3))
            .unwrap_or_else(|| "n/a".into())
    };
    for row in &summary.phases {
        lines.push(format!(
            "{}\t{}\t{}\t{}",
            row.phase,
            row.n,
            timing(row.median_ms),
            timing(row.p95_ms)
        ));
    }
    lines.extend(["Available successful detail totals from completed traces (ms); any failed segment excludes that detail in that trace:".into(),"detail\tn\tmedian\tp95".into()]);
    for row in &summary.details {
        lines.push(format!(
            "{}\t{}\t{}\t{}",
            row.phase,
            row.n,
            timing(row.median_ms),
            timing(row.p95_ms)
        ));
    }
    lines.extend(["Detail and coarse totals overlap and must not be added. Generation includes dispatch readiness, acknowledgement and answer waiting; it is not backend thinking.".into(),"Descriptive statistics for the selected files only. Detail p95 is the sample maximum when its n is below 20.".into()]);
    if summary.p95_low_sample && summary.completed_files > 0 {
        lines.push("Low sample: fewer than 20 completed runs; p95 is the sample maximum.".into());
    }
    lines.join("\n")
}

// Trace writing (reference src/cli/perfTrace.ts). Diagnostics never change a consultation:
// every write is best effort, and a process holds at most one trace.
const SECRET_VALUE_FLAGS: [&str; 9] = [
    "--api-key",
    "--browser-follow-up",
    "--browser-inline-cookies",
    "--browser-inline-cookies-file",
    "--message",
    "--prompt",
    "--remote-token",
    "--token",
    "-p",
];
const VALUE_FLAGS: [&str; 50] = [
    "--aspect",
    "--azure-api-version",
    "--azure-deployment",
    "--azure-endpoint",
    "--base-url",
    "--browser-archive",
    "--browser-attachments",
    "--browser-auto-reattach-delay",
    "--browser-auto-reattach-interval",
    "--browser-auto-reattach-timeout",
    "--browser-bundle-format",
    "--browser-cookie-names",
    "--browser-cookie-path",
    "--browser-cookie-wait",
    "--browser-input-timeout",
    "--browser-max-concurrent-tabs",
    "--browser-model-strategy",
    "--browser-port",
    "--browser-profile-lock-timeout",
    "--browser-recheck-delay",
    "--browser-recheck-timeout",
    "--browser-research",
    "--browser-reuse-wait",
    "--browser-tab",
    "--browser-timeout",
    "--browser-url",
    "--chatgpt-url",
    "--engine",
    "--followup",
    "--followup-model",
    "--heartbeat",
    "--http-timeout",
    "--max-file-size-bytes",
    "--model",
    "--models",
    "--output",
    "--partial",
    "--perf-trace-path",
    "--provider",
    "--request",
    "--remote-chrome",
    "--remote-host",
    "--slug",
    "--timeout",
    "--write-output",
    "--youtube",
    "--zombie-timeout",
    "-e",
    "-m",
    "-s",
];
fn value_flag(flag: &str) -> bool {
    SECRET_VALUE_FLAGS.contains(&flag) || VALUE_FLAGS.contains(&flag)
}
/// Reference normalizePerfTraceArgs: `--perf-trace[=path]` and `--perf-trace-path` become the
/// two parser options. Returns the arguments, the trace value (`"1"` or a path) and any
/// argument error.
pub fn normalise_args(args: &[String]) -> (Vec<String>, Option<String>, Option<String>) {
    const MISSING: &str = "option '--perf-trace-path <path>' argument missing";
    let mut normalised = vec![];
    let mut value: Option<String> = None;
    let mut skip_next = false;
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        index += 1;
        if skip_next {
            normalised.push(arg.clone());
            skip_next = false;
            continue;
        }
        if arg == "--" {
            normalised.extend(args[index - 1..].iter().cloned());
            break;
        }
        if let Some(path) = arg.strip_prefix("--perf-trace=") {
            normalised.push("--perf-trace".into());
            if path.is_empty() {
                value = Some("1".into());
            } else {
                normalised.extend(["--perf-trace-path".into(), path.into()]);
                value = Some(path.into());
            }
            continue;
        }
        if arg == "--perf-trace-path" {
            match args.get(index) {
                Some(path) if !path.starts_with('-') => {
                    normalised.extend([arg.clone(), path.clone()]);
                    value = Some(path.clone());
                    index += 1;
                    continue;
                }
                _ => return (normalised, value, Some(MISSING.into())),
            }
        }
        if let Some(path) = arg.strip_prefix("--perf-trace-path=") {
            if path.is_empty() {
                return (normalised, value, Some(MISSING.into()));
            }
            value = Some(path.into());
        } else if arg == "--perf-trace" {
            value.get_or_insert_with(|| "1".into());
        }
        normalised.push(arg.clone());
        skip_next = !arg.contains('=') && value_flag(arg);
    }
    (normalised, value, None)
}
fn redact(value: &str) -> String {
    use regex::Regex;
    use std::sync::LazyLock;
    static RULES: LazyLock<Vec<(Regex, &str)>> = LazyLock::new(|| {
        [
            (r"(?i)\bBearer\s+[A-Za-z0-9._\-+/=]+", "Bearer [redacted]"),
            (r"://([^:/?#\s]+):([^@/?#\s]+)@", "://$1:[redacted]@"),
            (r"(?i)([?&](?:access_)?token=)[^&#\s]+", "${1}[redacted]"),
            (
                r"(?i)([?&](?:api[-_]?key|auth|authorization|password|secret)=)[^&#\s]+",
                "${1}[redacted]",
            ),
            (r"\bsk-(?:ant-|or-)?[A-Za-z0-9_-]{8,}\b", "sk-...[redacted]"),
            (r"\bxai-[A-Za-z0-9_-]{8,}\b", "xai-...[redacted]"),
            (r"\bAIza[0-9A-Za-z_-]{8,}\b", "AIza...[redacted]"),
        ]
        .into_iter()
        .map(|(pattern, replacement)| (Regex::new(pattern).expect("static pattern"), replacement))
        .collect()
    });
    let mut value = value.to_owned();
    for (pattern, replacement) in RULES.iter() {
        value = pattern.replace_all(&value, *replacement).into_owned();
    }
    value
}
/// Reference sanitizeTraceArgv: prompts, tokens and positionals never reach a trace.
pub fn sanitise_argv(args: &[String]) -> Vec<String> {
    let mut output = vec![];
    let (mut redact_next, mut value_next, mut positional) = (false, false, false);
    for arg in args {
        if positional {
            output.push("[redacted-positional]".into());
        } else if arg == "--" {
            output.push(arg.clone());
            positional = true;
        } else if redact_next {
            output.push("[redacted]".into());
            redact_next = false;
        } else if value_next {
            output.push(redact(arg));
            value_next = false;
        } else if let Some((flag, value)) = arg
            .split_once('=')
            .filter(|_| !(arg.starts_with("-p") && !arg.starts_with("--")))
        {
            output.push(if SECRET_VALUE_FLAGS.contains(&flag) {
                format!("{flag}=[redacted]")
            } else {
                format!("{flag}={}", redact(value))
            });
        } else if arg.starts_with("-p") && arg.len() > 2 {
            output.push("-p[redacted]".into());
        } else if SECRET_VALUE_FLAGS.contains(&arg.as_str()) {
            output.push(arg.clone());
            redact_next = true;
        } else if value_flag(arg) {
            output.push(arg.clone());
            value_next = true;
        } else if !arg.starts_with('-') {
            output.push("[redacted-positional]".into());
        } else {
            output.push(arg.clone());
        }
    }
    output
}
/// Reference deriveDetachedPerfTraceEnv: a detached worker writes beside the caller's trace,
/// with the session ID before the extension.
fn derive_detached(value: &str, session: &str) -> String {
    let trimmed = crate::policy::trim(value);
    if trimmed.is_empty() || trimmed == "1" || trimmed.eq_ignore_ascii_case("true") {
        return value.to_owned();
    }
    let safe: String = session
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let slash = trimmed.rfind(['/', '\\']);
    match trimmed.rfind('.') {
        Some(dot) if slash.is_none_or(|slash| dot > slash) => {
            format!("{}.{safe}{}", &trimmed[..dot], &trimmed[dot..])
        }
        _ => format!("{trimmed}.{safe}.json"),
    }
}
type Running<T> = Option<(String, std::time::Instant, T)>;
struct Writer {
    path: PathBuf,
    argv: Vec<String>,
    cwd: PathBuf,
    origin: std::time::Instant,
    origin_ms: f64,
    events: Vec<Value>,
    checkpointing: bool,
    last: Option<std::time::Instant>,
    count: u64,
    option: Option<String>,
    suppressed: bool,
    phase: Running<u64>,
    detail: Running<Option<u64>>,
    flushed: bool,
    timer: bool,
}
static TRACE: std::sync::Mutex<Option<Writer>> = std::sync::Mutex::new(None);
fn with<T>(work: impl FnOnce(&mut Writer) -> T) -> Option<T> {
    TRACE.lock().ok()?.as_mut().map(work)
}
fn millis(duration: std::time::Duration) -> f64 {
    (duration.as_secs_f64() * 1_000_000.0).round() / 1000.0
}
/// Reference createPerfTrace: the option value, else ORACLE_PERF_TRACE; "1"/"true" selects
/// `.oracle-perf-<time>-<pid>.json` in the working directory.
pub fn start(option: Option<String>, argv: &[String], cwd: &Path) {
    let environment = std::env::var("ORACLE_PERF_TRACE").ok();
    let raw = match option.as_deref() {
        Some("1") => environment.clone().or(option.clone()),
        Some(_) => option.clone(),
        None => environment,
    };
    let Some(raw) = raw.filter(|value| !value.is_empty()) else {
        return;
    };
    let path = if raw == "1" || raw == "true" {
        let time = chrono::Utc::now()
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
            .replace([':', '.'], "-");
        cwd.join(format!(".oracle-perf-{time}-{}.json", std::process::id()))
    } else {
        cwd.join(raw)
    };
    let origin_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64() * 1000.0);
    if let Ok(mut trace) = TRACE.lock() {
        *trace = Some(Writer {
            path,
            argv: sanitise_argv(argv),
            cwd: cwd.to_owned(),
            origin: std::time::Instant::now(),
            origin_ms,
            events: vec![],
            checkpointing: false,
            last: None,
            count: 0,
            option,
            suppressed: false,
            phase: None,
            detail: None,
            flushed: false,
            timer: false,
        });
    }
    mark("cli-module-ready", None);
}
/// Reference suppressPerfTrace: perf-summary and browser-health never write a trace.
pub fn suppress() {
    with(|trace| trace.suppressed = true);
}
/// Reference preAction hook: begins checkpointing and marks the command.
pub fn pre_action(command: &str) {
    with(|trace| trace.checkpointing = !trace.suppressed);
    mark("pre-action", Some(serde_json::json!({"command":command})));
}
/// Reference mark: a checkpoint at most once a second, deferred to the end of that second when
/// one was written recently, so a long phase never leaves its latest events unwritten.
pub fn mark(name: &str, data: Option<Value>) {
    with(|trace| {
        if trace.flushed {
            return;
        }
        let mut event = serde_json::json!({"name":name,"ms":millis(trace.origin.elapsed())});
        if let Some(data) = data {
            event["data"] = data;
        }
        trace.events.push(event);
        if !trace.checkpointing {
            return;
        }
        let remaining = trace.last.map_or(std::time::Duration::ZERO, |last| {
            std::time::Duration::from_secs(1).saturating_sub(last.elapsed())
        });
        if remaining.is_zero() {
            trace.checkpoint(false);
        } else if !trace.timer {
            trace.timer = true;
            // Diagnostic I/O only; process exit never waits for this thread.
            std::thread::spawn(move || {
                std::thread::sleep(remaining);
                with(|trace| {
                    trace.timer = false;
                    if !trace.flushed {
                        trace.checkpoint(false);
                    }
                });
            });
        }
    });
}
impl Writer {
    fn checkpoint(&mut self, complete: bool) -> bool {
        self.last = Some(std::time::Instant::now());
        self.count += 1;
        let payload = serde_json::json!({"version":env!("CARGO_PKG_VERSION"),"argv":self.argv,"cwd":self.cwd,"pid":std::process::id(),"runtime":"rust","timeOrigin":self.origin_ms,"totalMs":millis(self.origin.elapsed()),"complete":complete,"checkpointCount":self.count,"events":self.events});
        let Ok(mut bytes) = serde_json::to_vec_pretty(&payload) else {
            return false;
        };
        bytes.push(b'\n');
        crate::storage::atomic_bytes(&self.path, &bytes).is_ok()
    }
}
/// The detached worker's ORACLE_PERF_TRACE (reference resolveDetachedPerfTraceEnv).
pub fn detached_value(environment: Option<&str>, session: &str) -> Option<String> {
    let option = TRACE
        .lock()
        .ok()
        .and_then(|t| t.as_ref().and_then(|t| t.option.clone()));
    match option.as_deref() {
        Some("1") => Some("1".into()),
        Some(path) => Some(derive_detached(path, session)),
        None => environment.map(|value| derive_detached(value, session)),
    }
    .filter(|value| !value.is_empty())
}
/// Final `exit` mark and complete write; a failed write is reported on stderr.
pub fn flush(code: i32) {
    mark("exit", Some(serde_json::json!({"exitCode":code})));
    with(|trace| {
        if trace.flushed {
            return;
        }
        trace.flushed = true;
        if !trace.suppressed && !trace.checkpoint(true) {
            eprintln!("Failed to write perf trace: {}", trace.path.display());
        }
    });
}
/// Reference browser phase timer: a new phase finishes the previous one as successful and
/// keeps its turn unless one is given.
pub fn phase(name: &str, turn: Option<u64>) {
    let turn = turn.unwrap_or_else(current_turn);
    phase_end(1);
    with(|trace| trace.phase = Some((name.into(), std::time::Instant::now(), turn)));
}
pub fn phase_end(ok: u8) {
    detail_end(ok);
    if let Some(Some((name, started, turn))) = with(|trace| trace.phase.take()) {
        mark(
            &format!("consultation.{name}"),
            Some(
                serde_json::json!({"durationMs":millis(started.elapsed()),"turnIndex":turn,"ok":ok}),
            ),
        );
    }
}
/// The current phase's turn, for details within it.
pub fn current_turn() -> u64 {
    with(|t| t.phase.as_ref().map(|p| p.2))
        .flatten()
        .unwrap_or(0)
}
/// Reference browser detail timer; output persistence carries no turn.
pub fn detail(name: &str, turn: Option<u64>) {
    detail_end(1);
    with(|trace| trace.detail = Some((name.into(), std::time::Instant::now(), turn)));
}
pub fn detail_end(ok: u8) {
    if let Some(Some((name, started, turn))) = with(|trace| trace.detail.take()) {
        let mut data =
            serde_json::json!({"detail":1,"durationMs":millis(started.elapsed()),"ok":ok});
        if let Some(turn) = turn {
            data["turnIndex"] = serde_json::json!(turn);
        }
        mark(&format!("consultation-detail.{name}"), Some(data));
    }
}

#[cfg(test)]
mod trace_tests {
    use super::*;
    fn strings(args: &[&str]) -> Vec<String> {
        args.iter().map(|a| (*a).to_owned()).collect()
    }
    #[test]
    fn argv_sanitising_matches_the_reference_redaction_cases() {
        assert_eq!(
            sanitise_argv(&strings(&[
                "-psecret prompt",
                "--prompt=secret prompt",
                "--browser-follow-up=sensitive follow-up",
                "--token",
                "service-token",
                "--connect=oracle+tcp://example.com:1234?token=service-token",
                "--remote-token=remote-secret",
                "--browser-inline-cookies",
                "[cookie-json]",
                "--",
                "--ask=private-details",
            ])),
            strings(&[
                "-p[redacted]",
                "--prompt=[redacted]",
                "--browser-follow-up=[redacted]",
                "--token",
                "[redacted]",
                "--connect=oracle+tcp://example.com:1234?token=[redacted]",
                "--remote-token=[redacted]",
                "--browser-inline-cookies",
                "[redacted]",
                "--",
                "[redacted-positional]",
            ])
        );
        assert_eq!(
            sanitise_argv(&strings(&[
                "PUBLIC positional",
                "--model",
                "gpt-6-pro",
                "--base-url",
                "https://user:pass@example.invalid/v1?api_key=sk-PUBLIC123456",
            ])),
            strings(&[
                "[redacted-positional]",
                "--model",
                "gpt-6-pro",
                "--base-url",
                "https://user:[redacted]@example.invalid/v1?api_key=[redacted]",
            ])
        );
    }
    #[test]
    fn detached_trace_paths_follow_the_reference() {
        assert_eq!(
            derive_detached("/tmp/oracle-trace.json", "abc123"),
            "/tmp/oracle-trace.abc123.json"
        );
        assert_eq!(
            derive_detached("/tmp/oracle-trace", "abc/123"),
            "/tmp/oracle-trace.abc_123.json"
        );
        assert_eq!(derive_detached("1", "abc123"), "1");
        assert_eq!(
            derive_detached(r"C:\traces.d\run", "abc"),
            r"C:\traces.d\run.abc.json"
        );
    }
    #[test]
    fn trace_arguments_normalise_like_the_reference() {
        let (args, value, error) =
            normalise_args(&strings(&["--perf-trace=PUBLIC.json", "-p", "x"]));
        assert_eq!(
            args,
            strings(&[
                "--perf-trace",
                "--perf-trace-path",
                "PUBLIC.json",
                "-p",
                "x"
            ])
        );
        assert_eq!((value.as_deref(), error), (Some("PUBLIC.json"), None));
        let (args, value, _) = normalise_args(&strings(&["--perf-trace", "-p", "--perf-trace"]));
        assert_eq!(args, strings(&["--perf-trace", "-p", "--perf-trace"]));
        assert_eq!(value.as_deref(), Some("1"));
        let (_, value, _) = normalise_args(&strings(&[
            "--perf-trace-path",
            "PUBLIC.json",
            "--perf-trace",
        ]));
        assert_eq!(value.as_deref(), Some("PUBLIC.json"));
        for args in [
            vec!["--perf-trace-path"],
            vec!["--perf-trace-path", "--wait"],
            vec!["--perf-trace-path="],
        ] {
            assert_eq!(
                normalise_args(&strings(&args)).2.as_deref(),
                Some("option '--perf-trace-path <path>' argument missing"),
                "{args:?}"
            );
        }
        let (args, value, _) = normalise_args(&strings(&["--", "--perf-trace=x"]));
        assert_eq!((args, value), (strings(&["--", "--perf-trace=x"]), None));
    }
}
