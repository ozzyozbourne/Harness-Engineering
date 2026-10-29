//! cmh Forth: a small, late-bound Forth that runs inside WebAssembly.
//!
//! Everything the Forth image consists of lives in this module's linear
//! memory: the dictionary (words, slots, namespaces), compiled code, the data
//! space (`HERE`, `VARIABLE`, string literals), the per-call string scratch
//! space, and the stacks. A copy of linear memory is therefore a complete
//! snapshot of the image; the host uses that for rollback.
//!
//! Late binding: every compiled call goes through a slot (an indirection cell
//! with a name). Redefining a word rebinds its slot, so every existing caller
//! uses the new code on its next call. Public slots are named after userspace
//! definitions; an unbound public slot makes the VM suspend with NEED_LOAD so
//! the host can compile that definition lazily and resume.
//!
//! Errors never panic: they come back to the host as JSON (kind, word, def,
//! version, line, token, data stack, call trace, console output).

use std::collections::BTreeMap;
use std::fmt::Write as _;

const PERM_SIZE: usize = 1 << 20;
const SCRATCH_SIZE: usize = 2 << 20;
const DS_MAX: usize = 1024;
const RS_MAX: usize = 512;
const US_MAX: usize = 256;
const LS_MAX: usize = 64;
const CONSOLE_MAX: usize = 4096;
const BUILDER_MAX: usize = 1 << 20;
const NO_WORD: u32 = u32::MAX;

const ST_OK: i32 = 0;
const ST_ERR: i32 = 1;
const ST_NEED: i32 = 2;

static mut PERM: [u8; PERM_SIZE] = [0; PERM_SIZE];
static mut SCRATCH: [u8; SCRATCH_SIZE] = [0; SCRATCH_SIZE];
static mut VM: Option<Vm> = None;
static mut MAIN: Exec = Exec::new();
static mut MAILBOX: Vec<u8> = Vec::new();
static mut OUT: Vec<u8> = Vec::new();
/// The executor that is currently running (read after a trap by `cmh_where`).
static mut CUR: *mut Exec = std::ptr::null_mut();

#[link(wasm_import_module = "cmh")]
unsafe extern "C" {
    /// Host word `op` on strings (a, al) and (b, bl). Returns a result length
    /// (string ops) or value (json-len); `i64::MIN` means error, with the
    /// message pending.
    fn host_word(op: i32, a: i32, al: i32, b: i32, bl: i32) -> i64;
    /// Copies the pending host result (or error message) to `dst`.
    fn host_take(dst: i32);
    fn host_pending_len() -> i32;
    /// 1 if `name` is a live userspace definition the VM may bind lazily.
    fn host_known(p: i32, l: i32) -> i32;
    fn host_panic(p: i32, l: i32);
}

