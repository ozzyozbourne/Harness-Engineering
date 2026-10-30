//! Command line: `cmh [options] <command>`.

use super::provider::{Anthropic, Mock, Provider};
use super::{Config, Hypervisor, TaskOutcome};
use std::io::{BufRead, Write};
use std::path::PathBuf;

const USAGE: &str = "usage: cmh [--state DIR] [--provider anthropic|mock] [--script FILE] [--model M]
           [--soak N] [--no-reflect] <command>

  repl            interactive; each line is a task. Slash commands:
                  /defs  /show NAME [VERSION]  /knowledge  /refine
                  /rollback NAME VERSION  /images  /quit
  run \"TASK\"      one task, prints the final answer, exits 0 on success
  refine          one refine task
  defs            print the registry
  knowledge       print lessons and verdicts
  images          print kernel image versions and their status";

struct Args {
    state: PathBuf,
    provider: String,
    script: Option<PathBuf>,
    model: String,
    soak: u32,
    reflect: bool,
    rest: Vec<String>,
}

fn parse(args: Vec<String>) -> Result<Args, String> {
    let mut a = Args {
        state: PathBuf::from(".cmh"),
        provider: "anthropic".into(),
        script: None,
        model: std::env::var("CMH_MODEL").ok().filter(|m| !m.is_empty()).unwrap_or_else(|| "claude-sonnet-5-5".into()),
        soak: 3,
        reflect: true,
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
            "--soak" => a.soak = val("--soak")?.parse().map_err(|_| "--soak needs a positive integer")?,
            "--no-reflect" => a.reflect = false,
            "-h" | "--help" => return Err(String::new()),
            _ => a.rest.push(arg),
        }
    }
    if a.soak == 0 {
        return Err("--soak needs a positive integer".into());
    }
    Ok(a)
}

fn provider(a: &Args) -> Result<Box<dyn Provider>, String> {
    match a.provider.as_str() {
        "anthropic" => Ok(Box::new(Anthropic::from_env(a.model.clone()))),
        "mock" => {
            let script = a.script.as_ref().ok_or("--provider mock needs --script FILE")?;
            std::fs::create_dir_all(&a.state).map_err(|e| e.to_string())?;
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
    let arity_ok = matches!((cmd.as_str(), a.rest.len()), ("run", 2) | ("refine" | "defs" | "knowledge" | "images" | "repl", 1));
    if !arity_ok {
        eprintln!("{USAGE}");
        return 2;
    }
    let mut cfg = Config::new(a.state.clone());
    cfg.soak = a.soak;
    cfg.reflect = a.reflect;
    cfg.echo_text = cmd == "repl";
    let hv = provider(&a).and_then(|p| Hypervisor::open(cfg, p));
    let mut hv = match hv {
        Ok(h) => h,
        Err(e) => {
            eprintln!("cmh: {e}");
            return 2;
        }
    };
    match cmd.as_str() {
        "run" => finish(hv.run_task(&a.rest[1])),
        "refine" => finish(hv.refine()),
        "images" => {
            print!("{}", hv.images_table());
            0
        }
        "repl" => repl(&mut hv),
        // defs, knowledge
        other => match hv.command(other) {
            Ok(s) => {
                print!("{s}");
                0
            }
            Err(e) => {
                eprintln!("cmh: {e}");
                1
            }
        },
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

fn repl(hv: &mut Hypervisor) -> i32 {
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
        match line {
            "" => {}
            "/quit" | "/exit" => return 0,
            "/images" => print!("{}", hv.images_table()),
            "/refine" => {
                hv.refine();
            }
            _ if line.starts_with('/') => match hv.command(line) {
                Ok(s) => println!("{}", s.trim_end()),
                Err(e) => println!("error: {e}"),
            },
            _ => {
                hv.run_task(line);
            }
        }
    }
}
