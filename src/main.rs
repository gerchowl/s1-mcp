//! s1-mcp — System One decision models as MCP tools for coding agents.

mod http;
mod log;
mod mcp;
mod registry;
mod s1;
mod tools;

use std::path::PathBuf;

const HELP: &str =
    "s1-mcp — System One decision models (Clef, Kev, Jev, any /v1/systemone endpoint) as MCP tools

USAGE
  s1-mcp [serve] [--registry PATH]   run the MCP server on stdio (what harnesses start)
  s1-mcp models [--probe]            list registered models (--probe: measure each live one)
  s1-mcp report [--use-case X] [--model M] [--since-days N] [--json]
                                     where System One helped, from this host's call log
  s1-mcp guide                       print the agent guide
  s1-mcp --version | --help

MODELS come from $S1_MCP_REGISTRY (or --registry), then ~/.config/s1-mcp/models.json
(personal overlay, overrides by id), else $SYSTEMONE_URL / $SYSTEMONE_URL_FULL.
CALL LOG: $S1_MCP_LOG_DIR or ~/.local/state/s1-mcp/calls.jsonl (0600; S1_MCP_LOG=off disables).";

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut take = |flag: &str| -> Option<String> {
        let i = args.iter().position(|a| a == flag)?;
        args.remove(i);
        (i < args.len()).then(|| args.remove(i))
    };
    let registry = take("--registry").map(PathBuf::from);
    let use_case = take("--use-case");
    let model = take("--model");
    let since = take("--since-days").and_then(|s| s.parse::<f64>().ok());
    let flag = |args: &mut Vec<String>, f: &str| match args.iter().position(|a| a == f) {
        Some(i) => {
            args.remove(i);
            true
        }
        None => false,
    };
    let probe = flag(&mut args, "--probe");
    let as_json = flag(&mut args, "--json");
    let cmd = args.first().map(String::as_str).unwrap_or("serve");
    match cmd {
        "serve" => {
            if let Err(e) = mcp::serve(registry) {
                eprintln!("s1-mcp: {e}");
                std::process::exit(1);
            }
        }
        "models" => {
            let o = tools::models(&registry::Registry::load(registry.as_deref()), probe);
            print_out(&o, as_json);
        }
        "report" => {
            let r = log::report(use_case.as_deref(), model.as_deref(), since);
            if as_json {
                println!("{}", serde_json::to_string_pretty(&r).unwrap_or_default());
            } else {
                let mut a = serde_json::json!({});
                if let Some(u) = &use_case {
                    a["use_case"] = serde_json::json!(u);
                }
                if let Some(m) = &model {
                    a["model"] = serde_json::json!(m);
                }
                if let Some(s) = since {
                    a["since_days"] = serde_json::json!(s);
                }
                print_out(
                    &tools::call(&registry::Registry::default(), "s1_report", &a),
                    false,
                );
            }
        }
        "guide" => print!("{}", mcp::GUIDE),
        "--version" | "-V" | "version" => println!("s1-mcp {}", env!("CARGO_PKG_VERSION")),
        "--help" | "-h" | "help" => println!("{HELP}"),
        other => {
            eprintln!("s1-mcp: unknown command {other:?}\n\n{HELP}");
            std::process::exit(2);
        }
    }
}

fn print_out(o: &tools::ToolOut, as_json: bool) {
    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&o.data).unwrap_or_default()
        );
    } else {
        println!("{}", o.text);
    }
    if o.is_error {
        std::process::exit(1);
    }
}
