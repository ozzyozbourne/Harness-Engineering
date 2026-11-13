//! Roc userspace: building the program for a call, the requests-as-data loop,
//! the scratch validation of a proposal, and structured error reports.
//!
//! Every evaluation runs in a forked child (`rocrt.isolate`). A program is
//! the called definition plus the live definitions it references
//! (transitively), so a call compiles a few definitions however many are
//! stored, and a newly installed version of a callee is picked up by every
//! caller on its next call (late binding by name).

const std = @import("std");
const u = @import("util.zig");
const rocrt = @import("rocrt.zig");
const tools = @import("tools.zig");
const st = @import("state.zig");
const Value = u.Value;

pub const MAX_ROUNDS = 24;
pub const READ_LIMIT = 256 * 1024;
pub const BYTES_LIMIT = 128 * 1024;
pub const BASH_TIMEOUT_MS = 30_000;
pub const BASH_CAP = 20 * 1024;

pub const Call = union(enum) {
    ok: []const u8,
    err: []const u8,
};

/// A live definition included in a program.
pub const Dep = struct { name: []const u8, version: u32, source: []const u8 };

pub const Roc = struct {
    engine: rocrt.Engine,
    work_dir: []const u8,
    protected: []const []const u8,
    timeout_ms: u32,
    /// Successful evaluations by (program, expression). Roc code is pure,
    /// so the same program and expression always give the same value.
    memo: std.StringHashMapUnmanaged([]const u8) = .empty,

    // ------------------------------------------------------- programs

    /// The live definitions `source` refers to, transitively, excluding
    /// `self_name` (the definition being called or proposed).
    pub fn deps(state: *const st.State, self_name: []const u8, source: []const u8, shadowed: []const []const u8) []Dep {
        return depsWith(state, self_name, source, shadowed, null);
    }

    /// `deps`, with `override` standing in for the live version of its name.
    pub fn depsWith(state: *const st.State, self_name: []const u8, source: []const u8, shadowed: []const []const u8, override: ?Dep) []Dep {
        var out: std.ArrayList(Dep) = .empty;
        var seen = std.StringHashMap(void).init(u.A);
        seen.put(self_name, {}) catch @panic("oom");
        for (shadowed) |s| seen.put(s, {}) catch @panic("oom");
        var queue: std.ArrayList([]const u8) = .empty;
        queue.append(u.A, source) catch @panic("oom");
        while (queue.pop()) |src| {
            for (identifiers(src)) |ident| {
                const name = ownerOf(state, ident) orelse continue;
                if (seen.contains(name)) continue;
                seen.put(name, {}) catch @panic("oom");
                if (override) |o| if (u.eql(o.name, name)) {
                    out.append(u.A, o) catch @panic("oom");
                    queue.append(u.A, o.source) catch @panic("oom");
                    continue;
                };
                const v = state.live(name) orelse continue;
                if (u.eql(v.kind, "note")) continue;
                out.append(u.A, .{ .name = name, .version = v.version, .source = v.source }) catch @panic("oom");
                queue.append(u.A, v.source) catch @panic("oom");
            }
        }
        return out.items;
    }

    pub fn program(dep_list: []const Dep, source: []const u8) []const u8 {
        var b: std.ArrayList(u8) = .empty;
        for (dep_list) |d| {
            b.appendSlice(u.A, d.source) catch @panic("oom");
            b.appendSlice(u.A, "\n\n") catch @panic("oom");
        }
        b.appendSlice(u.A, source) catch @panic("oom");
        return b.items;
    }

    /// Evaluates `expr` (a Str expression) over `prog` in a killable child.
    pub fn evalIsolated(self: *Roc, prog: []const u8, expr: []const u8) rocrt.Outcome {
        const key = u.fmt("{s}\x00{s}", .{ prog, expr });
        if (self.memo.get(key)) |v| return .{ .value = v };
        const child = rocrt.isolate(tools.io, u.A, self.timeout_ms, EvalCtx{ .roc = self, .prog = prog, .expr = expr }, evalJob) catch |e|
            return .{ .failed = .{ .kind = .internal, .message = u.fmt("could not start a child: {s}", .{@errorName(e)}) } };
        const out = decodeChild(child, self.timeout_ms);
        if (out == .value) self.memo.put(u.A, key, out.value) catch {};
        return out;
    }

    // ------------------------------------------------------------ calls

    /// Calls a live tool or hook by name. Tools run the request loop with
    /// the host performing each request under its rules.
    pub fn call(self: *Roc, state: *const st.State, name: []const u8, v: *const st.DefVersion, input: Value) Call {
        const dep_list = deps(state, name, v.source, &.{});
        const prog = program(dep_list, v.source);
        const where = Where{ .name = name, .version = v.version, .kind = v.kind, .source = v.source, .deps = dep_list };
        if (u.eql(v.kind, "tool")) {
            var live = LiveEval{ .roc = self };
            var performer = Performer{ .mode = .live, .work_dir = self.work_dir, .protected = self.protected };
            const r = driveTool(&live, &performer, prog, name, input);
            return switch (r) {
                .done => |text| .{ .ok = text },
                .failed => |f| .{ .err = report(where, f, input, r.requestsSoFar()) },
            };
        }
        const expr = hookExpr(name, input) orelse return .{ .err = u.fmt("contract: `{s}` is not a known hook", .{name}) };
        const out = self.evalIsolated(prog, expr);
        return switch (out) {
            .value => |json| hookOutput(name, json) catch |e| .{ .err = report(where, .{ .contract = u.fmt("{s}; got {s}", .{ @errorName(e), u.clip(json, 200) }) }, input, &.{}) },
            .failed => |f| .{ .err = report(where, .{ .roc = f }, input, &.{}) },
        };
    }

    // ---------------------------------------------------------- the gate

    pub const Validation = struct {
        ok: bool,
        names: []const []const u8 = &.{},
        report: []const u8 = "",
        passed: usize = 0,
        ms: i64 = 0,
    };

    /// Compiles `source` with its dependencies and runs `tests`, all in one
    /// child (a fresh deadline per phase). The live registry is untouched.
    pub fn validate(self: *Roc, state: *const st.State, name: []const u8, kind: []const u8, version: u32, source: []const u8, tests: []const Value) Validation {
        const t0 = u.nowMs();
        const dep_list = deps(state, name, source, &.{});
        // Live code that calls `name` must still compile against the candidate.
        var dependents: std.ArrayList(Dependent) = .empty;
        const cand = Dep{ .name = name, .version = version, .source = source };
        for (state.defs.keys()) |other| {
            if (u.eql(other, name)) continue;
            const ov = state.live(other) orelse continue;
            if (u.eql(ov.kind, "note")) continue;
            const od = depsWith(state, other, ov.source, &.{}, cand);
            var uses = false;
            for (od) |d| uses = uses or u.eql(d.name, name);
            if (!uses) continue;
            dependents.append(u.A, .{ .name = other, .version = ov.version, .program = program(od, ov.source) }) catch @panic("oom");
        }
        const ctx = GateCtx{ .roc = self, .name = name, .kind = kind, .source = source, .deps = dep_list, .tests = tests, .dependents = dependents.items };
        const child = rocrt.isolate(tools.io, u.A, self.timeout_ms, ctx, gateJob) catch |e|
            return .{ .ok = false, .report = u.fmt("could not start a child: {s}", .{@errorName(e)}) };
        const where = Where{ .name = name, .version = version, .kind = kind, .source = source, .deps = dep_list };
        var res = Validation{ .ok = false };
        const text = switch (child) {
            .done => |t| t,
            .timeout => |t| t.partial,
            .died => |d| d.partial,
        };
        var it = std.mem.splitScalar(u8, text, '\n');
        var last_test: ?usize = null;
        var names: std.ArrayList([]const u8) = .empty;
        while (it.next()) |raw| {
            const line = std.mem.trim(u8, raw, &.{ rocrt.phase_mark, ' ', '\r' });
            if (line.len == 0) continue;
            const ev = u.parseJson(line) catch continue;
            const what = u.str(ev, "event");
            if (u.eql(what, "names")) {
                for (u.arr(ev, "names")) |n| if (n == .string) names.append(u.A, n.string) catch {};
            } else if (u.eql(what, "test_start")) {
                last_test = @intCast(u.int(ev, "test") orelse 0);
            } else if (u.eql(what, "test_ok")) {
                res.passed += 1;
            } else if (u.eql(what, "fail")) {
                res.report = gateFailure(where, ev, tests);
                res.names = names.items;
                res.ms = u.nowMs() - t0;
                return res;
            }
        }
        res.names = names.items;
        res.ms = u.nowMs() - t0;
        switch (child) {
            .done => {
                res.ok = res.passed == tests.len or (u.eql(kind, "note"));
                if (!res.ok) res.report = "internal: the gate child finished without running every test";
            },
            .timeout => |t| res.report = if (last_test) |i|
                report(where, .{ .roc = .{ .kind = .timeout, .message = u.fmt("test {d} did not finish within {d} ms; the child was killed", .{ i + 1, t.ms }) } }, testInput(tests, i), &.{})
            else
                report(where, .{ .roc = .{ .kind = .timeout, .message = u.fmt("compiling the source did not finish within {d} ms; the child was killed", .{t.ms}) } }, .null, &.{}),
            .died => |d| res.report = report(where, .{ .roc = .{ .kind = .died, .message = u.fmt("the child {s}{s}", .{ d.status, if (last_test) |i| u.fmt(" while running test {d}", .{i + 1}) else "" }) } }, if (last_test) |i| testInput(tests, i) else .null, &.{}),
        }
        return res;
    }

    // -------------------------------------------------------- scratchpad

    /// `roc_eval`: definitions plus an expression in a throwaway child.
    pub fn scratch(self: *Roc, state: *const st.State, source: []const u8, expr: []const u8) tools.Result {
        if (std.mem.trim(u8, expr, " \n\t").len == 0) return .{ .err = "`expr` is required (a Roc expression)" };
        const shadow = topLevelNames(source);
        const dep_list = deps(state, "", u.fmt("{s}\n{s}", .{ source, expr }), shadow);
        const ctx = ScratchCtx{ .roc = self, .source = source, .expr = expr, .deps = dep_list };
        const child = rocrt.isolate(tools.io, u.A, self.timeout_ms, ctx, scratchJob) catch |e|
            return .{ .err = u.fmt("could not start a child: {s}", .{@errorName(e)}) };
        const where = Where{ .name = "roc_eval", .version = 0, .kind = "scratch", .source = source, .deps = dep_list };
        switch (child) {
            .done => |t| {
                const ev = u.parseJson(std.mem.trim(u8, t, &.{ rocrt.phase_mark, ' ', '\n' })) catch return .{ .err = "internal: bad scratch report" };
                if (u.eql(u.str(ev, "event"), "value")) return .{ .ok = u.fmt("value: {s}", .{u.truncate(u.str(ev, "value"), 20 * 1024)}) };
                return .{ .err = gateFailure(where, ev, &.{}) };
            },
            .timeout => |t| return .{ .err = report(where, .{ .roc = .{ .kind = .timeout, .message = u.fmt("no result within {d} ms; the child was killed", .{t.ms}) } }, .null, &.{}) },
            .died => |d| return .{ .err = report(where, .{ .roc = .{ .kind = .died, .message = u.fmt("the child {s}", .{d.status}) } }, .null, &.{}) },
        }
    }
};

