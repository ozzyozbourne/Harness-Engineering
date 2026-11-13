//! The Roc wall: the embedded compiler, and a killable child process for
//! every piece of agent code that runs.
//!
//! The parent keeps one pristine `ReplSession` (Builtin module published once,
//! no stored definitions). It never evaluates agent code itself. Every call,
//! test run, proposal check and scratchpad snippet runs in a forked child that
//! inherits the warm session copy-on-write, builds exactly the program it
//! needs, writes a report to a pipe and exits. The parent kills the child at
//! the deadline. A hang, a crash, a stack overflow or an abort only ever ends
//! the child.

const std = @import("std");
const CoreCtx = @import("ctx").CoreCtx;
const eval = @import("eval");
const reporting = @import("reporting");
pub const ReplSession = @import("ReplSession.zig");

const Allocator = std.mem.Allocator;

/// Allocator for compiler state. The session lives for the whole process.
const gpa = std.heap.c_allocator;

pub const Engine = struct {
    io: std.Io,
    session: ReplSession,
    report_config: reporting.ReportingConfig,

    pub fn init(io: std.Io) !Engine {
        return .{
            .io = io,
            .session = try ReplSession.initVirtual(gpa, CoreCtx.default(gpa, gpa, io), .interpreter, .lss),
            .report_config = reporting.ReportingConfig.initForTesting(),
        };
    }

    fn prePublished(self: *Engine) eval.Inspected.PrePublishedBuiltin {
        const b = self.session.builtin_modules;
        return .{ .env = b.builtin_module.env, .indices = b.builtin_indices, .artifact = &b.checked_artifact };
    }

    /// Compiles `defs` plus `main = || <expr>` as one module and returns the
    /// Str that `expr` evaluates to (evaluation happens at compile time).
    /// One compile per call: only the definitions passed in are checked.
    pub fn evalProgram(self: *Engine, arena: Allocator, defs: []const u8, expr: []const u8) !Outcome {
        const source = try std.fmt.allocPrint(gpa, "{s}\nmain = || (\n{s}\n)\n", .{ defs, expr });
        defer gpa.free(source);
        var resources = eval.Inspected.publishProgramKeepingReportedComptimeProblemsWithBuiltinAndContext(
            gpa,
            self.io,
            .module,
            source,
            &.{},
            self.prePublished(),
            self.session.roc_ctx,
            null,
        ) catch |err| switch (ReplSession.programFailureKind(err)) {
            .type_check => {
                const msg = eval.Inspected.renderProblemsWithConfigAndImports(gpa, .module, source, &.{}, self.report_config, self.session.roc_ctx) catch |e|
                    return .{ .failed = .{ .kind = .compile, .message = try std.fmt.allocPrint(arena, "type check failed ({s})", .{@errorName(e)}) } };
                defer gpa.free(msg);
                return .{ .failed = try classify(arena, msg) };
            },
            .parse => return .{ .failed = .{ .kind = .parse, .message = try std.fmt.allocPrint(arena, "the program did not parse:\n{s}", .{source}) } },
            .out_of_memory, .operational => return .{ .failed = .{ .kind = .internal, .message = try std.fmt.allocPrint(arena, "compiler error: {s}", .{@errorName(err)}) } },
        };
        defer resources.deinit(gpa);
        if (try eval.Inspected.parsedResourcesHaveErrorDiagnostics(gpa, &resources)) {
            const msg = try eval.Inspected.renderParsedResourcesProblemsWithConfig(gpa, &resources, self.report_config);
            defer gpa.free(msg);
            return .{ .failed = try classify(arena, msg) };
        }
        const out = eval.Inspected.finalizedComptimeReplStr(&resources) catch |err|
            return .{ .failed = .{ .kind = .internal, .message = try std.fmt.allocPrint(arena, "no value: {s}", .{@errorName(err)}) } };
        return .{ .value = try arena.dupe(u8, out) };
    }

    /// One REPL statement through the session (definitions are validated
    /// against everything stepped before them). Used by the gate in a child.
    pub fn step(self: *Engine, arena: Allocator, stmt: []const u8) !Step {
        const result = self.session.stepLanguageWithConfig(stmt, self.report_config) catch |err|
            return .{ .failed = .{ .kind = .internal, .message = try std.fmt.allocPrint(arena, "compiler error: {s}", .{@errorName(err)}) } };
        defer result.deinit(gpa);
        return switch (result) {
            .definition => |d| .{ .defined = .{ .name = try arena.dupe(u8, d.name), .kind = d.kind } },
            .expression => |out| .{ .value = try arena.dupe(u8, out) },
            .diagnostic => |diag| .{ .failed = switch (diag.kind) {
                .incomplete_input => .{ .kind = .parse, .message = try std.fmt.allocPrint(arena, "incomplete statement (unclosed bracket, string or block?):\n{s}", .{diag.message}) },
                .parse_error => .{ .kind = .parse, .message = try arena.dupe(u8, diag.message) },
                .unsupported_file_import => .{ .kind = .compile, .message = try arena.dupe(u8, diag.message) },
                .compile_error => try classify(arena, diag.message),
            } },
            .runtime_crash => |msg| .{ .failed = .{ .kind = .crash, .message = try arena.dupe(u8, msg) } },
            .statement, .none => .nothing,
        };
    }

    /// Splits source into top-level statements (the parser decides).
    pub fn statements(self: *Engine, arena: Allocator, source: []const u8) ![][]const u8 {
        const parts = try self.session.splitInputIntoStatements(source);
        defer self.session.freeStatementSlices(parts);
        const out = try arena.alloc([]const u8, parts.len);
        for (parts, 0..) |p, i| out[i] = try arena.dupe(u8, p);
        return out;
    }

    /// Drops every stored definition (in a child, before building a program).
    pub fn clear(self: *Engine) void {
        self.session.clear();
    }
};

