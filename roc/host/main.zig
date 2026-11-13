//! Command line: `cmh [options] <command>`.

const std = @import("std");
const u = @import("util.zig");
const tools = @import("tools.zig");
const kern = @import("kernel.zig");
const prov = @import("provider.zig");
const rocrt = @import("rocrt.zig");
const us = @import("userspace.zig");
const bench = @import("bench.zig");

const USAGE =
    \\usage: cmh [--state DIR] [--provider anthropic|mock] [--script FILE] [--model M]
    \\           [--soak N] [--no-reflect] <command>
    \\
    \\  repl            interactive; each line is a task. Slash commands:
    \\                  /defs  /show NAME [VERSION]  /knowledge  /refine
    \\                  /rollback NAME VERSION  /quit
    \\  run "TASK"      one task, prints the final answer, exits 0 on success
    \\  refine          one refine task
    \\  defs            print the registry
    \\  knowledge       print lessons and verdicts
    \\
    \\developer commands:
    \\  check-seeds      run every seed's tests through the gate
    \\  check-reference  run every example in the Roc reference block
    \\  bench            per-call latency vs number of stored definitions
    \\
;

fn usage(msg: ?[]const u8) u8 {
    if (msg) |m| u.trace("cmh: {s}", .{m});
    std.debug.print("{s}", .{USAGE});
    return 2;
}

pub fn main(init: std.process.Init) !u8 {
    tools.io = init.io;
    const args = try init.minimal.args.toSlice(u.A);
    var state_dir: []const u8 = ".cmh";
    var provider_name: []const u8 = "anthropic";
    var script: ?[]const u8 = null;
    var model: []const u8 = u.getenv("CMH_MODEL") orelse "claude-sonnet-5-5";
    var soak: u32 = 3;
    var reflect = true;
    var rest: std.ArrayList([]const u8) = .empty;
    var i: usize = 1;
    while (i < args.len) : (i += 1) {
        const a = args[i];
        const flags = [_][]const u8{ "--state", "--provider", "--script", "--model", "--soak" };
        var is_flag = false;
        for (flags) |f| is_flag = is_flag or u.eql(a, f);
        if (is_flag) {
            if (i + 1 >= args.len) return usage(u.fmt("{s} needs a value", .{a}));
            i += 1;
            const v = args[i];
            if (u.eql(a, "--state")) state_dir = v;
            if (u.eql(a, "--provider")) provider_name = v;
            if (u.eql(a, "--script")) script = v;
            if (u.eql(a, "--model")) model = v;
            if (u.eql(a, "--soak")) soak = std.fmt.parseInt(u32, v, 10) catch 0;
            if (u.eql(a, "--soak") and soak == 0) return usage("--soak needs a positive integer");
        } else if (u.eql(a, "--no-reflect")) {
            reflect = false;
        } else if (u.eql(a, "-h") or u.eql(a, "--help")) {
            return usage(null);
        } else try rest.append(u.A, a);
    }
    if (rest.items.len == 0) return usage(null);
    const cmd = rest.items[0];
    const n = rest.items.len;
    const valid = (u.eql(cmd, "run") and n == 2) or (n == 1 and (u.eql(cmd, "refine") or u.eql(cmd, "defs") or u.eql(cmd, "knowledge") or
        u.eql(cmd, "repl") or u.eql(cmd, "check-seeds") or u.eql(cmd, "check-reference") or u.eql(cmd, "bench")));
    if (!valid) return usage(null);
    if (u.eql(cmd, "bench")) {
        try bench.run(init.io, u.A);
        return 0;
    }
    if (u.eql(cmd, "check-reference")) return checkReference();

    var cwd_buf: [4096]u8 = undefined;
    const cwd_ptr = std.c.getcwd(&cwd_buf, cwd_buf.len) orelse return usage("cannot read the working directory");
    const cwd = std.mem.sliceTo(@as([*:0]u8, @ptrCast(cwd_ptr)), 0);
    const work_dir = try u.A.dupe(u8, cwd);

    const provider = try u.A.create(prov.Provider);
    if (u.eql(provider_name, "anthropic")) {
        provider.* = .{ .anthropic = prov.Anthropic.fromEnv(model) };
    } else if (u.eql(provider_name, "mock")) {
        const s = script orelse return usage("--provider mock needs --script FILE");
        var m = prov.Mock.fromFile(tools.resolve(work_dir, s)) catch |e| return usage(u.fmt("mock script: {s}", .{@errorName(e)}));
        m.log_path = u.fmt("{s}/mock-requests.jsonl", .{tools.resolve(work_dir, state_dir)});
        provider.* = .{ .mock = m };
    } else return usage(u.fmt("unknown provider `{s}`", .{provider_name}));

    var cfg = kern.Config{ .state_dir = state_dir, .work_dir = work_dir, .soak = soak, .reflect = reflect, .echo_text = u.eql(cmd, "repl") };
    if (u.getenv("CMH_ROC_TIMEOUT_MS")) |t| cfg.timeout_ms = std.fmt.parseInt(u32, t, 10) catch cfg.timeout_ms;
    const k = kern.Kernel.open(cfg, provider) catch |e| {
        u.trace("cmh: {s}", .{@errorName(e)});
        return 2;
    };

    if (u.eql(cmd, "run")) return finish(k.runTask(rest.items[1]));
    if (u.eql(cmd, "refine")) return finish(k.refine());
    if (u.eql(cmd, "defs")) {
        kern.writeStdout(k.defsTable());
        return 0;
    }
    if (u.eql(cmd, "knowledge")) {
        kern.writeStdout(k.knowledgeText());
        return 0;
    }
    if (u.eql(cmd, "check-seeds")) return checkSeeds(k);
    return repl(k);
}