// ------------------------------------------------------------ identifiers

/// Identifiers in Roc source, skipping comments and string contents (but
/// not interpolations, which are code).
pub fn identifiers(src: []const u8) [][]const u8 {
    var out: std.ArrayList([]const u8) = .empty;
    var i: usize = 0;
    var in_str = false;
    var interp_depth: usize = 0;
    while (i < src.len) {
        const c = src[i];
        if (in_str and interp_depth == 0) {
            if (c == '\\') {
                i += 2;
                continue;
            }
            if (c == '"') in_str = false;
            if (c == '$' and i + 1 < src.len and src[i + 1] == '{') {
                interp_depth = 1;
                i += 2;
                continue;
            }
            i += 1;
            continue;
        }
        if (c == '#') {
            while (i < src.len and src[i] != '\n') i += 1;
            continue;
        }
        if (c == '"') {
            in_str = true;
            i += 1;
            continue;
        }
        if (interp_depth > 0) {
            if (c == '{') interp_depth += 1;
            if (c == '}') {
                interp_depth -= 1;
                i += 1;
                continue;
            }
        }
        if (std.ascii.isAlphabetic(c) or c == '_') {
            const s = i;
            while (i < src.len and (std.ascii.isAlphanumeric(src[i]) or src[i] == '_')) i += 1;
            // `x.field` and `x.method()` are not references to definitions.
            if (!(s > 0 and src[s - 1] == '.')) out.append(u.A, src[s..i]) catch @panic("oom");
            continue;
        }
        i += 1;
    }
    return out.items;
}

