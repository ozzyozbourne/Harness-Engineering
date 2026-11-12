//! Prototype: a Zig host that keeps Roc code modifiable at runtime.
//!
//! The host embeds the Roc compiler and interpreter (`ReplSession` from the Roc
//! repo). It reads a scenario file of steps separated by `---` lines. Each step is
//! Roc source that either (re)defines something or evaluates an expression. The
//! host plays the role of nvim's `:luafile` / Lisp's `eval`:
//!   - a definition that parses and type-checks becomes the live version of its
//!     name (the host bumps a version number);
//!   - a rejected definition is reported with the compiler's message and the
//!     previous version stays live;
//!   - a Roc crash is caught and reported; the host keeps running.
//! Calls look names up at call time, so redefining `greet` changes what an
//! earlier-defined caller (`shout`) does: late binding.
//!
//! With `--timeout-ms N`, every statement first runs in a forked child that the
//! host kills if it takes longer than N ms. Roc code here is pure, so a statement
//! that succeeded in the child is replayed in the host's own session to commit it.
//!
//! The REPL session rejects effectful calls, so Roc code cannot call the host
//! directly. A `# drive FN` step uses requests-as-data instead: the host calls
//! `FN("")`, and while the result is `ReadFile("path")` it reads the file and calls
//! `FN(contents)`; it stops at `Done("text")`.

const std = @import("std");
const CoreCtx = @import("ctx").CoreCtx;
const reporting = @import("reporting");
const ReplSession = @import("ReplSession.zig");

const gpa = std.heap.page_allocator;

