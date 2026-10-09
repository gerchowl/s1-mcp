//! The call log: every decision is a replayable JSONL record, every rating a
//! second record pointing at it. Append-only; `report` joins the two.
//!
//! Lives in `$S1_MCP_LOG_DIR` or `$XDG_STATE_HOME/s1-mcp` (`~/.local/state/s1-mcp`),
//! directory 0700, file 0600. It never leaves the host. States are kept
//! (that is what makes a call replayable against the next model) after a light
//! redaction of things that look like credentials; images are logged by size
//! and origin only, never their bytes.
//!
//! Rotation: past `$S1_MCP_LOG_ROTATE_MB` (default 50) the file is renamed to
//! `calls-<UTC timestamp>.jsonl` and a fresh one started; rotated segments
//! whose last write is older than `$S1_MCP_LOG_RETENTION_DAYS` (default 90,
//! `0` keeps them forever) are deleted. Every agent session runs its own
//! s1-mcp process and they all append to the same file, so the size check,
//! the rename and the append happen under one inter-process lock
//! (`.calls.lock`): an in-process counter, which is what the rotating-log
//! crates keep, would let two sessions rotate at once.

use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub fn dir() -> PathBuf {
    if let Some(d) = std::env::var_os("S1_MCP_LOG_DIR") {
        return PathBuf::from(d);
    }
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
        .unwrap_or_else(std::env::temp_dir);
    base.join("s1-mcp")
}

pub fn path() -> PathBuf {
    dir().join("calls.jsonl")
}

pub fn disabled() -> bool {
    std::env::var("S1_MCP_LOG").is_ok_and(|v| v == "0" || v.eq_ignore_ascii_case("off"))
}

#[derive(Debug, Clone, Copy)]
pub struct Policy {
    pub rotate_bytes: u64,
    /// `None` keeps rotated segments forever.
    pub retention: Option<Duration>,
}

impl Policy {
    pub fn from_env() -> Policy {
        let num = |k: &str, d: f64| {
            std::env::var(k)
                .ok()
                .and_then(|v| v.trim().parse::<f64>().ok())
                .filter(|v| v.is_finite() && *v >= 0.0)
                .unwrap_or(d)
        };
        let mb = num("S1_MCP_LOG_ROTATE_MB", 50.0).max(1.0);
        let days = num("S1_MCP_LOG_RETENTION_DAYS", 90.0);
        Policy {
            rotate_bytes: (mb * 1024.0 * 1024.0) as u64,
            retention: (days > 0.0).then(|| Duration::from_secs_f64(days * 86_400.0)),
        }
    }
}

pub fn append(rec: &Value) -> Result<(), String> {
    if disabled() {
        return Ok(());
    }
    append_in(&dir(), rec, Policy::from_env())
}

fn open_private(p: &Path, append: bool) -> std::io::Result<std::fs::File> {
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true);
    if append {
        opts.append(true);
    } else {
        opts.write(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(p)
}

pub fn append_in(d: &Path, rec: &Value, policy: Policy) -> Result<(), String> {
    std::fs::create_dir_all(d).map_err(|e| format!("log dir {}: {e}", d.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o700));
    }
    let mut line = serde_json::to_vec(rec).map_err(|e| e.to_string())?;
    line.push(b'\n');

    // Held until the end of this function (dropping the file releases it).
    let lock = open_private(&d.join(".calls.lock"), false)
        .map_err(|e| format!("log lock {}: {e}", d.display()))?;
    lock.lock().map_err(|e| format!("log lock: {e}"))?;

    let current = d.join("calls.jsonl");
    let size = std::fs::metadata(&current).map(|m| m.len()).unwrap_or(0);
    let rotated = size > 0 && size + line.len() as u64 > policy.rotate_bytes;
    if rotated {
        let stamp: String = iso(now_ms())
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .collect();
        let mut target = d.join(format!("calls-{stamp}.jsonl"));
        let mut n = 1;
        while target.exists() {
            target = d.join(format!("calls-{stamp}-{n}.jsonl"));
            n += 1;
        }
        std::fs::rename(&current, &target).map_err(|e| format!("log rotate: {e}"))?;
    }
    // Prune on every rotation and once per process, so a host that never
    // reaches the size limit still drops expired segments.
    static PRUNED: std::sync::Once = std::sync::Once::new();
    let mut first = false;
    PRUNED.call_once(|| first = true);
    if (rotated || first)
        && let Some(keep) = policy.retention
    {
        prune(d, keep);
    }

    let mut f =
        open_private(&current, true).map_err(|e| format!("log {}: {e}", current.display()))?;
    // One write per record: O_APPEND keeps concurrent servers' lines whole
    // even for a reader that takes no lock.
    f.write_all(&line).map_err(|e| format!("log write: {e}"))
}