/// The definition that owns identifier `ident`: its name, a `name_helper`
/// or a `NamePascal...` type. Longest matching name wins.
pub fn ownerOf(state: *const st.State, ident: []const u8) ?[]const u8 {
    var best: ?[]const u8 = null;
    for (state.defs.keys()) |name| {
        if (owns(name, ident) and (best == null or name.len > best.?.len)) best = name;
    }
    return best;
}

/// Whether top-level name `ident` belongs to definition `name`: the name
/// itself, `name_...`, or a type `NamePascal` / `NamePascalMore`.
pub fn owns(name: []const u8, ident: []const u8) bool {
    if (u.eql(ident, name)) return true;
    if (std.mem.startsWith(u8, ident, name) and ident.len > name.len and ident[name.len] == '_') return true;
    const p = pascal(name);
    return std.mem.startsWith(u8, ident, p) and (ident.len == p.len or std.ascii.isUpper(ident[p.len]));
}

pub fn pascal(name: []const u8) []const u8 {
    var out: std.ArrayList(u8) = .empty;
    var up = true;
    for (name) |c| {
        if (c == '_') {
            up = true;
            continue;
        }
        out.append(u.A, if (up) std.ascii.toUpper(c) else c) catch @panic("oom");
        up = false;
    }
    return out.items;
}

/// Names defined at column 0 (`name =`, `name :`, `Type :`, `Type :=`).
pub fn topLevelNames(src: []const u8) []const []const u8 {
    var out: std.ArrayList([]const u8) = .empty;
    var it = std.mem.splitScalar(u8, src, '\n');
    while (it.next()) |line| {
        if (line.len == 0 or line[0] == ' ' or line[0] == '\t') continue;
        if (definedName(line)) |n| out.append(u.A, n) catch @panic("oom");
    }
    return out.items;
}

/// `name` if `stmt` (after leading comments) starts with `name =`/`name :`.
pub fn definedName(stmt: []const u8) ?[]const u8 {
    var s = stmt;
    while (true) {
        s = std.mem.trimStart(u8, s, " \t\r\n");
        if (s.len > 0 and s[0] == '#') {
            s = s[(std.mem.indexOfScalar(u8, s, '\n') orelse s.len)..];
            continue;
        }
        break;
    }
    if (s.len == 0 or !(std.ascii.isAlphabetic(s[0]) or s[0] == '_')) return null;
    var i: usize = 0;
    while (i < s.len and (std.ascii.isAlphanumeric(s[i]) or s[i] == '_' or s[i] == '!')) i += 1;
    const name = s[0..i];
    const rest = std.mem.trimStart(u8, s[i..], " \t");
    if (rest.len == 0) return null;
    if (rest[0] == ':') return name;
    if (rest[0] == '=' and !(rest.len > 1 and rest[1] == '=')) return name;
    return null;
}

// -------------------------------------------------------- Roc literals

/// A Roc string literal for `s`.
pub fn rocStr(s: []const u8) []const u8 {
    const text = u.validUtf8(s);
    var out: std.ArrayList(u8) = .empty;
    out.append(u.A, '"') catch @panic("oom");
    for (text) |c| {
        const piece: []const u8 = switch (c) {
            '\\' => "\\\\",
            '"' => "\\\"",
            '$' => "\\$",
            '\n' => "\\n",
            '\r' => "\\r",
            '\t' => "\\t",
            else => if (c < 0x20 or c == 0x7f) u.fmt("\\u({x})", .{c}) else &[_]u8{c},
        };
        out.appendSlice(u.A, if (piece.len == 1 and piece[0] == c) &[_]u8{c} else piece) catch @panic("oom");
    }
    out.append(u.A, '"') catch @panic("oom");
    return out.items;
}

fn rocStrList(items: []const []const u8) []const u8 {
    var parts: std.ArrayList([]const u8) = .empty;
    for (items) |s| parts.append(u.A, rocStr(s)) catch @panic("oom");
    return u.fmt("[{s}]", .{std.mem.join(u.A, ", ", parts.items) catch @panic("oom")});
}

fn stringsOf(vs: []const Value) []const []const u8 {
    var out: std.ArrayList([]const u8) = .empty;
    for (vs) |v| out.append(u.A, switch (v) {
        .string => |s| s,
        else => u.stringify(v),
    }) catch @panic("oom");
    return out.items;
}

pub fn toolExpr(name: []const u8, input: Value, results: []const []const u8) []const u8 {
    return u.fmt("Json.to_str({s}({s}, {s}))", .{ name, rocStr(u.stringify(input)), rocStrList(results) });
}

pub fn hookExpr(name: []const u8, input: Value) ?[]const u8 {
    if (u.eql(name, "compose_system")) {
        return u.fmt("Json.to_str(compose_system({s}, {s}))", .{ rocStrList(stringsOf(u.arr(input, "notes"))), rocStrList(stringsOf(u.arr(input, "lessons"))) });
    }
    if (u.eql(name, "select_lessons")) {
        var parts: std.ArrayList([]const u8) = .empty;
        for (u.arr(input, "lessons")) |l| parts.append(u.A, u.fmt("{{ id: {d}, text: {s}, score: {d} }}", .{ u.int(l, "id") orelse 0, rocStr(u.str(l, "text")), u.int(l, "score") orelse 0 })) catch @panic("oom");
        return u.fmt("Json.to_str(select_lessons({s}, [{s}]))", .{ rocStr(u.str(input, "task")), std.mem.join(u.A, ", ", parts.items) catch @panic("oom") });
    }
    return null;
}