/// `run`/`refine`: only the final answer on stdout; nonzero exit on failure.
fn finish(o: kern.TaskOutcome) u8 {
    if (o.answer.len > 0) kern.writeStdout(u.fmt("{s}\n", .{o.answer}));
    if (o.err) |e| u.trace("cmh: task failed: {s}", .{u.firstLine(e, 300)});
    return if (o.success) 0 else 1;
}

fn readLine() ?[]const u8 {
    var line: std.ArrayList(u8) = .empty;
    while (true) {
        var c: [1]u8 = undefined;
        const n = std.c.read(0, &c, 1);
        if (n <= 0) return if (line.items.len > 0) line.items else null;
        if (c[0] == '\n') return line.items;
        line.append(u.A, c[0]) catch return null;
    }
}

fn printResult(r: tools.Result) void {
    switch (r) {
        .ok => |s| kern.writeStdout(u.fmt("{s}\n", .{s})),
        .err => |e| kern.writeStdout(u.fmt("error: {s}\n", .{e})),
    }
}

fn repl(k: *kern.Kernel) u8 {
    while (true) {
        std.debug.print("cmh> ", .{});
        const raw = readLine() orelse return 0;
        const line = std.mem.trim(u8, raw, " \t\r");
        if (line.len == 0) continue;
        var parts: std.ArrayList([]const u8) = .empty;
        var it = std.mem.tokenizeAny(u8, line, " \t");
        while (it.next()) |p| parts.append(u.A, p) catch return 1;
        const p = parts.items;
        if (p[0][0] != '/') {
            _ = k.runTask(line);
            continue;
        }
        if (u.eql(p[0], "/quit") or u.eql(p[0], "/exit")) return 0;
        if (u.eql(p[0], "/defs")) {
            kern.writeStdout(k.defsTable());
        } else if (u.eql(p[0], "/knowledge")) {
            kern.writeStdout(k.knowledgeText());
        } else if (u.eql(p[0], "/refine")) {
            _ = k.refine();
        } else if (u.eql(p[0], "/show") and (p.len == 2 or p.len == 3)) {
            const v: ?u32 = if (p.len == 3) (std.fmt.parseInt(u32, std.mem.trimStart(u8, p[2], "v"), 10) catch {
                std.debug.print("usage: /show NAME [VERSION]\n", .{});
                continue;
            }) else null;
            printResult(k.showDef(p[1], v));
        } else if (u.eql(p[0], "/rollback") and p.len == 3) {
            const v = std.fmt.parseInt(u32, std.mem.trimStart(u8, p[2], "v"), 10) catch {
                std.debug.print("usage: /rollback NAME VERSION\n", .{});
                continue;
            };
            printResult(k.rollbackDef(p[1], v));
        } else std.debug.print("unknown command {s}\n", .{p[0]});
    }
}

