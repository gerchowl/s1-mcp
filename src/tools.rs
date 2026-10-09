//! The MCP tools: definitions (what agents read to discover them) and handlers.

use crate::log;
use crate::registry::{Model, Registry};
use crate::s1::{self, Image};
use serde_json::{Map, Value, json};
use std::time::{Duration, Instant};

/// A tool result: the human-readable text plus the structured payload.
pub struct ToolOut {
    pub text: String,
    pub data: Value,
    pub is_error: bool,
}

impl ToolOut {
    fn ok(text: String, data: Value) -> Self {
        ToolOut {
            text,
            data,
            is_error: false,
        }
    }
    pub fn err(msg: impl Into<String>) -> Self {
        let msg = msg.into();
        ToolOut {
            data: json!({"error": msg}),
            text: msg,
            is_error: true,
        }
    }
}

fn question_schema() -> Value {
    json!({
        "type": "object",
        "description": "Questions keyed by a short id you choose (the answers come back under the same ids). All questions see the same state but not each other; ask everything about one state in one call.",
        "minProperties": 1,
        "maxProperties": s1::MAX_QUESTIONS,
        "additionalProperties": {
            "type": "object",
            "required": ["type"],
            "properties": {
                "type": {"type": "string", "enum": ["noul", "choice", "score"],
                         "description": "noul = yes/no; choice = pick one option; score = place on an ordered scale"},
                "instructions": {"type": "string",
                                 "description": "The question itself. Required for noul; recommended for choice and score."},
                "criteria": {
                    "description": "choice: {\"option\": \"what this option means\"} (or an array of option names). score: array of levels, LOWEST first. noul: omit.",
                    "oneOf": [
                        {"type": "object", "additionalProperties": {"type": "string"}},
                        {"type": "array", "items": {"type": "string"}, "minItems": 2}
                    ]
                }
            }
        }
    })
}

fn state_schema() -> Value {
    json!({
        "description": "What the questions are about: text, or a JSON object/array. Must be self-contained (the model knows nothing else). Keep it to the relevant excerpt; accuracy drops on long documents.",
        "type": ["string", "object", "array"]
    })
}

fn use_case_schema() -> Value {
    json!({
        "type": "string",
        "description": "Short kebab-case tag for the kind of decision, reused across calls and sessions so value can be measured per use case (e.g. ci-failure-triage, ask-detection, pr-risk). See s1_report for tags already in use.",
        "minLength": 2, "maxLength": 64
    })
}

fn images_schema() -> Value {
    json!({
        "type": "array", "maxItems": s1::MAX_IMAGES,
        "description": "Images for image-capable models: absolute file paths (png/jpeg/gif/webp), data: URIs or raw base64. Giving images routes to an image-capable model when `model` is omitted.",
        "items": {"type": "string"}
    })
}

