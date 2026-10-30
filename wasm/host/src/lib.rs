//! cmh, WASM flavor: a continual, self-modifying coding-agent harness that
//! runs entirely inside a WebAssembly component (the kernel image, `../guest`)
//! under a thin Wasmtime hypervisor (`src/hv`). The agent can rewrite its own
//! userspace (Forth) and its own kernel image.

pub mod hv;

pub use hv::{Config, Hypervisor, TaskOutcome};