/// The hook's value as the kernel uses it: the prompt text for
/// compose_system, the JSON id array (`[3,1]`) for select_lessons.
fn hookOutput(name: []const u8, json_text: []const u8) !Call {
    const v = u.parseJson(json_text) catch return error.HookReturnedBadJson;
    if (u.eql(name, "compose_system")) {
        if (v != .string) return error.ComposeSystemMustReturnStr;
        return .{ .ok = v.string };
    }
    if (v != .array) return error.SelectLessonsMustReturnAListOfIds;
    var ids: std.ArrayList(i64) = .empty;
    for (v.array.items) |x| switch (x) {
        .integer => |i| try ids.append(u.A, i),
        .float => |f| try ids.append(u.A, @intFromFloat(f)),
        else => return error.SelectLessonsMustReturnAListOfIds,
    };
    return .{ .ok = u.stringify(u.json(ids.items)) };
}

// ------------------------------------------------------------- requests

pub const Request = union(enum) {
    done: []const u8,
    fail: []const u8,
    read_file: []const u8,
    read_bytes: []const u8,
    list_dir: []const u8,
    write_file: struct { path: []const u8, content: []const u8 },
    bash: []const u8,

    pub fn show(self: Request) []const u8 {
        return switch (self) {
            .done => |s| u.fmt("Done({s})", .{rocStr(u.clip(s, 80))}),
            .fail => |s| u.fmt("Fail({s})", .{rocStr(u.clip(s, 80))}),
            .read_file => |p| u.fmt("ReadFile({s})", .{rocStr(p)}),
            .read_bytes => |p| u.fmt("ReadBytes({s})", .{rocStr(p)}),
            .list_dir => |p| u.fmt("ListDir({s})", .{rocStr(p)}),
            .write_file => |w| u.fmt("WriteFile({s}, <{d} bytes>)", .{ rocStr(w.path), w.content.len }),
            .bash => |c| u.fmt("Bash({s})", .{rocStr(u.clip(c, 80))}),
        };
    }
};

/// Decodes what `Json.to_str(tool(...))` produced.
pub fn parseRequest(json_text: []const u8) ?Request {
    const v = u.parseJson(json_text) catch return null;
    switch (v) {
        .string => |s| return .{ .done = s },
        .object => |o| {
            if (o.count() != 1) return null;
            var it = o.iterator();
            const e = it.next().?;
            const tag = e.key_ptr.*;
            const arg = e.value_ptr.*;
            if (u.eql(tag, "WriteFile")) {
                if (arg != .array or arg.array.items.len != 2) return null;
                const a = arg.array.items;
                if (a[0] != .string or a[1] != .string) return null;
                return .{ .write_file = .{ .path = a[0].string, .content = a[1].string } };
            }
            if (arg != .string) return null;
            const s = arg.string;
            if (u.eql(tag, "Done")) return .{ .done = s };
            if (u.eql(tag, "Fail")) return .{ .fail = s };
            if (u.eql(tag, "ReadFile")) return .{ .read_file = s };
            if (u.eql(tag, "ReadBytes")) return .{ .read_bytes = s };
            if (u.eql(tag, "ListDir")) return .{ .list_dir = s };
            if (u.eql(tag, "Bash")) return .{ .bash = s };
            return null;
        },
        else => return null,
    }
}

/// Carries out requests under the host's rules (or scripts them in tests).
pub const Performer = struct {
    mode: enum { live, test_run },
    work_dir: []const u8,
    protected: []const []const u8,
    /// Test mode: scripted results, consumed in order (null: perform reads).
    responses: ?[]const Value = null,
    next_response: usize = 0,

    /// The result string fed back to the Roc function. Refusals and
    /// failures start with "ERROR: ".
    pub fn perform(self: *Performer, req: Request) []const u8 {
        if (self.responses) |rs| {
            if (self.next_response >= rs.len) return u.fmt("ERROR: test script exhausted: no scripted response for request {d} ({s})", .{ self.next_response + 1, req.show() });
            const r = rs[self.next_response];
            self.next_response += 1;
            return switch (r) {
                .string => |s| s,
                else => u.stringify(r),
            };
        }
        switch (req) {
            .done, .fail => unreachable,
            .read_file, .read_bytes, .list_dir => |p| {
                const path = self.inWorkDir(p) catch |e| return self.refusal(p, e);
                if (req == .list_dir) {
                    const listing = tools.listDir(path) catch |e| return u.fmt("ERROR: ListDir {s}: {s}", .{ p, @errorName(e) });
                    return listing;
                }
                const limit: usize = if (req == .read_file) READ_LIMIT else BYTES_LIMIT;
                const data = tools.readFileLimited(path, limit + 1) catch |e| return u.fmt("ERROR: {s} {s}: {s}", .{ if (req == .read_file) "ReadFile" else "ReadBytes", p, @errorName(e) });
                if (data.len > limit) return u.fmt("ERROR: {s} is larger than the {s} limit of {d} bytes", .{ p, if (req == .read_file) "ReadFile" else "ReadBytes", limit });
                if (req == .read_file) {
                    if (!std.unicode.utf8ValidateSlice(data)) return u.fmt("ERROR: {s} is not UTF-8 text; use ReadBytes for binary files", .{p});
                    return data;
                }
                var b: std.ArrayList(u8) = .empty;
                for (data, 0..) |byte, i| {
                    if (i > 0) b.append(u.A, ',') catch @panic("oom");
                    b.appendSlice(u.A, u.fmt("{d}", .{byte})) catch @panic("oom");
                }
                return b.items;
            },
            .write_file => |w| {
                if (self.mode == .test_run) return "ERROR: tests do not perform WriteFile; give the test `responses` to script it";
                const path = self.inWorkDir(w.path) catch |e| return self.refusal(w.path, e);
                if (tools.protectedBy(path, self.protected)) |prot| return u.fmt("ERROR: refused: WriteFile {s}: {s} is inside the protected path {s}", .{ w.path, path, prot });
                tools.writeFileCreatingDirs(path, w.content) catch |e| return u.fmt("ERROR: WriteFile {s}: {s}", .{ w.path, @errorName(e) });
                return u.fmt("ok: wrote {d} bytes to {s}", .{ w.content.len, w.path });
            },
            .bash => |cmd| {
                if (self.mode == .test_run) return "ERROR: tests do not perform Bash; give the test `responses` to script it";
                const r = tools.runBash(self.work_dir, cmd, BASH_TIMEOUT_MS, BASH_CAP) catch |e| return u.fmt("ERROR: Bash: {s}", .{@errorName(e)});
                if (r.timed_out) return u.fmt("{s}\n[timed out after {d}s]", .{ r.output, BASH_TIMEOUT_MS / 1000 });
                if (r.exit_code) |c| return u.fmt("{s}\n[exit code: {d}]", .{ r.output, c });
                return u.fmt("{s}\n[exit code: signal {d}]", .{ r.output, r.signal orelse 0 });
            },
        }
    }

    fn inWorkDir(self: *Performer, p: []const u8) ![]const u8 {
        if (p.len == 0) return error.EmptyPath;
        const path = tools.resolve(self.work_dir, p);
        if (!tools.within(path, self.work_dir)) return error.OutsideWorkDir;
        return path;
    }

    fn refusal(self: *Performer, p: []const u8, e: anyerror) []const u8 {
        return switch (e) {
            error.OutsideWorkDir => u.fmt("ERROR: refused: {s} resolves to {s}, outside the work dir {s}; use a path relative to the work dir", .{ p, tools.resolve(self.work_dir, p), self.work_dir }),
            else => u.fmt("ERROR: refused: empty path", .{}),
        };
    }
};

