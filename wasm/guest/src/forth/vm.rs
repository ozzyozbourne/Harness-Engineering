//! cmh Forth: a small, late-bound Forth interpreter. Copied from
//! `forth/vm` and turned from a singleton WebAssembly module into an
//! instance type, so the kernel image can hold the live VM and scratch VMs
//! at the same time.
//!
//! Each VM owns its data space and string scratch space (fixed heap
//! buffers). Forth addresses are real addresses inside those buffers, and
//! every memory access is checked against them: Forth code cannot touch the
//! rest of the kernel's memory. A step budget stops runaway loops with a
//! `fuel-exhausted` report, so Forth code never needs the hypervisor's
//! trap-and-restore path (which still backs it up).
//!
//! Late binding: every compiled call goes through a slot (an indirection cell
//! with a name). Redefining a word rebinds its slot, so every existing caller
//! uses the new code on its next call. Public slots are named after userspace
//! definitions; an unbound public slot makes the VM suspend with `Need` so
//! the kernel can compile that definition lazily and resume.
//!
//! Errors never panic: they come back as JSON (kind, word, def, version,
//! line, token, data stack, call trace, console output).

use serde_json::Value;
use std::collections::{BTreeMap, HashSet};
use std::fmt::Write as _;

const PERM_SIZE: usize = 1 << 20;
const SCRATCH_SIZE: usize = 2 << 20;
/// Data stack depth.
const DS_MAX: usize = 1024;
/// Call depth (`Exec::frames`).
const RS_MAX: usize = 512;
/// Items pushed with `>r` (`Exec::rstack`).
const US_MAX: usize = 256;
/// Nested DO loops (`Exec::loops`).
const LS_MAX: usize = 64;
const CONSOLE_MAX: usize = 4096;
const BUILDER_MAX: usize = 1 << 20;
const NO_WORD: u32 = u32::MAX;
const FILE_MAX: usize = 1 << 20;

/// File access for the `file-read` and `file-write` host words. The kernel
/// supplies it (checked paths inside the work dir).
pub trait ForthIo {
    fn read(&mut self, path: &str) -> Result<Vec<u8>, String>;
    fn write(&mut self, path: &str, content: &[u8]) -> Result<(), String>;
}

/// Result of `Vm::call` and `Vm::resume`.
pub enum Status {
    /// `run` finished; its output string.
    Ok(Vec<u8>),
    /// A JSON error report.
    Err(String),
    /// Suspended on this unbound public slot (lazy load); call `resume`.
    Need(String),
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum P {
    Dup,
    Drop,
    Swap,
    Over,
    Rot,
    MRot,
    Nip,
    Tuck,
    Dup2,
    Drop2,
    Swap2,
    Over2,
    Pick,
    Depth,
    ToR,
    RFrom,
    RFetch,
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    DivMod,
    Negate,
    Abs,
    Min,
    Max,
    Inc,
    Dec,
    Eq,
    Ne,
    Lt,
    Gt,
    Le,
    Ge,
    ZEq,
    ZLt,
    ZGt,
    And,
    Or,
    Xor,
    Invert,
    True,
    False,
    Within,
    Fetch,
    Store,
    CFetch,
    CStore,
    PlusStore,
    Here,
    Allot,
    Comma,
    CComma,
    Cells,
    I,
    J,
    Type,
    Emit,
    Cr,
    Dot,
    DotS,
    Bl,
    Compare,
    SEq,
    Index,
    Concat,
    SlashString,
    Lower,
    Trim,
    BStart,
    BAdd,
    BChar,
    BNum,
    BEnd,
    NToS,
    SToN,
    JsonGet,
    JsonLen,
    JsonStr,
    FileRead,
    FileWrite,
    Remember,
}

/// name, primitive, items it needs on the data stack, stack effect.
const PRIMS: &[(&str, P, usize, &str)] = &[
    ("dup", P::Dup, 1, "( x -- x x )"),
    ("drop", P::Drop, 1, "( x -- )"),
    ("swap", P::Swap, 2, "( a b -- b a )"),
    ("over", P::Over, 2, "( a b -- a b a )"),
    ("rot", P::Rot, 3, "( a b c -- b c a )"),
    ("-rot", P::MRot, 3, "( a b c -- c a b )"),
    ("nip", P::Nip, 2, "( a b -- b )"),
    ("tuck", P::Tuck, 2, "( a b -- b a b )"),
    ("2dup", P::Dup2, 2, "( a b -- a b a b )"),
    ("2drop", P::Drop2, 2, "( a b -- )"),
    ("2swap", P::Swap2, 4, "( a b c d -- c d a b )"),
    ("2over", P::Over2, 4, "( a b c d -- a b c d a b )"),
    ("pick", P::Pick, 1, "( xn .. x0 n -- xn .. x0 xn )"),
    ("depth", P::Depth, 0, "( -- n )"),
    (">r", P::ToR, 1, "( x -- ) ( R: -- x )"),
    ("r>", P::RFrom, 0, "( -- x ) ( R: x -- )"),
    ("r@", P::RFetch, 0, "( -- x ) ( R: x -- x )"),
    ("+", P::Add, 2, "( a b -- a+b )"),
    ("-", P::Sub, 2, "( a b -- a-b )"),
    ("*", P::Mul, 2, "( a b -- a*b )"),
    ("/", P::Div, 2, "( a b -- a/b )"),
    ("mod", P::Mod, 2, "( a b -- a%b )"),
    ("/mod", P::DivMod, 2, "( a b -- rem quot )"),
    ("negate", P::Negate, 1, "( n -- -n )"),
    ("abs", P::Abs, 1, "( n -- |n| )"),
    ("min", P::Min, 2, "( a b -- min )"),
    ("max", P::Max, 2, "( a b -- max )"),
    ("1+", P::Inc, 1, "( n -- n+1 )"),
    ("1-", P::Dec, 1, "( n -- n-1 )"),
    ("=", P::Eq, 2, "( a b -- flag )"),
    ("<>", P::Ne, 2, "( a b -- flag )"),
    ("<", P::Lt, 2, "( a b -- flag )"),
    (">", P::Gt, 2, "( a b -- flag )"),
    ("<=", P::Le, 2, "( a b -- flag )"),
    (">=", P::Ge, 2, "( a b -- flag )"),
    ("0=", P::ZEq, 1, "( n -- flag )"),
    ("0<", P::ZLt, 1, "( n -- flag )"),
    ("0>", P::ZGt, 1, "( n -- flag )"),
    ("and", P::And, 2, "( a b -- a&b )"),
    ("or", P::Or, 2, "( a b -- a|b )"),
    ("xor", P::Xor, 2, "( a b -- a^b )"),
    ("invert", P::Invert, 1, "( n -- ~n )"),
    ("true", P::True, 0, "( -- -1 )"),
    ("false", P::False, 0, "( -- 0 )"),
    ("within", P::Within, 3, "( n lo hi -- flag ) lo <= n < hi"),
    ("@", P::Fetch, 1, "( addr -- n )"),
    ("!", P::Store, 2, "( n addr -- )"),
    ("c@", P::CFetch, 1, "( addr -- c )"),
    ("c!", P::CStore, 2, "( c addr -- )"),
    ("+!", P::PlusStore, 2, "( n addr -- )"),
    ("here", P::Here, 0, "( -- addr )"),
    ("allot", P::Allot, 1, "( n -- )"),
    (",", P::Comma, 1, "( n -- )"),
    ("c,", P::CComma, 1, "( c -- )"),
    ("cells", P::Cells, 1, "( n -- n*8 )"),
    ("i", P::I, 0, "( -- index )"),
    ("j", P::J, 0, "( -- outer-index )"),
    ("type", P::Type, 2, "( addr len -- )"),
    ("emit", P::Emit, 1, "( c -- )"),
    ("cr", P::Cr, 0, "( -- )"),
    (".", P::Dot, 1, "( n -- )"),
    (".s", P::DotS, 0, "( -- )"),
    ("bl", P::Bl, 0, "( -- 32 )"),
    ("compare", P::Compare, 4, "( a1 u1 a2 u2 -- -1|0|1 )"),
    ("s=", P::SEq, 4, "( a1 u1 a2 u2 -- flag )"),
    ("index", P::Index, 4, "( a1 u1 a2 u2 -- pos|-1 )"),
    ("concat", P::Concat, 4, "( a1 u1 a2 u2 -- a3 u3 )"),
    ("/string", P::SlashString, 3, "( a u n -- a+n u-n )"),
    ("lower", P::Lower, 2, "( a u -- a2 u )"),
    ("trim", P::Trim, 2, "( a u -- a2 u2 )"),
    ("<b", P::BStart, 0, "( -- )"),
    ("b+", P::BAdd, 2, "( a u -- )"),
    ("bc+", P::BChar, 1, "( c -- )"),
    ("bn+", P::BNum, 1, "( n -- )"),
    ("b>", P::BEnd, 0, "( -- a u )"),
    ("n>s", P::NToS, 1, "( n -- a u )"),
    ("s>n", P::SToN, 2, "( a u -- n flag )"),
    ("json-get", P::JsonGet, 4, "( json-a json-u path-a path-u -- a u )"),
    ("json-len", P::JsonLen, 4, "( json-a json-u path-a path-u -- n )"),
    ("json-str", P::JsonStr, 2, "( a u -- a2 u2 )"),
    ("file-read", P::FileRead, 2, "( path-a path-u -- a u )"),
    ("file-write", P::FileWrite, 4, "( a u path-a path-u -- )"),
    ("remember", P::Remember, 2, "( a u -- )"),
];

/// Words handled by the compiler rather than executed.
const SYNTAX: &[&str] = &[
    ":", ";", "if", "else", "then", "begin", "until", "again", "while", "repeat", "do", "?do", "loop", "+loop", "leave", "exit", "recurse",
    "s\"", "s\\\"", ".\"", "(", "\\", "char", "[char]", "variable", "constant", "create",
];

fn prim(name: &str) -> Option<(P, usize, &'static str)> {
    PRIMS.iter().find(|p| p.0 == name).map(|p| (p.1, p.2, p.3))
}
/// PRIMS is in the same order as `P`, so a primitive's entry is its index.
fn prim_info(p: P) -> (&'static str, usize, &'static str) {
    let e = &PRIMS[p as usize];
    (e.0, e.2, e.3)
}

#[derive(Clone, Copy)]
enum Op {
    Prim(P),
    Lit(i64),
    Call(u32),
    Branch(u32),
    ZBranch(u32),
    Do,
    QDo(u32),
    Loop(u32),
    PLoop(u32),
    Leave(u32),
    Exit,
    Halt,
}

#[derive(Clone)]
struct Word {
    name: String,
    def: String,
    version: u32,
    start: u32,
}

#[derive(Clone)]
struct Slot {
    name: String,
    target: Option<u32>,
    /// For public slots: the definition version currently bound.
    version: u32,
}

/// One call in progress: where to return to.
#[derive(Clone, Copy)]
struct Frame {
    /// Instruction after the call.
    ret: u32,
    /// The calling word (`NO_WORD` for the entry trampoline).
    word: u32,
    /// `Exec::loops` depth at the call; restored on return so an EXIT from
    /// inside a DO loop drops that loop.
    loops_depth: u32,
}

struct Exec {
    /// Data stack.
    ds: Vec<i64>,
    /// Call frames (the hidden return stack).
    frames: Vec<Frame>,
    /// The user return stack of `>r`, `r>` and `r@`.
    rstack: Vec<i64>,
    /// Active DO loops: (index, limit), innermost last.
    loops: Vec<(i64, i64)>,
    ip: u32,
    /// The instruction being executed (for error reports).
    at: u32,
    word: u32,
}

impl Exec {
    const fn new() -> Exec {
        Exec { ds: Vec::new(), frames: Vec::new(), rstack: Vec::new(), loops: Vec::new(), ip: 0, at: 0, word: NO_WORD }
    }
    fn set_ip(&mut self, ip: u32) {
        self.ip = ip;
    }
    fn set_at(&mut self, at: u32) {
        self.at = at;
    }
    fn set_word(&mut self, w: u32) {
        self.word = w;
    }
    fn pop(&mut self) -> i64 {
        self.ds.pop().unwrap_or(0)
    }
}

enum Fail {
    /// A JSON error report.
    Err(String),
    /// Suspended on this unbound public slot.
    Need(u32),
}

/// Fields of one error report (see `Vm::report_json`).
struct Report<'a> {
    kind: &'a str,
    msg: &'a str,
    word: &'a str,
    def: &'a str,
    version: u32,
    line: u32,
    token: &'a str,
    /// Data stack depth.
    depth: usize,
    /// The top 8 data stack items, comma-separated.
    stack: String,
    /// Call trace entries as JSON strings, comma-separated.
    trace: String,
    /// The top two stack items read as an ( addr len ) string, if they are one.
    top_string: Option<String>,
}

