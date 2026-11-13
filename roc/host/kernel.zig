//! The kernel ring: agent loop, gate, knowledge and journal replay.
//! Immutable to the agent (`write_file` and the WriteFile request refuse the
//! whole roc/ flavor directory, the state dir and the base prompt).

const std = @import("std");
const u = @import("util.zig");
const tools = @import("tools.zig");
const st = @import("state.zig");
const us = @import("userspace.zig");
const rocrt = @import("rocrt.zig");
const prov = @import("provider.zig");
const Journal = @import("journal.zig").Journal;
const opts = @import("cmh_options");
const Value = u.Value;

pub const BASE_PROMPT_EMBEDDED = @embedFile("base-prompt.md");
pub const ROC_REFERENCE = @embedFile("roc_reference.md");

pub const REFINE_PROMPT = "Refine yourself. Review the recent task outcomes below, the verdicts in your system prompt, " ++
    "and your current userspace (use list_defs, show_def and recall). Then improve userspace through propose_def or knowledge " ++
    "through remember. Make at most 3 changes, prefer small ones, and give every tool or hook real tests. " ++
    "Finish with a short summary of what you changed and why.";

pub const Seed = struct {
    name: []const u8,
    kind: []const u8,
    source: []const u8,
    description: []const u8,
    tests: []const u8,
    input_schema: []const u8,
};

pub const SEEDS = [_]Seed{
    .{
        .name = "compose_system",
        .kind = "hook",
        .source = @embedFile("seed/compose_system.roc"),
        .description = "Joins notes under a Notes heading and lessons under a Lessons heading.",
        .tests = "[{\"input\": {\"notes\": [\"Be brief.\"], \"lessons\": [\"Run tests first.\"]}, \"expect\": \"## Lessons\\n- Run tests first.\"}, {\"input\": {\"notes\": [], \"lessons\": []}, \"expect\": \"\"}]",
        .input_schema = "null",
    },
    .{
        .name = "select_lessons",
        .kind = "hook",
        .source = @embedFile("seed/select_lessons.roc"),
        .description = "Top 8 lessons by score sharing a 4+ letter word with the task, else top 3 by score.",
        .tests = "[{\"input\": {\"task\": \"fix the parser bug\", \"lessons\": [{\"id\": 1, \"text\": \"the parser needs tests\", \"score\": 0}, {\"id\": 2, \"text\": \"unrelated\", \"score\": 5}]}, \"expect\": \"[1]\"}, {\"input\": {\"task\": \"zzz\", \"lessons\": [{\"id\": 1, \"text\": \"a\", \"score\": 0}, {\"id\": 2, \"text\": \"b\", \"score\": 5}]}, \"expect\": \"[2,1]\"}]",
        .input_schema = "null",
    },
    .{
        .name = "word_count",
        .kind = "tool",
        .source = @embedFile("seed/word_count.roc"),
        .description = "Counts whitespace-separated words in `text`.",
        .tests = "[{\"input\": {\"text\": \"one two three\"}, \"expect\": \"3\"}, {\"input\": {\"text\": \"  a\\nb  \"}, \"expect\": \"2\"}]",
        .input_schema = "{\"type\": \"object\", \"properties\": {\"text\": {\"type\": \"string\"}}, \"required\": [\"text\"]}",
    },
};

pub const Config = struct {
    state_dir: []const u8,
    work_dir: []const u8,
    soak: u32 = 3,
    reflect: bool = true,
    max_turns: usize = 40,
    /// Print assistant text to stdout as it arrives (repl). Otherwise it goes
    /// to the stderr trace and the caller prints the final answer.
    echo_text: bool = false,
    /// Deadline per phase of every child running Roc code.
    timeout_ms: u32 = 10_000,
};

pub const TaskOutcome = struct { success: bool, answer: []const u8, err: ?[]const u8 };

