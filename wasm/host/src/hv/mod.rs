//! The hypervisor: a thin host that boots the kernel image (a WebAssembly
//! component), gives it narrow capabilities, enforces limits, restores it
//! after traps, and manages kernel images. The agent loop, prompts, gate,
//! knowledge and userspace all live in the image (`../guest`).

pub mod cli;
pub mod guest;
pub mod images;
pub mod journal;
pub mod provider;
#[cfg(test)]
mod provider_tests;
pub mod sandbox;

use guest::{Env, GuestFault, GuestOutcome, Instance, Limits};
use images::{BuildCfg, ImageRec, Images};
use journal::Journal;
use provider::Provider;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

const BASE_PROMPT_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../base-prompt.md");
const BASE_PROMPT_EMBEDDED: &str = include_str!("../../../../base-prompt.md");
/// `wasm/`: the hypervisor and the built-in kernel source. Read-only for
/// the agent (bash, write_file) and for image builds.
const KERNEL_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/..");
/// The repository root (other flavors, evals): read-only for image builds.
const REPO_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
/// Times a task may be resumed after its image trapped.
const MAX_RESUMES: u32 = 3;

pub struct Config {
    pub state_dir: PathBuf,
    pub work_dir: PathBuf,
    /// Clean uses for a userspace trial and clean tasks for an image trial.
    pub soak: u32,
    pub reflect: bool,
    pub max_turns: usize,
    /// Print assistant text to stdout as it arrives (repl).
    pub echo_text: bool,
    pub limits: Limits,
    /// Shared image build area (`BuildCfg::default_cache_dir`).
    pub image_cache: PathBuf,
}

impl Config {
    pub fn new(state_dir: impl Into<PathBuf>) -> Config {
        Config {
            state_dir: state_dir.into(),
            work_dir: std::env::current_dir().unwrap_or_else(|_| ".".into()),
            soak: 3,
            reflect: true,
            max_turns: 40,
            echo_text: true,
            limits: Limits::default(),
            image_cache: BuildCfg::default_cache_dir(),
        }
    }
}

pub struct TaskOutcome {
    pub success: bool,
    pub answer: String,
    pub error: Option<String>,
}

impl From<GuestOutcome> for TaskOutcome {
    fn from(o: GuestOutcome) -> TaskOutcome {
        TaskOutcome { success: o.success, answer: o.answer, error: o.error }
    }
}

pub fn base_prompt() -> String {
    std::fs::read_to_string(BASE_PROMPT_PATH).unwrap_or_else(|_| BASE_PROMPT_EMBEDDED.to_string())
}

fn trace(line: &str) {
    eprintln!("{line}");
}

fn first_line(s: &str) -> String {
    let l = s.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
    l.chars().take(200).collect()
}

pub struct Hypervisor {
    pub cfg: Config,
    env: Arc<Env>,
    images: Arc<Mutex<Images>>,
    journal: Arc<Mutex<Journal>>,
    guest: Option<Instance>,
}

impl Hypervisor {
    /// Opens the state dir, replays the image history and boots the live image.
    pub fn open(mut cfg: Config, provider: Box<dyn Provider>) -> Result<Hypervisor, String> {
        let state_abs = std::path::absolute(&cfg.state_dir).map_err(|e| e.to_string())?;
        let journal = Arc::new(Mutex::new(Journal::open(&state_abs)?));
        cfg.state_dir = state_abs.canonicalize().map_err(|e| format!("{}: {e}", state_abs.display()))?;
        cfg.work_dir = cfg.work_dir.canonicalize().map_err(|e| format!("{}: {e}", cfg.work_dir.display()))?;
        let wd = cfg.work_dir.clone();
        let kernel_dir = sandbox::resolve(&wd, KERNEL_DIR);
        let protected = vec![cfg.state_dir.clone(), kernel_dir.clone(), sandbox::resolve(&wd, BASE_PROMPT_PATH)];
        let base = base_prompt();
        let build = BuildCfg { cache_dir: cfg.image_cache.clone(), read_only: vec![sandbox::resolve(&wd, REPO_DIR)] };
        let images = Images::open(&cfg.state_dir, journal.clone(), cfg.soak, build, base.clone(), kernel_dir.clone(), cfg.limits.clone())?;
        let images = Arc::new(Mutex::new(images));
        let (im1, im2) = (images.clone(), images.clone());
        let env = Arc::new(Env {
            state_dir: cfg.state_dir.clone(),
            work_dir: cfg.work_dir.clone(),
            kernel_dir,
            protected,
            base_prompt: base,
            soak: cfg.soak,
            reflect: cfg.reflect,
            max_turns: cfg.max_turns,
            echo_text: cfg.echo_text,
            journal: journal.clone(),
            provider: Arc::new(Mutex::new(provider)),
            propose: Some(Arc::new(move |req: &Value| im1.lock().unwrap_or_else(|e| e.into_inner()).propose(req))),
            images: Some(Arc::new(move || im2.lock().unwrap_or_else(|e| e.into_inner()).table())),
            allow_bash: true,
        });
        let mut hv = Hypervisor { cfg, env, images, journal, guest: None };
        hv.boot()?;
        Ok(hv)
    }