/// Every seed through the gate (scratch child, its own tests).
fn checkSeeds(k: *kern.Kernel) u8 {
    var failed = false;
    for (kern.SEEDS) |s| {
        const tests = (u.parseJson(s.tests) catch unreachable).array.items;
        const r = k.roc.validate(&k.state, s.name, s.kind, 1, s.source, tests);
        if (r.ok) {
            kern.writeStdout(u.fmt("ok   {s}: {d}/{d} tests passed ({d} ms)\n", .{ s.name, r.passed, tests.len, r.ms }));
        } else {
            failed = true;
            kern.writeStdout(u.fmt("FAIL {s}:\n{s}\n", .{ s.name, r.report }));
        }
    }
    return if (failed) 1 else 0;
}

/// Runs every example in the reference: in each ```roc block, a line with
/// `# => VALUE` is an expression and its expected Str.inspect rendering;
/// every other line is a definition available to the block's expressions.
fn checkReference() !u8 {
    var engine = try rocrt.Engine.init(tools.io);
    var it = std.mem.splitSequence(u8, kern.ROC_REFERENCE, "```roc\n");
    _ = it.next();
    var total: usize = 0;
    var bad: usize = 0;
    var block_no: usize = 0;
    while (it.next()) |chunk| {
        block_no += 1;
        const block = chunk[0 .. std.mem.indexOf(u8, chunk, "```") orelse chunk.len];
        var defs: std.ArrayList(u8) = .empty;
        const Ex = struct { expr: []const u8, want: []const u8 };
        var examples: std.ArrayList(Ex) = .empty;
        var lines = std.mem.splitScalar(u8, block, '\n');
        while (lines.next()) |line| {
            if (std.mem.indexOf(u8, line, "# => ")) |at| {
                try examples.append(u.A, .{ .expr = std.mem.trim(u8, line[0..at], " "), .want = std.mem.trim(u8, line[at + 5 ..], " ") });
            } else {
                try defs.appendSlice(u.A, line);
                try defs.append(u.A, '\n');
            }
        }
        if (examples.items.len == 0) {
            // A block of definitions only: it must compile.
            total += 1;
            const out = try engine.evalProgram(u.A, defs.items, "Str.inspect({})");
            if (out == .failed) {
                bad += 1;
                kern.writeStdout(u.fmt("FAIL block {d} (definitions):\n{s}\n{s}\n", .{ block_no, block, out.failed.message }));
            }
        }
        for (examples.items) |ex| {
            total += 1;
            const out = try engine.evalProgram(u.A, defs.items, u.fmt("Str.inspect((\n{s}\n))", .{ex.expr}));
            switch (out) {
                .value => |v| if (!u.eql(v, ex.want)) {
                    bad += 1;
                    kern.writeStdout(u.fmt("FAIL block {d}: {s}\n  want {s}\n  got  {s}\n", .{ block_no, ex.expr, ex.want, v }));
                },
                .failed => |f| {
                    bad += 1;
                    kern.writeStdout(u.fmt("FAIL block {d}: {s}\n{s}\n", .{ block_no, ex.expr, u.truncate(f.message, 1500) }));
                },
            }
        }
    }
    kern.writeStdout(u.fmt("{d}/{d} reference examples ok\n", .{ total - bad, total }));
    _ = us;
    return if (bad == 0) 0 else 1;
}
