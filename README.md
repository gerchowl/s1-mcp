# s1-mcp

An MCP server that gives coding agents **System One decision models**: typed yes/no (`noul`), pick-one (`choice`) and ordered-scale (`score`) questions about a piece of state, answered with calibrated probabilities in tens of milliseconds. It works with any endpoint that speaks the TypeSafe `/v1/systemone` API: Cloudflare's **Clef** (the gerchowl fleet runs `clef-flash` and `clef` on sage), **Kev**, TypeSafe's hosted **Jev**, or your own fine-tune.

It is built for two jobs:

1. **Use**: agents get a small, well-described tool surface for cheap judgements (verify that a run really failed, route an error, detect an ask, triage a list, get a second opinion), with answers already interpreted (`answer`, `p`, `margin`, `band`).
2. **Learn where it helps**: every call is tagged with a `use_case` and logged as a replayable record. Agents rate calls once they know the truth (`s1_rate`), and `s1_report` shows accuracy, usefulness and latency per use case × model. The same log replays against the next model, so swapping Kev for Clef stops being guesswork.

## Tools

| tool | what |
|---|---|
| `s1_decide` | ask one model (default, by id/alias, or auto-routed to an image-capable one when images are given) |
| `s1_compare` | same request on several models in parallel; agreement + confidence + latency |
| `s1_rate` | record right/wrong/mixed/unsure, useful or not, per question or per model |
| `s1_models` | the registry: capabilities, latency, weaknesses, calibration notes; `probe` measures live |
| `s1_report` | where System One helped, from this host's log; lists calls still awaiting a rating |

Resources: `s1://guide` (the agent guide, also sent as the server's `instructions`) and `s1://models`.

## Models: a registry, not code

s1-mcp has no built-in model. Entries come from:

1. `$S1_MCP_REGISTRY` / `--registry PATH`: the deployed registry (g-fleet renders it from `lib/system-one.nix`);
2. `~/.config/s1-mcp/models.json`: a personal overlay that overrides by `id`. Add any endpoint here to try it;
3. otherwise `$SYSTEMONE_URL` / `$SYSTEMONE_URL_FULL`.

```json
{
  "default": "clef-flash",
  "models": [
    {
      "id": "clef-flash", "aliases": ["fast"],
      "url": "http://sage.tail22bd7c.ts.net:8023/v1/systemone",
      "inputs": ["text"], "tier": "fast",
      "summary": "Clef flash (9B), text only, ~15 ms warm",
      "good_for": ["outcome verification", "routing"], "weak_at": ["knowledge questions"],
      "typical_latency_ms": 15, "timeout_ms": 3000, "status": "live"
    },
    {
      "id": "jev", "url": "https://api.typesafe.ai/v1/systemone", "request_model": "jev-latest",
      "auth": { "env": "TYPESAFE_API_KEY" }, "status": "live"
    }
  ]
}
```

Fields: `id`, `aliases`, `url`, `request_model` (sent as `model`; omit for one-model-per-port endpoints), `served_as` (names the endpoint may report back; anything else makes every answer carry a "this port may be serving a different model" warning), `inputs` (`text`, `image`), `image_field` (default `images`: top-level base64 array), `tier`, `summary`, `good_for`, `weak_at`, `max_state_tokens`, `typical_latency_ms`, `timeout_ms` (default 10000), `calibration`, `status` (`live` / `pending` / `disabled`), `auth` (`env` or `file`, optional `header`; default `Authorization: Bearer`).

The registry is re-read on every request, so an overlay edit applies without restarting the agent.

## Call log

`$S1_MCP_LOG_DIR` or `~/.local/state/s1-mcp/calls.jsonl`: directory 0700, file 0600, never sent anywhere. Each call records the state (with a light redaction of credential-shaped strings), the questions, every model's answers and latency, and context (host, harness, repo basename, session id). Images are recorded by origin and size, never their bytes. Ratings are separate records that point at a `call_id`. `S1_MCP_LOG=off` disables logging.

## CLI

```
s1-mcp [serve]            # the MCP server on stdio
s1-mcp models [--probe]   # what is registered, and is it up
s1-mcp report [--use-case X] [--model M] [--since-days N] [--json]
s1-mcp guide              # the agent guide
```

## Register it

Claude Code: `claude mcp add s1 -- s1-mcp`. codex: `[mcp_servers.s1] command = "s1-mcp"`. opencode: `"mcp": {"s1": {"type": "local", "command": ["s1-mcp"]}}`. On the gerchowl fleet this is declarative (g-fleet `fleet.agents.mcpServers`).

## Design notes

- No async runtime and no MCP SDK. The wire is ~100 lines of newline-delimited JSON-RPC, and owning it keeps the tool descriptions, the part agents actually read, exactly as written. Dependencies: rustls, serde, serde_json, webpki-roots.
- Every HTTP call has one wall-clock deadline covering DNS, connect, TLS, write and read (the client is adapted from gerchowl/watcher-s1).
- Answers are validated against the questions asked (missing answer, wrong type, out-of-range probability, a pick that isn't an option). The endpoint's `model` is reported as `served_by`, so a port that changed model underneath you is visible.

## Development

Enter the pinned Rust 1.95.0 environment with `nix develop` or `direnv allow`.
Run `nix flake check` for formatting, clippy, tests, and documentation checks.
Build the binary with `nix build .#default`.
Changes enter through PRs to `dev`; CI must pass (`CI Summary`).

## License

Apache-2.0.
