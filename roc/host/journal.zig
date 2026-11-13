//! Append-only `journal.jsonl`: the single source of truth.

const std = @import("std");
const u = @import("util.zig");
const Value = u.Value;

pub const Journal = struct {
    fd: std.c.fd_t,
    path: [:0]const u8,

    pub fn open(state_dir: []const u8) !Journal {
        try mkdirs(state_dir);
        const path = try std.fmt.allocPrintSentinel(u.A, "{s}/journal.jsonl", .{state_dir}, 0);
        const fd = std.c.open(path, .{ .ACCMODE = .RDWR, .CREAT = true, .APPEND = true, .CLOEXEC = true }, @as(std.c.mode_t, 0o644));
        if (fd < 0) return error.JournalOpenFailed;
        return .{ .fd = fd, .path = path };
    }

    /// Every event in order. A torn or unreadable line is skipped.
    pub fn readAll(self: Journal) ![]Value {
        const text = try readWhole(self.path);
        var out: std.ArrayList(Value) = .empty;
        var it = std.mem.splitScalar(u8, text, '\n');
        var line_no: usize = 0;
        while (it.next()) |line| {
            line_no += 1;
            if (std.mem.trim(u8, line, " \r\t").len == 0) continue;
            const v = u.parseJson(line) catch {
                u.trace("[journal] skipping unreadable line {d}", .{line_no});
                continue;
            };
            try out.append(u.A, v);
        }
        return out.items;
    }

    /// Stamps `type` and `ts` first, appends one line, returns the event.
    pub fn append(self: Journal, kind: []const u8, fields: Value) !Value {
        var ev = u.json(.{ .{ "type", kind }, .{ "ts", u.nowIso() } });
        if (fields == .object) {
            var it = fields.object.iterator();
            while (it.next()) |e| try ev.object.put(u.A, e.key_ptr.*, e.value_ptr.*);
        }
        const line = u.fmt("{s}\n", .{u.stringify(ev)});
        var rest = line;
        while (rest.len > 0) {
            const n = std.c.write(self.fd, rest.ptr, rest.len);
            if (n <= 0) return error.JournalWriteFailed;
            rest = rest[@intCast(n)..];
        }
        return ev;
    }
};

pub fn readWhole(path: [:0]const u8) ![]u8 {
    const fd = std.c.open(path, .{ .ACCMODE = .RDONLY, .CLOEXEC = true }, @as(std.c.mode_t, 0));
    if (fd < 0) return error.FileNotFound;
    defer _ = std.c.close(fd);
    var out: std.ArrayList(u8) = .empty;
    var buf: [65536]u8 = undefined;
    while (true) {
        const n = std.c.read(fd, &buf, buf.len);
        if (n < 0) return error.ReadFailed;
        if (n == 0) break;
        try out.appendSlice(u.A, buf[0..@intCast(n)]);
    }
    return out.items;
}

/// `mkdir -p`.
pub fn mkdirs(path: []const u8) !void {
    var i: usize = 1;
    while (i <= path.len) : (i += 1) {
        if (i == path.len or path[i] == '/') {
            const sub = try u.A.dupeSentinel(u8, path[0..i], 0);
            if (std.c.mkdir(sub, 0o755) != 0) {
                const d = std.c.opendir(sub) orelse return error.MkdirFailed;
                _ = std.c.closedir(d);
            }
        }
    }
}