pub const Failure = union(enum) {
    roc: rocrt.Failure,
    contract: []const u8,
    fail: []const u8,
};

pub const Drive = union(enum) {
    done: []const u8,
    failed: Failure,

    pub fn requestsSoFar(_: Drive) []const []const u8 {
        return last_requests;
    }
};

/// Requests of the last `driveTool` run, for reports.
var last_requests: []const []const u8 = &.{};

/// Evaluator used in the parent: one killable child per round.
const LiveEval = struct {
    roc: *Roc,
    fn eval(self: *LiveEval, prog: []const u8, expr: []const u8) rocrt.Outcome {
        return self.roc.evalIsolated(prog, expr);
    }
};

/// Evaluator used inside a gate child: in-process (the child is the wall).
const ChildEval = struct {
    roc: *Roc,
    pipe: rocrt.Pipe,
    fn eval(self: *ChildEval, prog: []const u8, expr: []const u8) rocrt.Outcome {
        self.pipe.phase(); // each round gets a fresh deadline
        return self.roc.engine.evalProgram(u.A, prog, expr) catch |e| .{ .failed = .{ .kind = .internal, .message = @errorName(e) } };
    }
};

/// The requests-as-data loop: call the tool, perform what it asks, call it
/// again with every result so far, until it returns Done/Fail or a Str.
pub fn driveTool(evaluator: anytype, performer: *Performer, prog: []const u8, name: []const u8, input: Value) Drive {
    var results: std.ArrayList([]const u8) = .empty;
    var shown: std.ArrayList([]const u8) = .empty;
    defer last_requests = shown.items;
    var round: usize = 0;
    while (round < MAX_ROUNDS) : (round += 1) {
        const out = evaluator.eval(prog, toolExpr(name, input, results.items));
        const json_text = switch (out) {
            .value => |v| v,
            .failed => |f| {
                last_requests = shown.items;
                return .{ .failed = .{ .roc = f } };
            },
        };
        const req = parseRequest(json_text) orelse {
            last_requests = shown.items;
            return .{ .failed = .{ .contract = u.fmt("`{s}` must return a Str or one of Done(Str), Fail(Str), ReadFile(Str), ReadBytes(Str), ListDir(Str), WriteFile(Str, Str), Bash(Str); got {s}", .{ name, u.clip(json_text, 300) }) } };
        };
        switch (req) {
            .done => |text| return .{ .done = text },
            .fail => |msg| {
                last_requests = shown.items;
                return .{ .failed = .{ .fail = msg } };
            },
            else => {},
        }
        const result = performer.perform(req);
        shown.append(u.A, u.fmt("{s} -> {s}", .{ req.show(), u.clip(u.firstLine(result, 100), 100) })) catch @panic("oom");
        if (performer.mode == .live) u.trace("[request] {s} {s} -> {s}", .{ name, req.show(), u.firstLine(result, 80) });
        results.append(u.A, result) catch @panic("oom");
    }
    last_requests = shown.items;
    return .{ .failed = .{ .contract = u.fmt("`{s}` made more than {d} requests in one call; return Done(...) sooner", .{ name, MAX_ROUNDS }) } };
}

// --------------------------------------------------------- child jobs

const EvalCtx = struct { roc: *Roc, prog: []const u8, expr: []const u8 };

fn evalJob(c: EvalCtx, pipe: rocrt.Pipe) void {
    const out = c.roc.engine.evalProgram(u.A, c.prog, c.expr) catch |e| rocrt.Outcome{ .failed = .{ .kind = .internal, .message = @errorName(e) } };
    pipe.write(u.stringify(switch (out) {
        .value => |v| u.json(.{ .{ "ok", true }, .{ "value", v } }),
        .failed => |f| u.json(.{ .{ "ok", false }, .{ "kind", @tagName(f.kind) }, .{ "message", f.message } }),
    }));
}

fn decodeChild(child: rocrt.Child, timeout_ms: u32) rocrt.Outcome {
    switch (child) {
        .done => |t| {
            const ev = u.parseJson(t) catch return .{ .failed = .{ .kind = .internal, .message = "unreadable child report" } };
            if (u.boolean(ev, "ok") orelse false) return .{ .value = u.str(ev, "value") };
            const kind = std.meta.stringToEnum(rocrt.Failure.Kind, u.str(ev, "kind")) orelse .internal;
            return .{ .failed = .{ .kind = kind, .message = u.str(ev, "message") } };
        },
        .timeout => return .{ .failed = .{ .kind = .timeout, .message = u.fmt("no result within {d} ms; the child was killed (infinite loop or runaway recursion?)", .{timeout_ms}) } },
        .died => |d| return .{ .failed = .{ .kind = .died, .message = u.fmt("the child {s}", .{d.status}) } },
    }
}

