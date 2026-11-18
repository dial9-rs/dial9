# Recovering a frameless leaf's immediate caller (64-bit Arm)

## What goes wrong

A **leaf** function is one that calls nothing. On 64-bit Arm (aarch64), Rust
builds leaf functions with no frame record, because `non-leaf` is the default
setting of `-C force-frame-pointers` for this target. Non-leaf functions do get
one.

That mix breaks a frame-pointer stack walk. Say `outer` calls `middle`, and
`middle` calls `leaf`, and we take a sample while `leaf` is running:

```
  outer()     has a frame record
  middle()    has a frame record    <-- this frame goes missing
  leaf()      NO frame record       <-- sample taken here
```

A frame-pointer walk starts from the program counter, which correctly names
`leaf`. Then it looks for the caller. `leaf` never touched the frame pointer
register, so that register still holds what `middle` put there: a pointer to
`middle`'s own frame record. Reading the saved return address out of that record
gives the return address into **`outer`**.

So the walk reports `leaf` then `outer`. `middle` — the function that actually
called `leaf` — is silently dropped.

**The missing address is still in the link register.** `leaf` makes no call, so
nothing has overwritten it. The fix is to read it and put the frame back.

A function that *does* get a frame record can lose its caller the same way. The
compiler may set the record up late, after the code that does the work
(*shrink-wrapping*), so a sample taken in that code sees exactly the state
above. `rayon-fanout`'s `hash_block`, built with `-C force-frame-pointers=yes`:

```
4aae0 ... 4ab14   the hot loop: no frame record yet
4ab1c             stp x29, x30, [sp, #16]   <- frame record stored here
4ab38             ret
```

Nearly every sample lands in the loop, before the frame record exists.

## Why this mattered

Until this change, dial9 needed `-C force-frame-pointers=yes` on 64-bit Arm so
that leaves got a frame record too. That flag is not free. On a benchmark whose
hot loop is a small `#[inline(never)]` leaf, it costs about **14% throughput**
on Graviton3 — roughly 0.65 cycles per leaf call (+3 instructions, +5
micro-operations, +1 store, +1 load).

The reason is the calling convention: a frame record on 64-bit Arm is the *pair*
of the frame pointer and the link register. Forcing a frame into a leaf
therefore also saves a link register the leaf never needed.

The flag is also not a complete fix: it covers leaves that set up their frame
record first, but not ones whose record the compiler sets up late (see above).
Recovery covers both.

## Turning it on

Recovery is opt-in, off by default:

```rust
CpuProfilingConfig::default().recover_leaf_callers(true)
SchedEventConfig::default().recover_leaf_callers(true)
```

The same setter exists on the lower-level `SamplerConfig`, and the environment
config reads `DIAL9_RECOVER_LEAF_CALLERS=true`. With it on, CPU and scheduler
profiling on 64-bit Arm do not need `-C force-frame-pointers=yes`. With it off,
nothing below runs: no table is built, the perf backend does not ask for the
link register, and the ctimer walk ignores it. The trace's segment metadata
records which way it was set, as `cpu.profile.recover_leaf_callers`.

The ctimer signal handler has its own on/off flag rather than checking whether
the table exists. Otherwise, a scheduler profiler that turned recovery on and
built the table would silently turn it on for a CPU profiler that left it off.

## How other profilers handle it

They all read the link register. What differs is how they decide whether it
holds the caller.

**The `perf` tool asks for the link register, then re-unwinds.**
`add_leaf_frame_caller_opts_aarch64` adds the link register to the registers it
samples. `get_leaf_frame_caller_aarch64` then runs perf's general unwinder for
exactly two frames, seeded with those registers, and takes the second one as the
caller. That unwinder reads the binary's unwind tables, so the answer is
authoritative rather than inferred.

Three things follow. It only runs when the recording mode is frame pointers and
the sample really carries the link register. It needs unwind tables and a library
to read them. And it happens in the tool afterwards, **not in the kernel** — so
any other program reading the same samples gets the unrepaired stack. That is
exactly the position dial9 was in.

