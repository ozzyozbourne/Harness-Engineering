//! Kernel tools that touch the outside world (bash, read_file, write_file),
//! the path rules shared with Roc requests, and the kernel tool schemas.

const std = @import("std");
const u = @import("util.zig");
const journal = @import("journal.zig");
const Value = u.Value;

pub var io: std.Io = undefined;

pub const KERNEL_TOOLS = [_][]const u8{
    "bash", "read_file", "write_file", "list_defs", "show_def", "roc_eval", "propose_def", "rollback_def", "remember", "recall",
};

pub fn isKernelTool(name: []const u8) bool {
    for (KERNEL_TOOLS) |t| if (u.eql(t, name)) return true;
    return false;
}

/// Tool results: `ok` text or an error text for the model.
pub const Result = union(enum) {
    ok: []const u8,
    err: []const u8,
};

// ------------------------------------------------------------------ paths

/// Absolute, lexically normalised path with the existing prefix canonicalised.
pub fn resolve(work_dir: []const u8, p: []const u8) []const u8 {
    const joined = if (std.fs.path.isAbsolute(p)) p else u.fmt("{s}/{s}", .{ work_dir, p });
    var parts: std.ArrayList([]const u8) = .empty;
    var it = std.mem.splitScalar(u8, joined, '/');
    while (it.next()) |c| {
        if (c.len == 0 or u.eql(c, ".")) continue;
        if (u.eql(c, "..")) {
            _ = parts.pop();
            continue;
        }
        parts.append(u.A, c) catch @panic("oom");
    }
    // Canonicalise the longest prefix that exists (symlinks resolved).
    var n = parts.items.len;
    while (true) : (n -= 1) {
        const prefix = u.fmt("/{s}", .{std.mem.join(u.A, "/", parts.items[0..n]) catch @panic("oom")});
        var buf: [4096]u8 = undefined;
        const z = u.A.dupeSentinel(u8, prefix, 0) catch @panic("oom");
        if (std.c.realpath(z, &buf)) |real| {
            const base = std.mem.span(real);
            if (n == parts.items.len) return u.A.dupe(u8, base) catch @panic("oom");
            const rest = std.mem.join(u.A, "/", parts.items[n..]) catch @panic("oom");
            return if (u.eql(base, "/")) u.fmt("/{s}", .{rest}) else u.fmt("{s}/{s}", .{ base, rest });
        }
        if (n == 0) break;
    }
    return u.fmt("/{s}", .{std.mem.join(u.A, "/", parts.items) catch @panic("oom")});
}

/// True if `path` is `dir` or inside it (both resolved).
pub fn within(path: []const u8, dir: []const u8) bool {
    if (!std.mem.startsWith(u8, path, dir)) return false;
    return path.len == dir.len or path[dir.len] == '/' or u.eql(dir, "/");
}

pub fn protectedBy(path: []const u8, protected: []const []const u8) ?[]const u8 {
    for (protected) |p| if (within(path, p)) return p;
    return null;
}

// ------------------------------------------------------------- files

pub fn readFileLimited(path: []const u8, limit: usize) ![]u8 {
    const z = try u.A.dupeSentinel(u8, path, 0);
    if (std.c.opendir(z)) |d| {
        _ = std.c.closedir(d);
        return error.IsDir;
    }
    const data = try journal.readWhole(z);
    if (data.len > limit) return data[0..limit];
    return data;
}

pub fn writeFileCreatingDirs(path: []const u8, content: []const u8) !void {
    if (std.fs.path.dirname(path)) |parent| try journal.mkdirs(parent);
    const z = try u.A.dupeSentinel(u8, path, 0);
    const fd = std.c.open(z, .{ .ACCMODE = .WRONLY, .CREAT = true, .TRUNC = true, .CLOEXEC = true }, @as(std.c.mode_t, 0o644));
    if (fd < 0) return error.OpenFailed;
    defer _ = std.c.close(fd);
    var rest = content;
    while (rest.len > 0) {
        const n = std.c.write(fd, rest.ptr, rest.len);
        if (n <= 0) return error.WriteFailed;
        rest = rest[@intCast(n)..];
    }
}