pub struct Vm {
    /// Data space (`HERE`, `VARIABLE`, string literals). Never reallocated,
    /// so addresses into it stay valid.
    perm: Box<[u8]>,
    /// Per-call string scratch space.
    scratch: Box<[u8]>,
    /// The executor of `call` (taken out while it runs).
    main: Exec,
    /// Live userspace names that may be bound lazily (set by the kernel).
    pub known: HashSet<String>,
    /// Lessons recorded by the `remember` host word during the last call.
    pub lessons: Vec<String>,
    io: Box<dyn ForthIo>,
    /// Steps (instructions) left in the current entry, and the budget.
    steps_left: u64,
    pub max_steps: u64,
    code: Vec<Op>,
    loc: Vec<(u32, String)>,
    words: Vec<Word>,
    slots: Vec<Slot>,
    public: BTreeMap<String, u32>,
    /// Private namespace per "def/version": word name -> slot.
    privs: BTreeMap<String, BTreeMap<String, u32>>,
    /// (def, version) -> word index of its `run`.
    compiled: BTreeMap<(String, u32), u32>,
    here: usize,
    scratch_top: usize,
    builders: Vec<Vec<u8>>,
    console: Vec<u8>,
    /// Definition being called (`cmh_call`) or loaded (`define`), for reports.
    ctx_def: String,
    ctx_version: u32,
    pending: Option<u32>,
}

fn js(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(o, "\\u{:04x}", c as u32);
            }
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

fn lev(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut cur = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != *cb);
            cur.push((prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1));
        }
        prev = cur;
    }
    prev[b.len()]
}

struct Src<'a> {
    b: &'a [u8],
    pos: usize,
    line: u32,
}

impl Src<'_> {
    fn token(&mut self) -> Option<(String, u32)> {
        while self.pos < self.b.len() && self.b[self.pos].is_ascii_whitespace() {
            if self.b[self.pos] == b'\n' {
                self.line += 1;
            }
            self.pos += 1;
        }
        if self.pos >= self.b.len() {
            return None;
        }
        let start = self.pos;
        while self.pos < self.b.len() && !self.b[self.pos].is_ascii_whitespace() {
            self.pos += 1;
        }
        Some((String::from_utf8_lossy(&self.b[start..self.pos]).into_owned(), self.line))
    }
    /// Text after one delimiter space up to `delim` (consumed).
    fn until(&mut self, delim: u8, escapes: bool) -> Option<Vec<u8>> {
        if self.pos < self.b.len() && self.b[self.pos].is_ascii_whitespace() {
            if self.b[self.pos] == b'\n' {
                self.line += 1;
            }
            self.pos += 1;
        }
        let mut out = Vec::new();
        while self.pos < self.b.len() {
            let c = self.b[self.pos];
            self.pos += 1;
            if c == b'\n' {
                self.line += 1;
            }
            if c == delim {
                return Some(out);
            }
            if escapes && c == b'\\' && self.pos < self.b.len() {
                let e = self.b[self.pos];
                self.pos += 1;
                out.push(match e {
                    b'n' => b'\n',
                    b't' => b'\t',
                    b'r' => b'\r',
                    b'0' => 0,
                    other => other,
                });
                continue;
            }
            out.push(c);
        }
        None
    }
    fn skip_line(&mut self) {
        while self.pos < self.b.len() && self.b[self.pos] != b'\n' {
            self.pos += 1;
        }
    }
}

/// An open control structure; the u32s are code positions to branch to or patch.
enum Ctrl {
    If(u32),
    Else(u32),
    Begin(u32),
    /// (BEGIN position, WHILE branch to patch)
    While(u32, u32),
    /// (loop body start, LEAVE and ?DO branches to patch with the exit)
    Do(u32, Vec<u32>),
}

/// The `: name ... ;` definition being compiled.
struct Open {
    name: String,
    slot: u32,
    start: u32,
    ctrl: Vec<Ctrl>,
}

/// A use of a not-yet-defined word inside a definition; it must be defined
/// by the end of the source.
struct Forward {
    slot: u32,
    line: u32,
    token: String,
}

/// One definition version being compiled (`Vm::define`).
struct Unit {
    /// Private namespace, "def/version".
    ns: String,
    /// Executor for top-level code, which runs at load time.
    tmp: Exec,
    open: Option<Open>,
    forwards: Vec<Forward>,
}

/// One source token: `raw` as written (for reports), `t` lowercased (for lookup).
struct Token {
    raw: String,
    t: String,
    line: u32,
}

impl Token {
    fn err(&self, msg: impl Into<String>) -> CompileFail {
        CompileFail::At(cerr(self.line, &self.raw, msg))
    }
}

struct CErr {
    line: u32,
    token: String,
    msg: String,
}

