//! Small shared helpers: the process allocator, JSON values, text clipping,
//! timestamps and the stderr trace.

const std = @import("std");
pub const Value = std.json.Value;
pub const ObjectMap = std.json.ObjectMap;

/// One arena for the whole process: `cmh` is a CLI that runs a task (or a
/// short repl session) and exits.
var arena_state = std.heap.ArenaAllocator.init(std.heap.page_allocator);
pub const A = arena_state.allocator();

pub fn fmt(comptime f: []const u8, args: anytype) []const u8 {
    return std.fmt.allocPrint(A, f, args) catch @panic("out of memory");
}

pub fn trace(comptime f: []const u8, args: anytype) void {
    std.debug.print(f ++ "\n", args);
}

pub fn eql(a: []const u8, b: []const u8) bool {
    return std.mem.eql(u8, a, b);
}

// ------------------------------------------------------------------- JSON

pub fn parseJson(text: []const u8) !Value {
    return std.json.parseFromSliceLeaky(Value, A, text, .{ .duplicate_field_behavior = .use_last });
}

pub fn stringify(v: Value) []const u8 {
    return std.json.Stringify.valueAlloc(A, v, .{}) catch @panic("out of memory");
}

pub fn pretty(v: Value) []const u8 {
    return std.json.Stringify.valueAlloc(A, v, .{ .whitespace = .indent_2 }) catch @panic("out of memory");
}

/// Builds a JSON value from a Zig value: strings, ints, bools, optionals,
/// `Value`s, slices of those, and tuples of `.{ "key", value }` pairs
/// (an object).
pub fn json(x: anytype) Value {
    const T = @TypeOf(x);
    if (T == Value) return x;
    switch (@typeInfo(T)) {
        .optional => return if (x) |v| json(v) else .null,
        .null => return .null,
        .bool => return .{ .bool = x },
        .int, .comptime_int => return .{ .integer = @intCast(x) },
        .float, .comptime_float => return .{ .float = x },
        .pointer => |p| {
            if (p.size == .one) {
                switch (@typeInfo(p.child)) {
                    .array => |a| if (a.child == u8) return .{ .string = x },
                    else => {},
                }
                return json(x.*);
            }
            if (p.child == u8) return .{ .string = x };
            var list = std.json.Array.init(A);
            for (x) |item| list.append(json(item)) catch @panic("out of memory");
            return .{ .array = list };
        },
        .array => |a| {
            if (a.child == u8) return .{ .string = A.dupe(u8, &x) catch @panic("out of memory") };
            return json(A.dupe(a.child, &x) catch @panic("out of memory"));
        },
        .@"struct" => |s| {
            if (s.field_names.len == 0) return .{ .object = .empty };
            if (!s.is_tuple) @compileError("json(): use a tuple of pairs for objects");
            var m: ObjectMap = .empty;
            inline for (x) |pair| m.put(A, pair[0], json(pair[1])) catch @panic("out of memory");
            return .{ .object = m };
        },
        else => @compileError("json(): unsupported type " ++ @typeName(T)),
    }
}

pub fn get(v: Value, key: []const u8) ?Value {
    return switch (v) {
        .object => |o| o.get(key),
        else => null,
    };
}

pub fn str(v: Value, key: []const u8) []const u8 {
    const f = get(v, key) orelse return "";
    return switch (f) {
        .string => |s| s,
        else => "",
    };
}

pub fn int(v: Value, key: []const u8) ?i64 {
    const f = get(v, key) orelse return null;
    return switch (f) {
        .integer => |i| i,
        .float => |x| @intFromFloat(x),
        .number_string => |s| std.fmt.parseInt(i64, s, 10) catch null,
        else => null,
    };
}

pub fn boolean(v: Value, key: []const u8) ?bool {
    const f = get(v, key) orelse return null;
    return switch (f) {
        .bool => |b| b,
        else => null,
    };
}

pub fn arr(v: Value, key: []const u8) []Value {
    const f = get(v, key) orelse return &.{};
    return switch (f) {
        .array => |a| a.items,
        else => &.{},
    };
}

pub fn put(v: *Value, key: []const u8, x: anytype) void {
    switch (v.*) {
        .object => |*o| o.put(A, key, json(x)) catch @panic("out of memory"),
        else => {},
    }
}

// ------------------------------------------------------------------- text

/// `s` cut to `max` bytes (on a UTF-8 boundary) with a note of the full size.
pub fn truncate(s: []const u8, max: usize) []const u8 {
    if (s.len <= max) return s;
    var cut = max;
    while (cut > 0 and (s[cut] & 0xC0) == 0x80) cut -= 1;
    return fmt("{s}\n... (truncated, {d} bytes total)", .{ s[0..cut], s.len });
}