pub fn readFile(work_dir: []const u8, input: Value) Result {
    const p = u.str(input, "path");
    if (p.len == 0) return .{ .err = "`path` (string) is required" };
    const path = resolve(work_dir, p);
    const data = readFileLimited(path, 50 * 1024 + 1) catch |e| return .{ .err = u.fmt("read {s}: {s}", .{ path, @errorName(e) }) };
    return .{ .ok = u.truncate(u.validUtf8(data), 50 * 1024) };
}

pub fn writeFile(work_dir: []const u8, input: Value, protected: []const []const u8) Result {
    const p = u.str(input, "path");
    if (p.len == 0) return .{ .err = "`path` (string) is required" };
    const content_v = u.get(input, "content") orelse return .{ .err = "`content` (string) is required" };
    if (content_v != .string) return .{ .err = "`content` (string) is required" };
    const path = resolve(work_dir, p);
    if (protectedBy(path, protected)) |prot| return .{ .err = u.fmt("refused: {s} is inside protected path {s}", .{ path, prot }) };
    writeFileCreatingDirs(path, content_v.string) catch |e| return .{ .err = u.fmt("write {s}: {s}", .{ path, @errorName(e) }) };
    return .{ .ok = u.fmt("wrote {d} bytes to {s}", .{ content_v.string.len, path }) };
}

/// Names in a directory, one per line, directories with a trailing `/`.
pub fn listDir(path: []const u8) ![]const u8 {
    var dir = try std.Io.Dir.cwd().openDir(io, path, .{ .iterate = true });
    defer dir.close(io);
    var names: std.ArrayList([]const u8) = .empty;
    var it = dir.iterate();
    while (try it.next(io)) |e| {
        try names.append(u.A, if (e.kind == .directory) u.fmt("{s}/", .{e.name}) else try u.A.dupe(u8, e.name));
    }
    std.mem.sort([]const u8, names.items, {}, struct {
        fn lt(_: void, a: []const u8, b: []const u8) bool {
            return std.mem.lessThan(u8, a, b);
        }
    }.lt);
    return std.mem.join(u.A, "\n", names.items);
}

// -------------------------------------------------------------------- bash

pub const BashOut = struct { output: []const u8, exit_code: ?u8, timed_out: bool, signal: ?u32 };

