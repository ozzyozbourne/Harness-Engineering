"""Add a `cmh` step to a roc-lang/roc checkout's build.zig.

Usage: python3 -I patch-build.py ROC_CHECKOUT CMH_DIR

CMH_DIR is this flavor's directory (the one holding host/, seed/ and this
script). The step builds host/main.zig as a native executable wired like
repl.wasm: it gets Roc's compiler modules and src/cli/ReplSession.zig, so the
`cmh` binary embeds the Roc compiler and interpreter. It also embeds the base
prompt and the seed sources, and passes CMH_DIR in as a build option so the
kernel knows which paths to protect.

The block is delimited by marker comments and is replaced on every run, so
running the script again (or with another CMH_DIR) is safe.
"""
import pathlib
import re
import sys

roc = pathlib.Path(sys.argv[1]).resolve()
cmh = pathlib.Path(sys.argv[2]).resolve()
repo = cmh.parent
build = roc / "build.zig"
text = build.read_text()
BEGIN, END = "    // cmh-begin\n", "    // cmh-end\n"
text = re.sub(re.escape(BEGIN) + ".*?" + re.escape(END), "", text, flags=re.S)
anchor = "    build_repl_wasm_step.dependOn(&repl_wasm_install.step);\n"
assert text.count(anchor) == 1, "anchor not found; the roc build.zig changed"


def zstr(p: pathlib.Path) -> str:
    return str(p).replace("\\", "\\\\").replace('"', '\\"')


embeds = {
    "base-prompt.md": repo / "base-prompt.md",
    "seed/word_count.roc": cmh / "seed" / "word_count.roc",
    "seed/compose_system.roc": cmh / "seed" / "compose_system.roc",
    "seed/select_lessons.roc": cmh / "seed" / "select_lessons.roc",
}
embed_lines = "".join(
    f'    cmh_exe.root_module.addAnonymousImport("{name}", .{{ .root_source_file = .{{ .cwd_relative = "{zstr(path)}" }} }});\n'
    for name, path in embeds.items()
)
snippet = f'''{BEGIN}    // cmh (Roc flavor): a native Zig host embedding the Roc compiler + interpreter.
    const cmh_exe = b.addExecutable(.{{
        .name = "cmh",
        .root_module = b.createModule(.{{
            .root_source_file = .{{ .cwd_relative = "{zstr(cmh / "host" / "main.zig")}" }},
            .target = target,
            .optimize = optimize,
            .link_libc = true,
        }}),
    }});
    cmh_exe.stack_size = stack_budget.roc_stack_size;
    configureBackend(cmh_exe, target);
    roc_modules.addAll(cmh_exe);
    cmh_exe.root_module.addImport("ReplSession.zig", b.createModule(.{{
        .root_source_file = b.path("src/cli/ReplSession.zig"),
        .target = target,
        .optimize = optimize,
        .imports = &.{{
            .{{ .name = "base", .module = roc_modules.base }},
            .{{ .name = "can", .module = roc_modules.can }},
            .{{ .name = "check", .module = roc_modules.check }},
            .{{ .name = "compile", .module = roc_modules.compile }},
            .{{ .name = "ctx", .module = roc_modules.ctx }},
            .{{ .name = "eval", .module = roc_modules.eval }},
            .{{ .name = "lir", .module = roc_modules.lir }},
            .{{ .name = "parse", .module = roc_modules.parse }},
            .{{ .name = "reporting", .module = roc_modules.reporting }},
        }},
    }}));
    cmh_exe.root_module.addImport("compiled_builtins", compiled_builtins_module);
    cmh_exe.step.dependOn(&write_compiled_builtins.step);
{embed_lines}    const cmh_opts = b.addOptions();
    cmh_opts.addOption([]const u8, "cmh_dir", "{zstr(cmh)}");
    cmh_opts.addOption([]const u8, "base_prompt_path", "{zstr(repo / "base-prompt.md")}");
    cmh_exe.root_module.addOptions("cmh_options", cmh_opts);
    b.step("cmh", "Build cmh, the Roc flavor of the self-modifying harness").dependOn(&b.addInstallArtifact(cmh_exe, .{{}}).step);
{END}'''
build.write_text(text.replace(anchor, anchor + snippet))
print("patched", build)