fn cerr(line: u32, token: &str, msg: impl Into<String>) -> CErr {
    CErr { line, token: token.to_string(), msg: msg.into() }
}

enum CompileFail {
    /// A compile error at a source position, reported as `compile-error`.
    At(CErr),
    /// Top-level code failed while running at load time: its runtime report (JSON).
    Report(String),
}

/// A copy of everything a definition or a call can change: the dictionary,
/// the code and the data space. `Vm::restore` puts it back in place.
pub struct Snapshot {
    perm: Vec<u8>,
    code: Vec<Op>,
    loc: Vec<(u32, String)>,
    words: Vec<Word>,
    slots: Vec<Slot>,
    public: BTreeMap<String, u32>,
    privs: BTreeMap<String, BTreeMap<String, u32>>,
    compiled: BTreeMap<(String, u32), u32>,
    here: usize,
}

impl Snapshot {
    /// Approximate size in bytes (for traces).
    pub fn len(&self) -> usize {
        self.here + self.code.len() * 16 + self.words.len() * 64
    }
}

impl Vm {
    pub fn new(io: Box<dyn ForthIo>, max_steps: u64) -> Vm {
        let mut vm = Vm {
            perm: vec![0u8; PERM_SIZE].into_boxed_slice(),
            scratch: vec![0u8; SCRATCH_SIZE].into_boxed_slice(),
            main: Exec::new(),
            known: HashSet::new(),
            lessons: Vec::new(),
            io,
            steps_left: max_steps,
            max_steps,
            code: Vec::new(),
            loc: Vec::new(),
            words: Vec::new(),
            slots: Vec::new(),
            public: BTreeMap::new(),
            privs: BTreeMap::new(),
            compiled: BTreeMap::new(),
            here: 0,
            scratch_top: 0,
            builders: Vec::new(),
            console: Vec::new(),
            ctx_def: String::new(),
            ctx_version: 0,
            pending: None,
        };
        // Trampoline: code[0] is patched with the slot being called. Its empty
        // token and line 0 keep "at" and "line" out of reports that point here.
        vm.emit(Op::Call(0), 0, "");
        vm.emit(Op::Halt, 0, "");
        vm
    }

