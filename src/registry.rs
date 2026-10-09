//! The model registry: which System One endpoints exist and what each is for.
//!
//! s1-mcp knows nothing about any particular model. Every model is an entry
//! here, read from (later sources override earlier ones, by `id`):
//!
//!   1. `$S1_MCP_REGISTRY` (or `--registry PATH`): the deployed registry, e.g.
//!      the one g-fleet renders from lib/system-one.nix.
//!   2. `$XDG_CONFIG_HOME/s1-mcp/models.json`: a personal overlay. Add an
//!      endpoint here to try it without touching the fleet.
//!   3. Neither present: `$SYSTEMONE_URL` (and `$SYSTEMONE_URL_FULL`) become
//!      anonymous entries, so the server still works on a bare host.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Model {
    /// Stable name agents pass as `model` (e.g. "clef-flash").
    pub id: String,
    /// Extra names that resolve to this entry ("fast", "full", …).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
    /// Full POST URL of the `/v1/systemone` endpoint.
    pub url: String,
    /// Value for the request's `model` field. Omitted when the endpoint
    /// serves exactly one model (Kev, the sage per-port shim).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_model: Option<String>,
    /// Names the endpoint may report in the response's `model` field. A
    /// different name means the port is serving another model than the
    /// registry says (e.g. not yet cut over); every answer then carries a
    /// warning. Empty: check against `request_model`, if any.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub served_as: Vec<String>,
    /// Input kinds the model accepts: "text", "image" (video is not supported
    /// by any engine yet).
    #[serde(default = "text_only")]
    pub inputs: Vec<String>,
    /// Where images go in the request body: a top-level array of base64
    /// strings under this key. Engines differ; the default is Ollama's.
    #[serde(default = "default_image_field")]
    pub image_field: String,
    /// "fast" / "full" / free text — a hint for picking, not semantics.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<String>,
    /// One line: what this model is.
    #[serde(default)]
    pub summary: String,
    /// Where it has been measured or is expected to do well.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub good_for: Vec<String>,
    /// Known weaknesses: route these elsewhere or don't trust the answer.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub weak_at: Vec<String>,
    /// Longest state the endpoint accepts (tokens), if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_state_tokens: Option<u64>,
    /// Typical warm latency for a short state, for planning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub typical_latency_ms: Option<u64>,
    /// Hard per-call deadline.
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
    /// What is known about calibration (are the probabilities trustworthy as
    /// probabilities, and on which data was that checked).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calibration: Option<String>,
    /// "live" (use it), "pending" (declared, not yet serving) or "disabled".
    #[serde(default = "default_status")]
    pub status: String,
    /// Hosted endpoints: where the API key comes from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<Auth>,
    /// Where this entry was read from (filled in at load time).
    #[serde(default, skip_deserializing)]
    pub source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Auth {
    /// Read the key from this environment variable…
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<String>,
    /// …or from the first line of this file (`~` expands to $HOME).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    /// Header name; default `Authorization` with a `Bearer ` prefix.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<String>,
}

fn text_only() -> Vec<String> {
    vec!["text".into()]
}
fn default_image_field() -> String {
    "images".into()
}
fn default_timeout_ms() -> u64 {
    10_000
}
fn default_status() -> String {
    "live".into()
}

#[derive(Debug, Default, Deserialize)]
struct RegistryFile {
    #[serde(default)]
    default: Option<String>,
    #[serde(default)]
    models: Vec<Model>,
}

#[derive(Debug, Clone, Default)]
pub struct Registry {
    pub models: Vec<Model>,
    pub default: Option<String>,
    /// Problems found while loading. Reported by `s1_models`, never fatal: a
    /// broken overlay must not take the whole server down.
    pub warnings: Vec<String>,
}

impl Model {
    pub fn accepts(&self, input: &str) -> bool {
        self.inputs.iter().any(|i| i == input)
    }

    pub fn is_live(&self) -> bool {
        self.status == "live"
    }

    /// A warning when the endpoint says it served something unexpected.
    pub fn served_mismatch(&self, served_by: Option<&str>) -> Option<String> {
        let sb = served_by?;
        let expected: Vec<&str> = if self.served_as.is_empty() {
            self.request_model.iter().map(String::as_str).collect()
        } else {
            self.served_as.iter().map(String::as_str).collect()
        };
        (!expected.is_empty() && !expected.iter().any(|e| e.eq_ignore_ascii_case(sb))).then(|| {
            format!(
                "{} answered as {sb:?}, but the registry expects {}: this port may be serving a different model; don't attribute these answers to {}",
                self.id,
                expected.join(" / "),
                self.id
            )
        })
    }

    pub fn matches(&self, name: &str) -> bool {
        self.id.eq_ignore_ascii_case(name)
            || self.aliases.iter().any(|a| a.eq_ignore_ascii_case(name))
    }