**async-profiler reads the link register and decides from where the program
counter is.** `StackFrame::link()` returns the register directly, and the
unwinder trusts it when the program counter sits at a function's entry, at a
`ret`, or before the instruction that saves the frame record. For compiled Java
methods it also knows the frame size from the just-in-time compiler, so it knows
whether a frame exists at all. For hand-written assembly stubs it matches the
function name against a list. It never inspects the link register's value.

**This change reads the link register and asks a different question:** do the
link register and the program counter belong to different functions? That needs
only a table of function start addresses — no unwind tables to ship, no
instruction decoding, and nothing to maintain per function. It is weaker than a
real unwind; "What this cannot do" below says exactly where.

## That it works

Samples that landed inside a frameless leaf, on Graviton3. The count varies per
run; what matters is that it is all-or-nothing:

| path | default? | setting off | setting on |
|---|---|---|---|
| perf backend (`perf_event_open`) | yes | 0/308 | 308/308 |
| ctimer backend (`SIGPROF` timer signal) | fallback | 0/31 | 31/31 |

Both columns run in CI: each backend test runs with the setting on and then off,
in that order, so the "off" run happens after the table is built and checks the
setting itself. With `-C force-frame-pointers=yes`, the test's leaf sets up its
frame record before its loop, so both backends report every leaf sample either
way, and no frame is duplicated.

The same effect shows up in a real program. `examples/rayon-fanout` has a
call-free function, `hash_block`, whose immediate caller is rayon's
`bridge_producer_consumer::helper`. The correct answer comes from
`perf record --call-graph dwarf`, which unwinds from the binary's own unwind
tables instead of frame pointers. Every row below is the same binary per build,
run with the setting off and then on, about 6,700 samples per run:

| run | `hash_block`'s reported caller |
|---|---|
| `perf` DWARF unwind (correct) | `helper` 100% |
| leaves frameless, setting off | a different function, 99.8% |
| leaves frameless, setting on | `helper` **100%** |
| `-C force-frame-pointers=yes`, setting off | a different function, 99.7% |
| `-C force-frame-pointers=yes`, setting on | `helper` **100%** |
| stripped, setting on | a different function |

With the setting off, the frame is dropped and the sample names the caller's
caller. `=yes` does not help here, because `hash_block` sets up its frame
record after its loop (see "What goes wrong"). On the stripped binary the
setting has no symbols to use, so the frame is lost again, but nothing is
invented.

`rayon-fanout`'s other leaf, `score_document`, cannot tell the cases apart:
`rayon::join` is recursive, so its caller and its caller's caller are the same
function.

### Beyond the test programs

`rayon-fanout` was written to exercise this, so we also checked three widely
used open-source Rust applications that we did not write and whose hot code we
did not choose:

* **ripgrep**, a text search tool: one process searching a 620 MB source tree
  15 times;
* **jaq**, a JSON query tool: a filter over a 215 MB JSON file;
* **oxipng**, a PNG optimizer: ten images at optimization level 4.

Each was built in release mode with Rust's default 64-bit Arm frame pointers, so
its leaves are frameless, and each binary was run three times: under dial9 with
the setting off, under dial9 with it on, and under
`perf record --call-graph dwarf` for the correct answer. For every leaf
function, we compared which immediate caller dial9 reported with which caller
perf reported, weighted by how often perf sampled that leaf:

| app | setting off | setting on |
|---|---|---|
| ripgrep | 82.5% | **99.1%** |
| jaq | 68.7% | **88.4%** |
| oxipng | 99.0% | 99.4% |