    fn perm_base(&self) -> i64 {
        self.perm.as_ptr() as usize as i64
    }
    fn scratch_base(&self) -> i64 {
        self.scratch.as_ptr() as usize as i64
    }

    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            perm: self.perm[..self.here].to_vec(),
            code: self.code.clone(),
            loc: self.loc.clone(),
            words: self.words.clone(),
            slots: self.slots.clone(),
            public: self.public.clone(),
            privs: self.privs.clone(),
            compiled: self.compiled.clone(),
            here: self.here,
        }
    }

    /// Puts the image back to `snap`. Data space past the snapshot's `HERE`
    /// is zeroed, so it reads as it did when it was first allotted.
    pub fn restore(&mut self, snap: &Snapshot) {
        self.perm[..snap.here].copy_from_slice(&snap.perm);
        let end = self.here.max(snap.here);
        self.perm[snap.here..end].fill(0);
        self.code = snap.code.clone();
        self.loc = snap.loc.clone();
        self.words = snap.words.clone();
        self.slots = snap.slots.clone();
        self.public = snap.public.clone();
        self.privs = snap.privs.clone();
        self.compiled = snap.compiled.clone();
        self.here = snap.here;
        self.pending = None;
    }

    /// Bytes of data space in use and number of compiled words.
    pub fn stats(&self) -> (u64, u64) {
        (self.here as u64, self.words.len() as u64)
    }

    /// Version bound to public slot `name` (0 if unbound).
    pub fn bound(&self, name: &str) -> u32 {
        self.public.get(name).map(|s| &self.slots[*s as usize]).filter(|s| s.target.is_some()).map_or(0, |s| s.version)
    }

    /// Points public slot `name` at an already compiled version. False if
    /// that version is not compiled.
    pub fn bind(&mut self, name: &str, version: u32) -> bool {
        let Some(run) = self.compiled.get(&(name.to_string(), version)).copied() else { return false };
        self.bind_public(name, version, run);
        true
    }

    /// Unbinds public slot `name`; its next call suspends with `Need`.
    pub fn unbind(&mut self, name: &str) {
        self.unbind_public(name);
    }

    fn emit(&mut self, op: Op, line: u32, tok: &str) {
        self.code.push(op);
        self.loc.push((line, tok.to_string()));
    }

    fn new_slot(&mut self, name: &str) -> u32 {
        self.slots.push(Slot { name: name.to_string(), target: None, version: 0 });
        (self.slots.len() - 1) as u32
    }

    fn public_slot(&mut self, name: &str) -> u32 {
        if let Some(s) = self.public.get(name) {
            return *s;
        }
        let s = self.new_slot(name);
        self.public.insert(name.to_string(), s);
        s
    }

    /// The slot of word `name` in private namespace `ns` ("def/version"),
    /// created unbound if it does not exist yet.
    fn private_slot(&mut self, ns: &str, name: &str) -> u32 {
        if let Some(s) = self.privs[ns].get(name) {
            return *s;
        }
        let s = self.new_slot(&format!("{ns}:{name}"));
        self.privs.get_mut(ns).expect("ns").insert(name.to_string(), s);
        s
    }

    /// Points public slot `name` at word `run` of definition version `version`.
    fn bind_public(&mut self, name: &str, version: u32, run: u32) {
        let slot = self.public_slot(name);
        self.slots[slot as usize].target = Some(run);
        self.slots[slot as usize].version = version;
    }

    /// Unbinds public slot `name` (if it exists): its next call suspends with NEED.
    fn unbind_public(&mut self, name: &str) {
        if let Some(s) = self.public.get(name).copied() {
            self.slots[s as usize].target = None;
            self.slots[s as usize].version = 0;
        }
    }

    /// What `tok` (lowercased) means in namespace `ns`: a private word first,
    /// then a primitive, then a public (userspace) slot that already exists.
    fn resolve(&self, ns: &str, tok: &str) -> Option<Op> {
        if let Some(slot) = self.privs[ns].get(tok) {
            return Some(Op::Call(*slot));
        }
        if let Some((p, _, _)) = prim(tok) {
            return Some(Op::Prim(p));
        }
        self.public.get(tok).map(|slot| Op::Call(*slot))
    }

    fn undefined_word_msg(&self, ns: &str, tok: &str) -> String {
        let mut msg = format!("undefined word `{tok}`");
        if let Some(s) = self.suggest(ns, tok) {
            let _ = write!(msg, "; did you mean `{s}`?");
        }
        msg
    }

    // ------------------------------------------------------------ reports

    fn word_info(&self, w: u32) -> (String, String, u32) {
        match self.words.get(w as usize) {
            Some(w) => (w.name.clone(), w.def.clone(), w.version),
            None => (String::new(), self.ctx_def.clone(), self.ctx_version),
        }
    }

    fn in_data(&self, a: i64, u: i64) -> bool {
        let (pb, sb) = (self.perm_base(), self.scratch_base());
        u >= 0
            && ((a >= pb && a.saturating_add(u) <= pb + PERM_SIZE as i64) || (a >= sb && a.saturating_add(u) <= sb + SCRATCH_SIZE as i64))
    }

    fn preview(&self, a: i64, u: i64) -> Option<String> {
        if u <= 0 || u > 4096 || !self.in_data(a, u) {
            return None;
        }
        let bytes = unsafe { std::slice::from_raw_parts(a as usize as *const u8, u as usize) };
        let s = String::from_utf8_lossy(&bytes[..bytes.len().min(80)]).into_owned();
        Some(if u > 80 { format!("{s}...") } else { s })
    }

    /// The JSON error report the host turns into text (`format_report`),
    /// with the console output so far.
    fn report_json(&self, r: &Report) -> String {
        format!(
            "{{\"kind\":{},\"message\":{},\"word\":{},\"def\":{},\"version\":{},\"line\":{},\"token\":{},\"depth\":{},\"stack\":[{}],\"trace\":[{}],\"top_string\":{},\"output\":{}}}",
            js(r.kind),
            js(r.msg),
            js(r.word),
            js(r.def),
            r.version,
            r.line,
            js(r.token),
            r.depth,
            r.stack,
            r.trace,
            r.top_string.as_deref().map_or("null".into(), js),
            js(&String::from_utf8_lossy(&self.console)),
        )
    }

    /// A runtime report for wherever `ex` is: the word, line and token being
    /// executed, the top of the data stack, and the call trace.
    fn report(&self, ex: &Exec, kind: &str, msg: &str) -> String {
        let (word, def, version) = self.word_info(ex.word);
        let (line, token) = self.loc.get(ex.at as usize).cloned().unwrap_or((0, String::new()));
        let n = ex.ds.len();
        let top: Vec<String> = ex.ds[n.saturating_sub(8)..].iter().map(|v| v.to_string()).collect();
        let mut trace = Vec::new();
        for f in ex.frames.iter().rev() {
            if f.word == NO_WORD {
                continue;
            }
            let (w, d, v) = self.word_info(f.word);
            let (l, t) = self.loc.get((f.ret as usize).saturating_sub(1)).cloned().unwrap_or((0, String::new()));
            trace.push(js(&format!("`{w}` ({d} v{v}) line {l} at `{t}`")));
        }
        self.report_json(&Report {
            kind,
            msg,
            word: &word,
            def: &def,
            version,
            line,
            token: &token,
            depth: n,
            stack: top.join(","),
            trace: trace.join(","),
            top_string: if n >= 2 { self.preview(ex.ds[n - 2], ex.ds[n - 1]) } else { None },
        })
    }

    fn fail(&self, ex: &Exec, kind: &str, msg: impl AsRef<str>) -> Fail {
        Fail::Err(self.report(ex, kind, msg.as_ref()))
    }

    /// A compile error has no runtime state: empty stack and trace.
    fn compile_report(&self, def: &str, version: u32, word: &str, e: &CErr) -> String {
        self.report_json(&Report {
            kind: "compile-error",
            msg: &e.msg,
            word,
            def,
            version,
            line: e.line,
            token: &e.token,
            depth: 0,
            stack: String::new(),
            trace: String::new(),
            top_string: None,
        })
    }

    // ------------------------------------------------------------- memory

    /// Raw cell/byte access: only data space and string scratch space. Any
    /// other address is refused, so Forth code cannot read or write the rest
    /// of the kernel's memory.
    fn addr(&self, ex: &Exec, a: i64, n: usize, what: &str) -> Result<*mut u8, Fail> {
        if self.in_data(a, n as i64) {
            return Ok(a as usize as *mut u8);
        }
        let (pb, sb) = (self.perm_base(), self.scratch_base());
        Err(self.fail(
            ex,
            "bad-address",
            format!(
                "{what}: address {a} is not in Forth data space (data space {pb}..{}, strings {sb}..{}); valid addresses come from HERE, VARIABLE, CREATE and strings",
                pb + PERM_SIZE as i64,
                sb + SCRATCH_SIZE as i64
            ),
        ))
    }

    fn bytes(&self, ex: &Exec, a: i64, u: i64, what: &str) -> Result<&'static [u8], Fail> {
        if u < 0 {
            return Err(self.fail(ex, "bad-address", format!("{what}: negative string length {u}")));
        }
        if u == 0 {
            return Ok(&[]);
        }
        if !self.in_data(a, u) {
            return Err(self.fail(
                ex,
                "bad-address",
                format!("{what}: ( {a} {u} ) is not a string in Forth data space; expected ( addr len ) from S\", json-get, B> etc."),
            ));
        }
        Ok(unsafe { std::slice::from_raw_parts(a as usize as *const u8, u as usize) })
    }

    fn salloc(&mut self, ex: &Exec, n: usize) -> Result<i64, Fail> {
        if self.scratch_top + n > SCRATCH_SIZE {
            return Err(self.fail(
                ex,
                "string-space",
                format!("string scratch space ({} bytes per call) exhausted allocating {n} bytes", SCRATCH_SIZE),
            ));
        }
        let a = self.scratch_base() + self.scratch_top as i64;
        self.scratch_top += n;
        Ok(a)
    }

    fn sput(&mut self, ex: &mut Exec, bytes: &[u8]) -> Result<(), Fail> {
        let a = self.salloc(ex, bytes.len())?;
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), a as usize as *mut u8, bytes.len()) };
        self.push(ex, a)?;
        self.push(ex, bytes.len() as i64)
    }

    fn palloc(&mut self, n: usize) -> Option<i64> {
        if self.here + n > PERM_SIZE {
            return None;
        }
        let a = self.perm_base() + self.here as i64;
        self.here += n;
        Some(a)
    }

    fn console_add(&mut self, b: &[u8]) {
        let room = CONSOLE_MAX.saturating_sub(self.console.len());
        self.console.extend_from_slice(&b[..b.len().min(room)]);
    }

    fn push(&self, ex: &mut Exec, v: i64) -> Result<(), Fail> {
        if ex.ds.len() >= DS_MAX {
            return Err(self.fail(
                ex,
                "stack-overflow",
                format!("data stack overflow ({DS_MAX} items); a loop is probably pushing without dropping"),
            ));
        }
        ex.ds.push(v);
        Ok(())
    }

    // ---------------------------------------------------------- execution

    fn run(&mut self, ex: &mut Exec) -> Result<(), Fail> {
        loop {
            if self.steps_left == 0 {
                let msg = format!("ran out of fuel ({} Forth steps) without finishing: probably an infinite loop", self.max_steps);
                return Err(self.fail(ex, "fuel-exhausted", msg));
            }
            self.steps_left -= 1;
            let ip = ex.ip as usize;
            let Some(op) = self.code.get(ip).copied() else {
                return Err(self.fail(ex, "vm-error", format!("instruction pointer {ip} out of code")));
            };
            ex.set_at(ip as u32);
            ex.set_ip(ip as u32 + 1);
            match op {
                Op::Lit(n) => self.push(ex, n)?,
                Op::Prim(p) => self.prim(ex, p)?,
                Op::Call(s) => match self.slots[s as usize].target {
                    Some(w) => {
                        if ex.frames.len() >= RS_MAX {
                            let name = self.slots[s as usize].name.clone();
                            return Err(self.fail(
                                ex,
                                "return-stack-overflow",
                                format!("call depth exceeded {RS_MAX} calling `{name}` (unbounded recursion?)"),
                            ));
                        }
                        ex.frames.push(Frame { ret: ip as u32 + 1, word: ex.word, loops_depth: ex.loops.len() as u32 });
                        ex.set_word(w);
                        ex.set_ip(self.words[w as usize].start);
                    }
                    None => {
                        ex.set_ip(ip as u32);
                        return Err(Fail::Need(s));
                    }
                },
                Op::Exit => match ex.frames.pop() {
                    Some(f) => {
                        ex.loops.truncate(f.loops_depth as usize);
                        ex.set_word(f.word);
                        ex.set_ip(f.ret);
                    }
                    None => return Ok(()),
                },
                Op::Halt => return Ok(()),
                Op::Branch(t) => ex.set_ip(t),
                Op::ZBranch(t) => {
                    if ex.ds.is_empty() {
                        return Err(self.fail(ex, "stack-underflow", "IF/WHILE/UNTIL needs a flag but the stack is empty"));
                    }
                    if ex.pop() == 0 {
                        ex.set_ip(t);
                    }
                }
                Op::Do | Op::QDo(_) => {
                    if ex.ds.len() < 2 {
                        return Err(self.fail(
                            ex,
                            "stack-underflow",
                            format!("DO ( limit start -- ) needs 2 items but the stack has {}", ex.ds.len()),
                        ));
                    }
                    let start = ex.pop();
                    let limit = ex.pop();
                    if let Op::QDo(t) = op
                        && start >= limit
                    {
                        ex.set_ip(t);
                        continue;
                    }
                    if ex.loops.len() >= LS_MAX {
                        return Err(self.fail(ex, "stack-overflow", format!("more than {LS_MAX} nested DO loops")));
                    }
                    ex.loops.push((start, limit));
                }
                Op::Loop(t) | Op::PLoop(t) => {
                    let step = if let Op::PLoop(_) = op {
                        if ex.ds.is_empty() {
                            return Err(self.fail(ex, "stack-underflow", "+LOOP needs a step but the stack is empty"));
                        }
                        ex.pop()
                    } else {
                        1
                    };
                    let Some(l) = ex.loops.last_mut() else {
                        return Err(self.fail(ex, "vm-error", "LOOP without an active DO"));
                    };
                    l.0 = l.0.wrapping_add(step);
                    let again = if step >= 0 { l.0 < l.1 } else { l.0 >= l.1 };
                    if again {
                        ex.set_ip(t);
                    } else {
                        ex.loops.pop();
                    }
                }
                Op::Leave(t) => {
                    ex.loops.pop();
                    ex.set_ip(t);
                }
            }
        }
    }

    fn prim(&mut self, ex: &mut Exec, p: P) -> Result<(), Fail> {
        let (name, need, effect) = prim_info(p);
        if ex.ds.len() < need {
            let have = ex.ds.len();
            return Err(self.fail(ex, "stack-underflow", format!("`{name}` {effect} needs {need} item(s) but the stack has {have}")));
        }
        let n = ex.ds.len();
        macro_rules! bin {
            ($f:expr) => {{
                let b = ex.pop();
                let a = ex.pop();
                let f: fn(i64, i64) -> i64 = $f;
                ex.ds.push(f(a, b));
            }};
        }
        macro_rules! flag {
            ($e:expr) => {
                if $e { -1 } else { 0 }
            };
        }
        match p {
            P::Dup => self.push(ex, ex.ds[n - 1])?,
            P::Drop => {
                ex.pop();
            }
            P::Swap => ex.ds.swap(n - 1, n - 2),
            P::Over => self.push(ex, ex.ds[n - 2])?,
            P::Rot => {
                let a = ex.ds.remove(n - 3);
                ex.ds.push(a);
            }
            P::MRot => {
                let c = ex.pop();
                ex.ds.insert(n - 3, c);
            }
            P::Nip => {
                ex.ds.remove(n - 2);
            }
            P::Tuck => {
                let b = ex.ds[n - 1];
                ex.ds.insert(n - 2, b);
                if ex.ds.len() > DS_MAX {
                    return Err(self.fail(ex, "stack-overflow", "data stack overflow"));
                }
            }
            P::Dup2 => {
                self.push(ex, ex.ds[n - 2])?;
                self.push(ex, ex.ds[n - 1])?;
            }
            P::Drop2 => {
                ex.ds.truncate(n - 2);
            }
            P::Swap2 => {
                ex.ds.swap(n - 4, n - 2);
                ex.ds.swap(n - 3, n - 1);
            }
            P::Over2 => {
                self.push(ex, ex.ds[n - 4])?;
                self.push(ex, ex.ds[n - 3])?;
            }
            P::Pick => {
                let k = ex.ds[n - 1];
                if k < 0 || k as usize >= n - 1 {
                    return Err(self.fail(ex, "stack-underflow", format!("`pick` index {k} but only {} item(s) below it", n - 1)));
                }
                ex.ds[n - 1] = ex.ds[n - 2 - k as usize];
            }
            P::Depth => self.push(ex, n as i64)?,
            P::ToR => {
                if ex.rstack.len() >= US_MAX {
                    return Err(self.fail(ex, "stack-overflow", format!("`>r` return stack full ({US_MAX})")));
                }
                let v = ex.pop();
                ex.rstack.push(v);
            }
            P::RFrom | P::RFetch => {
                let Some(&v) = ex.rstack.last() else {
                    return Err(self.fail(ex, "stack-underflow", format!("`{name}` but nothing was pushed with `>r`")));
                };
                if p == P::RFrom {
                    ex.rstack.pop();
                }
                self.push(ex, v)?;
            }
            P::Add => bin!(|a, b| a.wrapping_add(b)),
            P::Sub => bin!(|a, b| a.wrapping_sub(b)),
            P::Mul => bin!(|a, b| a.wrapping_mul(b)),
            P::Div | P::Mod | P::DivMod => {
                if ex.ds[n - 1] == 0 {
                    return Err(self.fail(ex, "division-by-zero", format!("`{name}` divides {} by zero", ex.ds[n - 2])));
                }
                let b = ex.pop();
                let a = ex.pop();
                match p {
                    P::Div => ex.ds.push(a.wrapping_div(b)),
                    P::Mod => ex.ds.push(a.wrapping_rem(b)),
                    _ => {
                        ex.ds.push(a.wrapping_rem(b));
                        ex.ds.push(a.wrapping_div(b));
                    }
                }
            }
            P::Negate => ex.ds[n - 1] = ex.ds[n - 1].wrapping_neg(),
            P::Abs => ex.ds[n - 1] = ex.ds[n - 1].wrapping_abs(),
            P::Min => bin!(|a, b| a.min(b)),
            P::Max => bin!(|a, b| a.max(b)),
            P::Inc => ex.ds[n - 1] = ex.ds[n - 1].wrapping_add(1),
            P::Dec => ex.ds[n - 1] = ex.ds[n - 1].wrapping_sub(1),
            P::Eq => bin!(|a, b| flag!(a == b)),
            P::Ne => bin!(|a, b| flag!(a != b)),
            P::Lt => bin!(|a, b| flag!(a < b)),
            P::Gt => bin!(|a, b| flag!(a > b)),
            P::Le => bin!(|a, b| flag!(a <= b)),
            P::Ge => bin!(|a, b| flag!(a >= b)),
            P::ZEq => ex.ds[n - 1] = flag!(ex.ds[n - 1] == 0),
            P::ZLt => ex.ds[n - 1] = flag!(ex.ds[n - 1] < 0),
            P::ZGt => ex.ds[n - 1] = flag!(ex.ds[n - 1] > 0),
            P::And => bin!(|a, b| a & b),
            P::Or => bin!(|a, b| a | b),
            P::Xor => bin!(|a, b| a ^ b),
            P::Invert => ex.ds[n - 1] = !ex.ds[n - 1],
            P::True => self.push(ex, -1)?,
            P::False => self.push(ex, 0)?,
            P::Within => {
                let hi = ex.pop();
                let lo = ex.pop();
                let v = ex.pop();
                ex.ds.push(flag!(lo <= v && v < hi));
            }
            P::Fetch => {
                let p = self.addr(ex, ex.ds[n - 1], 8, "`@`")?;
                ex.ds[n - 1] = unsafe { std::ptr::read_unaligned(p as *const i64) };
            }
            P::CFetch => {
                let p = self.addr(ex, ex.ds[n - 1], 1, "`c@`")?;
                ex.ds[n - 1] = i64::from(unsafe { std::ptr::read_volatile(p) });
            }
            P::Store | P::PlusStore => {
                // Store before popping: if the store traps, the report still
                // shows ( n addr ) on the stack.
                let ptr = self.addr(ex, ex.ds[n - 1], 8, if p == P::Store { "`!`" } else { "`+!`" })? as *mut i64;
                let v = ex.ds[n - 2];
                unsafe {
                    if p == P::Store {
                        std::ptr::write_unaligned(ptr, v);
                    } else {
                        let old = std::ptr::read_unaligned(ptr);
                        std::ptr::write_unaligned(ptr, old.wrapping_add(v));
                    }
                }
                ex.ds.truncate(n - 2);
            }
            P::CStore => {
                let p = self.addr(ex, ex.ds[n - 1], 1, "`c!`")?;
                unsafe { std::ptr::write_volatile(p, ex.ds[n - 2] as u8) };
                ex.ds.truncate(n - 2);
            }
            P::Here => self.push(ex, self.perm_base() + self.here as i64)?,
            P::Allot | P::Comma | P::CComma => {
                let v = ex.ds[n - 1];
                let size = match p {
                    P::Allot => v,
                    P::Comma => 8,
                    _ => 1,
                };
                if size < 0 || self.here as i64 + size > PERM_SIZE as i64 {
                    return Err(self.fail(
                        ex,
                        "data-space",
                        format!("`{name}` of {size} bytes does not fit: data space is {PERM_SIZE} bytes, {} used", self.here),
                    ));
                }
                ex.pop();
                let a = self.palloc(size as usize).unwrap_or(0) as usize;
                unsafe {
                    match p {
                        P::Comma => std::ptr::write_unaligned(a as *mut i64, v),
                        P::CComma => *(a as *mut u8) = v as u8,
                        _ => std::ptr::write_bytes(a as *mut u8, 0, size as usize),
                    }
                }
            }
            P::Cells => ex.ds[n - 1] = ex.ds[n - 1].wrapping_mul(8),
            P::I | P::J => {
                let depth = if p == P::I { 1 } else { 2 };
                if ex.loops.len() < depth {
                    return Err(self.fail(ex, "stack-underflow", format!("`{name}` used outside a DO loop (or J outside a nested loop)")));
                }
                let v = ex.loops[ex.loops.len() - depth].0;
                self.push(ex, v)?;
            }
            P::Type => {
                let u = ex.pop();
                let a = ex.pop();
                let b = self.bytes(ex, a, u, "`type`")?;
                self.console_add(b);
            }
            P::Emit => {
                let c = ex.pop();
                self.console_add(&[c as u8]);
            }
            P::Cr => self.console_add(b"\n"),
            P::Dot => {
                let v = ex.pop();
                self.console_add(format!("{v} ").as_bytes());
            }
            P::DotS => {
                let s = format!("<{}> {} ", n, ex.ds.iter().map(|v| v.to_string()).collect::<Vec<_>>().join(" "));
                self.console_add(s.as_bytes());
            }
            P::Bl => self.push(ex, 32)?,
            P::Compare | P::SEq | P::Index | P::Concat => {
                let u2 = ex.ds[n - 1];
                let a2 = ex.ds[n - 2];
                let u1 = ex.ds[n - 3];
                let a1 = ex.ds[n - 4];
                let what = format!("`{name}`");
                let s1 = self.bytes(ex, a1, u1, &what)?;
                let s2 = self.bytes(ex, a2, u2, &what)?;
                ex.ds.truncate(n - 4);
                match p {
                    P::Compare => ex.ds.push(s1.cmp(s2) as i64),
                    P::SEq => ex.ds.push(flag!(s1 == s2)),
                    P::Index => {
                        let pos = if s2.is_empty() { Some(0) } else { s1.windows(s2.len()).position(|w| w == s2) };
                        ex.ds.push(pos.map_or(-1, |p| p as i64));
                    }
                    _ => {
                        let mut v = Vec::with_capacity(s1.len() + s2.len());
                        v.extend_from_slice(s1);
                        v.extend_from_slice(s2);
                        self.sput(ex, &v)?;
                    }
                }
            }
            P::SlashString => {
                let k = ex.pop();
                let u = ex.ds[n - 2];
                let k = k.clamp(0, u.max(0));
                ex.ds[n - 3] += k;
                ex.ds[n - 2] = u - k;
            }
            P::Lower | P::Trim => {
                let u = ex.pop();
                let a = ex.pop();
                let s = self.bytes(ex, a, u, &format!("`{name}`"))?;
                if p == P::Lower {
                    let v = s.to_ascii_lowercase();
                    self.sput(ex, &v)?;
                } else {
                    let start = s.iter().position(|c| !c.is_ascii_whitespace()).unwrap_or(s.len());
                    let end = s.iter().rposition(|c| !c.is_ascii_whitespace()).map_or(start, |e| e + 1);
                    ex.ds.push(a + start as i64);
                    ex.ds.push((end - start) as i64);
                }
            }
            P::BStart => {
                if self.builders.len() >= 16 {
                    return Err(self.fail(ex, "stack-overflow", "more than 16 nested <B builders (missing B> ?)"));
                }
                self.builders.push(Vec::new());
            }
            P::BAdd | P::BChar | P::BNum => {
                if self.builders.is_empty() {
                    return Err(self.fail(ex, "builder", format!("`{name}` without an open builder; start one with <B")));
                }
                let add: Vec<u8> = match p {
                    P::BAdd => {
                        let u = ex.pop();
                        let a = ex.pop();
                        self.bytes(ex, a, u, "`b+`")?.to_vec()
                    }
                    P::BChar => vec![ex.pop() as u8],
                    _ => ex.pop().to_string().into_bytes(),
                };
                let b = self.builders.last_mut().expect("checked");
                if b.len() + add.len() > BUILDER_MAX {
                    return Err(self.fail(ex, "string-space", format!("string builder exceeds {BUILDER_MAX} bytes")));
                }
                b.extend_from_slice(&add);
            }
            P::BEnd => {
                let Some(b) = self.builders.pop() else {
                    return Err(self.fail(ex, "builder", "`b>` without an open builder; start one with <B"));
                };
                self.sput(ex, &b)?;
            }
            P::NToS => {
                let v = ex.pop();
                self.sput(ex, v.to_string().as_bytes())?;
            }
            P::SToN => {
                let u = ex.pop();
                let a = ex.pop();
                let s = self.bytes(ex, a, u, "`s>n`")?;
                match std::str::from_utf8(s).ok().and_then(|t| t.trim().parse::<i64>().ok()) {
                    Some(v) => {
                        ex.ds.push(v);
                        ex.ds.push(-1);
                    }
                    None => {
                        ex.ds.push(0);
                        ex.ds.push(0);
                    }
                }
            }
            P::JsonGet | P::JsonLen | P::JsonStr | P::FileRead | P::FileWrite | P::Remember => self.host(ex, p, name)?,
        }
        Ok(())
    }

    /// Runs a host word: checks its string arguments, runs it, and pushes
    /// the result (a string, a number, or nothing).
    fn host(&mut self, ex: &mut Exec, p: P, name: &str) -> Result<(), Fail> {
        // Words taking two strings ( a1 u1 a2 u2 ); the rest take one.
        let two = matches!(p, P::JsonGet | P::JsonLen | P::FileWrite);
        let n = ex.ds.len();
        let what = format!("`{name}`");
        let (a, b) =
            if two { ((ex.ds[n - 4], ex.ds[n - 3]), (ex.ds[n - 2], ex.ds[n - 1])) } else { ((ex.ds[n - 2], ex.ds[n - 1]), (0, 0)) };
        let x = self.bytes(ex, a.0, a.1, &what)?;
        let y = self.bytes(ex, b.0, b.1, &what)?;
        let r = match self.host_op(p, x, y) {
            Ok(r) => r,
            Err(e) => return Err(self.fail(ex, "host-error", format!("{what}: {e}"))),
        };
        ex.ds.truncate(n - if two { 4 } else { 2 });
        match r {
            HostOut::Str(s) => self.sput(ex, &s)?,
            HostOut::Num(v) => self.push(ex, v)?,
            HostOut::None => {}
        }
        Ok(())
    }

    fn host_op(&mut self, p: P, x: &[u8], y: &[u8]) -> Result<HostOut, String> {
        match p {
            // json-get ( json path -- str )
            P::JsonGet => {
                let v = parse_json(x)?;
                let path = String::from_utf8_lossy(y);
                Ok(HostOut::Str(match lookup(&v, &path)? {
                    Value::String(s) => s.clone().into_bytes(),
                    other => other.to_string().into_bytes(),
                }))
            }
            // json-len ( json path -- n ): -1 if missing
            P::JsonLen => {
                let v = parse_json(x)?;
                let path = String::from_utf8_lossy(y);
                Ok(HostOut::Num(match lookup(&v, &path) {
                    Err(_) => -1,
                    Ok(Value::Array(a)) => a.len() as i64,
                    Ok(Value::Object(m)) => m.len() as i64,
                    Ok(Value::String(s)) => s.len() as i64,
                    Ok(_) => 0,
                }))
            }
            // json-str ( str -- "json string" )
            P::JsonStr => Ok(HostOut::Str(Value::String(String::from_utf8_lossy(x).into_owned()).to_string().into_bytes())),
            // file-read ( path -- contents )
            P::FileRead => {
                let data = self.io.read(&String::from_utf8_lossy(x))?;
                if data.len() > FILE_MAX {
                    return Err(format!("the file is {} bytes; file-read is limited to {FILE_MAX}", data.len()));
                }
                Ok(HostOut::Str(data))
            }
            // file-write ( contents path -- )
            P::FileWrite => {
                self.io.write(&String::from_utf8_lossy(y), x)?;
                Ok(HostOut::None)
            }
            // remember ( text -- )
            _ => {
                let t = String::from_utf8_lossy(x).trim().to_string();
                if t.is_empty() {
                    return Err("empty lesson text".into());
                }
                if self.lessons.len() < 10 {
                    self.lessons.push(t);
                }
                Ok(HostOut::None)
            }
        }
    }

    // ----------------------------------------------------------- compiler

    /// Compiles one userspace definition version into its own namespace and,
    /// on success, binds the public slot `def` to its `run` word.
    /// Errors are JSON reports. Load-time code gets a fresh step budget.
    pub fn define(&mut self, def: &str, version: u32, src: &str) -> Result<(), String> {
        let saved = (std::mem::take(&mut self.ctx_def), self.ctx_version);
        self.ctx_def = def.to_string();
        self.ctx_version = version;
        self.steps_left = self.max_steps;
        let r = self.define_inner(def, version, src);
        (self.ctx_def, self.ctx_version) = saved;
        r
    }

    fn define_inner(&mut self, def: &str, version: u32, src: &str) -> Result<(), String> {
        if prim(def).is_some() || SYNTAX.contains(&def) {
            let e = cerr(0, def, format!("definition name `{def}` collides with a core Forth word; choose another name"));
            return Err(self.compile_report(def, version, "", &e));
        }
        let ns = format!("{def}/{version}");
        self.privs.insert(ns.clone(), BTreeMap::new());
        let mut u = Unit { ns, tmp: Exec::new(), open: None, forwards: Vec::new() };
        let r = self.compile(&mut u, src);
        // Errors are reported in the word that was being compiled, if any.
        let word = u.open.as_ref().map(|o| o.name.clone()).unwrap_or_default();
        let fail = |vm: &Vm, e: CErr| Err(vm.compile_report(def, version, &word, &e));
        match r {
            Ok(()) => {}
            Err(CompileFail::At(e)) => return fail(self, e),
            Err(CompileFail::Report(json)) => return Err(json),
        }
        if let Some(o) = &u.open {
            return fail(self, cerr(0, &o.name, format!("definition `{}` is not closed: missing `;`", o.name)));
        }
        for f in &u.forwards {
            if self.slots[f.slot as usize].target.is_none() {
                return fail(self, cerr(f.line, &f.token, self.undefined_word_msg(&u.ns, &f.token)));
            }
        }
        if !u.tmp.ds.is_empty() {
            let items: Vec<String> = u.tmp.ds.iter().map(|v| v.to_string()).collect();
            return fail(self, cerr(0, "", format!("top-level code left {} item(s) on the stack: [{}]", items.len(), items.join(" "))));
        }
        let Some(run) = self.privs[&u.ns].get("run").and_then(|s| self.slots[*s as usize].target) else {
            return fail(self, cerr(0, "", "the source must define `: run ( json-addr json-len -- out-addr out-len ) ... ;`"));
        };
        self.compiled.insert((def.to_string(), version), run);
        self.bind_public(def, version, run);
        Ok(())
    }

    fn suggest(&self, ns: &str, tok: &str) -> Option<String> {
        let lower = tok.to_lowercase();
        let privs = self.privs.get(ns).into_iter().flat_map(|m| m.keys().map(String::as_str));
        PRIMS
            .iter()
            .map(|p| p.0)
            .chain(SYNTAX.iter().copied())
            .chain(privs)
            .chain(self.public.keys().map(String::as_str))
            .map(|c| (lev(&lower, c), c))
            .filter(|(d, c)| *d > 0 && *d <= 2 && *d < c.len().max(2))
            .min()
            .map(|(_, c)| c.to_string())
    }

    /// Compiles `src` token by token. Outside a definition tokens are
    /// interpreted (run now); inside `: name ... ;` they are compiled.
    fn compile(&mut self, u: &mut Unit, src: &str) -> Result<(), CompileFail> {
        let mut s = Src { b: src.as_bytes(), pos: 0, line: 1 };
        while let Some((raw, line)) = s.token() {
            let tk = Token { t: raw.to_lowercase(), raw, line };
            if self.parse_word(u, &mut s, &tk)? {
                continue;
            }
            if u.open.is_none() {
                self.interpret_token(u, &mut s, &tk)?;
            } else {
                self.compile_token(u, &tk)?;
            }
        }
        Ok(())
    }

    /// Parsing words read the source after them and work in both states.
    /// Returns false if `tk` is not one.
    fn parse_word(&mut self, u: &mut Unit, s: &mut Src, tk: &Token) -> Result<bool, CompileFail> {
        match tk.t.as_str() {
            "\\" => s.skip_line(),
            "(" => {
                if s.until(b')', false).is_none() {
                    return Err(tk.err("unterminated comment: missing `)`"));
                }
            }
            "s\"" | "s\\\"" | ".\"" => self.string_literal(u, s, tk)?,
            "char" | "[char]" => {
                let Some((c, _)) = s.token() else {
                    return Err(tk.err(format!("`{}` needs a character after it", tk.raw)));
                };
                let v = i64::from(c.as_bytes()[0]);
                if u.open.is_some() {
                    self.emit(Op::Lit(v), tk.line, &tk.raw);
                } else {
                    u.tmp.ds.push(v);
                }
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    /// `s" ..."`, `s\" ..."` (with escapes) and `." ..."`. The text is stored
    /// in data space; inside a definition the word pushes (or types) it, at
    /// top level it is pushed (or printed) now.
    fn string_literal(&mut self, u: &mut Unit, s: &mut Src, tk: &Token) -> Result<(), CompileFail> {
        let Some(text) = s.until(b'"', tk.t == "s\\\"") else {
            return Err(tk.err(format!("unterminated string after `{}`: missing closing \"", tk.raw)));
        };
        let Some(a) = self.palloc(text.len()) else {
            return Err(tk.err("data space full while storing a string literal"));
        };
        unsafe { std::ptr::copy_nonoverlapping(text.as_ptr(), a as usize as *mut u8, text.len()) };
        let typed = tk.t == ".\"";
        if u.open.is_some() {
            self.emit(Op::Lit(a), tk.line, &tk.raw);
            self.emit(Op::Lit(text.len() as i64), tk.line, &tk.raw);
            if typed {
                self.emit(Op::Prim(P::Type), tk.line, &tk.raw);
            }
        } else if typed {
            self.console_add(&text);
        } else {
            u.tmp.ds.push(a);
            u.tmp.ds.push(text.len() as i64);
        }
        Ok(())
    }

    /// Outside a definition: defining words, numbers (pushed now) and words
    /// (run now on the load-time stack).
    fn interpret_token(&mut self, u: &mut Unit, s: &mut Src, tk: &Token) -> Result<(), CompileFail> {
        match tk.t.as_str() {
            ":" | "variable" | "constant" | "create" => return self.define_name(u, s, tk),
            ";" | "if" | "else" | "then" | "begin" | "until" | "again" | "while" | "repeat" | "do" | "?do" | "loop" | "+loop" | "leave"
            | "exit" | "recurse" => {
                return Err(tk.err(format!("`{}` is only valid inside a `: name ... ;` definition", tk.raw)));
            }
            _ => {}
        }
        if let Ok(v) = tk.t.parse::<i64>() {
            u.tmp.ds.push(v);
            return Ok(());
        }
        let Some(op) = self.resolve(&u.ns, &tk.t) else {
            return Err(tk.err(self.undefined_word_msg(&u.ns, &tk.raw)));
        };
        self.run_now(u, op, tk)
    }

    /// Runs `op` at load time through a two-instruction stub (op, HALT) that
    /// is removed again afterwards.
    fn run_now(&mut self, u: &mut Unit, op: Op, tk: &Token) -> Result<(), CompileFail> {
        let mark = self.code.len();
        self.emit(op, tk.line, &tk.raw);
        self.emit(Op::Halt, tk.line, &tk.raw);
        u.tmp.ip = mark as u32;
        u.tmp.word = NO_WORD;
        let r = self.run(&mut u.tmp);
        self.code.truncate(mark);
        self.loc.truncate(mark);
        match r {
            Ok(()) => Ok(()),
            Err(Fail::Err(json)) => Err(CompileFail::Report(json)),
            Err(Fail::Need(_)) => {
                Err(tk.err(format!("`{}` is loaded lazily and cannot run at load time; call it from inside a word instead", tk.raw)))
            }
        }
    }

    /// `:`, `variable`, `constant` or `create` NAME: a word in this
    /// definition's private namespace. `:` opens a definition; the others
    /// compile a word that pushes their value.
    fn define_name(&mut self, u: &mut Unit, s: &mut Src, tk: &Token) -> Result<(), CompileFail> {
        let Some((name, _)) = s.token() else {
            return Err(tk.err(format!("`{}` needs a name after it", tk.raw)));
        };
        let name = name.to_lowercase();
        if name.parse::<i64>().is_ok() || SYNTAX.contains(&name.as_str()) {
            return Err(tk.err(format!("`{name}` cannot be used as a word name")));
        }
        let slot = self.private_slot(&u.ns, &name);
        let start = self.code.len() as u32;
        let value = match tk.t.as_str() {
            ":" => {
                u.open = Some(Open { name, slot, start, ctrl: Vec::new() });
                return Ok(());
            }
            "variable" => {
                let Some(a) = self.palloc(8) else { return Err(tk.err("data space full")) };
                unsafe { std::ptr::write_unaligned(a as usize as *mut i64, 0) };
                a
            }
            "constant" => match u.tmp.ds.pop() {
                Some(v) => v,
                None => return Err(tk.err(format!("`constant {name}` needs a value on the stack, e.g. `10 constant {name}`"))),
            },
            // create: the address of the data space that follows.
            _ => self.perm_base() + self.here as i64,
        };
        self.emit(Op::Lit(value), tk.line, &name);
        self.emit(Op::Exit, tk.line, &name);
        self.bind_word(&u.ns, &name, slot, start);
        Ok(())
    }

    /// Inside `: name ... ;`. Control words emit branches and patch their
    /// targets once known; anything else compiles a literal or a call.
    fn compile_token(&mut self, u: &mut Unit, tk: &Token) -> Result<(), CompileFail> {
        let (t, line, tok) = (tk.t.as_str(), tk.line, tk.raw.as_str());
        let here = self.code.len() as u32;
        let o = u.open.as_mut().expect("open");
        match t {
            ";" => {
                if let Some(c) = o.ctrl.last() {
                    let what = match c {
                        Ctrl::If(_) | Ctrl::Else(_) => "IF without THEN",
                        Ctrl::Begin(_) => "BEGIN without UNTIL/AGAIN/REPEAT",
                        Ctrl::While(..) => "WHILE without REPEAT",
                        Ctrl::Do(..) => "DO without LOOP",
                    };
                    return Err(tk.err(format!("`;` reached with an unclosed control structure in `{}`: {what}", o.name)));
                }
                let (name, slot, start) = (o.name.clone(), o.slot, o.start);
                self.emit(Op::Exit, line, tok);
                self.bind_word(&u.ns, &name, slot, start);
                u.open = None;
            }
            ":" | "variable" | "constant" | "create" => {
                return Err(tk.err(format!("`{tok}` inside the definition of `{}`: missing `;` before it?", o.name)));
            }
            "if" => {
                o.ctrl.push(Ctrl::If(here));
                self.emit(Op::ZBranch(0), line, tok);
            }
            "else" => {
                let Some(Ctrl::If(at)) = o.ctrl.pop() else {
                    return Err(tk.err("ELSE without a matching IF"));
                };
                o.ctrl.push(Ctrl::Else(here));
                self.emit(Op::Branch(0), line, tok);
                self.code[at as usize] = Op::ZBranch(here + 1);
            }
            "then" => match o.ctrl.pop() {
                Some(Ctrl::If(at)) => self.code[at as usize] = Op::ZBranch(here),
                Some(Ctrl::Else(at)) => self.code[at as usize] = Op::Branch(here),
                _ => return Err(tk.err("THEN without a matching IF")),
            },
            "begin" => o.ctrl.push(Ctrl::Begin(here)),
            "until" | "again" => {
                let Some(Ctrl::Begin(b)) = o.ctrl.pop() else {
                    return Err(tk.err(format!("{} without a matching BEGIN", t.to_uppercase())));
                };
                self.emit(if t == "until" { Op::ZBranch(b) } else { Op::Branch(b) }, line, tok);
            }
            "while" => {
                let Some(Ctrl::Begin(b)) = o.ctrl.pop() else {
                    return Err(tk.err("WHILE without a matching BEGIN"));
                };
                o.ctrl.push(Ctrl::While(b, here));
                self.emit(Op::ZBranch(0), line, tok);
            }
            "repeat" => {
                let Some(Ctrl::While(b, w)) = o.ctrl.pop() else {
                    return Err(tk.err("REPEAT without a matching BEGIN ... WHILE"));
                };
                self.emit(Op::Branch(b), line, tok);
                self.code[w as usize] = Op::ZBranch(here + 1);
            }
            "do" | "?do" => {
                // ?DO jumps past the loop like a LEAVE, so it is patched with them.
                o.ctrl.push(Ctrl::Do(here + 1, if t == "?do" { vec![here] } else { vec![] }));
                self.emit(if t == "do" { Op::Do } else { Op::QDo(0) }, line, tok);
            }
            "loop" | "+loop" => {
                let Some(Ctrl::Do(body, leaves)) = o.ctrl.pop() else {
                    return Err(tk.err(format!("{} without a matching DO", t.to_uppercase())));
                };
                self.emit(if t == "loop" { Op::Loop(body) } else { Op::PLoop(body) }, line, tok);
                let after = here + 1;
                for l in leaves {
                    self.code[l as usize] = match self.code[l as usize] {
                        Op::QDo(_) => Op::QDo(after),
                        _ => Op::Leave(after),
                    };
                }
            }
            "leave" => {
                let Some(Ctrl::Do(_, leaves)) = o.ctrl.iter_mut().rev().find(|c| matches!(c, Ctrl::Do(..))) else {
                    return Err(tk.err("LEAVE outside a DO loop"));
                };
                leaves.push(here);
                self.emit(Op::Leave(0), line, tok);
            }
            "exit" => self.emit(Op::Exit, line, tok),
            "recurse" => {
                let slot = o.slot;
                self.emit(Op::Call(slot), line, tok);
            }
            _ => self.compile_word(u, tk),
        }
        Ok(())
    }

    /// A number or a word inside a definition. A word that does not resolve
    /// yet is either a live userspace definition (called through its public
    /// slot and compiled lazily) or a forward reference within this source,
    /// checked when the source ends.
    fn compile_word(&mut self, u: &mut Unit, tk: &Token) {
        let op = if let Ok(v) = tk.t.parse::<i64>() {
            Op::Lit(v)
        } else if let Some(op) = self.resolve(&u.ns, &tk.t) {
            op
        } else if self.known.contains(&tk.t) {
            Op::Call(self.public_slot(&tk.t))
        } else {
            let slot = self.private_slot(&u.ns, &tk.t);
            u.forwards.push(Forward { slot, line: tk.line, token: tk.raw.clone() });
            Op::Call(slot)
        };
        self.emit(op, tk.line, &tk.raw);
    }

    fn bind_word(&mut self, ns: &str, name: &str, slot: u32, start: u32) {
        let (def, version) = ns.rsplit_once('/').map_or((ns, 0), |(d, v)| (d, v.parse().unwrap_or(0)));
        self.words.push(Word { name: name.to_string(), def: def.to_string(), version, start });
        self.slots[slot as usize].target = Some((self.words.len() - 1) as u32);
    }

    // ------------------------------------------------------- entry points

    /// Calls the `run` of definition `name` with `input` (JSON text), with
    /// a fresh step budget.
    pub fn call(&mut self, name: &str, input: &[u8]) -> Status {
        let slot = self.public_slot(name);
        let mut ex = std::mem::replace(&mut self.main, Exec::new());
        ex.ds.clear();
        ex.frames.clear();
        ex.rstack.clear();
        ex.loops.clear();
        self.builders.clear();
        self.console.clear();
        self.lessons.clear();
        self.scratch_top = 0;
        self.pending = None;
        self.steps_left = self.max_steps;
        self.ctx_def = name.to_string();
        self.ctx_version = self.slots[slot as usize].version;
        ex.set_word(NO_WORD);
        ex.set_ip(0);
        self.code[0] = Op::Call(slot);
        let st = match self.sput(&mut ex, input) {
            Err(Fail::Err(json)) => Status::Err(json),
            _ => self.continue_main(&mut ex),
        };
        self.main = ex;
        st
    }

    fn continue_main(&mut self, ex: &mut Exec) -> Status {
        match self.run(ex) {
            // ctx_version is already the called version: `call` set it from
            // the slot, and `resume` updates it if that slot was loaded lazily.
            Ok(()) => {
                let n = ex.ds.len();
                if n != 2 || !self.in_data(ex.ds[0], ex.ds[1]) {
                    let what = if n != 2 {
                        format!("it left {n} item(s)")
                    } else {
                        format!("( {} {} ) is not a string in data space", ex.ds[0], ex.ds[1])
                    };
                    let msg = format!(
                        "contract: `run` of {} must consume its input ( json-addr json-len ) and leave exactly ( out-addr out-len ); {what}",
                        self.ctx_def
                    );
                    return Status::Err(self.report(ex, "contract", &msg));
                }
                Status::Ok(self.bytes(ex, ex.ds[0], ex.ds[1], "result").unwrap_or(&[]).to_vec())
            }
            Err(Fail::Err(json)) => Status::Err(json),
            Err(Fail::Need(s)) => {
                self.pending = Some(s);
                Status::Need(self.slots[s as usize].name.clone())
            }
        }
    }

    /// Continues a call suspended with `Need`: `ok` says whether the kernel
    /// compiled the missing definition. The step budget carries on.
    pub fn resume(&mut self, ok: bool) -> Status {
        let Some(s) = self.pending.take() else {
            return Status::Err("{\"kind\":\"vm-error\",\"message\":\"resume without a pending load\"}".into());
        };
        let mut ex = std::mem::replace(&mut self.main, Exec::new());
        let st = if !ok {
            let name = self.slots[s as usize].name.clone();
            let msg = format!("undefined word `{name}`: no live userspace definition with that name");
            Status::Err(self.report(&ex, "undefined-word", &msg))
        } else {
            if let Some(Op::Call(top)) = self.code.first()
                && *top == s
            {
                self.ctx_version = self.slots[s as usize].version;
            }
            self.continue_main(&mut ex)
        };
        self.main = ex;
        st
    }
}

enum HostOut {
    Str(Vec<u8>),
    Num(i64),
    None,
}

fn json_type(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

/// `a.b.0.c`: object keys and array indexes separated by dots; "" is the root.
fn lookup<'a>(root: &'a Value, path: &str) -> Result<&'a Value, String> {
    let mut cur = root;
    if path.is_empty() {
        return Ok(cur);
    }
    for seg in path.split('.') {
        cur = match cur {
            Value::Object(m) => m.get(seg).ok_or_else(|| {
                let keys: Vec<&String> = m.keys().take(20).collect();
                format!("key `{seg}` not found (path `{path}`); keys here: {keys:?}")
            })?,
            Value::Array(a) => {
                let i: usize = seg.parse().map_err(|_| format!("`{seg}` is not an index into an array of {} (path `{path}`)", a.len()))?;
                a.get(i).ok_or_else(|| format!("index {i} out of range: the array has {} item(s) (path `{path}`)", a.len()))?
            }
            other => return Err(format!("cannot index {} with `{seg}` (path `{path}`)", json_type(other))),
        };
    }
    Ok(cur)
}

fn parse_json(x: &[u8]) -> Result<Value, String> {
    serde_json::from_slice(x).map_err(|e| {
        let s = String::from_utf8_lossy(&x[..x.len().min(80)]).into_owned();
        format!("the JSON argument is not valid JSON ({e}): {s:?}")
    })
}