    /// Request headers for this endpoint (auth only, for now).
    pub fn headers(&self) -> Result<Vec<(String, String)>, String> {
        let Some(a) = &self.auth else {
            return Ok(vec![]);
        };
        let key = if let Some(var) = &a.env {
            std::env::var(var).ok().filter(|v| !v.trim().is_empty())
        } else {
            None
        };
        let key = match (key, &a.file) {
            (Some(k), _) => k,
            (None, Some(f)) => {
                let p = expand_home(f);
                std::fs::read_to_string(&p)
                    .map_err(|e| {
                        format!(
                            "model {}: cannot read API key file {}: {e}",
                            self.id,
                            p.display()
                        )
                    })?
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .trim()
                    .to_string()
            }
            (None, None) => {
                return Err(format!(
                    "model {}: needs an API key in ${} but it is unset",
                    self.id,
                    a.env.as_deref().unwrap_or("?")
                ));
            }
        };
        let header = a.header.clone().unwrap_or_else(|| "Authorization".into());
        let value = if header.eq_ignore_ascii_case("authorization") {
            format!("Bearer {key}")
        } else {
            key
        };
        Ok(vec![(header, value)])
    }
}

pub fn expand_home(p: &str) -> PathBuf {
    match (p.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(h)) => Path::new(&h).join(rest),
        _ => PathBuf::from(p),
    }
}

pub fn overlay_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| Path::new(&h).join(".config")))?;
    Some(base.join("s1-mcp").join("models.json"))
}

impl Registry {
    /// Load the deployed registry, the personal overlay, and the env fallback.
    pub fn load(explicit: Option<&Path>) -> Registry {
        let mut reg = Registry::default();
        let deployed = explicit
            .map(Path::to_path_buf)
            .or_else(|| std::env::var_os("S1_MCP_REGISTRY").map(PathBuf::from));
        if let Some(p) = &deployed {
            reg.merge_file(p, true);
        }
        if let Some(p) = overlay_path() {
            reg.merge_file(&p, false);
        }
        if reg.models.is_empty() {
            reg.add_env_models();
        }
        if reg
            .default
            .as_deref()
            .is_some_and(|d| reg.find(d).is_none())
        {
            reg.warnings.push(format!(
                "default model {:?} is not in the registry",
                reg.default.as_deref().unwrap()
            ));
            reg.default = None;
        }
        reg
    }

    fn merge_file(&mut self, path: &Path, required: bool) {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if !required && e.kind() == std::io::ErrorKind::NotFound => return,
            Err(e) => {
                self.warnings
                    .push(format!("registry {}: {e}", path.display()));
                return;
            }
        };
        match serde_json::from_str::<RegistryFile>(&text) {
            Ok(f) => {
                if f.default.is_some() {
                    self.default = f.default;
                }
                for mut m in f.models {
                    m.source = path.display().to_string();
                    if let Err(e) = m.validate() {
                        self.warnings.push(format!("{}: {e}", path.display()));
                        continue;
                    }
                    self.models.retain(|o| o.id != m.id);
                    self.models.push(m);
                }
            }
            Err(e) => self
                .warnings
                .push(format!("registry {}: {e}", path.display())),
        }
    }

    fn add_env_models(&mut self) {
        for (var, id, inputs) in [
            ("SYSTEMONE_URL", "default", text_only()),
            (
                "SYSTEMONE_URL_FULL",
                "full",
                vec!["text".into(), "image".into()],
            ),
        ] {
            if let Ok(url) = std::env::var(var) {
                // watcher-s1 accepts a comma/space-separated list; first wins here.
                let Some(url) = url.split([',', ' ']).find(|s| !s.is_empty()) else {
                    continue;
                };
                self.models.push(Model {
                    id: id.into(),
                    aliases: vec![],
                    url: url.into(),
                    request_model: None,
                    served_as: vec![],
                    inputs,
                    image_field: default_image_field(),
                    tier: None,
                    summary: format!(
                        "Unnamed System One endpoint from ${var} (no registry configured)."
                    ),
                    good_for: vec![],
                    weak_at: vec![],
                    max_state_tokens: None,
                    typical_latency_ms: None,
                    timeout_ms: default_timeout_ms(),
                    calibration: None,
                    status: "live".into(),
                    auth: None,
                    source: format!("${var}"),
                });
            }
        }
        if self.default.is_none() && !self.models.is_empty() {
            self.default = Some(self.models[0].id.clone());
        }
    }

    pub fn find(&self, name: &str) -> Option<&Model> {
        self.models.iter().find(|m| m.matches(name))
    }

    /// Pick a model for a call. Explicit name wins; otherwise the default if it
    /// can take the inputs, else the first live model that can.
    pub fn pick(&self, name: Option<&str>, needs_image: bool) -> Result<&Model, String> {
        if let Some(n) = name {
            let m = self.find(n).ok_or_else(|| {
                format!(
                    "no model named {n:?}. Known: {}. Call s1_models to see what each is for.",
                    self.names()
                )
            })?;
            if needs_image && !m.accepts("image") {
                return Err(format!(
                    "model {:?} is text-only but images were given. Image-capable: {}.",
                    m.id,
                    self.names_accepting("image")
                ));
            }
            if !m.is_live() {
                return Err(format!(
                    "model {:?} is {} (not serving). Live models: {}.",
                    m.id,
                    m.status,
                    self.live_names()
                ));
            }
            return Ok(m);
        }
        if self.models.is_empty() {
            return Err("no System One models are configured: set S1_MCP_REGISTRY, SYSTEMONE_URL, or add one to ~/.config/s1-mcp/models.json".into());
        }
        let ok = |m: &&Model| m.is_live() && (!needs_image || m.accepts("image"));
        if let Some(d) = self
            .default
            .as_deref()
            .and_then(|d| self.find(d))
            .filter(ok)
        {
            return Ok(d);
        }
        self.models.iter().find(ok).ok_or_else(|| {
            if needs_image {
                format!(
                    "no live model accepts images. Image-capable (any status): {}.",
                    self.names_accepting("image")
                )
            } else {
                format!("no model is live. Declared: {}.", self.names())
            }
        })
    }

    pub fn names(&self) -> String {
        join(self.models.iter().map(|m| m.id.as_str()))
    }
    fn live_names(&self) -> String {
        join(
            self.models
                .iter()
                .filter(|m| m.is_live())
                .map(|m| m.id.as_str()),
        )
    }
    fn names_accepting(&self, input: &str) -> String {
        join(
            self.models
                .iter()
                .filter(|m| m.accepts(input))
                .map(|m| m.id.as_str()),
        )
    }
}