ripgrep's second-hottest function, a substring searcher (14% of samples), is a
leaf. With the setting off, every one of its samples names the caller's caller;
with it on, every one is right. oxipng's hot functions all call other functions,
so there is almost nothing to recover, which also shows the setting does no
harm. In jaq the setting fixes most of the C allocator it links in (34.8% to
81.3%); what is left is C code built with no frame pointers at all (see "What
this cannot do").

Measured on Graviton3, Amazon Linux 2023, kernel 6.12. The harness is not in the
repository: it needs `perf` and the three applications' sources.

## Two paths needed fixing

dial9 collects stacks two ways, and they differ in who does the walking.

**The perf backend** asks the kernel. It requests `PERF_SAMPLE_CALLCHAIN`, and
the kernel's `arch_stack_walk_user` emits the program counter and then follows
the frame pointer chain. It never reads the link register — true of mainline
v6.12 and v6.18 and of Amazon Linux's v6.18.56. As above, the `perf` tool
repairs this in its own code rather than in the kernel, so dial9 got the
unrepaired stack.

**The ctimer backend** walks the chain itself in `fp_profiler/unwind.rs`, from a
`SIGPROF` signal handler.

**Memory profiling is unaffected and untouched.** It captures inside the
allocator hook, where every frame on the stack has a call in progress and so has
a frame record. Reaching the allocator requires a call, so the allocating
function is never a leaf. (A leaf can allocate *stack* space, but the hook never
sees that.)

One frame can still be missing there, for a different reason: if a function's
only call is a *tail* call into the allocator, the compiler turns it into a jump
and the function never gets a frame. That is tail-call elision, not a missing
leaf frame record. No frame-pointer setting fixes it, and link-register recovery
cannot either, because the tail call leaves the link register pointing into
*that function's* caller. Every frame-pointer unwinder has this gap.

## When to insert a frame

Both paths call one function, `func_starts::lr_is_missing_caller`. It inserts
the link register's call site as frame 1 only when all three hold:

1. it differs from what the stack already reports, and
2. both the link register and the sampled program counter land in known code,
   and
3. they land in **different functions**.

The rule lives in one place on purpose. It was briefly written twice, once per
path, and the copies drifted — the ctimer walk stripped pointer authentication
bits from the link register and the perf path did not, which turned a signed
address into an invented frame on the default backend.

**Condition 3 is the one that does the work:**

| situation | same function? | differs from stack? | action |
|---|---|---|---|
| frameless leaf | no — link register points into the caller | yes | **insert** |
| framed function that already called something | **yes** — stale link register points back into itself | yes | skip |
| framed function, link register not yet overwritten | no | **no** | skip |
| sampled before the frame record is stored (at the start of a function, or in code the compiler placed before setting it up) | no | yes | **insert** |

The last row is not a corner case: with late frame setup it covers a function's
hot loop, so it is where much of the recovery happens in practice.

Without condition 3 the common case would misfire constantly. A `bl` call
instruction leaves the link register pointing into the *calling* function, so
after any call the stale value resolves to the same function as the program
counter, and is rejected.

**Condition 2 catches a link register that holds something that is not a return
address at all:**

* **A signed address.** With pointer authentication enabled, the link register
  carries a signature until the first call. The kernel strips those bits from
  stack addresses but **not** from sampled registers. This is not hypothetical:
  Amazon Linux 2023's C library is built with pointer authentication, and 2,146
  of its 3,324 functions sign the link register. `lr_from_regs` strips the
  signature; anything left over falls outside known code and is rejected.
* **The link register reused as scratch.** Once a non-leaf has saved it, the
  compiler may put anything there. This is rare — a release build using `tokio`
  and `rayon` has 24 such writes in 501,092 instructions — but unbounded, so
  rarity alone is not an argument. A heap pointer, a small integer, or all-ones
  all fall outside known code.

## What this cannot do

This is inference from register values, not a real unwind, so it is weaker than
what the `perf` tool does and in the same tier as async-profiler's
position-based check. See "How other profilers handle it" above.

**When the table cannot identify an address, the frame is left out.** So the
normal failure is a missing frame, never a wrong one. That asymmetry is
deliberate: a missing frame is far less damaging to a flame graph than an
invented one.

Naming the **wrong** function needs the link register to hold a code address
belonging to some *other* real function. Two ways that happens:

**One function split across several symbols, reached by a jump.** GCC's
`-freorder-blocks-and-partition` emits `foo.cold`, and the BOLT optimizer emits
`.cold.N`. If framed `foo` calls `bar` and then *branches* into `foo.cold`, a
sample in `foo.cold` sees two different symbols and inserts a `foo` frame that
no call put there. Here symbol identity is not function identity, which is what
the argument above assumes.

This was measured, not assumed. The release `rayon-fanout` binary has **no**
`.cold` or `.part.` symbols, and neither does Amazon Linux 2023's C library. The
C library does have 34 `.part.N` symbols, but a partial-inlining clone is
*called*, so naming its parent is correct rather than invented.

**A code address that was never a return address** — a function pointer loaded
for an indirect call, or a computed label. It is inside known code and resolves
to a real function, so the bounds check does not reject it. Decoding the
instruction before it and requiring a call instruction would close this; that
check would complement condition 3 rather than replace it.

One case that *looks* like a problem but is not: a tail call into a leaf. If `F`
calls anything, `F` is a non-leaf and has a frame record, so its tail call must
restore the link register first. A sample in the tail-called leaf therefore
recovers `F`'s caller — which is correct, because `F`'s frame is already gone.

**Code built with no frame pointers at all** — usually a C or C++ dependency —
is out of reach. If such a function is not a leaf, the frame-pointer chain is
already broken by the time the sample is taken, and the link register no longer
holds its caller. That is most of the gap left in jaq below.

Finally, recovery only applies to samples from this process, since the table
describes this process's own memory map. `start_for_pid` and `inherit` can
deliver samples from other processes.

## Knowing which function an address belongs to

Condition 3 needs function *identity*, not sizes or names. `func_starts.rs`
keeps a sorted array of function start addresses; `entry_of(addr)` binary
searches for the greatest start at or below it. The result is never
dereferenced, never shown in a stack, and never symbolized — only compared:

```rust
match (table.entry_of(pc), table.entry_of(lr)) {
    (Some(pc_fn), Some(lr_fn)) => pc_fn != lr_fn,
    _ => false,
}
```

Two addresses in different functions always map to different starts, which is
the entire test. That is why starts alone are enough: we never ask "is this
address inside function F?".

**Lookups are bounded.** A sorted list of starts has no upper end, so any
address above the lowest one would resolve to whichever function sits below it —
a scratch value would name the last entry in the table. So the table also keeps
the executable address ranges the symbols came from, and `entry_of` returns
nothing outside them.

Those ranges only cover modules whose symbols were actually read. A module we
could not read stays outside the bounds, so its addresses cannot resolve to the
previous module's last function. Anonymous executable mappings (just-in-time
code, trampolines) are excluded for the same reason: no symbols means no claim.