pub fn definitions(reg: &Registry) -> Value {
    let names = reg.models.iter().map(|m| {
        let mut s = m.id.clone();
        if !m.aliases.is_empty() {
            s.push_str(&format!(" (alias {})", m.aliases.join(", ")));
        }
        if m.accepts("image") {
            s.push_str(" [text+image]");
        }
        if !m.is_live() {
            s.push_str(&format!(" [{}]", m.status));
        }
        s
    });
    let model_list = names.collect::<Vec<_>>().join("; ");
    let default = reg
        .default
        .clone()
        .unwrap_or_else(|| "first live model".into());
    json!([
        {
            "name": "s1_decide",
            "title": "Ask a System One model",
            "description": format!(
                "Ask typed questions (yes/no, pick-one, ordered scale) about a state and get calibrated probabilities back in ~10–100 ms. No text generation, so no made-up prose. Use it for small, well-defined judgements: verify a command really failed, classify/route an error or issue, detect whether a message asks the user something, triage a list, or get a cheap second opinion before acting. Not for knowledge, arithmetic or anything needing generated text: the answer must be in the state. Every call is logged with a call_id; rate it later with s1_rate so usefulness can be measured. Models: {model_list}. Default: {default}."
            ),
            "inputSchema": {
                "type": "object",
                "required": ["state", "questions", "use_case"],
                "properties": {
                    "state": state_schema(),
                    "questions": question_schema(),
                    "use_case": use_case_schema(),
                    "model": {"type": "string", "description": format!("Model id or alias. Omit to use the default ({default}), or an image-capable model when images are given.")},
                    "images": images_schema(),
                    "timeout_ms": {"type": "integer", "minimum": 100, "maximum": 120000, "description": "Hard deadline for this call (default: the model's own)."}
                }
            },
            "annotations": {"title": "Ask a System One model", "readOnlyHint": true, "openWorldHint": false, "idempotentHint": true}
        },
        {
            "name": "s1_compare",
            "title": "Compare System One models",
            "description": "Run the same state and questions on several models in parallel and see where they agree, how confident each is, and how fast. Use it when exploring a new use case or deciding which model a decision should go to. Rate the result with s1_rate (per_model lets you mark each model right or wrong).",
            "inputSchema": {
                "type": "object",
                "required": ["state", "questions", "use_case"],
                "properties": {
                    "state": state_schema(),
                    "questions": question_schema(),
                    "use_case": use_case_schema(),
                    "models": {"type": "array", "items": {"type": "string"}, "description": "Model ids/aliases to compare. Default: every live model that accepts the inputs."},
                    "images": images_schema(),
                    "timeout_ms": {"type": "integer", "minimum": 100, "maximum": 120000}
                }
            },
            "annotations": {"title": "Compare System One models", "readOnlyHint": true, "openWorldHint": false}
        },
        {
            "name": "s1_rate",
            "title": "Rate a System One call",
            "description": "Record whether a previous s1_decide / s1_compare call was right and whether it was useful, once you know the truth (re-ran the test, read the log, the user answered). This is how the fleet learns where System One adds value; unrated calls teach nothing. Rating again replaces the earlier rating.",
            "inputSchema": {
                "type": "object",
                "required": ["call_id", "verdict"],
                "properties": {
                    "call_id": {"type": "string", "description": "The call_id returned by s1_decide or s1_compare."},
                    "verdict": {"type": "string", "enum": ["right", "wrong", "mixed", "unsure"], "description": "Overall: were the answers correct? mixed = some questions right, some wrong (use per_question)."},
                    "useful": {"type": "boolean", "description": "Did the answer change or speed up what you did? A right answer you'd have known anyway is right but not useful."},
                    "per_question": {"type": "object", "additionalProperties": {"type": "string", "enum": ["right", "wrong", "unsure"]}, "description": "Optional verdict per question id."},
                    "per_model": {"type": "object", "additionalProperties": {"type": "string", "enum": ["right", "wrong", "unsure"]}, "description": "For s1_compare calls: verdict per model id."},
                    "truth": {"type": "string", "description": "What the correct answer turned out to be, briefly."},
                    "note": {"type": "string", "description": "Anything worth knowing: why it was wrong, a better question wording, etc."}
                }
            },
            "annotations": {"title": "Rate a System One call", "readOnlyHint": false, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false}
        },
        {
            "name": "s1_models",
            "title": "List System One models",
            "description": "List the registered System One models: what each is for, input kinds (text/image), typical latency, known weaknesses, calibration notes and serving status. probe: true sends each live model a tiny real question and reports reachability, latency and which model actually answered.",
            "inputSchema": {
                "type": "object",
                "properties": {"probe": {"type": "boolean", "description": "Measure each live model now (one tiny request each, in parallel)."}}
            },
            "annotations": {"title": "List System One models", "readOnlyHint": true, "openWorldHint": false}
        },
        {
            "name": "s1_report",
            "title": "Where System One helps",
            "description": "Aggregate this host's call log by use case × model: calls, errors, rated accuracy, usefulness, share of strong answers, median latency, which repos and harnesses used it, and the most recent calls still waiting for a rating. Use it to see which use cases are worth automating, and to find tags already in use.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "use_case": {"type": "string", "description": "Only use cases containing this text."},
                    "model": {"type": "string", "description": "Only this model id."},
                    "since_days": {"type": "number", "minimum": 0, "description": "Only calls from the last N days."}
                }
            },
            "annotations": {"title": "Where System One helps", "readOnlyHint": true, "openWorldHint": false}
        }
    ])
}

pub fn call(reg: &Registry, name: &str, args: &Value) -> ToolOut {
    let res = match name {
        "s1_decide" => decide(reg, args),
        "s1_compare" => compare(reg, args),
        "s1_rate" => rate(args),
        "s1_models" => Ok(models(
            reg,
            args.get("probe").and_then(Value::as_bool).unwrap_or(false),
        )),
        "s1_report" => Ok(report(args)),
        other => Err(format!(
            "unknown tool {other:?}; tools: s1_decide, s1_compare, s1_rate, s1_models, s1_report"
        )),
    };
    res.unwrap_or_else(ToolOut::err)
}

