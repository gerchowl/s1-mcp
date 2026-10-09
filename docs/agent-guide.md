# System One: fast decisions with calibrated probabilities

System One models (Clef, Kev, Jev, and others) take a piece of **state** (text, JSON and, on some models, images) and **typed questions** about it, and return **probabilities**, not prose. They answer in tens of milliseconds and don't hallucinate free text, because they never generate any. Use them where you would otherwise spend an LLM turn on a small judgement, or where you want a second, independent, cheap opinion.

## The three question types

| type | asks | answer you get |
|---|---|---|
| `noul` | a yes/no question | `answer` yes/no, `p_yes` |
| `choice` | pick exactly one option | `answer`, `p`, `runner_up`, `margin`, all `probabilities` |
| `score` | place it on an ordered scale (lowest first) | `most_likely` level, `expected_level` (probability-weighted index), `probabilities` |

Every answer carries a `band`: **strong** (p ≥ 0.85), **lean** (0.65–0.85) or **uncertain** (< 0.65). The band tells you how to read a probability. It does not tell you whether the model is right for *your* task: that is what `s1_rate` measures.

Questions in one call share the state but cannot see each other. Ask all your questions about one state in **one** call: the state is processed once and the extra questions are nearly free.

## Good fits (try these)

- **Verify an outcome:** did this command or test run actually fail? Paste the last ~4 KB of output, not the exit code. Piped commands hide failures.
- **Classify / route:** which kind of error is this; which subsystem owns this issue; is this log line noise.
- **Gate an action:** does this message ask the user something; does this diff touch auth or secrets; is this a routine change.
- **Triage a list cheaply:** score 50 issues or messages one call each, then spend your own attention on the top few.
- **Second opinion:** when you're about to act on your own judgement of a small, well-defined question, compare with `s1_decide`, and record whether it agreed.
- **Images** (image-capable models only): is this screenshot an error dialog; which of these UI states is shown.

## Poor fits (don't)

- Knowledge or arithmetic questions ("what year…", "is this API deprecated"): the answer must be **in the state**.
- Anything that needs generated text, plans or code.
- Very long states. Accuracy drops past a few thousand tokens. Send the relevant excerpt, not the whole file.
- Video. No serving engine supports it yet.

## Writing questions that work

- Make the state self-contained: the model knows only what you put in it.
- One idea per question. Prefer `choice` with descriptive options over several overlapping nouls.
- Describe choice options by **what they mean**, e.g. `{"flaky": "passes on retry, timing or network related", "real": "a deterministic bug in the code under test"}`, not bare labels.
- Add a `none`/`other` option when the real answer may be outside your list. Otherwise the model must pick something.
- For `score`, order levels from lowest to highest and keep them few (3–5).

## Workflow

1. `s1_models`: see which models exist, what each is for, and whether they're up (`probe: true` measures it live).
2. `s1_decide`: ask. Always give a short, reusable **`use_case`** tag (`ci-failure-triage`, `ask-detection`, `pr-risk`, …). The tag is how value gets measured across sessions. Reuse existing tags (see `s1_report`) rather than inventing near-duplicates.
3. Act on **strong** answers if the stakes are low. Treat **lean** and **uncertain** as "look yourself".
4. `s1_rate`: once you know the truth (you checked the log, the test reran, the user answered), rate the call with its `call_id`: `right` / `wrong` / `unsure`, plus whether it was **useful**. Unrated calls teach nothing. Rating takes one call.
5. `s1_compare`: run the same question on several models when you're exploring a new use case or deciding which model to route it to.
6. `s1_report`: see accuracy, latency and usefulness by use case × model, and which recent calls still need a rating.

## Exploring a new use case

When you notice a judgement you keep making by hand, try it on System One for a while. Use `s1_compare` across models, rate honestly (including "not useful"), and check `s1_report` after 10–20 rated calls. A use case is worth automating when one model is right on the strong answers and fast enough for the place it would run (hooks typically need < 1 s). Calibration differs between models: a threshold tuned on one model does not transfer.

Calls are logged on this host only (state lightly redacted for credentials), so they can be replayed against future models. Don't put secrets in the state.