Addresses come from the symbol tables of executable, file-backed mappings,
shifted by the load offset of the executable segment that covers them. Only
executable segments count, matched against the segment's page-aligned start.
That detail matters: the memory map reports a page-aligned file offset, and
under `-z noseparate-code` (the default for the lld and mold linkers) the
executable segment is not page-aligned, so a naive match lands in the preceding
read-only segment and shifts every symbol in the module.

## Checking a module is the one that is loaded

A module is re-opened by name, so before its symbols are used we confirm the
file really is what is loaded. Otherwise a library replaced on disk mid-run
would pair the new file's symbols with the old mapping's load offset.

It must be a regular file, and then:

* **If it has a GNU build-id**, that is compared against the same note in
  memory. The build-id is a hash of the linked output, so two differing builds
  get differing ids. (It is not generated afresh per link — two reproducible
  links of the same inputs get the same id, which is harmless here.)
* **If it has none**, the first 64 bytes of the executable mapping are compared.
  This is weaker and deliberately claims nothing about how often it matches by
  chance: depending on layout those bytes are the file header, `.init`, or
  `.plt`, none of which is build-specific. It shows the file is *a* build with
  that prologue, not that it is this one. An id present on only one side counts
  as a mismatch, not a reason to fall back.

**The live bytes come from the dynamic loader**, not from reading our own memory
some other way. `dl_iterate_phdr` reports each loaded object's address and its
in-memory program headers, and holds the loader's lock while our callback runs.
So the build-id is copied out *inside* that callback, where a concurrent
`dlclose` cannot unmap it mid-read. The note's address is checked against a
readable loadable segment of the same object first, since a note segment is not
required to sit inside one.

