//! cmh, Rust flavor: a continual, self-modifying coding-agent harness.
//!
//! Everything under `src/kernel/` is the immutable kernel ring. Userspace
//! definitions live in the journal and are compiled to cdylibs at runtime.

pub mod kernel;

pub use kernel::{Config, Kernel};