const Dependent = struct { name: []const u8, version: u32, program: []const u8 };

const GateCtx = struct {
    roc: *Roc,
    name: []const u8,
    kind: []const u8,
    source: []const u8,
    deps: []const Dep,
    tests: []const Value,
    dependents: []const Dependent,
};

fn emit(pipe: rocrt.Pipe, v: Value) void {
    pipe.write(u.stringify(v));
    pipe.write("\n");
}

/// Splits `source` into statements and checks every one is a definition.
/// Emits `fail` and returns null on a problem.
fn checkStatements(roc: *Roc, pipe: rocrt.Pipe, source: []const u8) ?[]const []const u8 {
    const stmts = roc.engine.statements(u.A, source) catch |e| {
        emit(pipe, u.json(.{ .{ "event", "fail" }, .{ "stage", "parse" }, .{ "kind", "parse" }, .{ "message", @errorName(e) } }));
        return null;
    };
    var names: std.ArrayList([]const u8) = .empty;
    var offset: usize = 0;
    for (stmts) |stmt| {
        const at = std.mem.indexOf(u8, source[offset..], std.mem.trim(u8, stmt, " \n\t\r")) orelse 0;
        const line = 1 + std.mem.count(u8, source[0 .. offset + at], "\n");
        offset += at;
        const n = definedName(stmt) orelse {
            emit(pipe, u.json(.{ .{ "event", "fail" }, .{ "stage", "shape" }, .{ "kind", "contract" }, .{ "line", line }, .{ "message", u.fmt("line {d}: top-level code must be definitions (`name = ...`, `name : Type`, `Type : ...`); this statement is not:\n    {s}", .{ line, u.firstLine(stmt, 120) }) } }));
            return null;
        };
        if (std.mem.startsWith(u8, std.mem.trimStart(u8, stmt, " \n"), "import")) {
            emit(pipe, u.json(.{ .{ "event", "fail" }, .{ "stage", "shape" }, .{ "kind", "contract" }, .{ "line", line }, .{ "message", "imports are not available; the builtins (Str, List, Dict, Set, Json, Num types) are always in scope" } }));
            return null;
        }
        names.append(u.A, n) catch @panic("oom");
    }
    return names.items;
}

/// Compiles the program once; on a parse failure, steps each statement
/// through the session to find and report the statement that does not parse.
fn checkCompiles(roc: *Roc, pipe: rocrt.Pipe, prog: []const u8, source: []const u8, stmts_source: []const u8) bool {
    _ = source;
    const out = roc.engine.evalProgram(u.A, prog, "Str.inspect({})") catch |e| {
        emit(pipe, u.json(.{ .{ "event", "fail" }, .{ "stage", "compile" }, .{ "kind", "internal" }, .{ "message", @errorName(e) } }));
        return false;
    };
    switch (out) {
        .value => return true,
        .failed => |f| {
            if (f.kind == .parse) {
                const stmts = roc.engine.statements(u.A, stmts_source) catch &.{};
                for (stmts) |stmt| {
                    switch (roc.engine.step(u.A, stmt) catch continue) {
                        .failed => |sf| if (sf.kind == .parse) {
                            const at = std.mem.indexOf(u8, stmts_source, std.mem.trim(u8, stmt, " \n\t\r")) orelse 0;
                            const line = 1 + std.mem.count(u8, stmts_source[0..at], "\n");
                            emit(pipe, u.json(.{ .{ "event", "fail" }, .{ "stage", "compile" }, .{ "kind", "parse" }, .{ "line", line }, .{ "message", sf.message } }));
                            return false;
                        },
                        else => {},
                    }
                }
            }
            emit(pipe, u.json(.{ .{ "event", "fail" }, .{ "stage", "compile" }, .{ "kind", @tagName(f.kind) }, .{ "message", f.message } }));
            return false;
        },
    }
}

