"""Add a `rochost` step to a roc-lang/roc checkout's build.zig.

Usage: python3 -I patch-build.py ROC_CHECKOUT HOST_ZIG

The step builds HOST_ZIG as a native executable wired like repl.wasm: it gets the
compiler modules and ReplSession.zig, so the host embeds the Roc compiler and
interpreter. Running it twice is a no-op.
"""
import pathlib, sys

roc, host = pathlib.Path(sys.argv[1]), pathlib.Path(sys.argv[2]).resolve()
build = roc / "build.zig"
text = build.read_text()
if "rochost" in text:
    sys.exit(0)
anchor = "    build_repl_wasm_step.dependOn(&repl_wasm_install.step);\n"
assert text.count(anchor) == 1, "anchor not found; the roc build.zig changed"
snippet = f'''
    // cmh prototype: a native Zig host embedding the Roc compiler + interpreter.
    const rochost = b.addExecutable(.{{
        .name = "rochost",
        .root_module = b.createModule(.{{
            .root_source_file = .{{ .cwd_relative = "{host}" }},
            .target = target,
            .optimize = optimize,
            .link_libc = true,
        }}),
    }});
    rochost.stack_size = stack_budget.roc_stack_size;
    configureBackend(rochost, target);
    roc_modules.addAll(rochost);
    rochost.root_module.addImport("ReplSession.zig", b.createModule(.{{
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
    rochost.root_module.addImport("compiled_builtins", compiled_builtins_module);
    rochost.step.dependOn(&write_compiled_builtins.step);
    b.step("rochost", "Build the cmh Roc-reload prototype host").dependOn(&b.addInstallArtifact(rochost, .{{}}).step);
'''
build.write_text(text.replace(anchor, anchor + snippet))
print("patched", build)
