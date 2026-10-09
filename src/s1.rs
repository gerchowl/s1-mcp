//! One System One call: validate the questions, build the request, POST it,
//! check the typed answers and turn them into something an agent can act on.

use crate::registry::Model;
use serde_json::{Map, Value, json};
use std::time::{Duration, Instant};

pub const MAX_QUESTIONS: usize = 32;
pub const MAX_IMAGES: usize = 8;
pub const MAX_IMAGE_BYTES: usize = 10 << 20;

/// An image ready to send, plus how it was given (for the log, never the bytes).
#[derive(Debug, Clone)]
pub struct Image {
    pub b64: String,
    pub origin: String,
    pub bytes: usize,
}

/// Normalise and check the agent's questions. Returns the wire form.
///
/// Accepted shorthands (normalised here so every engine sees the canonical
/// TypeSafe shape): `choice.criteria` as an array of option names, and
/// `instructions` under the alias `question`.
pub fn normalise_questions(q: &Value) -> Result<Map<String, Value>, String> {
    let obj = q
        .as_object()
        .ok_or("`questions` must be an object keyed by question id, e.g. {\"failed\": {\"type\": \"noul\", \"instructions\": \"Did the run fail?\"}}")?;
    if obj.is_empty() {
        return Err("`questions` is empty: ask at least one question".into());
    }
    if obj.len() > MAX_QUESTIONS {
        return Err(format!(
            "{} questions; at most {MAX_QUESTIONS} per call",
            obj.len()
        ));
    }
    let mut out = Map::new();
    for (id, spec) in obj {
        let ctx = |m: &str| format!("question {id:?}: {m}");
        if id.trim().is_empty() {
            return Err("a question id is empty".into());
        }
        let spec = spec
            .as_object()
            .ok_or_else(|| ctx("must be an object with `type`"))?;
        let ty = spec
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| ctx("missing `type` (noul | choice | score)"))?;
        let instructions = spec
            .get("instructions")
            .or_else(|| spec.get("question"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let mut w = Map::new();
        w.insert("type".into(), json!(ty));
        match ty {
            "noul" => {
                let ins = instructions
                    .ok_or_else(|| ctx("a noul needs `instructions`: the yes/no question"))?;
                w.insert("instructions".into(), json!(ins));
            }
            "choice" => {
                let crit = spec.get("criteria").ok_or_else(|| {
                    ctx("a choice needs `criteria`: {\"option\": \"what it means\"} (or an array of option names)")
                })?;
                let crit = match crit {
                    Value::Array(a) => {
                        let mut m = Map::new();
                        for o in a {
                            let s = o
                                .as_str()
                                .ok_or_else(|| ctx("choice options must be strings"))?;
                            m.insert(s.into(), json!(s));
                        }
                        m
                    }
                    Value::Object(m) => m
                        .iter()
                        .map(|(k, v)| (k.clone(), if v.is_null() { json!(k) } else { v.clone() }))
                        .collect(),
                    _ => return Err(ctx("`criteria` must be an object or an array")),
                };
                if crit.len() < 2 {
                    return Err(ctx("a choice needs at least 2 options"));
                }
                if let Some(i) = instructions {
                    w.insert("instructions".into(), json!(i));
                }
                w.insert("criteria".into(), Value::Object(crit));
            }
            "score" => {
                let levels = spec
                    .get("criteria")
                    .and_then(Value::as_array)
                    .ok_or_else(|| ctx("a score needs `criteria`: an array of levels, lowest first, e.g. [\"low\", \"medium\", \"high\"]"))?;
                if levels.len() < 2 || levels.len() > 255 {
                    return Err(ctx("a score needs 2..255 levels"));
                }
                if let Some(i) = instructions {
                    w.insert("instructions".into(), json!(i));
                }
                w.insert("criteria".into(), Value::Array(levels.clone()));
            }
            other => {
                return Err(ctx(&format!(
                    "unknown type {other:?}: use noul (yes/no), choice (pick one option) or score (ordered levels)"
                )));
            }
        }
        out.insert(id.clone(), Value::Object(w));
    }
    Ok(out)
}

pub fn build_request(
    m: &Model,
    state: &Value,
    questions: &Map<String, Value>,
    images: &[Image],
) -> Value {
    let mut req = Map::new();
    req.insert("state".into(), state.clone());
    req.insert("questions".into(), Value::Object(questions.clone()));
    if let Some(rm) = &m.request_model {
        req.insert("model".into(), json!(rm));
    }
    if !images.is_empty() {
        req.insert(
            m.image_field.clone(),
            Value::Array(images.iter().map(|i| json!(i.b64)).collect()),
        );
    }
    Value::Object(req)
}

/// The outcome of one call against one model.
#[derive(Debug, Clone)]
pub struct Outcome {
    /// What the endpoint said it served (`model` in the response), if anything.
    pub served_by: Option<String>,
    pub latency_ms: f64,
    /// Server-reported model time, when the engine reports it.
    pub server_ms: Option<f64>,
    pub answers: Map<String, Value>,
    pub raw: Value,
}

pub fn call(m: &Model, request: &Value, timeout: Duration) -> Result<Outcome, String> {
    let headers = m.headers()?;
    let body = serde_json::to_vec(request).map_err(|e| e.to_string())?;
    let t0 = Instant::now();
    let resp = crate::http::post_json(&m.url, &headers, &body, t0 + timeout).map_err(|e| {
        if e == "timeout" {
            format!("{} did not answer within {} ms", m.id, timeout.as_millis())
        } else {
            format!("{} unreachable ({}): {e}", m.id, m.url)
        }
    })?;
    let latency_ms = t0.elapsed().as_secs_f64() * 1000.0;
    let text = String::from_utf8_lossy(&resp.body);
    if resp.status != 200 {
        return Err(format!(
            "{} answered HTTP {}: {}",
            m.id,
            resp.status,
            clip(&text, 600)
        ));
    }
    let raw: Value = serde_json::from_str(&text)
        .map_err(|e| format!("{} sent non-JSON: {e}: {}", m.id, clip(&text, 200)))?;
    let answers = raw
        .get("answers")
        .and_then(Value::as_object)
        .cloned()
        .ok_or_else(|| {
            format!(
                "{} response has no `answers` object: {}",
                m.id,
                clip(&text, 300)
            )
        })?;
    Ok(Outcome {
        served_by: raw.get("model").and_then(Value::as_str).map(str::to_string),
        latency_ms,
        server_ms: raw.get("latency_ms").and_then(Value::as_f64),
        answers,
        raw,
    })
}

/// How sure the model is, in words. Thresholds are deliberately coarse: they
/// say how to *read* a probability, not whether it is right for your task.
pub fn band(p: f64) -> &'static str {
    if p >= 0.85 {
        "strong"
    } else if p >= 0.65 {
        "lean"
    } else {
        "uncertain"
    }
}