pub const Kernel = struct {
    cfg: Config,
    state: st.State = .{},
    journal: Journal,
    roc: us.Roc,
    protected: []const []const u8,
    provider: *prov.Provider,
    base_prompt: []const u8,
    kernel_schemas: []Value,

    pub fn open(cfg_in: Config, provider: *prov.Provider) !*Kernel {
        var cfg = cfg_in;
        try @import("journal.zig").mkdirs(tools.resolve(cfg.work_dir, cfg.state_dir));
        cfg.state_dir = tools.resolve(cfg.work_dir, cfg.state_dir);
        cfg.work_dir = tools.resolve("/", cfg.work_dir);
        const journal = try Journal.open(cfg.state_dir);
        const protected = try u.A.dupe([]const u8, &.{ cfg.state_dir, tools.resolve("/", opts.cmh_dir), tools.resolve("/", opts.base_prompt_path) });
        const base = tools.readFileLimited(opts.base_prompt_path, 1 << 20) catch BASE_PROMPT_EMBEDDED;
        const k = try u.A.create(Kernel);
        k.* = .{
            .cfg = cfg,
            .journal = journal,
            .roc = .{ .engine = try rocrt.Engine.init(tools.io), .work_dir = cfg.work_dir, .protected = protected, .timeout_ms = cfg.timeout_ms },
            .protected = protected,
            .provider = provider,
            .base_prompt = base,
            .kernel_schemas = (try u.parseJson(tools.SCHEMAS)).array.items,
        };
        const events = try journal.readAll();
        for (events) |ev| k.state.apply(ev);
        if (events.len == 0) {
            for (SEEDS) |s| {
                _ = try k.record("def_trial", u.json(.{
                    .{ "name", s.name },                              .{ "version", 1 },
                    .{ "kind", s.kind },                              .{ "source", s.source },
                    .{ "description", s.description },                .{ "origin", "seed" },
                    .{ "tests", try u.parseJson(s.tests) },           .{ "input_schema", try u.parseJson(s.input_schema) },
                    .{ "names", us.topLevelNames(s.source) },
                }));
                _ = try k.record("def_kept", u.json(.{ .{ "name", s.name }, .{ "version", 1 }, .{ "seed", true }, .{ "verdict", "" } }));
            }
        }
        return k;
    }

    fn record(self: *Kernel, kind: []const u8, fields: Value) !Value {
        const ev = try self.journal.append(kind, fields);
        self.state.apply(ev);
        return ev;
    }

    /// `record` for events whose loss must not stop the agent.
    fn note(self: *Kernel, kind: []const u8, fields: Value) void {
        _ = self.record(kind, fields) catch |e| u.trace("[journal] write failed: {s}", .{@errorName(e)});
    }

    // ------------------------------------------------------------- gate

    fn shapeCheck(self: *Kernel, name: []const u8, kind: []const u8, source: []const u8, tests: Value, schema: Value) ?[]const u8 {
        var valid = name.len > 0 and name.len <= 48 and std.ascii.isLower(name[0]);
        for (name) |c| valid = valid and (std.ascii.isLower(c) or std.ascii.isDigit(c) or c == '_');
        if (!valid) return u.fmt("invalid name `{s}`: use [a-z][a-z0-9_]*, at most 48 chars", .{name});
        if (tools.isKernelTool(name)) return u.fmt("`{s}` is a kernel tool and cannot be redefined", .{name});
        if (!(u.eql(kind, "tool") or u.eql(kind, "note") or u.eql(kind, "hook"))) return u.fmt("unknown kind `{s}` (tool, note, hook)", .{kind});
        if (self.state.defs.get(name)) |d| if (d.versions.items.len > 0) {
            const prev = d.versions.items[d.versions.items.len - 1];
            if (!u.eql(prev.kind, kind)) return u.fmt("`{s}` is a {s}; it cannot become a {s}", .{ name, prev.kind, kind });
        };
        if (std.mem.trim(u8, source, " \t\r\n").len == 0) return "source is empty";
        if (u.eql(kind, "note")) return null;
        if (u.eql(kind, "hook") and !(u.eql(name, "compose_system") or u.eql(name, "select_lessons")))
            return u.fmt("unknown hook `{s}`: hooks must be named compose_system or select_lessons", .{name});
        if (u.eql(kind, "tool") and !u.eql(u.str(schema, "type"), "object"))
            return "tools need an input_schema JSON Schema object with \"type\": \"object\"";
        if (tests != .array or tests.array.items.len == 0) return "tools and hooks need at least one test";
        for (tests.array.items, 0..) |t, i| {
            const e = u.get(t, "expect") orelse Value.null;
            const in = u.get(t, "input") orelse Value.null;
            if (e != .string or in != .object) return u.fmt("test {d}: needs an object `input` and a string `expect`", .{i + 1});
        }
        return null;
    }

    pub fn proposeDef(self: *Kernel, input: Value) tools.Result {
        const name = u.str(input, "name");
        const kind = u.str(input, "kind");
        const tests = u.get(input, "tests") orelse u.json(.{});
        const tests_v: Value = if (tests == .array) tests else blk: {
            // Models sometimes send the tests array as a JSON string.
            if (tests == .string) {
                const p = u.parseJson(tests.string) catch Value.null;
                if (p == .array) break :blk p;
            }
            break :blk u.json(@as([]const Value, &.{}));
        };
        const schema = if (u.eql(kind, "tool")) (u.get(input, "input_schema") orelse Value.null) else Value.null;
        self.note("def_proposed", u.json(.{ .{ "name", name }, .{ "kind", kind }, .{ "source", u.str(input, "source") }, .{ "description", u.str(input, "description") }, .{ "tests", tests_v }, .{ "input_schema", schema } }));
        const version = if (self.state.defs.get(name)) |d| d.nextVersion() else 1;
        return self.installTrial(name, .{
            .version = version,
            .kind = kind,
            .source = u.str(input, "source"),
            .description = u.str(input, "description"),
            .tests = tests_v,
            .input_schema = schema,
            .status = "trial",
            .names = &.{},
        }, "propose");
    }

    pub fn rollbackDef(self: *Kernel, name: []const u8, version: u32) tools.Result {
        const def = self.state.defs.get(name) orelse return .{ .err = u.fmt("no definition named `{s}`", .{name}) };
        const old = def.get(version) orelse return .{ .err = u.fmt("`{s}` has no version {d}", .{ name, version }) };
        var cand = old.*;
        cand.version = def.nextVersion();
        cand.status = "trial";
        return self.installTrial(name, cand, u.fmt("rollback to v{d}", .{version}));
    }

    fn installTrial(self: *Kernel, name: []const u8, cand_in: st.DefVersion, origin: []const u8) tools.Result {
        var cand = cand_in;
        const v = cand.version;
        if (self.shapeCheck(name, cand.kind, cand.source, cand.tests, cand.input_schema)) |e| return self.reject(name, v, e);
        var passed: usize = 0;
        var ms: i64 = 0;
        if (!u.eql(cand.kind, "note")) {
            const res = self.roc.validate(&self.state, name, cand.kind, v, cand.source, cand.tests.array.items);
            if (!res.ok) return self.reject(name, v, res.report);
            cand.names = res.names;
            passed = res.passed;
            ms = res.ms;
            u.trace("[gate] scratch child: {s} v{d} compiled, {d} test(s) passed in {d} ms", .{ name, v, passed, ms });
        }
        // A trial still soaking is superseded (reverted) first.
        if (self.state.defs.get(name)) |def| if (def.liveVersion()) |live| if (u.eql(live.status, "trial")) {
            const verdict = u.fmt("def {s} v{d} (\"{s}\") reverted: superseded by v{d} before its soak finished", .{ name, live.version, live.description, v });
            self.note("def_reverted", u.json(.{ .{ "name", name }, .{ "version", live.version }, .{ "restored", def.lastKept() }, .{ "error", "superseded" }, .{ "verdict", verdict } }));
        };
        self.note("def_trial", u.json(.{
            .{ "name", name },                         .{ "version", v },                     .{ "kind", cand.kind },
            .{ "source", cand.source },                .{ "description", cand.description }, .{ "tests", cand.tests },
            .{ "input_schema", cand.input_schema },    .{ "origin", origin },                 .{ "names", cand.names },
        }));
        if (u.eql(cand.kind, "note")) return .{ .ok = u.fmt("trial v{d}: note `{s}` is live; kept after {d} tasks that do not fail.", .{ v, name, self.cfg.soak }) };
        return .{ .ok = u.fmt("trial v{d}: compiled in a scratch child and passed {d} test(s); `{s}` is live now (every caller uses it from the next call) and is kept after {d} clean uses (any error reverts it).", .{ v, passed, name, self.cfg.soak }) };
    }

    fn reject(self: *Kernel, name: []const u8, version: u32, err: []const u8) tools.Result {
        self.note("def_rejected", u.json(.{ .{ "name", name }, .{ "version", version }, .{ "error", err } }));
        u.trace("[gate] {s} v{d} rejected: {s}", .{ name, version, u.firstLine(err, 120) });
        return .{ .err = u.fmt("rejected: {s}\nThe live version is unchanged.", .{err}) };
    }

    /// Records one use of a live definition and applies the soak rules.
    /// Returns a note for the model when the use reverted the trial.
    fn recordUse(self: *Kernel, name: []const u8, version: u32, err: ?[]const u8, input: Value) ?[]const u8 {
        self.note("def_use", u.json(.{ .{ "name", name }, .{ "version", version }, .{ "ok", err == null }, .{ "error", err } }));
        const def = self.state.defs.get(name) orelse return null;
        const live = def.liveVersion() orelse return null;
        if (live.version != version or !u.eql(live.status, "trial")) return null;
        if (err) |e| {
            const restored = def.lastKept();
            const verdict = u.fmt("def {s} v{d} (\"{s}\") reverted: {s} on input {s}", .{ name, version, live.description, u.firstLine(e, 200), u.clip(u.stringify(input), 200) });
            self.note("def_reverted", u.json(.{ .{ "name", name }, .{ "version", version }, .{ "restored", restored }, .{ "error", e }, .{ "verdict", verdict } }));
            const now = if (restored) |r| u.fmt("live: v{d}", .{r}) else "definition removed";
            u.trace("[gate] {s} v{d} reverted: {s} ({s})", .{ name, version, u.firstLine(e, 100), now });
            return u.fmt("trial v{d} of `{s}` was reverted by this error ({s}); the verdict is recorded.", .{ version, name, now });
        } else if (def.soak_uses >= self.cfg.soak) {
            const verdict = u.fmt("def {s} v{d} (\"{s}\") kept after {d} clean uses", .{ name, version, live.description, def.soak_uses });
            self.note("def_kept", u.json(.{ .{ "name", name }, .{ "version", version }, .{ "verdict", verdict } }));
            u.trace("[gate] {s} v{d} kept", .{ name, version });
        }
        return null;
    }

    /// Late binding: resolves `name` (and everything it calls) in the
    /// registry on every call.
    pub fn callDef(self: *Kernel, name: []const u8, input: Value) tools.Result {
        const live = self.state.live(name) orelse return .{ .err = u.fmt("no live definition `{s}`", .{name}) };
        if (u.eql(live.kind, "note")) return .{ .err = u.fmt("`{s}` is a note, not code", .{name}) };
        const version = live.version;
        const r = self.roc.call(&self.state, name, live, input);
        const err: ?[]const u8 = switch (r) {
            .ok => null,
            .err => |e| e,
        };
        const n = self.recordUse(name, version, err, input);
        return switch (r) {
            .ok => |s| .{ .ok = s },
            .err => |e| .{ .err = if (n) |x| u.fmt("{s}\n{s}", .{ e, x }) else e },
        };
    }

    // -------------------------------------------------------- prompting

    fn activeLessons(self: *Kernel) []st.Lesson {
        var out: std.ArrayList(st.Lesson) = .empty;
        for (self.state.lessons.items) |l| if (!l.retired) out.append(u.A, l) catch @panic("oom");
        return out.items;
    }

    pub fn selectLessons(self: *Kernel, task: []const u8) []i64 {
        const active = self.activeLessons();
        var ls: std.ArrayList(Value) = .empty;
        for (active) |l| ls.append(u.A, u.json(.{ .{ "id", l.id }, .{ "text", l.text }, .{ "score", l.score } })) catch @panic("oom");
        var ids: []i64 = &.{};
        switch (self.callDef("select_lessons", u.json(.{ .{ "task", task }, .{ "lessons", ls.items } }))) {
            .ok => |out| {
                var list: std.ArrayList(i64) = .empty;
                const v = u.parseJson(out) catch Value.null;
                if (v == .array) for (v.array.items) |x| if (x == .integer) list.append(u.A, x.integer) catch {};
                ids = list.items;
            },
            .err => |e| {
                u.trace("[hook] select_lessons failed, using default: {s}", .{u.firstLine(e, 100)});
                ids = defaultSelect(task, active);
            },
        }
        var seen: std.ArrayList(i64) = .empty;
        for (ids) |id| {
            var ok = false;
            for (active) |l| ok = ok or l.id == id;
            for (seen.items) |s| ok = ok and s != id;
            if (ok) seen.append(u.A, id) catch @panic("oom");
        }
        return seen.items;
    }

    /// System blocks: the immutable base prompt, the Roc reference, then the
    /// supplemental prompt built by `compose_system` plus the verdicts
    /// (which bypass `select_lessons` and cannot be dropped by a hook).
    pub fn systemPrompt(self: *Kernel, selected: []const i64) []const []const u8 {
        var notes: std.ArrayList([]const u8) = .empty;
        for (self.state.sortedNames()) |n| {
            const v = self.state.live(n) orelse continue;
            if (u.eql(v.kind, "note")) notes.append(u.A, v.source) catch @panic("oom");
        }
        var lessons: std.ArrayList([]const u8) = .empty;
        for (selected) |id| if (self.state.lesson(id)) |l| lessons.append(u.A, l.text) catch @panic("oom");
        var sup: std.ArrayList(u8) = .empty;
        switch (self.callDef("compose_system", u.json(.{ .{ "notes", notes.items }, .{ "lessons", lessons.items } }))) {
            .ok => |s| sup.appendSlice(u.A, s) catch @panic("oom"),
            .err => |e| {
                u.trace("[hook] compose_system failed, using default: {s}", .{u.firstLine(e, 100)});
                sup.appendSlice(u.A, defaultCompose(notes.items, lessons.items)) catch @panic("oom");
            },
        }
        const vs = self.state.verdicts.items;
        if (vs.len > 0) {
            sup.appendSlice(u.A, "\n\n## Verdicts on past userspace changes\n") catch @panic("oom");
            for (vs[vs.len - @min(vs.len, 20) ..]) |v| sup.appendSlice(u.A, u.fmt("- {s}\n", .{v.text})) catch @panic("oom");
        }
        return u.A.dupe([]const u8, &.{ self.base_prompt, std.mem.trim(u8, ROC_REFERENCE, " \n"), std.mem.trim(u8, sup.items, " \n") }) catch @panic("oom");
    }

    pub fn toolList(self: *Kernel) []Value {
        var list: std.ArrayList(Value) = .empty;
        list.appendSlice(u.A, self.kernel_schemas) catch @panic("oom");
        for (self.state.sortedNames()) |n| {
            const v = self.state.live(n) orelse continue;
            if (!u.eql(v.kind, "tool")) continue;
            list.append(u.A, u.json(.{ .{ "name", n }, .{ "description", v.description }, .{ "input_schema", v.input_schema } })) catch @panic("oom");
        }
        return list.items;
    }

    // ------------------------------------------------------------ tools

    fn execTool(self: *Kernel, name: []const u8, input: Value) tools.Result {
        const wd = self.cfg.work_dir;
        if (u.eql(name, "bash")) return tools.bash(wd, input);
        if (u.eql(name, "read_file")) return tools.readFile(wd, input);
        if (u.eql(name, "write_file")) return tools.writeFile(wd, input, self.protected);
        if (u.eql(name, "list_defs")) return .{ .ok = self.defsTable() };
        if (u.eql(name, "show_def")) return self.showDef(u.str(input, "name"), if (u.int(input, "version")) |x| @intCast(x) else null);
        if (u.eql(name, "roc_eval")) return self.roc.scratch(&self.state, u.str(input, "source"), u.str(input, "expr"));
        if (u.eql(name, "propose_def")) return self.proposeDef(input);
        if (u.eql(name, "rollback_def")) {
            const v = u.int(input, "version") orelse return .{ .err = "`version` (integer) is required" };
            return self.rollbackDef(u.str(input, "name"), @intCast(v));
        }
        if (u.eql(name, "remember")) return self.remember(u.str(input, "text"), "remember");
        if (u.eql(name, "recall")) return .{ .ok = self.recall(u.str(input, "query")) };
        if (self.state.live(name)) |v| if (u.eql(v.kind, "tool")) {
            return switch (self.callDef(name, input)) {
                .ok => |s| .{ .ok = u.truncate(s, 20 * 1024) },
                .err => |e| .{ .err = e },
            };
        };
        return .{ .err = u.fmt("unknown tool `{s}`", .{name}) };
    }

    pub fn defsTable(self: *Kernel) []const u8 {
        var out: std.ArrayList(u8) = .empty;
        for (self.state.sortedNames()) |n| {
            const def = self.state.defs.get(n).?;
            var line: []const u8 = undefined;
            if (def.liveVersion()) |v| {
                const soak = if (u.eql(v.status, "trial")) u.fmt(" ({d}/{d} clean uses)", .{ def.soak_uses, self.cfg.soak }) else "";
                line = u.fmt("{s:<16} {s:<5} v{d:<3} {s}{s}  {s}\n", .{ n, v.kind, v.version, v.status, soak, v.description });
            } else {
                const kind = if (def.versions.items.len > 0) def.versions.items[def.versions.items.len - 1].kind else "";
                line = u.fmt("{s:<16} {s:<5} -    removed\n", .{ n, kind });
            }
            out.appendSlice(u.A, line) catch @panic("oom");
        }
        return out.items;
    }

    pub fn showDef(self: *Kernel, name: []const u8, version: ?u32) tools.Result {
        const def = self.state.defs.get(name) orelse return .{ .err = u.fmt("no definition named `{s}`", .{name}) };
        const v = if (version) |x| def.get(x) else (def.liveVersion() orelse if (def.versions.items.len > 0) &def.versions.items[def.versions.items.len - 1] else null);
        const dv = v orelse return .{ .err = u.fmt("`{s}` has no such version", .{name}) };
        var all: std.ArrayList([]const u8) = .empty;
        for (def.versions.items) |x| all.append(u.A, u.fmt("v{d} {s}", .{ x.version, x.status })) catch @panic("oom");
        return .{ .ok = u.pretty(u.json(.{
            .{ "name", name },                 .{ "version", dv.version },          .{ "kind", dv.kind },
            .{ "status", dv.status },          .{ "live", def.live == dv.version }, .{ "description", dv.description },
            .{ "source", dv.source },          .{ "tests", dv.tests },              .{ "input_schema", dv.input_schema },
            .{ "all_versions", all.items },
        })) };
    }

    pub fn remember(self: *Kernel, text_in: []const u8, origin: []const u8) tools.Result {
        const text = std.mem.trim(u8, text_in, " \t\r\n");
        if (text.len == 0) return .{ .err = "`text` is required" };
        if (self.state.hasLessonText(text)) return .{ .ok = "already known" };
        const id = self.state.nextLessonId();
        _ = self.record("lesson_added", u.json(.{ .{ "id", id }, .{ "text", text }, .{ "origin", origin } })) catch |e| return .{ .err = @errorName(e) };
        return .{ .ok = u.fmt("lesson #{d} recorded", .{id}) };
    }

    pub fn recall(self: *Kernel, query: []const u8) []const u8 {
        const q = u.words(query, 3);
        const Hit = struct { score: i64, text: []const u8 };
        var hits: std.ArrayList(Hit) = .empty;
        for (self.activeLessons()) |l| if (u.sharesWord(u.words(l.text, 3), q))
            hits.append(u.A, .{ .score = l.score, .text = u.fmt("lesson #{d} (score {d}): {s}", .{ l.id, l.score, l.text }) }) catch @panic("oom");
        for (self.state.verdicts.items) |v| if (u.sharesWord(u.words(v.text, 3), q))
            hits.append(u.A, .{ .score = 0, .text = u.fmt("verdict: {s}", .{v.text}) }) catch @panic("oom");
        std.mem.sort(Hit, hits.items, {}, struct {
            fn gt(_: void, a: Hit, b: Hit) bool {
                return a.score > b.score;
            }
        }.gt);
        if (hits.items.len == 0) return "nothing relevant";
        var lines: std.ArrayList([]const u8) = .empty;
        for (hits.items) |h| lines.append(u.A, h.text) catch @panic("oom");
        return std.mem.join(u.A, "\n", lines.items) catch @panic("oom");
    }

    pub fn knowledgeText(self: *Kernel) []const u8 {
        var out: std.ArrayList(u8) = .empty;
        out.appendSlice(u.A, "Lessons:\n") catch @panic("oom");
        for (self.state.lessons.items) |l|
            out.appendSlice(u.A, u.fmt("  #{s:<3} score {s:>3}{s} ({s}) {s}\n", .{ u.fmt("{d}", .{l.id}), u.fmt("{d}", .{l.score}), if (l.retired) " [retired]" else "", l.origin, l.text })) catch @panic("oom");
        out.appendSlice(u.A, "Verdicts:\n") catch @panic("oom");
        for (self.state.verdicts.items) |v| out.appendSlice(u.A, u.fmt("  {s}\n", .{v.text})) catch @panic("oom");
        return out.items;
    }

    // ------------------------------------------------------- agent loop

    const Turns = struct {
        answer: []const u8 = "",
        stop: []const u8 = "",
        kernel_error: ?[]const u8 = null,
        tool_errors: std.ArrayList(bool) = .empty,
    };

    pub fn runTask(self: *Kernel, task: []const u8) TaskOutcome {
        const task_id: i64 = @intCast(self.state.tasks.items.len + 1);
        self.note("task_started", u.json(.{ .{ "task_id", task_id }, .{ "task", task } }));
        const selected = self.selectLessons(task);
        var transcript: std.ArrayList([]const u8) = .empty;
        transcript.append(u.A, u.fmt("TASK: {s}", .{task})) catch @panic("oom");
        var turns = self.runTurns(task, selected, &transcript);
        var tail_ok = true;
        const te = turns.tool_errors.items;
        for (te[te.len - @min(te.len, 2) ..]) |e| tail_ok = tail_ok and !e;
        const success = turns.kernel_error == null and u.eql(turns.stop, "end_turn") and tail_ok;
        self.soakTrialNotes(task, task_id, success);
        self.scoreLessons(selected, task_id, success);
        self.note("task_finished", u.json(.{ .{ "task_id", task_id }, .{ "success", success }, .{ "error", turns.kernel_error }, .{ "summary", u.firstLine(turns.answer, 200) } }));
        u.trace("[task] {d} {s}", .{ task_id, if (success) "succeeded" else "failed" });
        transcript.append(u.A, u.fmt("OUTCOME: {s}", .{if (success) "success" else "failure"})) catch @panic("oom");
        if (self.cfg.reflect) self.reflect(std.mem.join(u.A, "\n", transcript.items) catch @panic("oom"));
        _ = &turns;
        return .{ .success = success, .answer = turns.answer, .err = turns.kernel_error };
    }

    fn runTurns(self: *Kernel, task: []const u8, selected: []const i64, transcript: *std.ArrayList([]const u8)) Turns {
        var messages: std.ArrayList(Value) = .empty;
        messages.append(u.A, u.json(.{ .{ "role", "user" }, .{ "content", task } })) catch @panic("oom");
        var turns = Turns{};
        var turn: usize = 0;
        while (turn < self.cfg.max_turns) : (turn += 1) {
            // Recomposed every turn: each compose_system call is a use of the hook.
            const system = self.systemPrompt(selected);
            const tool_list = self.toolList();
            const resp = switch (self.provider.call(.{ .system = system, .tools = tool_list, .messages = messages.items })) {
                .ok => |r| r,
                .err => |e| {
                    u.trace("[kernel] model request failed: {s}", .{u.firstLine(e, 200)});
                    turns.kernel_error = e;
                    break;
                },
            };
            var texts: std.ArrayList([]const u8) = .empty;
            for (resp.content) |b| if (u.eql(u.str(b, "type"), "text")) texts.append(u.A, u.str(b, "text")) catch @panic("oom");
            const text = std.mem.trim(u8, std.mem.join(u.A, "\n", texts.items) catch @panic("oom"), " \t\r\n");
            if (text.len > 0) {
                if (self.cfg.echo_text) {
                    writeStdout(u.fmt("{s}\n", .{text}));
                } else u.trace("[assistant] {s}", .{u.firstLine(text, 160)});
                transcript.append(u.A, u.fmt("ASSISTANT: {s}", .{u.truncate(text, 400)})) catch @panic("oom");
                turns.answer = text;
            }
            turns.stop = resp.stop_reason;
            messages.append(u.A, u.json(.{ .{ "role", "assistant" }, .{ "content", resp.content } })) catch @panic("oom");
            if (!u.eql(turns.stop, "tool_use")) break;
            const results = self.runToolCalls(resp.content, transcript, &turns.tool_errors);
            if (results.len == 0) {
                turns.kernel_error = "stop_reason tool_use without tool_use blocks";
                break;
            }
            messages.append(u.A, u.json(.{ .{ "role", "user" }, .{ "content", results } })) catch @panic("oom");
            if (turn + 1 == self.cfg.max_turns) turns.kernel_error = u.fmt("turn limit ({d}) reached", .{self.cfg.max_turns});
        }
        return turns;
    }

    fn runToolCalls(self: *Kernel, content: []Value, transcript: *std.ArrayList([]const u8), tool_errors: *std.ArrayList(bool)) []Value {
        var results: std.ArrayList(Value) = .empty;
        for (content) |b| {
            if (!u.eql(u.str(b, "type"), "tool_use")) continue;
            const name = u.str(b, "name");
            const input = u.get(b, "input") orelse u.json(.{});
            const r = self.execTool(name, input);
            const out, const is_err = switch (r) {
                .ok => |s| .{ s, false },
                .err => |e| .{ u.fmt("error: {s}", .{e}), true },
            };
            u.trace("{s}", .{toolTrace(name, input, r)});
            transcript.append(u.A, u.fmt("TOOL {s} {s} -> {s}{s}", .{ name, u.clip(u.stringify(input), 150), if (is_err) "ERROR " else "", u.firstLine(out, 150) })) catch @panic("oom");
            tool_errors.append(u.A, is_err) catch @panic("oom");
            results.append(u.A, u.json(.{ .{ "type", "tool_result" }, .{ "tool_use_id", u.get(b, "id") orelse Value.null }, .{ "content", out }, .{ "is_error", is_err } })) catch @panic("oom");
        }
        return results.items;
    }

    fn soakTrialNotes(self: *Kernel, task: []const u8, task_id: i64, success: bool) void {
        for (self.state.sortedNames()) |n| {
            const v = self.state.live(n) orelse continue;
            if (!u.eql(v.kind, "note") or !u.eql(v.status, "trial")) continue;
            _ = self.recordUse(n, v.version, if (success) null else u.fmt("task {d} failed", .{task_id}), u.json(.{.{ "task", task }}));
        }
    }

    fn scoreLessons(self: *Kernel, selected: []const i64, task_id: i64, success: bool) void {
        const delta: i64 = if (success) 1 else -1;
        for (selected) |id| {
            const l = self.state.lesson(id) orelse continue;
            const score = l.score + delta;
            self.note("lesson_scored", u.json(.{ .{ "id", id }, .{ "delta", delta }, .{ "score", score }, .{ "task_id", task_id } }));
            if (score <= -3) {
                self.note("lesson_retired", u.json(.{.{ "id", id }}));
                u.trace("[learn] lesson #{d} retired (score {d})", .{ id, score });
            }
        }
    }

    fn reflect(self: *Kernel, transcript: []const u8) void {
        const prompt = u.fmt("Reflect on this finished task.\n\n{s}\n\nReply with only a JSON array of 0 to 3 short, reusable lessons " ++
            "(strings) that would help with future tasks. Skip task-specific trivia; reply [] if there is nothing durable.", .{u.truncate(transcript, 12_000)});
        const messages = [_]Value{u.json(.{ .{ "role", "user" }, .{ "content", prompt } })};
        const system = [_][]const u8{self.base_prompt};
        const resp = switch (self.provider.call(.{ .system = &system, .tools = &.{}, .messages = &messages })) {
            .ok => |r| r,
            .err => |e| {
                u.trace("[reflect] failed: {s}", .{u.firstLine(e, 160)});
                return;
            },
        };
        var text: std.ArrayList(u8) = .empty;
        for (resp.content) |b| text.appendSlice(u.A, u.str(b, "text")) catch @panic("oom");
        const t = text.items;
        const a = std.mem.indexOfScalar(u8, t, '[');
        const z = std.mem.lastIndexOfScalar(u8, t, ']');
        if (a == null or z == null or z.? < a.?) {
            u.trace("[reflect] no JSON array in reply", .{});
            return;
        }
        const v = u.parseJson(t[a.? .. z.? + 1]) catch {
            u.trace("[reflect] no JSON array in reply", .{});
            return;
        };
        if (v != .array) return;
        var added: usize = 0;
        for (v.array.items[0..@min(v.array.items.len, 3)]) |l| {
            if (l != .string) continue;
            switch (self.remember(l.string, "reflection")) {
                .ok => |m| if (std.mem.startsWith(u8, m, "lesson")) {
                    added += 1;
                },
                .err => {},
            }
        }
        u.trace("[reflect] +{d} lesson(s)", .{added});
    }

    pub fn refine(self: *Kernel) TaskOutcome {
        var lines: std.ArrayList([]const u8) = .empty;
        const ts = self.state.tasks.items;
        for (ts[ts.len - @min(ts.len, 10) ..]) |t| {
            const outcome = if (t.success) |s| (if (s) "success" else "failure") else "unfinished";
            lines.append(u.A, u.fmt("- task {d} [{s}]: {s} => {s}", .{ t.id, outcome, u.firstLine(t.task, 120), t.summary })) catch @panic("oom");
        }
        const recent = if (lines.items.len == 0) "(no tasks yet)" else std.mem.join(u.A, "\n", lines.items) catch @panic("oom");
        return self.runTask(u.fmt("{s}\n\nRecent task outcomes:\n{s}", .{ REFINE_PROMPT, recent }));
    }
};

