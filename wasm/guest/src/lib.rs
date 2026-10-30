//! cmh kernel image: the whole harness (agent loop, prompts, gate, journal
//! replay, knowledge, Forth userspace) as a WebAssembly component for
//! wasm32-wasip2. It talks to the outside world only through the
//! hypervisor's imports in `wit/cmh.wit`.

wit_bindgen::generate!({ world: "harness", path: "wit" });

mod forth;
mod kernel;
mod selftest;
mod state;
mod sys;
mod tools;
mod util;

use std::cell::RefCell;
use std::rc::Rc;

thread_local! {
    static KERNEL: RefCell<Option<kernel::Kernel>> = const { RefCell::new(None) };
}

/// Runs `f` on the kernel after catching up with the journal.
fn with_kernel<R>(f: impl FnOnce(&mut kernel::Kernel) -> R) -> Result<R, String> {
    KERNEL.with(|k| match k.borrow_mut().as_mut() {
        Some(k) => {
            k.sync();
            Ok(f(k))
        }
        None => Err("the kernel is not initialised (call init first)".into()),
    })
}

fn outcome(r: Result<kernel::TaskOutcome, String>) -> TaskOutcome {
    match r {
        Ok(o) => TaskOutcome { success: o.success, answer: o.answer, error: o.error },
        Err(e) => TaskOutcome { success: false, answer: String::new(), error: Some(e) },
    }
}

struct Image;

impl Guest for Image {
    fn init() -> Result<(), String> {
        let k = kernel::Kernel::open(Rc::new(sys::HostSys))?;
        KERNEL.with(|cell| *cell.borrow_mut() = Some(k));
        Ok(())
    }

    fn run_task(task: String) -> TaskOutcome {
        outcome(with_kernel(|k| k.run_task(&task)))
    }

    fn resume_task(checkpoint: String, report: String) -> TaskOutcome {
        outcome(with_kernel(|k| k.resume_task(&checkpoint, &report)))
    }

    fn refine() -> TaskOutcome {
        outcome(with_kernel(|k| k.refine()))
    }

    fn command(line: String) -> Result<String, String> {
        with_kernel(|k| k.command(&line))?
    }

    fn self_test() -> Result<String, String> {
        selftest::run()
    }
}

export!(Image);