/// Rotated segments, oldest first (the UTC stamp in the name sorts).
fn rotated_segments(d: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(d)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("calls-") && n.ends_with(".jsonl"))
        })
        .collect();
    v.sort();
    v
}

fn prune(d: &Path, keep: Duration) {
    let now = SystemTime::now();
    for p in rotated_segments(d) {
        let old = std::fs::metadata(&p)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| now.duration_since(t).ok())
            .is_some_and(|age| age > keep);
        if old {
            let _ = std::fs::remove_file(&p);
        }
    }
}

/// Every segment, oldest first; the live file last.
fn segments(d: &Path) -> Vec<PathBuf> {
    let mut v = rotated_segments(d);
    v.push(d.join("calls.jsonl"));
    v
}

pub fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// RFC 3339 UTC timestamp without pulling in a time crate.
pub fn iso(ms: u128) -> String {
    let secs = (ms / 1000) as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Civil-from-days (Howard Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// A short, unique-enough id an agent can paste back into `s1_rate`.
pub fn new_call_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let seed = now_ms() as u64
        ^ (u64::from(std::process::id()) << 32)
        ^ n.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    format!("s1-{:010x}", fnv1a(&seed.to_le_bytes()) & 0xff_ffff_ffff)
}

pub fn fnv1a(b: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for x in b {
        h ^= u64::from(*x);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

/// Where this call came from: enough to answer "in which kind of work did
/// System One help", nothing that identifies content.
pub fn context() -> Value {
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    let harness = if env("CLAUDECODE").is_some() || env("CLAUDE_CODE_ENTRYPOINT").is_some() {
        "claude"
    } else if env("CODEX_HOME").is_some()
        || env("CODEX_SANDBOX").is_some()
        || env("CODEX_THREAD_ID").is_some()
    {
        "codex"
    } else if env("OPENCODE").is_some() || env("OPENCODE_BIN_PATH").is_some() {
        "opencode"
    } else if env("GEMINI_CLI").is_some() {
        "gemini"
    } else {
        "unknown"
    };
    let harness = env("S1_MCP_HARNESS").unwrap_or_else(|| harness.into());
    let cwd = std::env::current_dir().ok();
    let repo = cwd.as_ref().and_then(|c| {
        c.ancestors()
            .find(|a| a.join(".git").exists())
            .and_then(|r| r.file_name())
            .map(|n| n.to_string_lossy().into_owned())
    });
    json!({
        "host": hostname(),
        "harness": harness,
        "repo": repo,
        "session": env("CLAUDE_CODE_SESSION_ID").or_else(|| env("CODEX_THREAD_ID")).or_else(|| env("FLOCK_PANE_ID")),
    })
}

fn hostname() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .filter(|h| !h.is_empty())
        .or_else(|| {
            std::process::Command::new("hostname")
                .arg("-s")
                .output()
                .ok()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        })
        .unwrap_or_else(|| "unknown".into())
}

const SECRET_PREFIXES: &[&str] = &[
    "sk-",
    "sk_live_",
    "sk_test_",
    "ghp_",
    "gho_",
    "ghu_",
    "ghs_",
    "ghr_",
    "github_pat_",
    "glpat-",
    "xoxb-",
    "xoxp-",
    "xapp-",
    "AKIA",
    "ASIA",
    "AIza",
    "hf_",
    "npm_",
    "age1",
    "AGE-SECRET-KEY-",
];

/// Light, best-effort credential scrub for logged states. Not a guarantee:
/// the log is private to the host for that reason.
pub fn redact(v: &Value) -> Value {
    match v {
        Value::String(s) => Value::String(redact_str(s)),
        Value::Array(a) => Value::Array(a.iter().map(redact).collect()),
        Value::Object(m) => Value::Object(
            m.iter()
                .map(|(k, v)| {
                    let kl = k.to_ascii_lowercase();
                    if [
                        "password",
                        "passwd",
                        "secret",
                        "token",
                        "api_key",
                        "apikey",
                        "authorization",
                    ]
                    .iter()
                    .any(|s| kl.contains(s))
                        && v.is_string()
                    {
                        (k.clone(), json!("[REDACTED]"))
                    } else {
                        (k.clone(), redact(v))
                    }
                })
                .collect(),
        ),
        other => other.clone(),
    }
}

fn redact_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    // PEM private-key blocks first.
    let mut rest = s;
    while let Some(i) = rest.find("-----BEGIN") {
        let block_end = rest[i..].find("PRIVATE KEY-----").and_then(|j| {
            let after = i + j + "PRIVATE KEY-----".len();
            rest[after..].find("-----END").map(|k| {
                let tail = after + k;
                tail + rest[tail..]
                    .find("KEY-----")
                    .map(|x| x + "KEY-----".len())
                    .unwrap_or(0)
            })
        });
        match block_end {
            Some(end) => {
                out.push_str(&rest[..i]);
                out.push_str("[REDACTED PRIVATE KEY]");
                rest = &rest[end..];
            }
            None => break,
        }
    }
    out.push_str(rest);
    // Then token-shaped words.
    let mut res = String::with_capacity(out.len());
    let mut word = String::new();
    let flush = |word: &mut String, res: &mut String| {
        let looks_secret = word.len() >= 16 && SECRET_PREFIXES.iter().any(|p| word.starts_with(p))
            || (word.len() >= 24 && word.starts_with("Bearer"));
        if looks_secret {
            res.push_str("[REDACTED]");
        } else {
            res.push_str(word);
        }
        word.clear();
    };
    for c in out.chars() {
        if c.is_whitespace()
            || matches!(
                c,
                '"' | '\'' | '`' | ',' | ';' | '=' | '(' | ')' | '<' | '>'
            )
        {
            flush(&mut word, &mut res);
            res.push(c);
        } else {
            word.push(c);
        }
    }
    flush(&mut word, &mut res);
    res
}

fn read_file(p: &Path) -> Vec<Value> {
    let Ok(f) = std::fs::File::open(p) else {
        return vec![];
    };
    std::io::BufReader::new(f)
        .lines()
        .map_while(Result::ok)
        .filter_map(|l| serde_json::from_str(&l).ok())
        .collect()
}

/// Records from every segment last written at or after `since_ms` (all when None).
fn read_since(since_ms: Option<u128>) -> Vec<Value> {
    segments(&dir())
        .iter()
        .filter(|p| {
            let Some(cut) = since_ms else { return true };
            std::fs::metadata(p)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .is_none_or(|t| t.as_millis() >= cut)
        })
        .flat_map(|p| read_file(p))
        .collect()
}

/// Newest segment first: a call is usually rated soon after it was made.
pub fn find_call(id: &str) -> Option<Value> {
    segments(&dir()).iter().rev().find_map(|p| {
        read_file(p).into_iter().find(|r| {
            r.get("kind").and_then(Value::as_str) == Some("call")
                && r.get("call_id").and_then(Value::as_str) == Some(id)
        })
    })
}

#[derive(Default)]
struct Agg {
    calls: u64,
    errors: u64,
    rated: u64,
    right: u64,
    wrong: u64,
    useful: u64,
    not_useful: u64,
    latencies: Vec<f64>,
    strong: u64,
    answered: u64,
    repos: BTreeMap<String, u64>,
    harnesses: BTreeMap<String, u64>,
}

/// Aggregate the log by use case × model. `since_days` and filters narrow it.
pub fn report(use_case: Option<&str>, model: Option<&str>, since_days: Option<f64>) -> Value {
    let cutoff = since_days.map(|d| now_ms().saturating_sub((d * 86_400_000.0) as u128));
    let recs = read_since(cutoff);
    let mut ratings: BTreeMap<String, Value> = BTreeMap::new();
    for r in &recs {
        if r.get("kind").and_then(Value::as_str) == Some("rating")
            && let Some(id) = r.get("call_id").and_then(Value::as_str)
        {
            ratings.insert(id.into(), r.clone()); // last rating wins
        }
    }
    let mut groups: BTreeMap<(String, String), Agg> = BTreeMap::new();
    let mut unrated = vec![];
    for r in recs
        .iter()
        .filter(|r| r.get("kind").and_then(Value::as_str) == Some("call"))
    {
        let uc = r
            .get("use_case")
            .and_then(Value::as_str)
            .unwrap_or("?")
            .to_string();
        let ts = r
            .get("ts_ms")
            .and_then(Value::as_u64)
            .map(u128::from)
            .unwrap_or(0);
        if use_case.is_some_and(|u| !uc.contains(u)) || cutoff.is_some_and(|c| ts < c) {
            continue;
        }
        let ctx = r.get("context").cloned().unwrap_or(Value::Null);
        for res in r
            .get("results")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let m = res
                .get("model")
                .and_then(Value::as_str)
                .unwrap_or("?")
                .to_string();
            if model.is_some_and(|f| !m.eq_ignore_ascii_case(f)) {
                continue;
            }
            let g = groups.entry((uc.clone(), m.clone())).or_default();
            g.calls += 1;
            if let Some(repo) = ctx.get("repo").and_then(Value::as_str) {
                *g.repos.entry(repo.into()).or_default() += 1;
            }
            if let Some(h) = ctx.get("harness").and_then(Value::as_str) {
                *g.harnesses.entry(h.into()).or_default() += 1;
            }
            if res.get("error").is_some() {
                g.errors += 1;
                continue;
            }
            if let Some(l) = res.get("latency_ms").and_then(Value::as_f64) {
                g.latencies.push(l);
            }
            for a in res
                .get("answers")
                .and_then(Value::as_object)
                .into_iter()
                .flat_map(|m| m.values())
            {
                g.answered += 1;
                if a.get("band").and_then(Value::as_str) == Some("strong") {
                    g.strong += 1;
                }
            }
            let id = r.get("call_id").and_then(Value::as_str).unwrap_or_default();
            match ratings.get(id) {
                Some(rt) => {
                    // A comparison call is rated per model when `models` is given.
                    let verdict = rt
                        .get("per_model")
                        .and_then(|p| p.get(&m))
                        .or_else(|| rt.get("verdict"))
                        .and_then(Value::as_str);
                    if let Some(v) = verdict {
                        g.rated += 1;
                        match v {
                            "right" => g.right += 1,
                            "wrong" => g.wrong += 1,
                            _ => {}
                        }
                    }
                    match rt.get("useful").and_then(Value::as_bool) {
                        Some(true) => g.useful += 1,
                        Some(false) => g.not_useful += 1,
                        None => {}
                    }
                }
                None => {
                    if unrated.len() < 400 {
                        unrated.push((ts, id.to_string(), uc.clone()));
                    }
                }
            }
        }
    }
    unrated.sort_by_key(|u| std::cmp::Reverse(u.0));
    unrated.dedup_by(|a, b| a.1 == b.1);
    let rows: Vec<Value> = groups
        .into_iter()
        .map(|((uc, m), mut g)| {
            g.latencies.sort_by(f64::total_cmp);
            let p50 = g.latencies.get(g.latencies.len() / 2).copied();
            let decided = g.right + g.wrong;
            let mut row = Map::new();
            row.insert("use_case".into(), json!(uc));
            row.insert("model".into(), json!(m));
            row.insert("calls".into(), json!(g.calls));
            row.insert("errors".into(), json!(g.errors));
            row.insert("rated".into(), json!(g.rated));
            row.insert(
                "accuracy".into(),
                if decided > 0 {
                    json!(crate::s1::round(g.right as f64 / decided as f64))
                } else {
                    Value::Null
                },
            );
            row.insert("right".into(), json!(g.right));
            row.insert("wrong".into(), json!(g.wrong));
            row.insert("useful".into(), json!(g.useful));
            row.insert("not_useful".into(), json!(g.not_useful));
            row.insert(
                "strong_share".into(),
                if g.answered > 0 {
                    json!(crate::s1::round(g.strong as f64 / g.answered as f64))
                } else {
                    Value::Null
                },
            );
            row.insert(
                "p50_latency_ms".into(),
                p50.map(|p| json!(p.round())).unwrap_or(Value::Null),
            );
            row.insert("repos".into(), json!(g.repos));
            row.insert("harnesses".into(), json!(g.harnesses));
            Value::Object(row)
        })
        .collect();
    json!({
        "log": path().display().to_string(),
        "groups": rows,
        "unrated_recent": unrated.iter().take(10).map(|(ts, id, uc)| json!({"call_id": id, "use_case": uc, "at": iso(*ts)})).collect::<Vec<_>>(),
        "unrated_total": unrated.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_tokens_and_keys_but_keeps_prose() {
        let s =
            "token=ghp_abcdefghijklmnopqrstuvwxyz0123 and sk-proj-AAAAAAAAAAAAAAAAAAAA ok sky-high";
        let r = redact_str(s);
        assert!(!r.contains("ghp_abc") && !r.contains("sk-proj"));
        assert!(r.contains("sky-high") && r.contains("token="));
        let pem = format!(
            "a\n-----BEGIN {k}-----\nabc\n-----END {k}-----\nb",
            k = ["OPENSSH", "PRIVATE", "KEY"].join(" ")
        );
        assert_eq!(redact_str(&pem), "a\n[REDACTED PRIVATE KEY]\nb");
        let v = redact(&json!({"api_key": "hunter2", "n": 3, "msg": ["AKIAABCDEFGHIJKLMNOP"]}));
        assert_eq!(v["api_key"], "[REDACTED]");
        assert_eq!(v["msg"][0], "[REDACTED]");
        assert_eq!(v["n"], 3);
    }

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("s1-mcp-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn lines(d: &Path) -> usize {
        segments(d).iter().map(|p| read_file(p).len()).sum()
    }

    #[test]
    fn rotates_past_the_limit_without_losing_records() {
        let d = tmp("rotate");
        let policy = Policy {
            rotate_bytes: 200,
            retention: None,
        };
        for i in 0..10 {
            append_in(
                &d,
                &json!({"kind": "call", "i": i, "pad": "x".repeat(60)}),
                policy,
            )
            .unwrap();
        }
        assert!(rotated_segments(&d).len() >= 3, "expected several segments");
        assert_eq!(lines(&d), 10);
        // Each segment stays within the limit (no single record exceeds it).
        for p in segments(&d) {
            assert!(std::fs::metadata(&p).unwrap().len() <= 200);
        }
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn concurrent_writers_rotate_safely() {
        // Threads stand in for the many s1-mcp processes of a busy host: the
        // lock is a file lock, so it serialises threads the same way.
        let d = tmp("concurrent");
        let policy = Policy {
            rotate_bytes: 1_000,
            retention: None,
        };
        std::thread::scope(|sc| {
            for t in 0..8 {
                let d = &d;
                sc.spawn(move || {
                    for i in 0..50 {
                        append_in(
                            d,
                            &json!({"kind": "call", "t": t, "i": i, "pad": "y".repeat(40)}),
                            policy,
                        )
                        .unwrap();
                    }
                });
            }
        });
        assert_eq!(lines(&d), 400, "every record survives rotation");
        let mut seen = std::collections::HashSet::new();
        for r in segments(&d).iter().flat_map(|p| read_file(p)) {
            assert!(
                seen.insert((r["t"].as_u64(), r["i"].as_u64())),
                "duplicate {r}"
            );
        }
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn prunes_only_expired_rotated_segments() {
        let d = tmp("prune");
        std::fs::create_dir_all(&d).unwrap();
        let old = d.join("calls-20250101T000000Z.jsonl");
        let fresh = d.join("calls-20261001T000000Z.jsonl");
        for p in [&old, &fresh] {
            std::fs::write(p, "{}\n").unwrap();
        }
        let f = std::fs::File::options().write(true).open(&old).unwrap();
        f.set_modified(SystemTime::now() - Duration::from_secs(100 * 86_400))
            .unwrap();
        std::fs::write(d.join("calls.jsonl"), "{}\n").unwrap();
        prune(&d, Duration::from_secs(90 * 86_400));
        assert!(!old.exists());
        assert!(fresh.exists() && d.join("calls.jsonl").exists());
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn policy_reads_env_defaults() {
        let p = Policy::from_env();
        assert_eq!(p.rotate_bytes, 50 * 1024 * 1024);
        assert_eq!(p.retention, Some(Duration::from_secs(90 * 86_400)));
    }

    #[test]
    fn iso_formats_epoch() {
        assert_eq!(iso(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso(1_791_504_000_000), "2026-10-09T00:00:00Z");
    }

    #[test]
    fn report_joins_ratings() {
        let dir = std::env::temp_dir().join(format!("s1-mcp-log-{}", std::process::id()));
        // SAFETY: tests in this module are the only users of S1_MCP_LOG_DIR.
        unsafe { std::env::set_var("S1_MCP_LOG_DIR", &dir) };
        let _ = std::fs::remove_dir_all(&dir);
        let call = |id: &str, m: &str| {
            json!({"kind": "call", "call_id": id, "ts_ms": now_ms() as u64, "use_case": "ci-triage",
                   "context": {"repo": "cx", "harness": "claude"},
                   "results": [{"model": m, "latency_ms": 20.0, "answers": {"q": {"band": "strong"}}}]})
        };
        append(&call("a", "clef-flash")).unwrap();
        append(&call("b", "clef-flash")).unwrap();
        append(&call("c", "clef-flash")).unwrap();
        append(&json!({"kind": "rating", "call_id": "a", "verdict": "right", "useful": true}))
            .unwrap();
        append(&json!({"kind": "rating", "call_id": "b", "verdict": "wrong"})).unwrap();
        let r = report(Some("ci"), None, Some(1.0));
        let g = &r["groups"][0];
        assert_eq!(g["calls"], 3);
        assert_eq!(g["accuracy"], 0.5);
        assert_eq!(g["useful"], 1);
        assert_eq!(r["unrated_total"], 1);
        assert_eq!(r["unrated_recent"][0]["call_id"], "c");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