pub const DefKind = ReplSession.DefinitionKind;

pub const Failure = struct {
    kind: Kind,
    message: []const u8,

    pub const Kind = enum { parse, compile, crash, timeout, died, internal };
};

pub const Outcome = union(enum) {
    value: []const u8,
    failed: Failure,
};

pub const Step = union(enum) {
    defined: struct { name: []const u8, kind: DefKind },
    value: []const u8,
    nothing,
    failed: Failure,
};

/// A compile-time crash is reported by the compiler as a diagnostic; tell it
/// apart from type errors by its report title.
fn classify(arena: Allocator, msg: []const u8) !Failure {
    const lower = try std.ascii.allocLowerString(arena, firstLine(msg));
    const crash = std.mem.indexOf(u8, lower, "crash") != null or std.mem.indexOf(u8, lower, "overflow") != null or
        std.mem.indexOf(u8, lower, "panic") != null;
    return .{ .kind = if (crash) .crash else .compile, .message = try arena.dupe(u8, std.mem.trim(u8, msg, " \n")) };
}

fn firstLine(s: []const u8) []const u8 {
    const t = std.mem.trimStart(u8, s, " \n");
    return t[0 .. std.mem.indexOfScalar(u8, t, '\n') orelse t.len];
}

// ------------------------------------------------------------- isolation

/// What came back from a child.
pub const Child = union(enum) {
    /// The child finished; this is everything it wrote.
    done: []const u8,
    /// The deadline passed; the child was killed. `partial` is what it wrote.
    timeout: struct { ms: u32, partial: []const u8 },
    /// The child died without finishing (signal or abnormal exit).
    died: struct { status: []const u8, partial: []const u8 },
};

/// Marker a child writes to start a new phase: the deadline restarts.
pub const phase_mark: u8 = 0x1e;
/// Marker a child writes right before exiting normally.
const done_mark: u8 = 0x04;