const Host = struct {
    io: std.Io,
    arena: std.mem.Allocator,
    session: ReplSession,
    report_config: reporting.ReportingConfig,
    versions: std.StringHashMap(u32),
    timeout_ms: ?i32,

    /// What one statement did, as seen by the host. Payloads live in the arena.
    const Outcome = union(enum) {
        defined: []const u8, // name of a committed value definition
        other_definition,
        value: []const u8,
        rejected: []const u8,
        crashed: []const u8,
        killed: []const u8, // timeout or fatal signal in the child
        nothing,
    };

    /// Run one statement in the host's own session.
    fn stepInProcess(self: *Host, stmt: []const u8) !Outcome {
        const result = self.session.stepLanguageWithConfig(stmt, self.report_config) catch |err|
            return .{ .rejected = @errorName(err) };
        defer result.deinit(gpa);
        return switch (result) {
            .definition => |d| if (d.kind == .value)
                .{ .defined = try self.arena.dupe(u8, d.name) }
            else
                .other_definition,
            .expression => |out| .{ .value = try self.arena.dupe(u8, out) },
            .diagnostic => |diag| .{ .rejected = try std.fmt.allocPrint(self.arena, "{s}: {s}", .{ @tagName(diag.kind), diag.message }) },
            .runtime_crash => |msg| .{ .crashed = try self.arena.dupe(u8, msg) },
            .statement, .none => .nothing,
        };
    }

    /// Run one statement in a forked child with a deadline. A success that
    /// changes the session is replayed in-process, so the host's session only
    /// ever holds statements that finished in time.
    fn step(self: *Host, stmt: []const u8) !Outcome {
        const limit = self.timeout_ms orelse return self.stepInProcess(stmt);

        var fds: [2]std.c.fd_t = undefined;
        if (std.c.pipe(&fds) != 0) return error.PipeFailed;
        const pid = std.c.fork();
        if (pid < 0) return error.ForkFailed;
        if (pid == 0) {
            // Child: run the statement, write "<tag>\n<payload>" to the pipe, exit.
            _ = std.c.close(fds[0]);
            const out = self.stepInProcess(stmt) catch std.c._exit(3);
            const tag, const payload = switch (out) {
                .defined => |s| .{ "defined", s },
                .other_definition => .{ "other_definition", "" },
                .value => |s| .{ "value", s },
                .rejected => |s| .{ "rejected", s },
                .crashed => |s| .{ "crashed", s },
                .killed => |s| .{ "killed", s },
                .nothing => .{ "nothing", "" },
            };
            writeAll(fds[1], tag);
            writeAll(fds[1], "\n");
            writeAll(fds[1], payload);
            std.c._exit(0);
        }

        // Parent: collect the child's report until EOF or the deadline.
        _ = std.c.close(fds[1]);
        defer _ = std.c.close(fds[0]);
        var report: std.ArrayList(u8) = .empty;
        const start = std.Io.Clock.awake.now(self.io);
        var timed_out = false;
        while (true) {
            const elapsed: i64 = @intCast(start.durationTo(std.Io.Clock.awake.now(self.io)).toMilliseconds());
            if (elapsed >= limit) {
                timed_out = true;
                break;
            }
            var pfd = [_]std.c.pollfd{.{ .fd = fds[0], .events = std.c.POLL.IN, .revents = 0 }};
            if (std.c.poll(&pfd, 1, @intCast(limit - elapsed)) == 0) continue; // re-check the deadline
            var buf: [4096]u8 = undefined;
            const n = std.c.read(fds[0], &buf, buf.len);
            if (n <= 0) break; // EOF: the child finished or died
            try report.appendSlice(self.arena, buf[0..@intCast(n)]);
        }
        if (timed_out) {
            _ = std.c.kill(pid, .KILL);
            _ = std.c.waitpid(pid, null, 0);
            return .{ .killed = try std.fmt.allocPrint(self.arena, "no result after {d} ms; child killed", .{limit}) };
        }
        var status: c_int = 0;
        _ = std.c.waitpid(pid, &status, 0);
        const nl = std.mem.indexOfScalar(u8, report.items, '\n') orelse
            return .{ .killed = try std.fmt.allocPrint(self.arena, "child died without a report (wait status {d})", .{status}) };
        const tag = report.items[0..nl];
        const payload = report.items[nl + 1 ..];

        // Replay committed changes so the host's session matches the child's.
        if (std.mem.eql(u8, tag, "defined") or std.mem.eql(u8, tag, "other_definition"))
            return self.stepInProcess(stmt);
        if (std.mem.eql(u8, tag, "value")) return .{ .value = payload };
        if (std.mem.eql(u8, tag, "rejected")) return .{ .rejected = payload };
        if (std.mem.eql(u8, tag, "crashed")) return .{ .crashed = payload };
        return .nothing;
    }

    fn print(self: *Host, out: Outcome) !void {
        switch (out) {
            .defined => |name| {
                const gop = try self.versions.getOrPut(name);
                gop.value_ptr.* = if (gop.found_existing) gop.value_ptr.* + 1 else 1;
                std.debug.print("[host] `{s}` v{d} is live\n", .{ name, gop.value_ptr.* });
            },
            .other_definition => std.debug.print("[host] accepted\n", .{}),
            .value => |v| std.debug.print("=> {s}\n", .{v}),
            .rejected => |m| std.debug.print("[host] rejected; live versions unchanged:\n{s}\n", .{firstLines(m, 12)}),
            .crashed => |m| std.debug.print("[host] crash caught, host still running: {s}\n", .{m}),
            .killed => |m| std.debug.print("[host] {s}; live versions unchanged, host still running\n", .{m}),
            .nothing => {},
        }
    }

    /// Requests-as-data loop: Roc asks, the host performs, Roc gets the answer.
    fn drive(self: *Host, func: []const u8, dir: std.Io.Dir) !void {
        var input: []const u8 = "";
        for (0..5) |_| {
            const call = try std.fmt.allocPrint(self.arena, "{s}(\"{s}\")", .{ func, try rocEscape(self.arena, input) });
            std.debug.print("roc> {s}\n", .{call});
            const out = try self.step(call);
            const rendered = switch (out) {
                .value => |v| v,
                else => return self.print(out),
            };
            std.debug.print("=> {s}\n", .{rendered});
            const request = parseRequest(self.arena, rendered) orelse {
                std.debug.print("[host] not a request; stopping\n", .{});
                return;
            };
            if (std.mem.eql(u8, request.tag, "Done")) {
                std.debug.print("[host] done: {s}\n", .{request.arg});
                return;
            }
            if (!std.mem.eql(u8, request.tag, "ReadFile")) {
                input = try std.fmt.allocPrint(self.arena, "error: unknown request {s}", .{request.tag});
            } else if (std.mem.indexOf(u8, request.arg, "..") != null or std.fs.path.isAbsolute(request.arg)) {
                input = "error: path refused (must stay in the work dir)"; // the rules
            } else {
                input = dir.readFileAlloc(self.io, request.arg, self.arena, .limited(1 << 16)) catch |err|
                    try std.fmt.allocPrint(self.arena, "error: {s}", .{@errorName(err)});
                std.debug.print("[host] performed ReadFile({s}): {d} bytes\n", .{ request.arg, input.len });
            }
        }
        std.debug.print("[host] gave up after 5 rounds\n", .{});
    }
};

fn writeAll(fd: std.c.fd_t, bytes: []const u8) void {
    var rest = bytes;
    while (rest.len > 0) {
        const n = std.c.write(fd, rest.ptr, rest.len);
        if (n <= 0) return;
        rest = rest[@intCast(n)..];
    }
}