That design is forced, because every other way of reading our own memory breaks
on a hardened process — and a plain pointer dereference cannot be made safe
here, since `safe_load`'s fault handler only recovers faults from inside its own
instruction window:

* **`/proc/self/mem`** is mode 0600, and the kernel gives `/proc/self/*` to root
  as soon as a process is not dumpable — after a `setuid` or `setgid` privilege
  drop under the default `fs.suid_dumpable=0`, with file capabilities, or after
  an explicit `prctl(PR_SET_DUMPABLE, 0)`. The open then fails with `EACCES`
  *before* the kernel's same-process exemption is even consulted, so every
  module is rejected and recovery turns off for the whole process. Measured in a
  container as user 1000: the file is owned by user 1000 and opens; after
  clearing the dumpable flag it is owned by root and returns `EACCES`, while
  `/proc/self/maps` still opens.
* **`process_vm_readv`** is a debug-class system call. A hardened filter such as
  systemd's `SystemCallFilter=@system-service` answers it by *killing* the
  process rather than returning an error, so it cannot even serve as a fallback.

A module the loader does not know about — a hand-mapped image — is rejected
rather than guessed at: there are no trustworthy live bytes for it and no
trustworthy load offset either. An empty table logs a warning, since it disables
recovery process-wide.

`(dev, inode)` is deliberately **not** used for identity. The memory map and
`fstat` report different devices on overlayfs before kernel 6.8 and on btrfs
subvolumes, so comparing them rejects every module and silently disables
recovery. Measured in a container on Amazon Linux 2023's 6.1 kernel: the map
says `103:01`, `fstat` says `0x32`.

### Why the symbol parsing is hand-written

It is about 130 lines of bounds-checked reads. `blazesym` is already a
dependency and reads the same symbols, so the reason is cost, not dependency
count. Measured on Graviton3 over one 226.8 MiB binary, both finding the same
34,912 symbols:

| | time | heap | resident |
|---|---|---|---|
| this parser | **1.1 ms** | 0 | +128 KiB |
| `blazesym`, symbols only | 46.6 ms | +1 KiB | +720 KiB |
| `blazesym`, default settings | 274 ms | +233 KiB | **+49 MiB** |

42x on the cheaper path is this feature's entire startup budget. The default
path is worse than slow: `blazesym` defaults to consulting DWARF debug
information, and for a module without it — a distribution C library — it reports
**zero** symbols, which would silently drop every C library function from the
table.

Every offset is built with checked arithmetic and bounds-checked against the
file, and a section that does not lie inside the file is skipped. The loader
validates none of this, and this code runs during startup and on the sample
drain path: an unchecked section size of `u64::MAX` would otherwise spin about
7.7e17 times and hang the process.

### Why not reuse the existing symbolization

Three options were considered and rejected:

* **`resolve_symbol().base_addr`** is the right datum, but using it would put
  `blazesym` symbolization into the sample drain path, which dial9 deliberately
  keeps cheap by deferring symbolization offline.
* **The offline symbolizer** would be tidier, but that stage is append-only: it
  appends symbol entries keyed by address and never rewrites a stack. Since no
  later stage legitimately adds frames, the frame must be there at capture time.