fn use_case(args: &Value) -> Result<String, String> {
    let raw = args
        .get("use_case")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or_default();
    let tag: String = raw
        .to_ascii_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '/' {
                c
            } else {
                '-'
            }
        })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    if tag.len() < 2 {
        return Err("`use_case` is required: a short reusable tag like `ci-failure-triage`, so value can be measured per use case".into());
    }
    Ok(tag.chars().take(64).collect())
}

fn state(args: &Value) -> Result<Value, String> {
    match args.get("state") {
        None | Some(Value::Null) => {
            Err("`state` is required: the text or JSON the questions are about".into())
        }
        Some(Value::String(s)) if s.trim().is_empty() => Err("`state` is empty".into()),
        Some(v) => Ok(v.clone()),
    }
}

fn timeout(args: &Value, m: &Model) -> Duration {
    Duration::from_millis(
        args.get("timeout_ms")
            .and_then(Value::as_u64)
            .unwrap_or(m.timeout_ms)
            .clamp(100, 120_000),
    )
}

pub fn load_images(args: &Value) -> Result<Vec<Image>, String> {
    let Some(list) = args.get("images") else {
        return Ok(vec![]);
    };
    let list = list
        .as_array()
        .ok_or("`images` must be an array of paths, data: URIs or base64 strings")?;
    if list.len() > s1::MAX_IMAGES {
        return Err(format!("{} images; at most {}", list.len(), s1::MAX_IMAGES));
    }
    list.iter()
        .enumerate()
        .map(|(i, v)| {
            let s = v.as_str().ok_or_else(|| format!("images[{i}] is not a string"))?.trim();
            if let Some(rest) = s.strip_prefix("data:") {
                let (meta, b64) = rest.split_once(',').ok_or_else(|| format!("images[{i}]: malformed data: URI"))?;
                if !meta.ends_with(";base64") {
                    return Err(format!("images[{i}]: data: URI must be base64"));
                }
                return Ok(Image { bytes: b64.len() * 3 / 4, b64: b64.into(), origin: format!("data-uri:{}", meta.trim_end_matches(";base64")) });
            }
            let path = crate::registry::expand_home(s);
            if s.starts_with('/') || s.starts_with("~/") || s.starts_with("./") || path.exists() {
                let bytes = std::fs::read(&path).map_err(|e| format!("images[{i}]: cannot read {}: {e}", path.display()))?;
                if bytes.len() > s1::MAX_IMAGE_BYTES {
                    return Err(format!("images[{i}]: {} is {} bytes; max {}", path.display(), bytes.len(), s1::MAX_IMAGE_BYTES));
                }
                let kind = sniff(&bytes).ok_or_else(|| {
                    if is_video(&bytes) {
                        format!("images[{i}]: {} is a video; no System One engine accepts video yet. Extract a few frames as images instead.", path.display())
                    } else {
                        format!("images[{i}]: {} is not a png/jpeg/gif/webp image", path.display())
                    }
                })?;
                return Ok(Image { b64: b64encode(&bytes), origin: format!("file:{} ({kind})", path.display()), bytes: bytes.len() });
            }
            if s.len() >= 64 && s.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'=' | b'\n' | b'\r')) {
                return Ok(Image { bytes: s.len() * 3 / 4, b64: s.replace(['\n', '\r'], ""), origin: "base64".into() });
            }
            Err(format!("images[{i}]: not a readable file path, data: URI or base64 string"))
        })
        .collect()
}

fn sniff(b: &[u8]) -> Option<&'static str> {
    if b.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("png")
    } else if b.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("jpeg")
    } else if b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a") {
        Some("gif")
    } else if b.len() > 12 && &b[..4] == b"RIFF" && &b[8..12] == b"WEBP" {
        Some("webp")
    } else {
        None
    }
}

fn is_video(b: &[u8]) -> bool {
    (b.len() >= 8 && &b[4..8] == b"ftyp") || b.starts_with(&[0x1A, 0x45, 0xDF, 0xA3])
}