    fn images(&self) -> std::sync::MutexGuard<'_, Images> {
        self.images.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Boots the live image. An image other than the built-in one that
    /// fails to boot is reverted and the next one is tried.
    fn boot(&mut self) -> Result<(), String> {
        self.guest = None;
        loop {
            let v = self.images().live;
            let component = self.images().component(v);
            let booted = component.and_then(|c| Instance::boot(&c, v, self.env.clone(), self.cfg.limits.clone()).map_err(|f| f.report));
            match booted {
                Ok(inst) => {
                    if v != 1 {
                        trace(&format!("[hv] booted kernel image v{v}"));
                    }
                    self.guest = Some(inst);
                    return Ok(());
                }
                Err(e) if v == 1 => return Err(format!("the built-in kernel image failed to boot: {e}")),
                Err(e) => {
                    trace(&format!("[hv] image v{v} failed to boot: {}", first_line(&e)));
                    self.images().revert(v, &format!("failed to boot: {}", first_line(&e)));
                }
            }
        }
    }

    /// The running instance, booting the live image if needed.
    fn guest(&mut self) -> Result<&mut Instance, String> {
        let live = self.images().live;
        if self.guest.as_ref().is_none_or(|g| g.dead || g.image != live) {
            self.boot()?;
        }
        self.guest.as_mut().ok_or_else(|| "no kernel instance".to_string())
    }

    pub fn run_task(&mut self, task: &str) -> TaskOutcome {
        self.task(|g| g.run_task(task))
    }

    pub fn refine(&mut self) -> TaskOutcome {
        self.task(|g| g.refine())
    }

    /// Runs one task in the image. If the image traps, the hypervisor
    /// reverts it if it is a trial, boots a fresh instance (which replays
    /// the journal) and resumes the task from its last checkpoint, with the
    /// trap report as the result of the interrupted tool calls.
    fn task(&mut self, first: impl FnOnce(&mut Instance) -> Result<GuestOutcome, GuestFault>) -> TaskOutcome {
        let ran = match self.guest() {
            Ok(g) => g.image,
            Err(e) => return TaskOutcome { success: false, answer: String::new(), error: Some(e) },
        };
        let mut res = first(self.guest.as_mut().expect("booted"));
        let (mut trapped, mut resumes) = (false, 0);
        let outcome = loop {
            let fault = match res {
                Ok(o) => break TaskOutcome::from(o),
                Err(f) => f,
            };
            trapped = true;
            let (image, checkpoint) = match self.guest.as_mut() {
                Some(g) => (g.image, g.take_checkpoint()),
                None => (ran, None),
            };
            trace(&format!("[hv] {}", first_line(&fault.report)));
            self.images().revert(image, &format!("trapped: {}", first_line(&fault.report)));
            if let Err(e) = self.boot() {
                self.finish_unfinished(&fault.report);
                break TaskOutcome { success: false, answer: String::new(), error: Some(format!("{}\n{e}", fault.report)) };
            }
            let guest = self.guest.as_mut().expect("booted");
            match checkpoint {
                Some(cp) if resumes < MAX_RESUMES => {
                    resumes += 1;
                    trace(&format!("[hv] restored image v{} from the journal; resuming the task", guest.image));
                    res = guest.resume_task(&cp, &fault.report);
                }
                _ => {
                    self.finish_unfinished(&fault.report);
                    break TaskOutcome { success: false, answer: String::new(), error: Some(fault.report) };
                }
            }
        };
        if !trapped {
            let task_id = self.last_task_id();
            self.images().task_done(ran, task_id, outcome.success);
        }
        outcome
    }

    fn events(&self) -> Vec<Value> {
        self.journal.lock().unwrap_or_else(|e| e.into_inner()).read_all().unwrap_or_default()
    }

    fn last_task_id(&self) -> u64 {
        self.events().iter().rev().find(|e| e["type"] == "task_finished").and_then(|e| e["task_id"].as_u64()).unwrap_or(0)
    }

    /// A task whose image could not finish it is recorded as failed.
    fn finish_unfinished(&self, report: &str) {
        let events = self.events();
        let Some(id) = events.iter().rev().find(|e| e["type"] == "task_started").and_then(|e| e["task_id"].as_u64()) else {
            return;
        };
        if events.iter().any(|e| e["type"] == "task_finished" && e["task_id"].as_u64() == Some(id)) {
            return;
        }
        let ev = json!({"task_id": id, "success": false, "error": report, "summary": "", "by": "hypervisor"});
        if let Err(e) = self.journal.lock().unwrap_or_else(|e| e.into_inner()).append_host("task_finished", ev) {
            trace(&format!("[journal] write failed: {e}"));
        }
    }

    /// A non-task command in the image (`defs`, `knowledge`, `/show`, ...).
    pub fn command(&mut self, line: &str) -> Result<String, String> {
        let r = self.guest()?.command(line);
        match r {
            Ok(r) => r,
            Err(f) => {
                trace(&format!("[hv] {}", first_line(&f.report)));
                Err(f.report)
            }
        }
    }

    /// Runs the running image's own `self-test` export.
    pub fn self_test(&mut self) -> Result<String, String> {
        match self.guest()?.self_test() {
            Ok(r) => r,
            Err(f) => Err(f.report),
        }
    }

    /// Builds and tests a kernel image directly (as `propose_kernel` does).
    pub fn propose_image(&mut self, req: &Value) -> Result<String, String> {
        self.images().propose(req)
    }

    pub fn images_table(&self) -> String {
        self.images().table()
    }

    pub fn live_image(&self) -> u32 {
        self.images().live
    }

    pub fn image_recs(&self) -> Vec<ImageRec> {
        self.images().recs.clone()
    }

    /// The image the current instance runs (None before the first boot or after a trap).
    pub fn running_image(&self) -> Option<u32> {
        self.guest.as_ref().filter(|g| !g.dead).map(|g| g.image)
    }

    pub fn state_dir(&self) -> &std::path::Path {
        &self.cfg.state_dir
    }
}