fn perm_base() -> i64 {
    (&raw const PERM) as usize as i64
}
fn scratch_base() -> i64 {
    (&raw const SCRATCH) as usize as i64
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum P {
    Dup, Drop, Swap, Over, Rot, MRot, Nip, Tuck, Dup2, Drop2, Swap2, Over2, Pick, Depth, ToR, RFrom, RFetch,
    Add, Sub, Mul, Div, Mod, DivMod, Negate, Abs, Min, Max, Inc, Dec,
    Eq, Ne, Lt, Gt, Le, Ge, ZEq, ZLt, ZGt, And, Or, Xor, Invert, True, False, Within,
    Fetch, Store, CFetch, CStore, PlusStore, Here, Allot, Comma, CComma, Cells,
    I, J, Type, Emit, Cr, Dot, DotS, Bl,
    Compare, SEq, Index, Concat, SlashString, Lower, Trim,
    BStart, BAdd, BChar, BNum, BEnd, NToS, SToN,
    JsonGet, JsonLen, JsonStr, FileRead, FileWrite, Remember,
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
    ":", ";", "if", "else", "then", "begin", "until", "again", "while", "repeat", "do", "?do", "loop", "+loop",
    "leave", "exit", "recurse", "s\"", "s\\\"", ".\"", "(", "\\", "char", "[char]", "variable", "constant", "create",
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

struct Word {
    name: String,
    def: String,
    version: u32,
    start: u32,
}

struct Slot {
    name: String,
    target: Option<u32>,
    /// For public slots: the definition version currently bound.
    version: u32,
}

#[derive(Clone, Copy)]
struct Frame {
    ret: u32,
    word: u32,
    lsd: u32,
}

struct Exec {
    ds: Vec<i64>,
    rs: Vec<Frame>,
    us: Vec<i64>,
    ls: Vec<(i64, i64)>,
    ip: u32,
    /// The instruction being executed (for error reports).
    at: u32,
    word: u32,
}

impl Exec {
    const fn new() -> Exec {
        Exec { ds: Vec::new(), rs: Vec::new(), us: Vec::new(), ls: Vec::new(), ip: 0, at: 0, word: NO_WORD }
    }
    // Volatile so that after a trap (e.g. out of fuel in a tight loop) the
    // host can still see where execution was.
    fn set_ip(&mut self, ip: u32) {
        unsafe { std::ptr::write_volatile(&raw mut self.ip, ip) }
    }
    fn set_at(&mut self, at: u32) {
        unsafe { std::ptr::write_volatile(&raw mut self.at, at) }
    }
    fn set_word(&mut self, w: u32) {
        unsafe { std::ptr::write_volatile(&raw mut self.word, w) }
    }
    fn pop(&mut self) -> i64 {
        self.ds.pop().unwrap_or(0)
    }
}

enum Fail {
    Err(String),
    Need(u32),
}

struct Vm {
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

enum Ctrl {
    If(u32),
    Else(u32),
    Begin(u32),
    While(u32, u32),
    Do(u32, Vec<u32>),
}

struct Open {
    name: String,
    slot: u32,
    start: u32,
    ctrl: Vec<Ctrl>,
}

struct CErr {
    line: u32,
    token: String,
    msg: String,
}

fn cerr(line: u32, token: &str, msg: impl Into<String>) -> CErr {
    CErr { line, token: token.to_string(), msg: msg.into() }
}

impl Vm {
    fn new() -> Vm {
        let mut vm = Vm {
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
        // Trampoline: code[0] is patched with the slot being called.
        vm.emit(Op::Call(0), 0, "<entry>");
        vm.emit(Op::Halt, 0, "<entry>");
        vm
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

    // ------------------------------------------------------------ reports

    fn word_info(&self, w: u32) -> (String, String, u32) {
        match self.words.get(w as usize) {
            Some(w) => (w.name.clone(), w.def.clone(), w.version),
            None => (String::new(), self.ctx_def.clone(), self.ctx_version),
        }
    }

    fn in_data(&self, a: i64, u: i64) -> bool {
        let (pb, sb) = (perm_base(), scratch_base());
        u >= 0
            && ((a >= pb && a.saturating_add(u) <= pb + PERM_SIZE as i64)
                || (a >= sb && a.saturating_add(u) <= sb + SCRATCH_SIZE as i64))
    }

    fn preview(&self, a: i64, u: i64) -> Option<String> {
        if u <= 0 || u > 4096 || !self.in_data(a, u) {
            return None;
        }
        let bytes = unsafe { std::slice::from_raw_parts(a as usize as *const u8, u as usize) };
        let s = String::from_utf8_lossy(&bytes[..bytes.len().min(80)]).into_owned();
        Some(if u > 80 { format!("{s}...") } else { s })
    }

    fn report(&self, ex: &Exec, kind: &str, msg: &str) -> String {
        let (word, def, version) = self.word_info(ex.word);
        let (line, tok) = self.loc.get(ex.at as usize).cloned().unwrap_or((0, String::new()));
        let n = ex.ds.len();
        let top: Vec<String> = ex.ds[n.saturating_sub(8)..].iter().map(|v| v.to_string()).collect();
        let mut trace = Vec::new();
        for f in ex.rs.iter().rev() {
            if f.word == NO_WORD {
                continue;
            }
            let (w, d, v) = self.word_info(f.word);
            let (l, t) = self.loc.get((f.ret as usize).saturating_sub(1)).cloned().unwrap_or((0, String::new()));
            trace.push(js(&format!("`{w}` ({d} v{v}) line {l} at `{t}`")));
        }
        let hint = if n >= 2 { self.preview(ex.ds[n - 2], ex.ds[n - 1]) } else { None };
        format!(
            "{{\"kind\":{},\"message\":{},\"word\":{},\"def\":{},\"version\":{},\"line\":{},\"token\":{},\"depth\":{},\"stack\":[{}],\"trace\":[{}],\"top_string\":{},\"output\":{}}}",
            js(kind),
            js(msg),
            js(&word),
            js(&def),
            version,
            line,
            js(&tok),
            n,
            top.join(","),
            trace.join(","),
            hint.map_or("null".into(), |h| js(&h)),
            js(&String::from_utf8_lossy(&self.console)),
        )
    }

    fn fail(&self, ex: &Exec, kind: &str, msg: impl AsRef<str>) -> Fail {
        Fail::Err(self.report(ex, kind, msg.as_ref()))
    }

    fn compile_report(&self, def: &str, version: u32, word: &str, e: &CErr) -> String {
        format!(
            "{{\"kind\":\"compile-error\",\"message\":{},\"word\":{},\"def\":{},\"version\":{},\"line\":{},\"token\":{},\"depth\":0,\"stack\":[],\"trace\":[],\"top_string\":null,\"output\":{}}}",
            js(&e.msg),
            js(word),
            js(def),
            version,
            e.line,
            js(&e.token),
            js(&String::from_utf8_lossy(&self.console)),
        )
    }

    // ------------------------------------------------------------- memory

    /// Raw cell/byte access. Data space and string scratch are allowed; other
    /// addresses inside linear memory belong to the VM itself and are refused;
    /// addresses past the end of linear memory are left to the WebAssembly
    /// sandbox, which traps.
    fn addr(&self, ex: &Exec, a: i64, n: usize, what: &str) -> Result<*mut u8, Fail> {
        if self.in_data(a, n as i64) {
            return Ok(a as usize as *mut u8);
        }
        let end = a.saturating_add(n as i64);
        if a < 0 || end > u32::MAX as i64 {
            return Err(self.fail(ex, "bad-address", format!("{what}: address {a} is outside the 32-bit address space")));
        }
        let mem = core::arch::wasm32::memory_size(0) as i64 * 65536;
        if end <= mem {
            let (pb, sb) = (perm_base(), scratch_base());
            return Err(self.fail(
                ex,
                "bad-address",
                format!(
                    "{what}: address {a} is not in Forth data space (data space {pb}..{}, strings {sb}..{}); valid addresses come from HERE, VARIABLE, CREATE and strings",
                    pb + PERM_SIZE as i64,
                    sb + SCRATCH_SIZE as i64
                ),
            ));
        }
        Ok(a as usize as *mut u8)
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
        let a = scratch_base() + self.scratch_top as i64;
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
        let a = perm_base() + self.here as i64;
        self.here += n;
        Some(a)
    }

    fn console_add(&mut self, b: &[u8]) {
        let room = CONSOLE_MAX.saturating_sub(self.console.len());
        self.console.extend_from_slice(&b[..b.len().min(room)]);
    }

    fn push(&self, ex: &mut Exec, v: i64) -> Result<(), Fail> {
        if ex.ds.len() >= DS_MAX {
            return Err(self.fail(ex, "stack-overflow", format!("data stack overflow ({DS_MAX} items); a loop is probably pushing without dropping")));
        }
        ex.ds.push(v);
        Ok(())
    }

    // ---------------------------------------------------------- execution

    fn run(&mut self, ex: &mut Exec) -> Result<(), Fail> {
        unsafe { CUR = ex as *mut Exec };
        loop {
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
                        if ex.rs.len() >= RS_MAX {
                            let name = self.slots[s as usize].name.clone();
                            return Err(self.fail(
                                ex,
                                "return-stack-overflow",
                                format!("call depth exceeded {RS_MAX} calling `{name}` (unbounded recursion?)"),
                            ));
                        }
                        ex.rs.push(Frame { ret: ip as u32 + 1, word: ex.word, lsd: ex.ls.len() as u32 });
                        ex.set_word(w);
                        ex.set_ip(self.words[w as usize].start);
                    }
                    None => {
                        ex.set_ip(ip as u32);
                        return Err(Fail::Need(s));
                    }
                },
                Op::Exit => match ex.rs.pop() {
                    Some(f) => {
                        ex.ls.truncate(f.lsd as usize);
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
                    if ex.ls.len() >= LS_MAX {
                        return Err(self.fail(ex, "stack-overflow", format!("more than {LS_MAX} nested DO loops")));
                    }
                    ex.ls.push((start, limit));
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
                    let Some(l) = ex.ls.last_mut() else {
                        return Err(self.fail(ex, "vm-error", "LOOP without an active DO"));
                    };
                    l.0 = l.0.wrapping_add(step);
                    let again = if step >= 0 { l.0 < l.1 } else { l.0 >= l.1 };
                    if again {
                        ex.set_ip(t);
                    } else {
                        ex.ls.pop();
                    }
                }
                Op::Leave(t) => {
                    ex.ls.pop();
                    ex.set_ip(t);
                }
            }
        }
    }

    fn prim(&mut self, ex: &mut Exec, p: P) -> Result<(), Fail> {
        let (name, need, effect) = prim_info(p);
        if ex.ds.len() < need {
            let have = ex.ds.len();
            return Err(self.fail(
                ex,
                "stack-underflow",
                format!("`{name}` {effect} needs {need} item(s) but the stack has {have}"),
            ));
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
                if ex.us.len() >= US_MAX {
                    return Err(self.fail(ex, "stack-overflow", format!("`>r` return stack full ({US_MAX})")));
                }
                let v = ex.pop();
                ex.us.push(v);
            }
            P::RFrom | P::RFetch => {
                let Some(&v) = ex.us.last() else {
                    return Err(self.fail(ex, "stack-underflow", format!("`{name}` but nothing was pushed with `>r`")));
                };
                if p == P::RFrom {
                    ex.us.pop();
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
            P::Here => self.push(ex, perm_base() + self.here as i64)?,
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
                if ex.ls.len() < depth {
                    return Err(self.fail(ex, "stack-underflow", format!("`{name}` used outside a DO loop (or J outside a nested loop)")));
                }
                let v = ex.ls[ex.ls.len() - depth].0;
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
                        let pos = if s2.is_empty() {
                            Some(0)
                        } else {
                            s1.windows(s2.len()).position(|w| w == s2)
                        };
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

    fn host(&mut self, ex: &mut Exec, p: P, name: &str) -> Result<(), Fail> {
        let op = match p {
            P::JsonGet => 1,
            P::JsonLen => 2,
            P::JsonStr => 3,
            P::FileRead => 4,
            P::FileWrite => 5,
            _ => 6,
        };
        let two = matches!(p, P::JsonGet | P::JsonLen | P::FileWrite);
        let n = ex.ds.len();
        let what = format!("`{name}`");
        let (a, b) = if two {
            ((ex.ds[n - 4], ex.ds[n - 3]), (ex.ds[n - 2], ex.ds[n - 1]))
        } else {
            ((ex.ds[n - 2], ex.ds[n - 1]), (0, 0))
        };
        self.bytes(ex, a.0, a.1, &what)?;
        self.bytes(ex, b.0, b.1, &what)?;
        let r = unsafe { host_word(op, a.0 as i32, a.1 as i32, b.0 as i32, b.1 as i32) };
        if r == i64::MIN {
            let len = unsafe { host_pending_len() }.max(0) as usize;
            let mut buf = vec![0u8; len];
            unsafe { host_take(buf.as_mut_ptr() as usize as i32) };
            return Err(self.fail(ex, "host-error", format!("{what}: {}", String::from_utf8_lossy(&buf))));
        }
        ex.ds.truncate(n - if two { 4 } else { 2 });
        match op {
            1 | 3 | 4 => {
                let addr = self.salloc(ex, r.max(0) as usize)?;
                unsafe { host_take(addr as i32) };
                self.push(ex, addr)?;
                self.push(ex, r)?;
            }
            2 => self.push(ex, r)?,
            _ => {}
        }
        Ok(())
    }

    // ----------------------------------------------------------- compiler

    /// Compiles one userspace definition version into its own namespace and,
    /// on success, binds the public slot `def` to its `run` word.
    fn define(&mut self, def: &str, version: u32, src: &str) -> Result<(), String> {
        let saved = (std::mem::take(&mut self.ctx_def), self.ctx_version);
        let cur = unsafe { CUR };
        self.ctx_def = def.to_string();
        self.ctx_version = version;
        let r = self.define_inner(def, version, src);
        (self.ctx_def, self.ctx_version) = saved;
        // The compile-time executor lived on this stack frame; never leave
        // the trap reporter pointing at it.
        unsafe { CUR = cur };
        r
    }

    fn define_inner(&mut self, def: &str, version: u32, src: &str) -> Result<(), String> {
        let ns = format!("{def}/{version}");
        if prim(def).is_some() || SYNTAX.contains(&def) {
            let e = cerr(0, def, format!("definition name `{def}` collides with a core Forth word; choose another name"));
            return Err(self.compile_report(def, version, "", &e));
        }
        self.privs.insert(ns.clone(), BTreeMap::new());
        let mut tmp = Exec::new();
        let mut open: Option<Open> = None;
        let mut forwards: Vec<(u32, u32, String)> = Vec::new();
        let r = self.compile(&ns, src, &mut tmp, &mut open, &mut forwards);
        let word = open.as_ref().map(|o| o.name.clone()).unwrap_or_default();
        let fail = |vm: &Vm, e: CErr| Err(vm.compile_report(def, version, &word, &e));
        match r {
            Ok(()) => {}
            Err(Ok(e)) => return fail(self, e),
            Err(Err(json)) => return Err(json),
        }
        if let Some(o) = &open {
            return fail(self, cerr(0, &o.name, format!("definition `{}` is not closed: missing `;`", o.name)));
        }
        for (slot, line, tok) in &forwards {
            if self.slots[*slot as usize].target.is_none() {
                let mut msg = format!("undefined word `{tok}`");
                if let Some(s) = self.suggest(&ns, tok) {
                    let _ = write!(msg, "; did you mean `{s}`?");
                }
                return fail(self, cerr(*line, tok, msg));
            }
        }
        if !tmp.ds.is_empty() {
            let items: Vec<String> = tmp.ds.iter().map(|v| v.to_string()).collect();
            return fail(self, cerr(0, "", format!("top-level code left {} item(s) on the stack: [{}]", items.len(), items.join(" "))));
        }
        let Some(run) = self.privs[&ns].get("run").and_then(|s| self.slots[*s as usize].target) else {
            return fail(self, cerr(0, "", "the source must define `: run ( json-addr json-len -- out-addr out-len ) ... ;`"));
        };
        self.compiled.insert((def.to_string(), version), run);
        let slot = self.public_slot(def);
        self.slots[slot as usize].target = Some(run);
        self.slots[slot as usize].version = version;
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

    #[allow(clippy::type_complexity)]
    fn compile(
        &mut self,
        ns: &str,
        src: &str,
        tmp: &mut Exec,
        open: &mut Option<Open>,
        forwards: &mut Vec<(u32, u32, String)>,
    ) -> Result<(), Result<CErr, String>> {
        let mut s = Src { b: src.as_bytes(), pos: 0, line: 1 };
        while let Some((tok, line)) = s.token() {
            let t = tok.to_lowercase();
            let err = |m: String| Err(Ok(cerr(line, &tok, m)));
            // Parsing words, valid in both states.
            match t.as_str() {
                "\\" => {
                    s.skip_line();
                    continue;
                }
                "(" => {
                    if s.until(b')', false).is_none() {
                        return err("unterminated comment: missing `)`".into());
                    }
                    continue;
                }
                "s\"" | "s\\\"" | ".\"" => {
                    let Some(text) = s.until(b'"', t == "s\\\"") else {
                        return err(format!("unterminated string after `{tok}`: missing closing \""));
                    };
                    let Some(a) = self.palloc(text.len()) else {
                        return err("data space full while storing a string literal".into());
                    };
                    unsafe { std::ptr::copy_nonoverlapping(text.as_ptr(), a as usize as *mut u8, text.len()) };
                    let ops = [Op::Lit(a), Op::Lit(text.len() as i64)];
                    if open.is_some() {
                        for op in ops {
                            self.emit(op, line, &tok);
                        }
                        if t == ".\"" {
                            self.emit(Op::Prim(P::Type), line, &tok);
                        }
                    } else if t == ".\"" {
                        self.console_add(&text);
                    } else {
                        tmp.ds.push(a);
                        tmp.ds.push(text.len() as i64);
                    }
                    continue;
                }
                "char" | "[char]" => {
                    let Some((c, _)) = s.token() else {
                        return err(format!("`{tok}` needs a character after it"));
                    };
                    let v = i64::from(c.as_bytes()[0]);
                    if open.is_some() {
                        self.emit(Op::Lit(v), line, &tok);
                    } else {
                        tmp.ds.push(v);
                    }
                    continue;
                }
                _ => {}
            }
            if open.is_none() {
                match t.as_str() {
                    ":" | "variable" | "constant" | "create" => {
                        let Some((name, _)) = s.token() else {
                            return err(format!("`{tok}` needs a name after it"));
                        };
                        let name = name.to_lowercase();
                        if name.parse::<i64>().is_ok() || SYNTAX.contains(&name.as_str()) {
                            return err(format!("`{name}` cannot be used as a word name"));
                        }
                        let slot = match self.privs[ns].get(&name) {
                            Some(s) => *s,
                            None => {
                                let s = self.new_slot(&format!("{ns}:{name}"));
                                self.privs.get_mut(ns).expect("ns").insert(name.clone(), s);
                                s
                            }
                        };
                        let start = self.code.len() as u32;
                        if t == ":" {
                            *open = Some(Open { name, slot, start, ctrl: Vec::new() });
                            continue;
                        }
                        let value = match t.as_str() {
                            "variable" => match self.palloc(8) {
                                Some(a) => {
                                    unsafe { std::ptr::write_unaligned(a as usize as *mut i64, 0) };
                                    a
                                }
                                None => return err("data space full".into()),
                            },
                            "constant" => match tmp.ds.pop() {
                                Some(v) => v,
                                None => return err(format!("`constant {name}` needs a value on the stack, e.g. `10 constant {name}`")),
                            },
                            _ => perm_base() + self.here as i64,
                        };
                        self.emit(Op::Lit(value), line, &name);
                        self.emit(Op::Exit, line, &name);
                        self.bind_word(ns, &name, slot, start);
                        continue;
                    }
                    ";" | "if" | "else" | "then" | "begin" | "until" | "again" | "while" | "repeat" | "do" | "?do"
                    | "loop" | "+loop" | "leave" | "exit" | "recurse" => {
                        return err(format!("`{tok}` is only valid inside a `: name ... ;` definition"));
                    }
                    _ => {}
                }
                // Interpret: numbers push, words execute now.
                if let Ok(v) = t.parse::<i64>() {
                    tmp.ds.push(v);
                    continue;
                }
                let op = match self.privs[ns].get(&t).copied() {
                    Some(slot) => Op::Call(slot),
                    None => match prim(&t) {
                        Some((p, _, _)) => Op::Prim(p),
                        None => match self.public.get(&t) {
                            Some(slot) => Op::Call(*slot),
                            None => {
                                let mut msg = format!("undefined word `{tok}`");
                                if let Some(s) = self.suggest(ns, &tok) {
                                    let _ = write!(msg, "; did you mean `{s}`?");
                                }
                                return err(msg);
                            }
                        },
                    },
                };
                let mark = self.code.len();
                self.emit(op, line, &tok);
                self.emit(Op::Halt, line, &tok);
                tmp.ip = mark as u32;
                tmp.word = NO_WORD;
                let r = self.run(tmp);
                self.code.truncate(mark);
                self.loc.truncate(mark);
                match r {
                    Ok(()) => {}
                    Err(Fail::Err(json)) => return Err(Err(json)),
                    Err(Fail::Need(_)) => {
                        return err(format!(
                            "`{tok}` is loaded lazily and cannot run at load time; call it from inside a word instead"
                        ));
                    }
                }
                continue;
            }
            // Compiling.
            let here = self.code.len() as u32;
            let o = open.as_mut().expect("open");
            match t.as_str() {
                ";" => {
                    if let Some(c) = o.ctrl.last() {
                        let what = match c {
                            Ctrl::If(_) | Ctrl::Else(_) => "IF without THEN",
                            Ctrl::Begin(_) => "BEGIN without UNTIL/AGAIN/REPEAT",
                            Ctrl::While(..) => "WHILE without REPEAT",
                            Ctrl::Do(..) => "DO without LOOP",
                        };
                        return err(format!("`;` reached with an unclosed control structure in `{}`: {what}", o.name));
                    }
                    let (name, slot, start) = (o.name.clone(), o.slot, o.start);
                    self.emit(Op::Exit, line, &tok);
                    self.bind_word(ns, &name, slot, start);
                    *open = None;
                }
                ":" | "variable" | "constant" | "create" => {
                    return err(format!("`{tok}` inside the definition of `{}`: missing `;` before it?", o.name));
                }
                "if" => {
                    o.ctrl.push(Ctrl::If(here));
                    self.emit(Op::ZBranch(0), line, &tok);
                }
                "else" => {
                    let Some(Ctrl::If(at)) = o.ctrl.pop() else {
                        return err("ELSE without a matching IF".into());
                    };
                    o.ctrl.push(Ctrl::Else(here));
                    self.emit(Op::Branch(0), line, &tok);
                    self.code[at as usize] = Op::ZBranch(here + 1);
                }
                "then" => match o.ctrl.pop() {
                    Some(Ctrl::If(at)) => self.code[at as usize] = Op::ZBranch(here),
                    Some(Ctrl::Else(at)) => self.code[at as usize] = Op::Branch(here),
                    _ => return err("THEN without a matching IF".into()),
                },
                "begin" => o.ctrl.push(Ctrl::Begin(here)),
                "until" | "again" => {
                    let Some(Ctrl::Begin(b)) = o.ctrl.pop() else {
                        return err(format!("{} without a matching BEGIN", t.to_uppercase()));
                    };
                    self.emit(if t == "until" { Op::ZBranch(b) } else { Op::Branch(b) }, line, &tok);
                }
                "while" => {
                    let Some(Ctrl::Begin(b)) = o.ctrl.pop() else {
                        return err("WHILE without a matching BEGIN".into());
                    };
                    o.ctrl.push(Ctrl::While(b, here));
                    self.emit(Op::ZBranch(0), line, &tok);
                }
                "repeat" => {
                    let Some(Ctrl::While(b, w)) = o.ctrl.pop() else {
                        return err("REPEAT without a matching BEGIN ... WHILE".into());
                    };
                    self.emit(Op::Branch(b), line, &tok);
                    self.code[w as usize] = Op::ZBranch(here + 1);
                }
                "do" | "?do" => {
                    o.ctrl.push(Ctrl::Do(here + 1, if t == "?do" { vec![here] } else { vec![] }));
                    self.emit(if t == "do" { Op::Do } else { Op::QDo(0) }, line, &tok);
                }
                "loop" | "+loop" => {
                    let Some(Ctrl::Do(body, leaves)) = o.ctrl.pop() else {
                        return err(format!("{} without a matching DO", t.to_uppercase()));
                    };
                    self.emit(if t == "loop" { Op::Loop(body) } else { Op::PLoop(body) }, line, &tok);
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
                        return err("LEAVE outside a DO loop".into());
                    };
                    leaves.push(here);
                    self.emit(Op::Leave(0), line, &tok);
                }
                "exit" => self.emit(Op::Exit, line, &tok),
                "recurse" => {
                    let slot = o.slot;
                    self.emit(Op::Call(slot), line, &tok);
                }
                _ => {
                    if let Ok(v) = t.parse::<i64>() {
                        self.emit(Op::Lit(v), line, &tok);
                        continue;
                    }
                    let op = if let Some(slot) = self.privs[ns].get(&t) {
                        Op::Call(*slot)
                    } else if let Some((p, _, _)) = prim(&t) {
                        Op::Prim(p)
                    } else if let Some(slot) = self.public.get(&t) {
                        Op::Call(*slot)
                    } else if unsafe { host_known(t.as_ptr() as usize as i32, t.len() as i32) } == 1 {
                        Op::Call(self.public_slot(&t))
                    } else {
                        // Forward reference within this definition; checked at the end.
                        let slot = self.new_slot(&format!("{ns}:{t}"));
                        self.privs.get_mut(ns).expect("ns").insert(t.clone(), slot);
                        forwards.push((slot, line, tok.clone()));
                        Op::Call(slot)
                    };
                    self.emit(op, line, &tok);
                }
            }
        }
        Ok(())
    }

    fn bind_word(&mut self, ns: &str, name: &str, slot: u32, start: u32) {
        let (def, version) = ns.rsplit_once('/').map_or((ns, 0), |(d, v)| (d, v.parse().unwrap_or(0)));
        self.words.push(Word { name: name.to_string(), def: def.to_string(), version, start });
        self.slots[slot as usize].target = Some((self.words.len() - 1) as u32);
    }

    // ------------------------------------------------------- entry points

    fn call(&mut self, name: &str, input: &[u8]) -> i32 {
        let slot = self.public_slot(name);
        let ex = unsafe { &mut *(&raw mut MAIN) };
        unsafe { CUR = ex as *mut Exec };
        ex.ds.clear();
        ex.rs.clear();
        ex.us.clear();
        ex.ls.clear();
        self.builders.clear();
        self.console.clear();
        self.scratch_top = 0;
        self.pending = None;
        self.ctx_def = name.to_string();
        self.ctx_version = self.slots[slot as usize].version;
        ex.set_word(NO_WORD);
        ex.set_ip(0);
        self.code[0] = Op::Call(slot);
        if let Err(Fail::Err(json)) = self.sput(ex, input) {
            set_out(json.as_bytes());
            return ST_ERR;
        }
        self.continue_main()
    }

    fn continue_main(&mut self) -> i32 {
        let ex = unsafe { &mut *(&raw mut MAIN) };
        match self.run(ex) {
            Ok(()) => {
                if let Some(Op::Call(s)) = self.code.first() {
                    self.ctx_version = self.slots[*s as usize].version;
                }
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
                    set_out(self.report(ex, "contract", &msg).as_bytes());
                    return ST_ERR;
                }
                let out = self.bytes(ex, ex.ds[0], ex.ds[1], "result").unwrap_or(&[]);
                set_out(out);
                ST_OK
            }
            Err(Fail::Err(json)) => {
                set_out(json.as_bytes());
                ST_ERR
            }
            Err(Fail::Need(s)) => {
                self.pending = Some(s);
                let name = self.slots[s as usize].name.clone();
                set_out(name.as_bytes());
                ST_NEED
            }
        }
    }

    fn resume(&mut self, ok: bool) -> i32 {
        let Some(s) = self.pending.take() else {
            set_out(b"{\"kind\":\"vm-error\",\"message\":\"resume without a pending load\"}");
            return ST_ERR;
        };
        if !ok {
            let ex = unsafe { &mut *(&raw mut MAIN) };
            let name = self.slots[s as usize].name.clone();
            let msg = format!("undefined word `{name}`: no live userspace definition with that name");
            set_out(self.report(ex, "undefined-word", &msg).as_bytes());
            return ST_ERR;
        }
        if let Some(Op::Call(top)) = self.code.first()
            && *top == s
        {
            self.ctx_version = self.slots[s as usize].version;
        }
        self.continue_main()
    }
}

fn vm() -> &'static mut Vm {
    unsafe {
        let p = &raw mut VM;
        if (*p).is_none() {
            *p = Some(Vm::new());
        }
        (*p).as_mut().expect("vm")
    }
}

fn set_out(b: &[u8]) {
    unsafe {
        let o = &mut *(&raw mut OUT);
        o.clear();
        o.extend_from_slice(b);
    }
}

fn mailbox() -> &'static [u8] {
    unsafe { &*(&raw const MAILBOX) }
}

fn mail_str(from: usize, len: usize) -> String {
    let m = mailbox();
    String::from_utf8_lossy(&m[from.min(m.len())..(from + len).min(m.len())]).into_owned()
}

#[unsafe(no_mangle)]
pub extern "C" fn cmh_init() {
    std::panic::set_hook(Box::new(|info| {
        let msg = info.to_string();
        unsafe { host_panic(msg.as_ptr() as usize as i32, msg.len() as i32) };
    }));
    vm();
}

/// Ensures the mailbox holds `len` bytes and returns its address.
#[unsafe(no_mangle)]
pub extern "C" fn cmh_mailbox(len: i32) -> i32 {
    unsafe {
        let m = &mut *(&raw mut MAILBOX);
        m.clear();
        m.resize(len.max(0) as usize, 0);
        m.as_ptr() as usize as i32
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn cmh_out_ptr() -> i32 {
    unsafe { (*(&raw const OUT)).as_ptr() as usize as i32 }
}

#[unsafe(no_mangle)]
pub extern "C" fn cmh_out_len() -> i32 {
    unsafe { (*(&raw const OUT)).len() as i32 }
}

/// Mailbox: name ++ source.
#[unsafe(no_mangle)]
pub extern "C" fn cmh_define(name_len: i32, src_len: i32, version: i32) -> i32 {
    let name = mail_str(0, name_len as usize);
    let src = mail_str(name_len as usize, src_len as usize);
    match vm().define(&name, version as u32, &src) {
        Ok(()) => {
            set_out(b"");
            ST_OK
        }
        Err(json) => {
            set_out(json.as_bytes());
            ST_ERR
        }
    }
}

/// Mailbox: name ++ input JSON.
#[unsafe(no_mangle)]
pub extern "C" fn cmh_call(name_len: i32, input_len: i32) -> i32 {
    let name = mail_str(0, name_len as usize);
    let m = mailbox();
    let input = m[(name_len as usize).min(m.len())..((name_len + input_len) as usize).min(m.len())].to_vec();
    vm().call(&name, &input)
}

#[unsafe(no_mangle)]
pub extern "C" fn cmh_resume(ok: i32) -> i32 {
    vm().resume(ok != 0)
}

/// Points public slot `name` at an already compiled version. 1 if bound.
#[unsafe(no_mangle)]
pub extern "C" fn cmh_bind(name_len: i32, version: i32) -> i32 {
    let name = mail_str(0, name_len as usize);
    let vm = vm();
    let Some(run) = vm.compiled.get(&(name.clone(), version as u32)).copied() else { return 0 };
    let slot = vm.public_slot(&name);
    vm.slots[slot as usize].target = Some(run);
    vm.slots[slot as usize].version = version as u32;
    1
}

#[unsafe(no_mangle)]
pub extern "C" fn cmh_unbind(name_len: i32) {
    let name = mail_str(0, name_len as usize);
    let vm = vm();
    if let Some(s) = vm.public.get(&name).copied() {
        vm.slots[s as usize].target = None;
        vm.slots[s as usize].version = 0;
    }
}

/// Version bound to public slot `name`, 0 if unbound.
#[unsafe(no_mangle)]
pub extern "C" fn cmh_bound(name_len: i32) -> i32 {
    let name = mail_str(0, name_len as usize);
    let vm = vm();
    vm.public
        .get(&name)
        .map(|s| &vm.slots[*s as usize])
        .filter(|s| s.target.is_some())
        .map_or(0, |s| s.version as i32)
}

/// After a trap: a report for wherever execution was. Mailbox: kind ++ message.
#[unsafe(no_mangle)]
pub extern "C" fn cmh_where(kind_len: i32, msg_len: i32) -> i32 {
    let kind = mail_str(0, kind_len as usize);
    let msg = mail_str(kind_len as usize, msg_len as usize);
    let vm = vm();
    let cur = unsafe { CUR };
    let json = if cur.is_null() {
        format!("{{\"kind\":{},\"message\":{}}}", js(&kind), js(&msg))
    } else {
        vm.report(unsafe { &*cur }, &kind, &msg)
    };
    set_out(json.as_bytes());
    ST_OK
}

/// Bytes of data space in use and number of compiled words (diagnostics).
#[unsafe(no_mangle)]
pub extern "C" fn cmh_stats() -> i64 {
    let vm = vm();
    ((vm.here as i64) << 32) | vm.words.len() as i64
}