/// Validate one typed answer against its question and summarise it.
/// Returns (compact JSON for the agent, one human line).
pub fn interpret(id: &str, q: &Value, a: Option<&Value>) -> (Value, String) {
    let want = q.get("type").and_then(Value::as_str).unwrap_or("?");
    let Some(a) = a else {
        return (
            json!({"error": "no answer returned for this question"}),
            format!("{id}: NO ANSWER (the endpoint dropped this question)"),
        );
    };
    let got = a.get("type").and_then(Value::as_str).unwrap_or(want);
    if got != want {
        return (
            json!({"error": format!("asked a {want}, got a {got}"), "raw": a}),
            format!("{id}: INVALID (asked {want}, got {got})"),
        );
    }
    match want {
        "noul" => match a
            .get("noul")
            .and_then(Value::as_f64)
            .filter(|p| (0.0..=1.0).contains(p))
        {
            Some(p) => {
                let (verdict, conf) = if p >= 0.5 {
                    ("yes", p)
                } else {
                    ("no", 1.0 - p)
                };
                (
                    json!({"type": "noul", "answer": verdict, "p_yes": round(p), "band": band(conf)}),
                    format!("{id}: {verdict}  p(yes)={:.3}  [{}]", p, band(conf)),
                )
            }
            None => (
                json!({"error": "noul without a 0..1 probability", "raw": a}),
                format!("{id}: INVALID noul"),
            ),
        },
        "choice" => {
            let probs = probs_sorted(a);
            let pick = a
                .get("choice")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| probs.first().map(|(k, _)| k.clone()));
            let options: Vec<&String> = q
                .get("criteria")
                .and_then(Value::as_object)
                .map(|m| m.keys().collect())
                .unwrap_or_default();
            match pick {
                Some(pick) if options.is_empty() || options.contains(&&pick) => {
                    let p = probs
                        .iter()
                        .find(|(k, _)| *k == pick)
                        .map(|(_, p)| *p)
                        .unwrap_or(f64::NAN);
                    let runner = probs.iter().find(|(k, _)| *k != pick);
                    let margin = runner.map(|(_, r)| p - r);
                    let mut o = json!({
                        "type": "choice", "answer": pick, "p": round(p), "band": band(p),
                        "probabilities": probs.iter().map(|(k, v)| (k.clone(), json!(round(*v)))).collect::<Map<_, _>>(),
                    });
                    if let Some((r, rp)) = runner {
                        o["runner_up"] = json!(r);
                        o["margin"] = json!(round(p - rp));
                    }
                    let line = match (runner, margin) {
                        (Some((r, rp)), Some(mg)) => format!(
                            "{id}: {pick}  p={p:.3}  [{}]  next: {r} {rp:.3}, margin {mg:.3}",
                            band(p)
                        ),
                        _ => format!("{id}: {pick}  p={p:.3}  [{}]", band(p)),
                    };
                    (o, line)
                }
                Some(pick) => (
                    json!({"error": format!("answered {pick:?}, not one of the options"), "raw": a}),
                    format!("{id}: INVALID (answered {pick:?}, not an option)"),
                ),
                None => (
                    json!({"error": "choice without a pick", "raw": a}),
                    format!("{id}: INVALID choice"),
                ),
            }
        }
        "score" => {
            let legend = a.get("legend").and_then(Value::as_object);
            let probs = probs_sorted(a);
            let expected = a.get("score").and_then(Value::as_f64);
            match (expected, probs.first()) {
                (Some(s), Some((top, p))) => {
                    let label = |k: &str| {
                        legend
                            .and_then(|l| l.get(k))
                            .and_then(Value::as_str)
                            .map(str::to_string)
                            .unwrap_or_else(|| k.to_string())
                    };
                    let level = label(top);
                    (
                        json!({
                            "type": "score", "expected_level": round(s), "most_likely": level, "most_likely_index": top,
                            "p": round(*p), "band": band(*p),
                            "probabilities": probs.iter().map(|(k, v)| (label(k), json!(round(*v)))).collect::<Map<_, _>>(),
                        }),
                        format!(
                            "{id}: {level} (level {top})  p={p:.3}  expected level {s:.2}  [{}]",
                            band(*p)
                        ),
                    )
                }
                _ => (
                    json!({"error": "score without probabilities", "raw": a}),
                    format!("{id}: INVALID score"),
                ),
            }
        }
        _ => (json!({"raw": a}), format!("{id}: {a}")),
    }
}