fn gateJob(c: GateCtx, pipe: rocrt.Pipe) void {
    pipe.phase();
    const names = checkStatements(c.roc, pipe, c.source) orelse return;
    for (names) |n| if (!owns(c.name, n)) {
        emit(pipe, u.json(.{ .{ "event", "fail" }, .{ "stage", "shape" }, .{ "kind", "contract" }, .{ "message", u.fmt("top-level name `{s}` is not allowed: every top-level name in `{s}` must be `{s}`, start with `{s}_`, or (types) start with `{s}`, so definitions cannot collide", .{ n, c.name, c.name, c.name, pascal(c.name) }) } }));
        return;
    };
    emit(pipe, u.json(.{ .{ "event", "names" }, .{ "names", names } }));
    const prog = Roc.program(c.deps, c.source);
    if (!checkCompiles(c.roc, pipe, prog, c.source, c.source)) return;
    var has_entry = false;
    for (names) |n| has_entry = has_entry or u.eql(n, c.name);
    if (!has_entry) {
        emit(pipe, u.json(.{ .{ "event", "fail" }, .{ "stage", "shape" }, .{ "kind", "contract" }, .{ "message", u.fmt("the source must define `{s}` (the definition's own name); it defines: {s}", .{ c.name, std.mem.join(u.A, ", ", names) catch "" }) } }));
        return;
    }
    for (c.tests, 0..) |t, i| {
        pipe.phase();
        emit(pipe, u.json(.{ .{ "event", "test_start" }, .{ "test", i } }));
        const input = u.get(t, "input") orelse u.json(.{});
        const expect = u.str(t, "expect");
        var output: []const u8 = undefined;
        var requests: []const []const u8 = &.{};
        if (u.eql(c.kind, "tool")) {
            var ev = ChildEval{ .roc = c.roc, .pipe = pipe };
            const responses: ?[]const Value = if (u.get(t, "responses")) |r| (if (r == .array) r.array.items else null) else null;
            var performer = Performer{ .mode = .test_run, .work_dir = c.roc.work_dir, .protected = c.roc.protected, .responses = responses };
            const r = driveTool(&ev, &performer, prog, c.name, input);
            requests = r.requestsSoFar();
            switch (r) {
                .done => |text| output = text,
                .failed => |f| {
                    emitTestFailure(pipe, i, f, requests);
                    return;
                },
            }
            // Request-sequence checks.
            for (u.arr(t, "expect_requests"), 0..) |want, k| {
                const w = if (want == .string) want.string else u.stringify(want);
                if (k >= requests.len or std.mem.indexOf(u8, requests[k], w) == null) {
                    emit(pipe, u.json(.{ .{ "event", "fail" }, .{ "stage", "test" }, .{ "test", i }, .{ "kind", "mismatch" }, .{ "requests", requests }, .{ "message", u.fmt("request {d} should contain {s}; the requests were:\n{s}", .{ k + 1, w, if (requests.len == 0) "(none)" else std.mem.join(u.A, "\n", requests) catch "" }) } }));
                    return;
                }
            }
        } else {
            const expr = hookExpr(c.name, input) orelse {
                emit(pipe, u.json(.{ .{ "event", "fail" }, .{ "stage", "test" }, .{ "test", i }, .{ "kind", "contract" }, .{ "message", "unknown hook" } }));
                return;
            };
            const out = c.roc.engine.evalProgram(u.A, prog, expr) catch |e| rocrt.Outcome{ .failed = .{ .kind = .internal, .message = @errorName(e) } };
            switch (out) {
                .value => |json_text| {
                    const h = hookOutput(c.name, json_text) catch |e| {
                        emitTestFailure(pipe, i, .{ .contract = u.fmt("{s}; got {s}", .{ @errorName(e), u.clip(json_text, 200) }) }, &.{});
                        return;
                    };
                    output = h.ok;
                },
                .failed => |f| {
                    emitTestFailure(pipe, i, .{ .roc = f }, &.{});
                    return;
                },
            }
        }
        if (std.mem.indexOf(u8, output, expect) == null) {
            emit(pipe, u.json(.{ .{ "event", "fail" }, .{ "stage", "test" }, .{ "test", i }, .{ "kind", "mismatch" }, .{ "requests", requests }, .{ "message", u.fmt("expected the output to contain {s}, got {s}", .{ rocStr(expect), rocStr(u.truncate(output, 500)) }) } }));
            return;
        }
        emit(pipe, u.json(.{ .{ "event", "test_ok" }, .{ "test", i } }));
    }
    for (c.dependents) |d| {
        pipe.phase();
        const out = c.roc.engine.evalProgram(u.A, d.program, "Str.inspect({})") catch continue;
        if (out == .failed) {
            emit(pipe, u.json(.{ .{ "event", "fail" }, .{ "stage", "dependents" }, .{ "kind", "contract" }, .{ "message", u.fmt("`{s}` v{d} calls `{s}` and no longer compiles against this version, so existing callers would break:\n{s}", .{ d.name, d.version, c.name, u.truncate(out.failed.message, 3000) }) } }));
            return;
        }
    }
}

fn emitTestFailure(pipe: rocrt.Pipe, i: usize, f: Failure, requests: []const []const u8) void {
    const kind: []const u8, const msg: []const u8 = switch (f) {
        .roc => |r| .{ @tagName(r.kind), r.message },
        .contract => |m| .{ "contract", m },
        .fail => |m| .{ "fail", u.fmt("the tool returned Fail({s})", .{rocStr(m)}) },
    };
    emit(pipe, u.json(.{ .{ "event", "fail" }, .{ "stage", "test" }, .{ "test", i }, .{ "kind", kind }, .{ "requests", requests }, .{ "message", msg } }));
}

const ScratchCtx = struct { roc: *Roc, source: []const u8, expr: []const u8, deps: []const Dep };

fn scratchJob(c: ScratchCtx, pipe: rocrt.Pipe) void {
    const prog = Roc.program(c.deps, c.source);
    if (std.mem.trim(u8, c.source, " \n\t").len > 0) {
        if (checkStatements(c.roc, pipe, c.source) == null) return;
    }
    const out = c.roc.engine.evalProgram(u.A, prog, u.fmt("Str.inspect((\n{s}\n))", .{c.expr})) catch |e| rocrt.Outcome{ .failed = .{ .kind = .internal, .message = @errorName(e) } };
    switch (out) {
        .value => |v| emit(pipe, u.json(.{ .{ "event", "value" }, .{ "value", v } })),
        .failed => |f| {
            if (f.kind == .parse) {
                // Find the statement (or the expression) that does not parse.
                if (!checkCompiles(c.roc, pipe, prog, c.source, u.fmt("{s}\n{s}", .{ c.source, c.expr }))) return;
            }
            emit(pipe, u.json(.{ .{ "event", "fail" }, .{ "stage", "eval" }, .{ "kind", @tagName(f.kind) }, .{ "message", f.message } }));
        },
    }
}

// ------------------------------------------------------------- reports

pub const Where = struct {
    name: []const u8,
    version: u32,
    kind: []const u8,
    source: []const u8,
    deps: []const Dep,
};

fn testInput(tests: []const Value, i: usize) Value {
    if (i >= tests.len) return .null;
    return u.get(tests[i], "input") orelse .null;
}

fn gateFailure(where: Where, ev: Value, tests: []const Value) []const u8 {
    const kind_s = u.str(ev, "kind");
    const msg = u.str(ev, "message");
    const stage = u.str(ev, "stage");
    var requests: std.ArrayList([]const u8) = .empty;
    for (u.arr(ev, "requests")) |r| if (r == .string) requests.append(u.A, r.string) catch {};
    const test_i: ?usize = if (u.int(ev, "test")) |t| @intCast(t) else null;
    const input = if (test_i) |i| testInput(tests, i) else Value.null;
    const failure: Failure = if (u.eql(kind_s, "contract") or u.eql(kind_s, "mismatch"))
        .{ .contract = msg }
    else if (u.eql(kind_s, "fail"))
        .{ .fail = msg }
    else
        .{ .roc = .{ .kind = std.meta.stringToEnum(rocrt.Failure.Kind, kind_s) orelse .internal, .message = msg } };
    var r = report(where, failure, input, requests.items);
    if (test_i) |i| r = u.fmt("test {d} failed{s}: {s}", .{ i + 1, if (u.eql(kind_s, "mismatch")) " (wrong output)" else "", r });
    if (u.int(ev, "line")) |line| {
        if (!std.mem.containsAtLeast(u8, r, 1, "line ")) r = u.fmt("{s}\n(line {d} of the source)", .{ r, line });
    }
    _ = stage;
    return r;
}

