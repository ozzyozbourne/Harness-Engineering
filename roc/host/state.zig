//! Registry and knowledge, rebuilt purely by applying journal events.
//!
//! Live operations append an event and then apply it through the same
//! `State.apply`, so replay reproduces exactly the state the process had.

const std = @import("std");
const u = @import("util.zig");
const Value = u.Value;

pub const DefVersion = struct {
    version: u32,
    kind: []const u8,
    source: []const u8,
    description: []const u8,
    tests: Value,
    input_schema: Value,
    status: []const u8,
    /// Top-level names the source defines (from the gate), for the
    /// dependency scan and name-collision checks.
    names: []const []const u8,
};

pub const Def = struct {
    name: []const u8,
    versions: std.ArrayList(DefVersion) = .empty,
    live: ?u32 = null,
    /// Clean uses counted for the live version while it is `trial`.
    soak_uses: u32 = 0,

    pub fn get(self: *const Def, version: u32) ?*DefVersion {
        for (self.versions.items) |*v| if (v.version == version) return v;
        return null;
    }
    pub fn liveVersion(self: *const Def) ?*DefVersion {
        return self.get(self.live orelse return null);
    }
    /// The version a failing trial reverts to: the most recent `kept` one.
    pub fn lastKept(self: *const Def) ?u32 {
        var i = self.versions.items.len;
        while (i > 0) {
            i -= 1;
            if (u.eql(self.versions.items[i].status, "kept")) return self.versions.items[i].version;
        }
        return null;
    }
    pub fn nextVersion(self: *const Def) u32 {
        var m: u32 = 0;
        for (self.versions.items) |v| m = @max(m, v.version);
        return m + 1;
    }
};

pub const Lesson = struct { id: i64, text: []const u8, score: i64, retired: bool, origin: []const u8 };
pub const Verdict = struct { name: []const u8, version: u32, outcome: []const u8, text: []const u8 };
pub const TaskRecord = struct { id: i64, task: []const u8, success: ?bool, summary: []const u8 };

pub const State = struct {
    defs: std.StringArrayHashMapUnmanaged(*Def) = .empty,
    lessons: std.ArrayList(Lesson) = .empty,
    verdicts: std.ArrayList(Verdict) = .empty,
    tasks: std.ArrayList(TaskRecord) = .empty,

    pub fn apply(self: *State, ev: Value) void {
        self.applyInner(ev) catch @panic("out of memory");
    }

    fn applyInner(self: *State, ev: Value) !void {
        const name = u.str(ev, "name");
        const t = u.str(ev, "type");
        if (u.eql(t, "def_trial")) {
            const gop = try self.defs.getOrPut(u.A, name);
            if (!gop.found_existing) {
                const d = try u.A.create(Def);
                d.* = .{ .name = name };
                gop.value_ptr.* = d;
            }
            const def = gop.value_ptr.*;
            const version: u32 = @intCast(u.int(ev, "version") orelse 0);
            var names: std.ArrayList([]const u8) = .empty;
            for (u.arr(ev, "names")) |n| if (n == .string) try names.append(u.A, n.string);
            try def.versions.append(u.A, .{
                .version = version,
                .kind = u.str(ev, "kind"),
                .source = u.str(ev, "source"),
                .description = u.str(ev, "description"),
                .tests = u.get(ev, "tests") orelse u.json(.{}),
                .input_schema = u.get(ev, "input_schema") orelse .null,
                .status = "trial",
                .names = names.items,
            });
            def.live = version;
            def.soak_uses = 0;
        } else if (u.eql(t, "def_use")) {
            const def = self.defs.get(name) orelse return;
            const ok = u.boolean(ev, "ok") orelse false;
            const lv = def.liveVersion() orelse return;
            if (ok and u.eql(lv.status, "trial") and lv.version == @as(u32, @intCast(u.int(ev, "version") orelse 0)))
                def.soak_uses += 1;
        } else if (u.eql(t, "def_kept")) {
            try self.settle(name, ev, "kept");
        } else if (u.eql(t, "def_reverted")) {
            try self.settle(name, ev, "reverted");
        } else if (u.eql(t, "lesson_added")) {
            try self.lessons.append(u.A, .{ .id = u.int(ev, "id") orelse 0, .text = u.str(ev, "text"), .score = 0, .retired = false, .origin = u.str(ev, "origin") });
        } else if (u.eql(t, "lesson_scored")) {
            if (self.lesson(u.int(ev, "id") orelse 0)) |l| l.score = u.int(ev, "score") orelse l.score;
        } else if (u.eql(t, "lesson_retired")) {
            if (self.lesson(u.int(ev, "id") orelse 0)) |l| l.retired = true;
        } else if (u.eql(t, "task_started")) {
            try self.tasks.append(u.A, .{ .id = u.int(ev, "task_id") orelse 0, .task = u.str(ev, "task"), .success = null, .summary = "" });
        } else if (u.eql(t, "task_finished")) {
            const id = u.int(ev, "task_id") orelse 0;
            for (self.tasks.items) |*task| if (task.id == id) {
                task.success = u.boolean(ev, "success");
                task.summary = u.str(ev, "summary");
            };
        }
        // def_proposed / def_rejected are history only.
    }

    /// Ends a soak: the version gets `status`, a revert makes `restored`
    /// live (none if missing), and the verdict is kept.
    fn settle(self: *State, name: []const u8, ev: Value, status: []const u8) !void {
        if (self.defs.get(name)) |def| {
            if (def.get(@intCast(u.int(ev, "version") orelse 0))) |v| v.status = status;
            if (u.eql(status, "reverted")) {
                def.live = if (u.int(ev, "restored")) |r| @intCast(r) else null;
            }
            def.soak_uses = 0;
        }
        if (u.boolean(ev, "seed") orelse false) return;
        try self.verdicts.append(u.A, .{ .name = name, .version = @intCast(u.int(ev, "version") orelse 0), .outcome = status, .text = u.str(ev, "verdict") });
    }

    pub fn lesson(self: *State, id: i64) ?*Lesson {
        for (self.lessons.items) |*l| if (l.id == id) return l;
        return null;
    }

    pub fn live(self: *const State, name: []const u8) ?*DefVersion {
        const def = self.defs.get(name) orelse return null;
        return def.liveVersion();
    }

    /// Definition names in alphabetical order.
    pub fn sortedNames(self: *const State) [][]const u8 {
        const names = u.A.dupe([]const u8, self.defs.keys()) catch @panic("oom");
        std.mem.sort([]const u8, names, {}, struct {
            fn lt(_: void, a: []const u8, b: []const u8) bool {
                return std.mem.lessThan(u8, a, b);
            }
        }.lt);
        return names;
    }

    pub fn nextLessonId(self: *const State) i64 {
        var m: i64 = 0;
        for (self.lessons.items) |l| m = @max(m, l.id);
        return m + 1;
    }

    pub fn hasLessonText(self: *const State, text: []const u8) bool {
        const t = std.mem.trim(u8, text, " \t\r\n");
        for (self.lessons.items) |l| {
            const s = std.mem.trim(u8, l.text, " \t\r\n");
            if (std.ascii.eqlIgnoreCase(s, t)) return true;
        }
        return false;
    }
};
