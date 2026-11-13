//! `cmh bench`: per-call latency as the number of stored definitions grows.
//!
//! Compares two ways of running a call:
//!   - session: every definition lives in one REPL session, so each call
//!     compiles and checks all of them (what the prototype did);
//!   - closure: the call's program holds only the called definition and the
//!     definitions it references (what the kernel does).
//! Both run in-process here, so the numbers exclude the fork (measured
//! separately as "closure+fork").

const std = @import("std");
const rocrt = @import("rocrt.zig");

fn defSource(arena: std.mem.Allocator, k: usize) ![]const u8 {
    return std.fmt.allocPrint(arena,
        \\bench{d}_words : Str -> List(Str)
        \\bench{d}_words = |s| Str.split_on(s, " ").keep_if(|w| !Str.is_empty(w))
        \\
        \\bench{d}_stats : List(Str) -> {{ count : U64, longest : Str }}
        \\bench{d}_stats = |ws| List.fold(ws, {{ count: 0, longest: "" }}, |acc, w|
        \\    {{ count: acc.count + 1, longest: if Str.count_utf8_bytes(w) > Str.count_utf8_bytes(acc.longest) w else acc.longest }})
        \\
        \\bench{d} : Str, List(Str) -> [Done(Str), ReadFile(Str)]
        \\bench{d} = |input, results| {{
        \\    st = bench{d}_stats(bench{d}_words(input))
        \\    match results {{
        \\        [] => Done("${{st.count.to_str()}} words, longest ${{st.longest}}")
        \\        [r, ..] => Done(Str.trim(r))
        \\    }}
        \\}}
    , .{ k, k, k, k, k, k, k, k });
}

fn ms(io: std.Io, start: std.Io.Timestamp) f64 {
    const d = start.durationTo(std.Io.Clock.awake.now(io));
    return @as(f64, @floatFromInt(d.toNanoseconds())) / 1e6;
}

const ForkCtx = struct { engine: *rocrt.Engine, arena: std.mem.Allocator, defs: []const u8, expr: []const u8 };

fn forkJob(c: ForkCtx, pipe: rocrt.Pipe) void {
    const out = c.engine.evalProgram(c.arena, c.defs, c.expr) catch return;
    switch (out) {
        .value => |v| pipe.write(v),
        .failed => |f| pipe.write(f.message),
    }
}

pub fn run(io: std.Io, arena: std.mem.Allocator) !void {
    var engine = try rocrt.Engine.init(io);
    const expr = "Json.to_str(bench0(\"alpha beta gamma delta\", []))";
    const counts = [_]usize{ 1, 10, 50, 100 };
    var all: std.ArrayList(u8) = .empty;
    var stored: usize = 0;
    std.debug.print("defs  add_def_ms  session_call_ms  closure_call_ms  closure+fork_ms\n", .{});
    for (counts) |n| {
        // Grow the session to n definitions (each 3 statements + annotations).
        var add_ms: f64 = 0;
        while (stored < n) : (stored += 1) {
            const src = try defSource(arena, stored);
            try all.appendSlice(arena, src);
            try all.append(arena, '\n');
            const t0 = std.Io.Clock.awake.now(io);
            for (try engine.statements(arena, src)) |stmt| {
                switch (try engine.step(arena, stmt)) {
                    .failed => |f| {
                        std.debug.print("bench def {d} failed: {s}\n", .{ stored, f.message });
                        return error.BenchFailed;
                    },
                    else => {},
                }
            }
            add_ms = ms(io, t0);
        }
        // Session: the call compiles all n definitions.
        var session_ms: f64 = 0;
        const reps = 3;
        for (0..reps) |_| {
            const t0 = std.Io.Clock.awake.now(io);
            const out = try engine.step(arena, expr);
            session_ms += ms(io, t0);
            if (out != .value) return error.BenchFailed;
        }
        // Closure: a program with only bench0.
        const closure = try defSource(arena, 0);
        var closure_ms: f64 = 0;
        var fork_ms: f64 = 0;
        for (0..reps) |_| {
            const t0 = std.Io.Clock.awake.now(io);
            const out = try engine.evalProgram(arena, closure, expr);
            closure_ms += ms(io, t0);
            if (out != .value) {
                std.debug.print("closure call failed: {s}\n", .{out.failed.message});
                return error.BenchFailed;
            }
            const t1 = std.Io.Clock.awake.now(io);
            const child = try rocrt.isolate(io, arena, 20_000, ForkCtx{ .engine = &engine, .arena = arena, .defs = closure, .expr = expr }, forkJob);
            fork_ms += ms(io, t1);
            if (child != .done) return error.BenchFailed;
        }
        std.debug.print("{d:>4}  {d:>10.1}  {d:>15.1}  {d:>15.1}  {d:>15.1}\n", .{ n, add_ms, session_ms / reps, closure_ms / reps, fork_ms / reps });
    }
    const check = try engine.evalProgram(arena, try defSource(arena, 0), expr);
    std.debug.print("sample result: {s}\n", .{check.value});
}