/// The first non-blank line of `s`, trimmed and clipped to `max` bytes.
pub fn firstLine(s: []const u8, max: usize) []const u8 {
    var it = std.mem.splitScalar(u8, s, '\n');
    while (it.next()) |line| {
        const t = std.mem.trim(u8, line, " \t\r");
        if (t.len == 0) continue;
        return clip(t, max);
    }
    return "";
}

pub fn clip(s: []const u8, max: usize) []const u8 {
    if (s.len <= max) return s;
    var cut = max;
    while (cut > 0 and (s[cut] & 0xC0) == 0x80) cut -= 1;
    return fmt("{s}...", .{s[0..cut]});
}

/// Replaces invalid UTF-8 with U+FFFD so the text can go into JSON or Roc.
pub fn validUtf8(s: []const u8) []const u8 {
    if (std.unicode.utf8ValidateSlice(s)) return s;
    var out: std.ArrayList(u8) = .empty;
    var i: usize = 0;
    while (i < s.len) {
        const n = std.unicode.utf8ByteSequenceLength(s[i]) catch {
            out.appendSlice(A, "\u{FFFD}") catch @panic("out of memory");
            i += 1;
            continue;
        };
        if (i + n <= s.len and std.unicode.utf8ValidateSlice(s[i .. i + n])) {
            out.appendSlice(A, s[i .. i + n]) catch @panic("out of memory");
            i += n;
        } else {
            out.appendSlice(A, "\u{FFFD}") catch @panic("out of memory");
            i += 1;
        }
    }
    return out.items;
}

/// Lowercased words of at least `min` letters/digits.
pub fn words(s: []const u8, min: usize) [][]const u8 {
    var out: std.ArrayList([]const u8) = .empty;
    var start: ?usize = null;
    var i: usize = 0;
    while (i <= s.len) : (i += 1) {
        const alnum = i < s.len and (std.ascii.isAlphanumeric(s[i]) or s[i] >= 0x80);
        if (alnum) {
            if (start == null) start = i;
        } else if (start) |st| {
            const w = s[st..i];
            if (w.len >= min) out.append(A, std.ascii.allocLowerString(A, w) catch @panic("oom")) catch @panic("oom");
            start = null;
        }
    }
    return out.items;
}

pub fn sharesWord(a: [][]const u8, b: [][]const u8) bool {
    for (a) |x| for (b) |y| if (eql(x, y)) return true;
    return false;
}

// ------------------------------------------------------------------- time

/// ISO-8601 UTC timestamp with milliseconds.
pub fn nowIso() []const u8 {
    var ts: std.c.timespec = undefined;
    _ = std.c.clock_gettime(.REALTIME, &ts);
    const secs: i64 = @intCast(ts.sec);
    const days = @divFloor(secs, 86400);
    const rem: i64 = @mod(secs, 86400);
    // civil_from_days (H. Hinnant)
    const z = days + 719468;
    const era = @divFloor(z, 146097);
    const doe = z - era * 146097;
    const yoe = @divFloor(doe - @divFloor(doe, 1460) + @divFloor(doe, 36524) - @divFloor(doe, 146096), 365);
    const doy = doe - (365 * yoe + @divFloor(yoe, 4) - @divFloor(yoe, 100));
    const mp = @divFloor(5 * doy + 2, 153);
    const d = doy - @divFloor(153 * mp + 2, 5) + 1;
    const m = if (mp < 10) mp + 3 else mp - 9;
    const y = yoe + era * 400 + @as(i64, if (m <= 2) 1 else 0);
    return fmt("{d:0>4}-{d:0>2}-{d:0>2}T{d:0>2}:{d:0>2}:{d:0>2}.{d:0>3}Z", .{
        @as(u64, @intCast(y)),       @as(u64, @intCast(m)),            @as(u64, @intCast(d)),
        @as(u64, @intCast(@divFloor(rem, 3600))), @as(u64, @intCast(@divFloor(@mod(rem, 3600), 60))), @as(u64, @intCast(@mod(rem, 60))),
        @as(u64, @intCast(@divFloor(ts.nsec, 1_000_000))),
    });
}

/// Milliseconds on a monotonic clock.
pub fn nowMs() i64 {
    var ts: std.c.timespec = undefined;
    _ = std.c.clock_gettime(.MONOTONIC, &ts);
    return @as(i64, @intCast(ts.sec)) * 1000 + @divFloor(@as(i64, @intCast(ts.nsec)), 1_000_000);
}

pub fn getenv(name: [:0]const u8) ?[]const u8 {
    const v = std.c.getenv(name) orelse return null;
    const s = std.mem.span(v);
    if (std.mem.trim(u8, s, " \t\r\n").len == 0) return null;
    return s;
}