/// The structured report for the model:
///   roc error [kind] in NAME vN (kind), line L: title
///   <compiler report>
///   input / requests so far / hint
pub fn report(where: Where, f: Failure, input: Value, requests: []const []const u8) []const u8 {
    var kind: []const u8 = undefined;
    var body: []const u8 = undefined;
    switch (f) {
        .roc => |r| {
            kind = switch (r.kind) {
                .parse => "parse-error",
                .compile => "type-error",
                .crash => "crash",
                .timeout => "timeout",
                .died => "child-died",
                .internal => "internal",
            };
            body = r.message;
        },
        .contract => |m| {
            kind = "contract";
            body = m;
        },
        .fail => |m| {
            kind = "fail";
            body = m;
        },
    }
    if (std.mem.indexOf(u8, body, "wrong output") != null or std.mem.startsWith(u8, body, "expected the output") or std.mem.startsWith(u8, body, "request ")) kind = "wrong-output";
    const title = u.firstLine(body, 120);
    const loc = locate(where, body);
    var b: std.ArrayList(u8) = .empty;
    const w = struct {
        fn add(list: *std.ArrayList(u8), s: []const u8) void {
            list.appendSlice(u.A, s) catch @panic("oom");
        }
    };
    const vtext = if (where.version > 0) u.fmt(" v{d}", .{where.version}) else "";
    w.add(&b, u.fmt("roc error [{s}] in {s}{s} ({s}){s}: {s}\n", .{ kind, where.name, vtext, where.kind, loc, std.mem.trim(u8, title, "*") }));
    if (body.len > title.len) w.add(&b, u.fmt("{s}\n", .{u.truncate(std.mem.trim(u8, body, " \n"), 4000)}));
    if (input != .null) w.add(&b, u.fmt("input: {s}\n", .{u.clip(u.stringify(input), 400)}));
    if (requests.len > 0) {
        w.add(&b, "requests so far:\n");
        for (requests) |r| w.add(&b, u.fmt("  {s}\n", .{r}));
    }
    w.add(&b, u.fmt("hint: {s}", .{hint(kind, body, where)}));
    return b.items;
}

/// ", line L" (or ", in DEP vN line L") from the compiler report's code
/// snippet, matched against the definition's own source and its deps.
fn locate(where: Where, body: []const u8) []const u8 {
    const start = std.mem.indexOf(u8, body, "```roc\n") orelse return "";
    const rest = body[start + 7 ..];
    const end = std.mem.indexOf(u8, rest, "```") orelse return "";
    var it = std.mem.splitScalar(u8, rest[0..end], '\n');
    const first = std.mem.trim(u8, it.next() orelse return "", " \t\r");
    if (first.len == 0) return "";
    if (lineOf(where.source, first)) |l| return u.fmt(", line {d}", .{l});
    for (where.deps) |d| if (lineOf(d.source, first)) |l| return u.fmt(", in {s} v{d} line {d}", .{ d.name, d.version, l });
    if (std.mem.startsWith(u8, first, "main =") or std.mem.indexOf(u8, first, "Json.to_str(") != null) return ", in the kernel's call of it";
    return "";
}

fn lineOf(source: []const u8, needle: []const u8) ?usize {
    var it = std.mem.splitScalar(u8, source, '\n');
    var n: usize = 0;
    while (it.next()) |line| {
        n += 1;
        if (u.eql(std.mem.trim(u8, line, " \t\r"), needle)) return n;
    }
    it = std.mem.splitScalar(u8, source, '\n');
    n = 0;
    while (it.next()) |line| {
        n += 1;
        const t = std.mem.trim(u8, line, " \t\r");
        if (t.len > 3 and std.mem.indexOf(u8, needle, t) != null) return n;
    }
    return null;
}

fn hint(kind: []const u8, body: []const u8, where: Where) []const u8 {
    if (u.eql(kind, "parse-error")) return "check brackets, commas and `|args|` lambdas against the Roc reference; try the snippet in roc_eval";
    if (u.eql(kind, "type-error")) {
        if (std.mem.indexOf(u8, body, "does not exist") != null or std.mem.indexOf(u8, body, "Does Not Exist") != null)
            return "that function is not in the builtins; see the Roc reference for the available Str/List functions, or try alternatives in roc_eval";
        if (std.mem.indexOf(u8, body, "kernel's call") != null or std.mem.indexOf(u8, body, "main =") != null)
            return contractHint(where.kind, where.name);
        return "fix the types at the marked spot; annotate the function (e.g. `name : Str, List(Str) -> Str`) to get errors closer to the cause";
    }
    if (u.eql(kind, "crash")) return "a `crash`, a failed `expect`, an overflow or too-deep recursion ran; handle that input (e.g. match on Err) instead of crashing";
    if (u.eql(kind, "timeout")) return "the call never finished: look for unbounded recursion or a loop whose condition never changes";
    if (u.eql(kind, "child-died")) return "the evaluator process died (deep recursion overflowing the stack is the usual cause); use List.walk/for loops instead of deep recursion";
    if (u.eql(kind, "contract")) return contractHint(where.kind, where.name);
    if (u.eql(kind, "wrong-output")) return "the code ran but produced something else; reproduce it with roc_eval and compare";
    if (u.eql(kind, "fail")) return "the tool reported failure itself with Fail(...)";
    return "this looks like a kernel problem; try a simpler definition";
}

fn contractHint(kind: []const u8, name: []const u8) []const u8 {
    if (u.eql(kind, "tool")) return u.fmt("a tool is called as `{s}(input_json, results)`: `{s} : Str, List(Str) -> [Done(Str), ReadFile(Str), ...]` (or `-> Str`)", .{ name, name });
    if (u.eql(name, "compose_system")) return "the hook is called as `compose_system(notes, lessons)` with two List(Str) and must return a Str";
    if (u.eql(name, "select_lessons")) return "the hook is called as `select_lessons(task, lessons)` with a Str and a List({ id : I64, text : Str, score : I64 }) and must return a List(I64)";
    return "see the contract in the propose_def description";
}