pub fn writeStdout(s: []const u8) void {
    var rest = s;
    while (rest.len > 0) {
        const n = std.c.write(1, rest.ptr, rest.len);
        if (n <= 0) return;
        rest = rest[@intCast(n)..];
    }
}

fn toolTrace(name: []const u8, input: Value, r: tools.Result) []const u8 {
    const arg = if (u.eql(name, "bash"))
        u.firstLine(u.str(input, "command"), 60)
    else if (u.eql(name, "propose_def") or u.eql(name, "show_def") or u.eql(name, "rollback_def"))
        (if (u.int(input, "version")) |v| u.fmt("{s} {d}", .{ u.str(input, "name"), v }) else u.str(input, "name"))
    else if (u.eql(name, "read_file") or u.eql(name, "write_file"))
        u.str(input, "path")
    else if (u.eql(name, "roc_eval"))
        u.firstLine(u.str(input, "expr"), 60)
    else if (u.eql(name, "remember"))
        u.firstLine(u.str(input, "text"), 60)
    else if (u.eql(name, "recall"))
        u.firstLine(u.str(input, "query"), 60)
    else
        u.clip(u.stringify(input), 60);
    const res = switch (r) {
        .ok => |s| if (u.eql(name, "propose_def") or u.eql(name, "rollback_def"))
            (if (std.mem.indexOfScalar(u8, s, ':')) |c| s[0..c] else s)
        else if (u.eql(name, "bash")) "exit 0" else u.firstLine(s, 60),
        .err => |e| if (u.eql(name, "propose_def") or u.eql(name, "rollback_def")) "rejected" else u.fmt("error: {s}", .{u.firstLine(e, 80)}),
    };
    return u.fmt("[tool] {s} {s} -> {s}", .{ name, arg, res });
}

