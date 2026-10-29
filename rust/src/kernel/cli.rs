//! Command line: `cmh [options] <command>`.

use super::provider::{Anthropic, Mock, Provider};
use super::{Config, Kernel, TaskOutcome};
use std::io::{BufRead, Write};
use std::path::PathBuf;

const USAGE: &str = "usage: cmh [--state DIR] [--provider anthropic|mock] [--script FILE] [--model M]
           [--soak N] [--no-reflect] <command>

  repl            interactive; each line is a task. Slash commands:
                  /defs  /show NAME [VERSION]  /knowledge  /refine
                  /rollback NAME VERSION  /quit
  run \"TASK\"      one task, prints the final answer, exits 0 on success
  refine          one refine task
  defs            print the registry
  knowledge       print lessons and verdicts";

struct Args {
    state: PathBuf,
    provider: String,
    script: Option<PathBuf>,
    model: String,
    /// `None` keeps `Config::new`'s defaults.
    soak: Option<u32>,
    no_reflect: bool,
    rest: Vec<String>,
}

fn parse(args: Vec<String>) -> Result<Args, String> {
    let mut a = Args {
        state: PathBuf::from(".cmh"),
        provider: "anthropic".into(),
        script: None,
        model: std::env::var("CMH_MODEL").ok().filter(|m| !m.is_empty()).unwrap_or_else(|| "claude-sonnet-5-5".into()),
        soak: None,
        no_reflect: false,
        rest: Vec::new(),
    };
    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        let mut val = |flag: &str| it.next().ok_or(format!("{flag} needs a value"));
        match arg.as_str() {
            "--state" => a.state = val("--state")?.into(),
            "--provider" => a.provider = val("--provider")?,
            "--script" => a.script = Some(val("--script")?.into()),
            "--model" => a.model = val("--model")?,
            "--soak" => a.soak = Some(val("--soak")?.parse().map_err(|_| "--soak needs a positive integer")?),
            "--no-reflect" => a.no_reflect = true,
            "-h" | "--help" => return Err(String::new()),
            _ => a.rest.push(arg),
        }
    }
    if a.soak == Some(0) {
        return Err("--soak needs a positive integer".into());
    }
    Ok(a)
}

fn provider(a: &Args) -> Result<Box<dyn Provider>, String> {
    match a.provider.as_str() {
        "anthropic" => Ok(Box::new(Anthropic::from_env(a.model.clone()))),
        "mock" => {
            let script = a.script.as_ref().ok_or("--provider mock needs --script FILE")?;
            Ok(Box::new(Mock::from_file(script)?.with_log(a.state.join("mock-requests.jsonl"))))
        }
        other => Err(format!("unknown provider `{other}`")),
    }
}

pub fn main(args: Vec<String>) -> i32 {
    let a = match parse(args) {
        Ok(a) => a,
        Err(e) => {
            if !e.is_empty() {
                eprintln!("cmh: {e}");
            }
            eprintln!("{USAGE}");
            return 2;
        }
    };
    let Some(cmd) = a.rest.first().cloned() else {
        eprintln!("{USAGE}");
        return 2;
    };
    let mut cfg = Config::new(a.state.clone());
    if let Some(soak) = a.soak {
        cfg.soak = soak;
    }
    if a.no_reflect {
        cfg.reflect = false;
    }
    cfg.echo_text = cmd == "repl";
    let kernel = provider(&a).and_then(|p| Kernel::open(cfg, p));
    let mut k = match kernel {
        Ok(k) => k,
        Err(e) => {
            eprintln!("cmh: {e}");
            return 2;
        }
    };
    match (cmd.as_str(), a.rest.len()) {
        ("run", 2) => finish(k.run_task(&a.rest[1])),
        ("refine", 1) => finish(k.refine()),
        ("defs", 1) => {
            print!("{}", k.defs_table());
            0
        }
        ("knowledge", 1) => {
            print!("{}", k.knowledge_text());
            0
        }
        ("repl", 1) => repl(&mut k),
        _ => {
            eprintln!("{USAGE}");
            2
        }
    }
}

/// `run`/`refine`: only the final answer on stdout; nonzero exit on failure.
fn finish(o: TaskOutcome) -> i32 {
    if !o.answer.is_empty() {
        println!("{}", o.answer);
    }
    if let Some(e) = &o.error {
        eprintln!("cmh: task failed: {}", e.lines().next().unwrap_or(""));
    }
    if o.success { 0 } else { 1 }
}

fn repl(k: &mut Kernel) -> i32 {
    let stdin = std::io::stdin();
    loop {
        eprint!("cmh> ");
        let _ = std::io::stderr().flush();
        let mut line = String::new();
        match stdin.lock().read_line(&mut line) {
            Ok(0) | Err(_) => return 0,
            Ok(_) => {}
        }
        let line = line.trim();
        let parts: Vec<&str> = line.split_whitespace().collect();
        match parts.as_slice() {
            [] => {}
            ["/quit"] | ["/exit"] => return 0,
            ["/defs"] => print!("{}", k.defs_table()),
            ["/knowledge"] => print!("{}", k.knowledge_text()),
            ["/refine"] => {
                k.refine();
            }
            ["/show", name] => print_result(k.show_def(name, None)),
            ["/show", name, v] => match parse_version(v) {
                Some(v) => print_result(k.show_def(name, Some(v))),
                None => eprintln!("usage: /show NAME [VERSION]"),
            },
            ["/rollback", name, v] => match parse_version(v) {
                Some(v) => print_result(k.rollback_def(name, v)),
                None => eprintln!("usage: /rollback NAME VERSION"),
            },
            [cmd, ..] if cmd.starts_with('/') => eprintln!("unknown command {cmd}"),
            _ => {
                k.run_task(line);
            }
        }
    }
}

/// `3` or `v3`.
fn parse_version(v: &str) -> Option<u32> {
    v.trim_start_matches('v').parse().ok()
}

fn print_result(r: Result<String, String>) {
    match r {
        Ok(s) => println!("{s}"),
        Err(e) => println!("error: {e}"),
    }
}