pub fn b64encode(b: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(b.len().div_ceil(3) * 4);
    for c in b.chunks(3) {
        let n = (u32::from(c[0]) << 16)
            | (u32::from(*c.get(1).unwrap_or(&0)) << 8)
            | u32::from(*c.get(2).unwrap_or(&0));
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if c.len() > 2 {
            T[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// Run one model and shape its result for the agent and the log.
fn run_one(
    m: &Model,
    state: &Value,
    qs: &Map<String, Value>,
    images: &[Image],
    t: Duration,
) -> (Value, Vec<String>) {
    let req = s1::build_request(m, state, qs, images);
    match s1::call(m, &req, t) {
        Ok(o) => {
            let mut answers = Map::new();
            let mut lines = vec![];
            for (id, q) in qs {
                let (a, line) = s1::interpret(id, q, o.answers.get(id));
                answers.insert(id.clone(), a);
                lines.push(line);
            }
            let mut r =
                json!({"model": m.id, "latency_ms": s1::round(o.latency_ms), "answers": answers});
            if let Some(sb) = &o.served_by {
                r["served_by"] = json!(sb);
            }
            if let Some(w) = m.served_mismatch(o.served_by.as_deref()) {
                lines.push(format!("WARNING: {w}"));
                r["served_mismatch"] = json!(w);
            }
            if let Some(ms) = o.server_ms {
                r["server_ms"] = json!(ms);
            }
            if let Some(u) = o.raw.get("usage") {
                r["usage"] = u.clone();
            }
            (r, lines)
        }
        Err(e) => (
            json!({"model": m.id, "error": e}),
            vec![format!("ERROR: {e}")],
        ),
    }
}

fn log_call(
    kind: &str,
    uc: &str,
    state: &Value,
    qs: &Map<String, Value>,
    images: &[Image],
    results: &[Value],
) -> (String, Option<String>) {
    let id = log::new_call_id();
    let ts = log::now_ms();
    let rec = json!({
        "kind": "call", "tool": kind, "call_id": id, "ts_ms": ts as u64, "at": log::iso(ts),
        "use_case": uc, "context": log::context(),
        "state": log::redact(state), "questions": qs,
        "images": images.iter().map(|i| json!({"origin": i.origin, "bytes": i.bytes})).collect::<Vec<_>>(),
        "results": results,
    });
    let warn = log::append(&rec)
        .err()
        .map(|e| format!("(call log not written: {e})"));
    (id, warn)
}

fn decide(reg: &Registry, args: &Value) -> Result<ToolOut, String> {
    let st = state(args)?;
    let qs = s1::normalise_questions(args.get("questions").unwrap_or(&Value::Null))?;
    let uc = use_case(args)?;
    let images = load_images(args)?;
    let m = reg.pick(
        args.get("model").and_then(Value::as_str),
        !images.is_empty(),
    )?;
    let (result, lines) = run_one(m, &st, &qs, &images, timeout(args, m));
    let (call_id, warn) = log_call(
        "s1_decide",
        &uc,
        &st,
        &qs,
        &images,
        std::slice::from_ref(&result),
    );
    if let Some(e) = result.get("error").and_then(Value::as_str) {
        let others: Vec<&str> = reg
            .models
            .iter()
            .filter(|o| o.id != m.id && o.is_live() && (images.is_empty() || o.accepts("image")))
            .map(|o| o.id.as_str())
            .collect();
        let hint = if others.is_empty() {
            String::new()
        } else {
            format!(" Other live models: {}.", others.join(", "))
        };
        return Ok(ToolOut {
            text: format!("{e}.{hint} (call_id {call_id})"),
            data: json!({"call_id": call_id, "error": e}),
            is_error: true,
        });
    }
    let mut text = format!(
        "{} · {} ms · call_id {call_id}\n{}",
        m.id,
        result["latency_ms"]
            .as_f64()
            .map(|l| l.round())
            .unwrap_or(0.0),
        lines.join("\n")
    );
    text.push_str("\nRate it with s1_rate once you know whether it was right.");
    if let Some(w) = warn {
        text.push('\n');
        text.push_str(&w);
    }
    let mut data = result;
    data["call_id"] = json!(call_id);
    data["use_case"] = json!(uc);
    Ok(ToolOut::ok(text, data))
}

fn compare(reg: &Registry, args: &Value) -> Result<ToolOut, String> {
    let st = state(args)?;
    let qs = s1::normalise_questions(args.get("questions").unwrap_or(&Value::Null))?;
    let uc = use_case(args)?;
    let images = load_images(args)?;
    let chosen: Vec<&Model> = match args.get("models").and_then(Value::as_array) {
        Some(names) if !names.is_empty() => names
            .iter()
            .map(|n| reg.pick(Some(n.as_str().unwrap_or_default()), !images.is_empty()))
            .collect::<Result<_, _>>()?,
        _ => reg
            .models
            .iter()
            .filter(|m| m.is_live() && (images.is_empty() || m.accepts("image")))
            .collect(),
    };
    if chosen.len() < 2 {
        return Err(format!(
            "need at least 2 models to compare; available: {}. Use s1_decide for a single model.",
            chosen
                .iter()
                .map(|m| m.id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    let results: Vec<(Value, Vec<String>)> = std::thread::scope(|sc| {
        let hs: Vec<_> = chosen
            .iter()
            .map(|m| sc.spawn(|| run_one(m, &st, &qs, &images, timeout(args, m))))
            .collect();
        hs.into_iter()
            .map(|h| {
                h.join()
                    .unwrap_or_else(|_| (json!({"error": "worker panicked"}), vec![]))
            })
            .collect()
    });
    let only: Vec<Value> = results.iter().map(|(r, _)| r.clone()).collect();
    let (call_id, warn) = log_call("s1_compare", &uc, &st, &qs, &images, &only);
    let mut text = format!("compare · call_id {call_id}\n");
    let mut agreement = Map::new();
    for id in qs.keys() {
        let picks: Vec<(String, String, f64)> = only
            .iter()
            .filter_map(|r| {
                let a = r.get("answers")?.get(id)?;
                let ans = a
                    .get("answer")
                    .or_else(|| a.get("most_likely"))?
                    .as_str()?
                    .to_string();
                Some((
                    r["model"].as_str()?.to_string(),
                    ans,
                    a.get("p")
                        .or_else(|| a.get("p_yes"))
                        .and_then(Value::as_f64)
                        .unwrap_or(f64::NAN),
                ))
            })
            .collect();
        let agree = picks.windows(2).all(|w| w[0].1 == w[1].1) && picks.len() == only.len();
        agreement.insert(id.clone(), json!(agree));
        text.push_str(&format!(
            "{id}: {}\n",
            if agree { "AGREE" } else { "DISAGREE" }
        ));
        for (m, a, p) in &picks {
            text.push_str(&format!("   {m:<14} {a}  ({p:.3})\n"));
        }
    }
    for r in &only {
        match r.get("error").and_then(Value::as_str) {
            Some(e) => text.push_str(&format!(
                "{}: ERROR {e}\n",
                r["model"].as_str().unwrap_or("?")
            )),
            None => text.push_str(&format!(
                "{}: {} ms\n",
                r["model"].as_str().unwrap_or("?"),
                r["latency_ms"].as_f64().unwrap_or(0.0).round()
            )),
        }
    }
    text.push_str("Rate with s1_rate (per_model marks each model right/wrong).");
    if let Some(w) = warn {
        text.push('\n');
        text.push_str(&w);
    }
    Ok(ToolOut::ok(
        text,
        json!({"call_id": call_id, "use_case": uc, "agreement": agreement, "results": only}),
    ))
}

fn rate(args: &Value) -> Result<ToolOut, String> {
    let id = args
        .get("call_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or("`call_id` is required (from s1_decide / s1_compare)")?;
    let verdict = args
        .get("verdict")
        .and_then(Value::as_str)
        .ok_or("`verdict` is required: right, wrong, mixed or unsure")?;
    if !matches!(verdict, "right" | "wrong" | "mixed" | "unsure") {
        return Err(format!(
            "verdict {verdict:?}: use right, wrong, mixed or unsure"
        ));
    }
    let call = if log::disabled() {
        None
    } else {
        Some(log::find_call(id).ok_or_else(|| {
            format!(
                "no call {id:?} in {}; call_ids come from s1_decide / s1_compare on this host",
                log::path().display()
            )
        })?)
    };
    let mut rec = json!({"kind": "rating", "call_id": id, "verdict": verdict, "ts_ms": log::now_ms() as u64, "at": log::iso(log::now_ms())});
    for k in ["useful", "per_question", "per_model", "truth", "note"] {
        if let Some(v) = args.get(k).filter(|v| !v.is_null()) {
            rec[k] = if k == "truth" || k == "note" {
                log::redact(v)
            } else {
                v.clone()
            };
        }
    }
    log::append(&rec)?;
    let uc = call
        .as_ref()
        .and_then(|c| c.get("use_case"))
        .and_then(Value::as_str)
        .unwrap_or("?");
    Ok(ToolOut::ok(
        format!(
            "rated {id} ({uc}): {verdict}{}. Thanks; see s1_report for the running totals.",
            match args.get("useful").and_then(Value::as_bool) {
                Some(true) => ", useful",
                Some(false) => ", not useful",
                None => "",
            }
        ),
        json!({"call_id": id, "recorded": true}),
    ))
}

pub fn probe(m: &Model) -> Value {
    let qs = s1::normalise_questions(
        &json!({"ok": {"type": "noul", "instructions": "Is this a connectivity check?"}}),
    )
    .expect("static question");
    let req = s1::build_request(m, &json!("connectivity check from s1-mcp"), &qs, &[]);
    let t = Duration::from_millis(m.timeout_ms.min(15_000));
    let t0 = Instant::now();
    match s1::call(m, &req, t) {
        Ok(o) => {
            let valid = s1::interpret("ok", &qs["ok"], o.answers.get("ok"))
                .0
                .get("error")
                .is_none();
            json!({"reachable": true, "valid_answer": valid, "latency_ms": s1::round(o.latency_ms), "served_by": o.served_by,
                   "served_mismatch": m.served_mismatch(o.served_by.as_deref())})
        }
        Err(e) => {
            json!({"reachable": false, "error": e, "after_ms": t0.elapsed().as_millis() as u64})
        }
    }
}

pub fn models(reg: &Registry, do_probe: bool) -> ToolOut {
    let probes: Vec<Option<Value>> = if do_probe {
        std::thread::scope(|sc| {
            let hs: Vec<_> = reg
                .models
                .iter()
                .map(|m| sc.spawn(move || m.is_live().then(|| probe(m))))
                .collect();
            hs.into_iter().map(|h| h.join().ok().flatten()).collect()
        })
    } else {
        vec![None; reg.models.len()]
    };
    let mut text = String::new();
    let mut list = vec![];
    for (m, p) in reg.models.iter().zip(probes) {
        let default = reg.default.as_deref() == Some(m.id.as_str());
        text.push_str(&format!(
            "{}{}{} — {} [{}] {}\n",
            m.id,
            if m.aliases.is_empty() {
                String::new()
            } else {
                format!(" ({})", m.aliases.join(", "))
            },
            if default { " *default*" } else { "" },
            m.inputs.join("+"),
            m.status,
            m.summary
        ));
        if !m.good_for.is_empty() {
            text.push_str(&format!("   good for: {}\n", m.good_for.join("; ")));
        }
        if !m.weak_at.is_empty() {
            text.push_str(&format!("   weak at: {}\n", m.weak_at.join("; ")));
        }
        if let Some(l) = m.typical_latency_ms {
            text.push_str(&format!("   typical latency: ~{l} ms warm"));
            if let Some(t) = m.max_state_tokens {
                text.push_str(&format!(" · max state {t} tokens"));
            }
            text.push('\n');
        }
        if let Some(c) = &m.calibration {
            text.push_str(&format!("   calibration: {c}\n"));
        }
        if let Some(p) = &p {
            if p["reachable"].as_bool() == Some(true) {
                text.push_str(&format!(
                    "   probe: up, {} ms{}{}\n",
                    p["latency_ms"].as_f64().unwrap_or(0.0).round(),
                    p["served_by"]
                        .as_str()
                        .map(|s| format!(", served by {s}"))
                        .unwrap_or_default(),
                    if p["valid_answer"].as_bool() == Some(false) {
                        ", INVALID answer shape"
                    } else {
                        ""
                    }
                ));
                if let Some(w) = p["served_mismatch"].as_str() {
                    text.push_str(&format!("   WARNING: {w}\n"));
                }
            } else {
                text.push_str(&format!(
                    "   probe: DOWN — {}\n",
                    p["error"].as_str().unwrap_or("?")
                ));
            }
        }
        let mut v = serde_json::to_value(m).unwrap_or(Value::Null);
        if let Some(o) = v.as_object_mut() {
            o.remove("auth");
            o.insert("default".into(), json!(default));
            if let Some(p) = p {
                o.insert("probe".into(), p);
            }
        }
        list.push(v);
    }
    if reg.models.is_empty() {
        text.push_str("No models configured. Set S1_MCP_REGISTRY or SYSTEMONE_URL, or add entries to ~/.config/s1-mcp/models.json.\n");
    }
    for w in &reg.warnings {
        text.push_str(&format!("warning: {w}\n"));
    }
    if let Some(p) = crate::registry::overlay_path() {
        text.push_str(&format!(
            "Add your own endpoint (any /v1/systemone API) in {}.",
            p.display()
        ));
    }
    ToolOut::ok(
        text,
        json!({"default": reg.default, "models": list, "warnings": reg.warnings}),
    )
}

fn report(args: &Value) -> ToolOut {
    let r = log::report(
        args.get("use_case").and_then(Value::as_str),
        args.get("model").and_then(Value::as_str),
        args.get("since_days").and_then(Value::as_f64),
    );
    let mut text = String::new();
    let groups = r["groups"].as_array().cloned().unwrap_or_default();
    if groups.is_empty() {
        text.push_str("No System One calls logged yet on this host. Try s1_decide on a small judgement you'd otherwise make yourself.\n");
    } else {
        text.push_str("use case × model: calls (errors) · rated accuracy · useful/rated · strong share · p50 latency\n");
        for g in &groups {
            text.push_str(&format!(
                "{} × {}: {} ({}) · {} · {}/{} · {} · {} ms\n",
                g["use_case"].as_str().unwrap_or("?"),
                g["model"].as_str().unwrap_or("?"),
                g["calls"],
                g["errors"],
                g["accuracy"]
                    .as_f64()
                    .map(|a| format!(
                        "{:.0}% of {}",
                        a * 100.0,
                        g["right"].as_u64().unwrap_or(0) + g["wrong"].as_u64().unwrap_or(0)
                    ))
                    .unwrap_or_else(|| "unrated".into()),
                g["useful"],
                g["rated"],
                g["strong_share"]
                    .as_f64()
                    .map(|s| format!("{:.0}% strong", s * 100.0))
                    .unwrap_or_default(),
                g["p50_latency_ms"]
                    .as_f64()
                    .map(|l| l.to_string())
                    .unwrap_or_else(|| "-".into()),
            ));
        }
    }
    let unrated = r["unrated_total"].as_u64().unwrap_or(0);
    if unrated > 0 {
        text.push_str(&format!("{unrated} calls not yet rated; most recent:\n"));
        for u in r["unrated_recent"].as_array().into_iter().flatten() {
            text.push_str(&format!(
                "   {} {} ({})\n",
                u["call_id"].as_str().unwrap_or("?"),
                u["use_case"].as_str().unwrap_or("?"),
                u["at"].as_str().unwrap_or("?")
            ));
        }
    }
    text.push_str(&format!("log: {}", r["log"].as_str().unwrap_or("?")));
    ToolOut::ok(text, r)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_rfc4648() {
        assert_eq!(b64encode(b""), "");
        assert_eq!(b64encode(b"M"), "TQ==");
        assert_eq!(b64encode(b"Ma"), "TWE=");
        assert_eq!(b64encode(b"Man"), "TWFu");
        assert_eq!(b64encode(b"Many hands"), "TWFueSBoYW5kcw==");
    }

    #[test]
    fn use_case_is_normalised_and_required() {
        assert_eq!(
            use_case(&json!({"use_case": "CI failure  triage!"})).unwrap(),
            "ci-failure-triage"
        );
        assert!(use_case(&json!({})).is_err());
    }

    #[test]
    fn images_reject_video_and_garbage() {
        let dir = std::env::temp_dir().join(format!("s1-mcp-img-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let png = dir.join("a.png");
        std::fs::write(&png, b"\x89PNG\r\n\x1a\nrest").unwrap();
        let mp4 = dir.join("a.mp4");
        std::fs::write(&mp4, b"\0\0\0\x18ftypmp42").unwrap();
        let ok = load_images(
            &json!({"images": [png.to_str().unwrap(), "data:image/png;base64,iVBORw0KGgo="]}),
        )
        .unwrap();
        assert_eq!(ok.len(), 2);
        assert!(
            load_images(&json!({"images": [mp4.to_str().unwrap()]}))
                .unwrap_err()
                .contains("video")
        );
        assert!(load_images(&json!({"images": ["hello"]})).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