fn probs_sorted(a: &Value) -> Vec<(String, f64)> {
    let mut v: Vec<(String, f64)> = a
        .get("probabilities")
        .and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .filter_map(|(k, p)| p.as_f64().map(|p| (k.clone(), p)))
                .collect()
        })
        .unwrap_or_default();
    v.sort_by(|a, b| b.1.total_cmp(&a.1));
    v
}

pub fn round(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
}

pub fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalises_shorthands_and_rejects_bad_specs() {
        let q = normalise_questions(&json!({
            "c": {"type": "choice", "question": "Which?", "criteria": ["a", "b"]},
            "n": {"type": "noul", "instructions": "Yes?"},
            "s": {"type": "score", "criteria": ["lo", "hi"]}
        }))
        .unwrap();
        assert_eq!(q["c"]["criteria"], json!({"a": "a", "b": "b"}));
        assert_eq!(q["c"]["instructions"], json!("Which?"));
        assert!(
            normalise_questions(&json!({"n": {"type": "noul"}}))
                .unwrap_err()
                .contains("instructions")
        );
        assert!(
            normalise_questions(&json!({"c": {"type": "choice", "criteria": ["a"]}}))
                .unwrap_err()
                .contains("2 options")
        );
        assert!(
            normalise_questions(&json!({"x": {"type": "bool"}}))
                .unwrap_err()
                .contains("unknown type")
        );
        assert!(normalise_questions(&json!({})).is_err());
    }

    #[test]
    fn interprets_real_kev_shapes() {
        let q = normalise_questions(&json!({
            "o": {"type": "choice", "criteria": {"pass": "all passed", "fail": "one failed"}},
            "a": {"type": "noul", "instructions": "asks?"},
            "s": {"type": "score", "criteria": ["Calm", "Frustrated", "Very angry"]}
        }))
        .unwrap();
        let (o, line) = interpret(
            "o",
            &q["o"],
            Some(
                &json!({"type": "choice", "choice": "fail", "confidence": 0.98, "probabilities": {"pass": 0.0081, "fail": 0.9919}}),
            ),
        );
        assert_eq!(o["answer"], "fail");
        assert_eq!(o["band"], "strong");
        assert_eq!(o["runner_up"], "pass");
        assert!(line.contains("margin"));
        let (a, _) = interpret("a", &q["a"], Some(&json!({"type": "noul", "noul": 0.024})));
        assert_eq!(a["answer"], "no");
        assert_eq!(a["band"], "strong");
        let (s, _) = interpret(
            "s",
            &q["s"],
            Some(&json!({"type": "score", "score": 1.44, "confidence": 0.34,
            "legend": {"0": "Calm", "1": "Frustrated", "2": "Very angry"}, "probabilities": {"0": 0.0, "1": 0.56, "2": 0.44}})),
        );
        assert_eq!(s["most_likely"], "Frustrated");
        assert_eq!(s["band"], "uncertain");
    }

    #[test]
    fn flags_invalid_answers() {
        let q =
            normalise_questions(&json!({"o": {"type": "choice", "criteria": ["x", "y"]}})).unwrap();
        assert!(interpret("o", &q["o"], None).1.contains("NO ANSWER"));
        assert!(
            interpret("o", &q["o"], Some(&json!({"type": "noul", "noul": 0.5})))
                .1
                .contains("INVALID")
        );
        assert!(
            interpret(
                "o",
                &q["o"],
                Some(&json!({"type": "choice", "choice": "z", "probabilities": {"z": 1.0}}))
            )
            .1
            .contains("not an option")
        );
    }
}