* **Keeping the flag** was viable — 14% is an upper bound, not a forecast for a
  service that spends its time in network I/O. But the cost is permanent and the
  fix is bounded, so it is worth paying once.

## When the table is built

A lookup is signal-safe: binary search over an immutable slice, no allocation,
no locks. *Building* it is not. So there are two entry points over one
`OnceLock`:

| entry point | caller | why |
|---|---|---|
| `init()` | `CtimerSampler::start`, when the setting is on | the signal handler reads the table from signal context and cannot build it on demand, so it must already exist |
| `get_or_build()` | the perf backend's sample drain | ordinary context, so build on first use — a perf-only process never pays for a table it will not read |

The ctimer sampler builds it rather than `install_handler`, because the signal
walk is the only consumer that reads it from signal context. Building it in
`install_handler` would charge every `Unwinder::install()` — memory profiling
included — for parsing every module to produce a table it never reads. The call
is gated to 64-bit Arm, the only architecture whose walk reads it.

Modules are memory-mapped rather than read. Only the section headers and symbol
table are touched, so only those pages are faulted in; a read copies the whole
image. On a 226.8 MiB service binary that is the difference between ~2 ms and
~100 ms, and between no transient allocation and a full copy of the image.

`func_starts::cost::report_build_cost` reports the cost without asserting a
threshold, since it is a property of the host:

```sh
cargo test --release -p dial9-perf-self-profile --lib \
  func_starts::cost -- --ignored --nocapture
```

## Cost per sample

With the setting on, the perf path adds one extra 64-bit value per sample in the
ring buffer: the link register, requested through `PERF_SAMPLE_REGS_USER`. `lr_from_regs` reads
it back and strips pointer authentication bits, which the kernel does for stack
addresses but not for registers.

## x86-64

The same frame loss happens on x86-64: a frameless leaf leaves the frame pointer
at its caller's frame, so the walk skips the caller. The difference is where the
missing return address lives. On 64-bit Arm it is in the link register; on
x86-64 the call instruction pushes it onto the stack, at `*rsp`. Recovering it
would mean reading the stack top at sample time, which is a different mechanism,
so this change does not do it and the setting has no effect there.

The trap is also less likely. On 64-bit Arm, `non-leaf` is the *target default*,
so an ordinary build silently gets framed callers with frameless leaves. x86-64
has no such default middle state:

* rustc omits frame pointers entirely by default (measured: 781 frame-pointer
  sites by default, 1,300 with `-C force-frame-pointers=yes`), so you get
  nothing to walk rather than a partial chain;
* `-fno-omit-frame-pointer` in C and C++ covers leaves on x86-64, unlike 64-bit
  Arm.

But `-C force-frame-pointers=yes` is not a complete fix there either. Late frame
setup happens on x86-64 too: built with `=yes`, `rayon-fanout`'s `hash_block`
runs its whole loop before `push %rbp` (instruction 28 of 34), so samples in the
loop lose its caller, and nothing in this change recovers them.

The cost of `=yes` is also about 18x lower there (+0.037 against +0.653 cycles
per leaf call), because the call instruction has already pushed the return
address, so no link register has to be saved. For comparison, the `perf` tool
does no x86-64 leaf recovery at all, and async-profiler does.

## Known gaps

* **An unsymbolized span inside known code collapses onto the preceding
  symbol.** Two addresses there look like one function, so the frame is dropped
  rather than invented. Bounding each symbol by its own size would close this,
  at 16 bytes per function instead of 8.
* **Android gets nothing for a library embedded in an Android package (APK), and
  the rest is untested.** The table is built there and the link register is
  available. But with `extractNativeLibs=false` — the Android Gradle plugin
  default for `minSdk >= 23` — the app's shared library is mapped straight out
  of `base.apk` at a non-zero file offset. The memory map names the `.apk`, and
  the parser looks for the file header at offset 0, finds the zip header, and
  returns nothing. So the app's own code contributes no symbols and its leaves
  never recover, while the table stays non-empty from the system libraries — so
  the empty-table warning does not fire either. Parsing from the mapping's file
  offset would fix it. Beyond that, nothing exercises Android: the build job
  only builds, with no device in the loop.