pub const Pipe = struct {
    fd: std.c.fd_t,
    pub fn write(self: Pipe, bytes: []const u8) void {
        var rest = bytes;
        while (rest.len > 0) {
            const n = std.c.write(self.fd, rest.ptr, rest.len);
            if (n <= 0) return;
            rest = rest[@intCast(n)..];
        }
    }
    pub fn print(self: Pipe, comptime fmt: []const u8, args: anytype) void {
        var buf: [512]u8 = undefined;
        self.write(std.fmt.bufPrint(&buf, fmt, args) catch return);
    }
    /// Starts a new phase (each test, each request round) with a fresh deadline.
    pub fn phase(self: Pipe) void {
        self.write(&.{phase_mark});
    }
};

/// Runs `job(ctx, pipe)` in a forked child and collects what it writes. The
/// child is killed if a phase runs longer than `timeout_ms`.
pub fn isolate(io: std.Io, arena: Allocator, timeout_ms: u32, ctx: anytype, comptime job: fn (@TypeOf(ctx), Pipe) void) !Child {
    var fds: [2]std.c.fd_t = undefined;
    if (std.c.pipe(&fds) != 0) return error.PipeFailed;
    const pid = std.c.fork();
    if (pid < 0) return error.ForkFailed;
    if (pid == 0) {
        _ = std.c.close(fds[0]);
        const pipe = Pipe{ .fd = fds[1] };
        job(ctx, pipe);
        pipe.write(&.{done_mark});
        std.c._exit(0);
    }
    _ = std.c.close(fds[1]);
    defer _ = std.c.close(fds[0]);

    var report: std.ArrayList(u8) = .empty;
    var phase_start = std.Io.Clock.awake.now(io);
    var timed_out = false;
    while (true) {
        const elapsed: i64 = @intCast(phase_start.durationTo(std.Io.Clock.awake.now(io)).toMilliseconds());
        if (elapsed >= timeout_ms) {
            timed_out = true;
            break;
        }
        var pfd = [_]std.c.pollfd{.{ .fd = fds[0], .events = std.c.POLL.IN, .revents = 0 }};
        const ready = std.c.poll(&pfd, 1, @intCast(@as(i64, timeout_ms) - elapsed));
        if (ready == 0) continue;
        if (ready < 0) continue; // EINTR
        var buf: [16384]u8 = undefined;
        const n = std.c.read(fds[0], &buf, buf.len);
        if (n <= 0) break;
        const chunk = buf[0..@intCast(n)];
        if (std.mem.lastIndexOfScalar(u8, chunk, phase_mark) != null) phase_start = std.Io.Clock.awake.now(io);
        try report.appendSlice(arena, chunk);
    }
    if (timed_out) {
        _ = std.c.kill(pid, .KILL);
        _ = std.c.waitpid(pid, null, 0);
        return .{ .timeout = .{ .ms = timeout_ms, .partial = report.items } };
    }
    var status: c_int = 0;
    _ = std.c.waitpid(pid, &status, 0);
    const items = report.items;
    if (items.len > 0 and items[items.len - 1] == done_mark) return .{ .done = items[0 .. items.len - 1] };
    const st: u32 = @bitCast(status);
    const why = if (st & 0x7f != 0)
        try std.fmt.allocPrint(arena, "killed by signal {d} ({s})", .{ st & 0x7f, signalName(st & 0x7f) })
    else
        try std.fmt.allocPrint(arena, "exited with status {d}", .{(st >> 8) & 0xff});
    return .{ .died = .{ .status = why, .partial = items } };
}

fn signalName(sig: u32) []const u8 {
    return switch (sig) {
        6 => "SIGABRT: abort, often a failed assertion or panic in the compiler",
        9 => "SIGKILL: out of memory?",
        11 => "SIGSEGV: segmentation fault, often a stack overflow from deep recursion",
        7 => "SIGBUS",
        8 => "SIGFPE",
        else => "signal",
    };
}
