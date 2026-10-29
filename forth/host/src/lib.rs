//! cmh, Forth flavor: a continual, self-modifying coding-agent harness whose
//! userspace is written in a small late-bound Forth running inside
//! WebAssembly (Wasmtime, fuel-metered, memory-capped).
//!
//! Everything under `src/kernel/` is the immutable kernel ring.

pub mod kernel;

pub use kernel::{Config, Kernel};