fn firstLines(text: []const u8, max: usize) []const u8 {
    var count: usize = 0;
    for (text, 0..) |c, i| if (c == '\n') {
        count += 1;
        if (count == max) return text[0..i];
    };
    return text;
}

/// Escape text for a Roc string literal.
fn rocEscape(arena: std.mem.Allocator, s: []const u8) ![]const u8 {
    var out: std.ArrayList(u8) = .empty;
    for (s) |c| switch (c) {
        '\\' => try out.appendSlice(arena, "\\\\"),
        '"' => try out.appendSlice(arena, "\\\""),
        '$' => try out.appendSlice(arena, "\\$"),
        '\n' => try out.appendSlice(arena, "\\n"),
        else => try out.append(arena, c),
    };
    return out.items;
}

/// Parse a rendered `Tag("payload")` value.
fn parseRequest(arena: std.mem.Allocator, v: []const u8) ?struct { tag: []const u8, arg: []const u8 } {
    const open = std.mem.indexOf(u8, v, "(\"") orelse return null;
    if (!std.mem.endsWith(u8, v, "\")")) return null;
    const raw = v[open + 2 .. v.len - 2];
    var out: std.ArrayList(u8) = .empty;
    var i: usize = 0;
    while (i < raw.len) : (i += 1) {
        if (raw[i] == '\\' and i + 1 < raw.len) {
            i += 1;
            out.append(arena, if (raw[i] == 'n') '\n' else raw[i]) catch return null;
        } else out.append(arena, raw[i]) catch return null;
    }
    return .{ .tag = v[0..open], .arg = out.items };
}

pub fn main(init: std.process.Init) !void {
    const io = init.io;
    const arena = init.arena.allocator();
    const args = try init.minimal.args.toSlice(arena);

    var timeout_ms: ?i32 = null;
    var scenario_path: ?[]const u8 = null;
    var i: usize = 1;
    while (i < args.len) : (i += 1) {
        if (std.mem.eql(u8, args[i], "--timeout-ms") and i + 1 < args.len) {
            i += 1;
            timeout_ms = try std.fmt.parseInt(i32, args[i], 10);
        } else scenario_path = args[i];
    }
    const path = scenario_path orelse {
        std.debug.print("usage: rochost [--timeout-ms N] SCENARIO\n", .{});
        std.process.exit(2);
    };
    const scenario = try std.Io.Dir.cwd().readFileAlloc(io, path, arena, .limited(1 << 20));
    // `# drive` reads files relative to the scenario's directory (the work dir).
    const work_dir = try std.Io.Dir.cwd().openDir(io, std.fs.path.dirname(path) orelse ".", .{});

    var host = Host{
        .io = io,
        .arena = arena,
        .session = try ReplSession.initVirtual(gpa, CoreCtx.default(gpa, gpa, io), .interpreter, .lss),
        .report_config = reporting.ReportingConfig.initForTesting(),
        .versions = std.StringHashMap(u32).init(arena),
        .timeout_ms = timeout_ms,
    };
    defer host.session.deinit();

    var steps = std.mem.splitSequence(u8, scenario, "\n---\n");
    var step_no: usize = 0;
    while (steps.next()) |raw| {
        const text = std.mem.trim(u8, raw, " \t\r\n");
        if (text.len == 0) continue;
        step_no += 1;

        // A leading `# label` line titles the step; `# drive FN` runs the request loop.
        var src = text;
        if (std.mem.startsWith(u8, text, "#")) {
            const nl = std.mem.indexOfScalar(u8, text, '\n') orelse text.len;
            const label = std.mem.trim(u8, text[1..nl], " ");
            std.debug.print("\n== step {d}: {s}\n", .{ step_no, label });
            src = std.mem.trim(u8, text[nl..], " \t\r\n");
            if (std.mem.startsWith(u8, label, "drive ")) {
                try host.drive(std.mem.trim(u8, label["drive ".len..], " "), work_dir);
                continue;
            }
        } else std.debug.print("\n== step {d}\n", .{step_no});

        const statements = try host.session.splitInputIntoStatements(src);
        defer host.session.freeStatementSlices(statements);
        for (statements) |stmt| {
            std.debug.print("roc> {s}\n", .{stmt});
            try host.print(try host.step(stmt));
        }
    }

    std.debug.print("\n== registry at exit\n", .{});
    var it = host.versions.iterator();
    while (it.next()) |e| std.debug.print("{s} v{d}\n", .{ e.key_ptr.*, e.value_ptr.* });
}
