//! Model providers: the Anthropic Messages API and a scripted mock.

const std = @import("std");
const u = @import("util.zig");
const tools = @import("tools.zig");
const Value = u.Value;

pub const CLAUDE_CODE_IDENTITY = "You are Claude Code, Anthropic's official CLI for Claude.";

pub const Request = struct {
    /// System blocks chosen by the kernel: base prompt, reference, supplemental.
    system: []const []const u8,
    tools: []const Value,
    messages: []const Value,
};

pub const Response = struct {
    content: []Value,
    stop_reason: []const u8,
};

pub const Result = union(enum) { ok: Response, err: []const u8 };

fn systemBlocks(texts: []const []const u8, oauth: bool) Value {
    var blocks: std.ArrayList(Value) = .empty;
    if (oauth) blocks.append(u.A, u.json(.{ .{ "type", "text" }, .{ "text", CLAUDE_CODE_IDENTITY } })) catch @panic("oom");
    for (texts) |t| {
        if (std.mem.trim(u8, t, " \n\t").len == 0) continue;
        blocks.append(u.A, u.json(.{ .{ "type", "text" }, .{ "text", t } })) catch @panic("oom");
    }
    return u.json(blocks.items);
}

/// The exact JSON body sent to `/v1/messages`.
pub fn requestBody(model: []const u8, req: Request, oauth: bool) Value {
    var body = u.json(.{
        .{ "model", model },
        .{ "max_tokens", 32000 },
        .{ "system", systemBlocks(req.system, oauth) },
        .{ "messages", req.messages },
    });
    if (req.tools.len > 0) u.put(&body, "tools", req.tools);
    return body;
}

/// `$ANTHROPIC_BASE_URL/v1/messages`; plain `http://` bases are allowed.
pub fn messagesUrl(base: []const u8) []const u8 {
    return u.fmt("{s}/v1/messages", .{std.mem.trimEnd(u8, std.mem.trim(u8, base, " \t\r\n"), "/")});
}

pub const Provider = union(enum) {
    anthropic: Anthropic,
    mock: Mock,

    /// Returns the response or an error message for the trace.
    pub fn call(self: *Provider, req: Request) Result {
        return switch (self.*) {
            .anthropic => |*a| a.call(req),
            .mock => |*m| m.call(req),
        };
    }
};

pub const Anthropic = struct {
    model: []const u8,
    url: []const u8,
    api_key: ?[]const u8,
    oauth_token: ?[]const u8,
    client: std.http.Client,

    pub fn fromEnv(model: []const u8) Anthropic {
        return .{
            .model = model,
            .url = messagesUrl(u.getenv("ANTHROPIC_BASE_URL") orelse "https://api.anthropic.com"),
            .api_key = u.getenv("ANTHROPIC_API_KEY"),
            .oauth_token = u.getenv("CLAUDE_CODE_OAUTH_TOKEN"),
            .client = .{ .allocator = std.heap.c_allocator, .io = tools.io },
        };
    }

    fn call(self: *Anthropic, req: Request) Result {
        const oauth = self.api_key == null and self.oauth_token != null;
        if (self.api_key == null and self.oauth_token == null) return .{ .err = "no credentials: set ANTHROPIC_API_KEY or CLAUDE_CODE_OAUTH_TOKEN" };
        const body = u.stringify(requestBody(self.model, req, oauth));
        var headers: std.ArrayList(std.http.Header) = .empty;
        headers.append(u.A, .{ .name = "anthropic-version", .value = "2023-06-01" }) catch @panic("oom");
        if (self.api_key) |k| {
            headers.append(u.A, .{ .name = "x-api-key", .value = k }) catch @panic("oom");
        } else {
            headers.append(u.A, .{ .name = "anthropic-beta", .value = "oauth-2025-04-20" }) catch @panic("oom");
        }
        const bearer = if (oauth) u.fmt("Bearer {s}", .{self.oauth_token.?}) else null;
        var attempt: u32 = 0;
        while (true) {
            attempt += 1;
            var aw: std.Io.Writer.Allocating = .init(u.A);
            const res = self.client.fetch(.{
                .location = .{ .url = self.url },
                .method = .POST,
                .payload = body,
                .keep_alive = false,
                .headers = .{
                    .content_type = .{ .override = "application/json" },
                    .authorization = if (bearer) |b| .{ .override = b } else .default,
                    .accept_encoding = .{ .override = "identity" },
                },
                .privileged_headers = headers.items,
                .response_writer = &aw.writer,
            }) catch |e| {
                if (attempt < 3) {
                    sleepMs(2000);
                    continue;
                }
                return .{ .err = u.fmt("API transport error: {s}", .{@errorName(e)}) };
            };
            const text = aw.written();
            const code = @intFromEnum(res.status);
            if (code >= 200 and code < 300) {
                const v = u.parseJson(text) catch return .{ .err = u.fmt("bad API response: {s}", .{u.clip(text, 300)}) };
                const content = u.arr(v, "content");
                return .{ .ok = .{ .content = content, .stop_reason = u.str(v, "stop_reason") } };
            }
            const retryable = switch (code) {
                429, 500, 502, 503, 504, 529 => true,
                else => false,
            };
            if (retryable and attempt < 4) {
                sleepMs(@as(u64, 1000) << @intCast(attempt));
                continue;
            }
            return .{ .err = u.fmt("API error {d}: {s}", .{ code, u.clip(text, 1000) }) };
        }
    }
};

fn sleepMs(ms: u64) void {
    const ts = std.c.timespec{ .sec = @intCast(ms / 1000), .nsec = @intCast((ms % 1000) * 1_000_000) };
    _ = std.c.nanosleep(&ts, null);
}

/// Replays a JSON array of assistant `content` arrays, one per request, and
/// appends every request body it received to a JSONL log.
pub const Mock = struct {
    script: []Value,
    next: usize = 0,
    log_path: ?[]const u8 = null,

    pub fn fromFile(path: []const u8) !Mock {
        const text = tools.readFileLimited(path, 64 << 20) catch return error.MockScriptUnreadable;
        const v = u.parseJson(text) catch return error.MockScriptNotJson;
        if (v != .array) return error.MockScriptNotAnArray;
        return .{ .script = v.array.items };
    }

    fn call(self: *Mock, req: Request) Result {
        const body = requestBody("mock", req, false);
        if (self.log_path) |p| {
            const z = u.A.dupeSentinel(u8, p, 0) catch @panic("oom");
            const fd = std.c.open(z, .{ .ACCMODE = .WRONLY, .CREAT = true, .APPEND = true, .CLOEXEC = true }, @as(std.c.mode_t, 0o644));
            if (fd >= 0) {
                const line = u.fmt("{s}\n", .{u.stringify(body)});
                _ = std.c.write(fd, line.ptr, line.len);
                _ = std.c.close(fd);
            }
        }
        if (self.next >= self.script.len) return .{ .err = u.fmt("mock script exhausted after {d} responses", .{self.next}) };
        const entry = self.script[self.next];
        self.next += 1;
        const content: []Value = switch (entry) {
            .array => |a| a.items,
            .string => |s| blk: {
                const one = u.A.alloc(Value, 1) catch @panic("oom");
                one[0] = u.json(.{ .{ "type", "text" }, .{ "text", s } });
                break :blk one;
            },
            else => return .{ .err = u.fmt("mock entry {d} is not a content array", .{self.next}) },
        };
        var uses_tool = false;
        for (content) |b| uses_tool = uses_tool or u.eql(u.str(b, "type"), "tool_use");
        return .{ .ok = .{ .content = content, .stop_reason = if (uses_tool) "tool_use" else "end_turn" } };
    }
};