pub fn defaultCompose(notes: []const []const u8, lessons: []const []const u8) []const u8 {
    var out: std.ArrayList(u8) = .empty;
    if (notes.len > 0) {
        out.appendSlice(u.A, "## Notes\n") catch @panic("oom");
        for (notes) |n| out.appendSlice(u.A, u.fmt("{s}\n\n", .{std.mem.trim(u8, n, " \n")})) catch @panic("oom");
    }
    if (lessons.len > 0) {
        out.appendSlice(u.A, "## Lessons\n") catch @panic("oom");
        for (lessons) |l| out.appendSlice(u.A, u.fmt("- {s}\n", .{std.mem.trim(u8, l, " \n")})) catch @panic("oom");
    }
    return std.mem.trimEnd(u8, out.items, " \n");
}

fn defaultSelect(task: []const u8, active: []st.Lesson) []i64 {
    const tw = u.words(task, 4);
    const sorted = u.A.dupe(st.Lesson, active) catch @panic("oom");
    std.mem.sort(st.Lesson, sorted, {}, struct {
        fn lt(_: void, a: st.Lesson, b: st.Lesson) bool {
            return a.score > b.score or (a.score == b.score and a.id < b.id);
        }
    }.lt);
    var hits: std.ArrayList(i64) = .empty;
    for (sorted) |l| if (hits.items.len < 8 and u.sharesWord(u.words(l.text, 4), tw)) hits.append(u.A, l.id) catch @panic("oom");
    if (hits.items.len > 0) return hits.items;
    for (sorted[0..@min(sorted.len, 3)]) |l| hits.append(u.A, l.id) catch @panic("oom");
    return hits.items;
}
