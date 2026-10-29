//! The host <-> VM ABI: numbers both sides must agree on.
//!
//! This one file is compiled into the VM (`mod abi;`) and into the host
//! (`#[path = ...] mod abi;` in `host/src/kernel/forth.rs`), so the two
//! copies cannot drift apart.

/// Status of `cmh_define`, `cmh_call` and `cmh_resume`: done, output is the result.
pub const ST_OK: i32 = 0;
/// Failed; the output is a JSON error report.
pub const ST_ERR: i32 = 1;
/// Suspended on an unbound public slot; the output is its name. The host
/// compiles it (or not) and calls `cmh_resume`.
pub const ST_NEED: i32 = 2;

/// Returned by `host_word` when the host word failed; the error message is pending.
pub const HOST_ERR: i64 = i64::MIN;

/// Host word op codes passed to `host_word`.
pub const OP_JSON_GET: i32 = 1;
pub const OP_JSON_LEN: i32 = 2;
pub const OP_JSON_STR: i32 = 3;
pub const OP_FILE_READ: i32 = 4;
pub const OP_FILE_WRITE: i32 = 5;
pub const OP_REMEMBER: i32 = 6;