/// Runs `bash -c cmd` in `work_dir` in its own process group; combined
/// stdout/stderr; the whole group is killed at the deadline.
pub fn runBash(work_dir: []const u8, cmd: []const u8, timeout_ms: i64, cap: usize) !BashOut {
    var fds: [2]std.c.fd_t = undefined;
    if (std.c.pipe(&fds) != 0) return error.PipeFailed;
    const wd = try u.A.dupeSentinel(u8, work_dir, 0);
    const cmdz = try u.A.dupeSentinel(u8, cmd, 0);
    const argv = [_:null]?[*:0]const u8{ "bash", "-c", cmdz };
    const pid = std.c.fork();
    if (pid < 0) return error.ForkFailed;
    if (pid == 0) {
        _ = std.c.setpgid(0, 0);
        _ = std.c.close(fds[0]);
        const devnull = std.c.open("/dev/null", .{ .ACCMODE = .RDONLY }, @as(std.c.mode_t, 0));
        if (devnull >= 0) _ = std.c.dup2(devnull, 0);
        _ = std.c.dup2(fds[1], 1);
        _ = std.c.dup2(fds[1], 2);
        if (fds[1] > 2) _ = std.c.close(fds[1]);
        if (std.c.chdir(wd) != 0) std.c._exit(126);
        _ = std.c.execve("/bin/bash", &argv, @ptrCast(std.c.environ));
        std.c._exit(127);
    }
    _ = std.c.close(fds[1]);
    defer _ = std.c.close(fds[0]);
    var out: std.ArrayList(u8) = .empty;
    const deadline = u.nowMs() + timeout_ms;
    var timed_out = false;
    var total: usize = 0;
    while (true) {
        const left = deadline - u.nowMs();
        if (left <= 0) {
            timed_out = true;
            break;
        }
        var pfd = [_]std.c.pollfd{.{ .fd = fds[0], .events = std.c.POLL.IN, .revents = 0 }};
        const ready = std.c.poll(&pfd, 1, @intCast(@min(left, 1000)));
        if (ready <= 0) {
            // The shell may have exited while a background child holds the pipe.
            var st: c_int = 0;
            const r = std.c.waitpid(pid, &st, std.c.W.NOHANG);
            if (r == pid) {
                _ = std.c.kill(-pid, .KILL);
                return finishBash(&out, st, false, cap, total);
            }
            continue;
        }
        var buf: [16384]u8 = undefined;
        const n = std.c.read(fds[0], &buf, buf.len);
        if (n <= 0) break;
        total += @intCast(n);
        if (out.items.len < cap) try out.appendSlice(u.A, buf[0..@min(@as(usize, @intCast(n)), cap - out.items.len)]);
    }
    if (timed_out) {
        _ = std.c.kill(-pid, .KILL);
        _ = std.c.kill(pid, .KILL);
    }
    var st: c_int = 0;
    _ = std.c.waitpid(pid, &st, 0);
    return finishBash(&out, st, timed_out, cap, total);
}

fn finishBash(out: *std.ArrayList(u8), st: c_int, timed_out: bool, cap: usize, total: usize) BashOut {
    const s: u32 = @bitCast(st);
    const sig = s & 0x7f;
    var text: []const u8 = u.validUtf8(out.items);
    if (total > cap) text = u.fmt("{s}\n... (truncated, {d} bytes total)", .{ text, total });
    return .{
        .output = text,
        .exit_code = if (!timed_out and sig == 0) @intCast((s >> 8) & 0xff) else null,
        .timed_out = timed_out,
        .signal = if (sig != 0 and !timed_out) sig else null,
    };
}

pub fn bash(work_dir: []const u8, input: Value) Result {
    const cmd = u.str(input, "command");
    if (cmd.len == 0) return .{ .err = "`command` (string) is required" };
    var timeout_s: f64 = 60;
    if (u.get(input, "timeout_s")) |t| switch (t) {
        .integer => |i| timeout_s = @floatFromInt(i),
        .float => |f| timeout_s = f,
        else => {},
    };
    timeout_s = std.math.clamp(timeout_s, 1, 3600);
    const r = runBash(work_dir, cmd, @intFromFloat(timeout_s * 1000), 20 * 1024) catch |e| return .{ .err = u.fmt("spawn bash: {s}", .{@errorName(e)}) };
    if (r.timed_out) return .{ .err = u.fmt("{s}\n[timed out after {d}s]", .{ r.output, timeout_s }) };
    if (r.exit_code) |c| {
        if (c == 0) return .{ .ok = u.fmt("{s}\n[exit code: 0]", .{r.output}) };
        return .{ .err = u.fmt("{s}\n[exit code: {d}]", .{ r.output, c }) };
    }
    return .{ .err = u.fmt("{s}\n[exit code: signal {d}]", .{ r.output, r.signal orelse 0 }) };
}

// ----------------------------------------------------------------- schemas