* **A stripped module falls back to stack-only.** Its addresses resolve to
  nothing and the stack is left alone. The empty-table warning rarely fires,
  because the other mapped modules still contribute. A per-module warning is
  still missing.
* **The table is built once.** A module loaded by `dlopen` afterwards is outside
  the bounds, so its leaves never recover. After `dlclose`, a new mapping over
  the same range can resolve to a stale symbol. Rebuilding when the memory map
  changes, on the perf path only, would close both.
* **The virtual shared library (vDSO) is excluded**, because it has no
  file-backed path. Frequently sampled leaves like `clock_gettime` fall outside
  the bounds.
* **A sample taken entirely in the kernel recovers nothing.** The rule needs a
  user-space program counter, and such a sample has none.
* **x86-64 gets no recovery**, including for functions that set up their frame
  record late, which lose their caller even with `-C force-frame-pointers=yes`.
  Reading the return address from the top of the stack would close it; see
  "x86-64".
* **Code built with no frame pointers at all**, usually a C or C++ dependency,
  breaks the chain wherever it is not a leaf, and nothing recovers that.
* **The recursion self-test cannot see this class of bug.** Its helper makes a
  call, so it is a non-leaf and gets a frame record; the self-test passes while
  leaves are frameless. `Unwinder::leaf_frame_pointers_present()` closes that by
  reading the prologue of a known leaf in dial9's own code.

## Testing

| test | covers | needs |
|---|---|---|
| `perf_backend_leaf_caller.rs` | the perf path | `perf_event_paranoid <= 1` |
| `leaf_caller_recovery.rs` | the ctimer path | `DIAL9_FORCE_CTIMER=1` |
| `leaf_frame_pointers.rs` | leaf detection, and the self-test blind spot | — |
| `leaf-caller-recovery` job | both backends, built without the flag, with the setting on and then off | `DIAL9_NO_SKIP`, `DIAL9_EXPECT_LEAVES` |
| `rule_tests` | the rule, one case per row of the table above, plus scratch and signed values | — |
| `func_starts` unit tests | lookups, the bounds, load offsets, section bounds, the per-module symbol cap, and the loaded-image check | — |
| note-walk tests (`rstest`) | empty descriptor, another vendor's name, other GNU note types, an id behind an earlier note, unpadded and oversized descriptors, truncation, sizes near the 32-bit limit | — |
| `the_table_is_built_in_a_non_dumpable_process` | the table is still built after `prctl(PR_SET_DUMPABLE, 0)`, in a re-executed child since the flag cannot be set back | — |
| `maps_tests` | memory-map parsing: spaces in paths, deleted files, anonymous and pseudo mappings | — |
| `user_pc_tests` | the user-frame index with and without kernel frames, and the process-id gate | 64-bit Arm |
| `lr_regs_tests` | signature stripping and register-mask indexing | 64-bit Arm |
| `func_starts::cost` (ignored) | build cost report | — |

Both sampling tests assert which backend they actually got, and skip otherwise.
`PerfSampler::start` picks a backend automatically, so a test that does not pin
one silently measures whichever path the host allows. That mistake once made a
"32/32" result look like evidence about dial9 generally when it covered only one
backend.

To reproduce the frameless case you must override the repo's
`.cargo/config.toml`, which sets `force-frame-pointers=yes`. Note that
`RUSTFLAGS` *replaces* that value rather than adding to it:

```sh
RUSTFLAGS="--cfg tokio_unstable" \
  cargo test --release -p dial9-perf-self-profile \
  --test perf_backend_leaf_caller -- --nocapture
```

A release build is also required: at `opt-level=0` even leaves get frame
pointers, so the bug does not reproduce.