fn join<'a>(it: impl Iterator<Item = &'a str>) -> String {
    let v: Vec<&str> = it.collect();
    if v.is_empty() {
        "(none)".into()
    } else {
        v.join(", ")
    }
}

impl Model {
    fn validate(&self) -> Result<(), String> {
        if self.id.trim().is_empty() {
            return Err("a model entry has an empty id".into());
        }
        crate::http::parse_url(&self.url).map_err(|e| format!("model {}: {e}", self.id))?;
        if !matches!(self.status.as_str(), "live" | "pending" | "disabled") {
            return Err(format!(
                "model {}: status must be live, pending or disabled",
                self.id
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(id: &str, inputs: &[&str], status: &str) -> Model {
        serde_json::from_value(serde_json::json!({
            "id": id, "url": "http://h:1/v1/systemone", "inputs": inputs, "status": status,
            "aliases": [format!("{id}-alias")]
        }))
        .unwrap()
    }

    fn reg() -> Registry {
        Registry {
            models: vec![
                m("fast", &["text"], "live"),
                m("full", &["text", "image"], "live"),
                m("next", &["text"], "pending"),
            ],
            default: Some("fast".into()),
            warnings: vec![],
        }
    }

    #[test]
    fn picks_default_then_capable() {
        let r = reg();
        assert_eq!(r.pick(None, false).unwrap().id, "fast");
        assert_eq!(r.pick(None, true).unwrap().id, "full");
        assert_eq!(r.pick(Some("FULL-alias"), false).unwrap().id, "full");
    }

    #[test]
    fn flags_a_port_serving_another_model() {
        let mut x = m("clef-flash", &["text"], "live");
        assert!(x.served_mismatch(Some("kev-latest")).is_none());
        x.served_as = vec!["s1-fast".into()];
        assert!(x.served_mismatch(Some("S1-FAST")).is_none());
        assert!(
            x.served_mismatch(Some("kev-latest"))
                .unwrap()
                .contains("different model")
        );
        assert!(x.served_mismatch(None).is_none());
    }

    #[test]
    fn explains_refusals() {
        let r = reg();
        assert!(
            r.pick(Some("fast"), true)
                .unwrap_err()
                .contains("text-only")
        );
        assert!(r.pick(Some("next"), false).unwrap_err().contains("pending"));
        assert!(
            r.pick(Some("nope"), false)
                .unwrap_err()
                .contains("Known: fast, full, next")
        );
    }

    #[test]
    fn overlay_overrides_by_id() {
        let dir = std::env::temp_dir().join(format!("s1-mcp-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.json");
        std::fs::write(&a, r#"{"default":"x","models":[{"id":"x","url":"http://a:1/v1/systemone"},{"id":"bad","url":"ftp://no"}]}"#).unwrap();
        let mut r = Registry::default();
        r.merge_file(&a, true);
        assert_eq!(r.models.len(), 1);
        assert_eq!(r.warnings.len(), 1);
        let b = dir.join("b.json");
        std::fs::write(
            &b,
            r#"{"models":[{"id":"x","url":"http://b:2/v1/systemone","inputs":["text","image"]}]}"#,
        )
        .unwrap();
        r.merge_file(&b, false);
        assert_eq!(r.models.len(), 1);
        assert_eq!(r.models[0].url, "http://b:2/v1/systemone");
        assert_eq!(r.default.as_deref(), Some("x"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