pub const SCHEMAS =
    \\[
    \\ {"name": "bash", "description": "Run a bash command in the working directory. Returns combined stdout/stderr and the exit code (truncated to 20 KB).",
    \\  "input_schema": {"type": "object", "properties": {"command": {"type": "string"}, "timeout_s": {"type": "number", "description": "default 60"}}, "required": ["command"]}},
    \\ {"name": "read_file", "description": "Read a file (truncated to 50 KB).",
    \\  "input_schema": {"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}},
    \\ {"name": "write_file", "description": "Write a file, creating parent directories. Refuses the state dir and the kernel source.",
    \\  "input_schema": {"type": "object", "properties": {"path": {"type": "string"}, "content": {"type": "string"}}, "required": ["path", "content"]}},
    \\ {"name": "list_defs", "description": "List userspace definitions: name, kind, live version, status, description.",
    \\  "input_schema": {"type": "object", "properties": {}, "required": []}},
    \\ {"name": "show_def", "description": "Show one definition version (default: live) with source and tests.",
    \\  "input_schema": {"type": "object", "properties": {"name": {"type": "string"}, "version": {"type": "integer"}}, "required": ["name"]}},
    \\ {"name": "roc_eval", "description": "Scratchpad: compile and run Roc in a throwaway child process, without proposing anything. `source` holds definitions (may be empty) and `expr` is an expression whose value is shown with Str.inspect. Live userspace definitions referenced by name are included. Nothing is journaled or made live. Returns the value or the same structured report the gate gives (parse/type errors with line numbers, crashes, timeouts). Use it to check syntax and try code before propose_def.",
    \\  "input_schema": {"type": "object", "properties": {"source": {"type": "string", "description": "top-level Roc definitions (optional)"}, "expr": {"type": "string", "description": "a Roc expression to evaluate, e.g. my_fn(\"x\")"}}, "required": ["expr"]}},
    \\ {"name": "propose_def", "description": "Propose a new version of a userspace definition written in Roc (see the Roc reference in the system prompt). It is compiled and its tests are run in a scratch child process, then it goes live as a trial and is kept after clean uses or reverted on the first error.\nThe source must define a value with the definition's own name; other top-level names must start with NAME_ (types: with the name in PascalCase).\n- tool: `NAME : Str, List(Str) -> [Done(Str), Fail(Str), ReadFile(Str), ReadBytes(Str), ListDir(Str), WriteFile(Str, Str), Bash(Str)]` (or `-> Str`). First argument: the tool input as JSON text (decode with Json.parse). Second: results of the requests made so far in this call, oldest first ([] on the first call). Needs input_schema.\n- hook `compose_system : List(Str), List(Str) -> Str` (notes, lessons) returns the supplemental system prompt.\n- hook `select_lessons : Str, List({ id : I64, text : Str, score : I64 }) -> List(I64)` (task, lessons) returns lesson ids.\n- note: plain text appended to the system prompt.\ntests: [{\"input\": {...}, \"expect\": \"substring of the output\", \"responses\": [optional scripted request results], \"expect_requests\": [optional substrings of each request, in order]}], at least one for tools and hooks. Hooks get their arguments as input: {\"notes\", \"lessons\"} or {\"task\", \"lessons\"}.",
    \\  "input_schema": {"type": "object", "properties": {"name": {"type": "string", "description": "lowercase identifier [a-z][a-z0-9_]*"}, "kind": {"type": "string", "enum": ["tool", "note", "hook"]}, "source": {"type": "string"}, "description": {"type": "string"}, "tests": {"type": "array", "items": {"type": "object"}}, "input_schema": {"type": "object", "description": "JSON Schema for a tool's input"}}, "required": ["name", "kind", "source", "description"]}},
    \\ {"name": "rollback_def", "description": "Reinstall an older version of a definition as a new trial version.",
    \\  "input_schema": {"type": "object", "properties": {"name": {"type": "string"}, "version": {"type": "integer"}}, "required": ["name", "version"]}},
    \\ {"name": "remember", "description": "Record a durable, reusable lesson.",
    \\  "input_schema": {"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]}},
    \\ {"name": "recall", "description": "Find lessons and verdicts sharing words with the query, highest score first.",
    \\  "input_schema": {"type": "object", "properties": {"query": {"type": "string"}}, "required": ["query"]}}
    \\]
;
