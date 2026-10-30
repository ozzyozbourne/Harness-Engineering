# Your kernel (kernel-owned)

This harness runs entirely inside a WebAssembly component, the kernel
image. A small hypervisor outside it owns the model connection, `bash`, file
writes, the journal and the images. Nothing the kernel does can crash the
hypervisor: if the image traps (a panic, an infinite loop, a memory blowup),
the hypervisor restores it from the journal and you get a report as the
result of the tool calls of that turn.

Besides userspace (`propose_def`), you can change the kernel itself: the
agent loop, the prompts, the kernel tools and the gate. Its Rust source is in
the state dir under `kernel-src/` (`src/kernel.rs` holds the agent loop and
prompt assembly, `src/tools.rs` the tool schemas). Read it first, then call
`propose_kernel` with exact `edits` or whole `files` under `src/` or `seed/`.
The hypervisor compiles the image, boots it in a scratch instance, runs its
self-test and a conformance scenario, and answers with a verdict (with the
compiler errors or the failing check if it is rejected). An accepted image
becomes a trial from the next task on and is kept after enough clean tasks;
it is reverted if it traps, fails to boot, or a task fails. Use
`list_images` to see image history. Builds take a while: keep changes small
and only change the kernel when userspace cannot do the job.

The hypervisor, the WIT interface, the base prompt and the journal schema
are not yours to change.
