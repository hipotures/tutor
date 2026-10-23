# Web vault development on an 8 GiB VM

Use the installed, patched `vw_web_builds` source checkout, not the Rust
repository. No reinstall, Rust build, swap, or production image is needed:

```sh
python3 scripts/web-vault/dev.py \
  --source /home/user/webvault-build \
  --node /home/user/.local/node24/bin/node
```

This builds into `apps/web/build` and watches for changes. Stop with Ctrl-C.
Use `--mode once` for one build or `--mode serve` for upstream's HTTPS dev
server. The measured workflow below is `watch`, writing files to disk; the
optional dev server has not been included in these measurements. Point the
local backend's `WEB_VAULT_FOLDER` at the checkout's `apps/web/build` if it
serves those files. Its URLs/proxy configuration still comes from the existing
web checkout; the launcher does not reconfigure or start the Rust backend.

Requires installed frontend dependencies, Node >=24.17, Python >=3.9, and a
working systemd user manager with cgroup v2 memory control. The command fails
if those controls cannot be installed. It does not fall back to an unlimited
build. A lock prevents two instances of this launcher building the same checkout.

The entire build service is capped at **4 GiB**, with `MemorySwapMax=0`,
`OOMPolicy=kill`, and a **3 GiB** V8 old-generation limit. The remaining budget
is for young-generation heap, native allocations, buffers, file cache, and
kernel accounting. The cap applies to all descendants, not only the parent
Node process. The monitor runs outside that cgroup.

The command prints its log directory under `~/.cache/tutor-web-vault/`:

- `build.log`: errors, warnings, build durations.
- `v8.jsonl`: RSS, heap, external buffers, CPU counters, invalidations and builds.
- `cgroup.jsonl`: 250 ms samples of the complete process tree's summed RSS,
  cgroup memory peak/current, OOM events and CPU counters.
- `summary.txt`: final systemd result, exact cgroup peak and sampled RSS peak.

RSS samples can miss a short spike. The kernel's `memory.peak` captures those
spikes but also includes file cache and kernel memory; it is not RSS.
V8 samples are delayed during synchronous compilation. CPU counters use
microseconds (`v8.jsonl`) or the units named by cgroup/systemd.

## Changes and rationale

`webpack.dev.cjs` wraps the existing OSS/selfhosted configuration. Production
commands, dependencies, source patches, AOT compilation, strict template checks,
SDK functionality, and all entry points remain in use. The wrapper rejects
`NODE_ENV=production`.

1. `wasm-fallback-loader.cjs` splits the SDK's generated wasm2js module into a
   small ESM boundary and a dependency-free CommonJS factory. Only the factory
   gets `module.noParse`. Its generated body is preserved verbatim and runs in
   strict mode. The boundary supplies the original shared SDK bindings and
   exposes the original named exports. There is no eval, `new Function`, SDK
   stub, disabled fallback, or extra network dependency. Unknown import/export
   formats fail the build rather than silently producing a different module.
2. Development uses CSP-compatible `cheap-module-source-map`: TypeScript line
   mapping remains available; column-level mapping is omitted.
3. Webpack processes at most two modules concurrently on this two-vCPU VM.
4. Watch uses memory caching with `maxGenerations: 1`, retiring old cache
   generations. Babel's existing disk cache remains enabled. Webpack's large
   persistent source/map pack is not serialized during watch idle time.

No frontend source files or production configurations need editing. The
checkout's `webpack.base.js` is upstream's original (`devtool: "source-map"`);
the wrapper supplies its own development source-map setting.

## Investigation and measurements (2026-10-23)

Machine: 7.8 GiB usable RAM, two vCPUs, no swap. Checkout:
`vw_web_builds` commit `a868ea02ea78b4a0eb8656664cf8d29f9fdab8c2`, with the existing
Tutor vault-filter patches. Installed versions: Node 24.22.1, webpack 5.106.2,
`@ngtools/webpack` 21.2.9, Angular compiler 21.2.17, TypeScript 5.9.3, Babel
7.29.6, SDK `0.2.0-main.950`. No dependency versions were changed.

The preserved systemd journal for `run-p16451-i15994.scope` reports a kernel
OOM kill at exactly 5 GiB after 36.466 s (`Result=oom-kill`, `OOMPolicy=stop`).
Thus the wrapper's exit 143 does not identify the original victim; systemd's
OOM result provides the evidence. System journal access was unavailable, so
the kernel victim's PID could not be independently recovered from it.

The main measured bottleneck was the SDK's **25,617,874-byte**
`bitwarden_wasm_internal_bg.wasm.js` fallback. Babel already excluded this
file, but webpack still parsed its generated JavaScript into a large Acorn AST.
An isolated Acorn parse, without Angular or webpack plugins, exhausted a
768 MiB V8 heap in approximately four seconds (0.80 GiB cgroup peak).
Avoiding that AST allowed the application to finish bundling.

Before bundling, the Angular program contained 7,274 source files, including
declarations and Angular shims, from 16 explicit roots. Four connector spec
files were unintentionally included by `src/connectors/*.ts`; they generated
unused-source warnings. There was no broad include of all applications or
stories in the build config, and those four tests were not the measured
multi-GiB bottleneck. AOT/TypeScript diagnostics used about 1.06 GiB of V8 heap;
template diagnostics/emitter preparation brought this to about 1.54 GiB.
Development minification was already disabled. No extra compiler workers were
observed. The installed-version issue search did not establish a relevant
upstream memory regression, so no version change was justified.

| Configuration | Initial build | Sampled tree peak RSS | Exact cgroup peak | Watch outcome |
| --- | --- | --- | --- | --- |
| Existing checkout, 4 GiB cap, 4 GiB V8 old heap | Failed after 27.5 s | 3.84 GiB | 4.00 GiB (cap) | Kernel cgroup OOM |
| Two modules at a time, TS maps off, 2.5 GiB V8 heap | Failed at ~35 s | Not used for comparison | 2.88 GiB | V8 heap OOM during bundling |
| Generated-body noParse, filesystem cache, 2.5 GiB V8 heap | Completed in 42.5 s | Not used for comparison | 3.52 GiB | V8 OOM after build, during idle cache work |
| Final config, memory cache, 3 GiB V8 heap | **35.0 s** | **3.34 GiB** | **3.41 GiB** | Initial build and repeated edits passed |

The failed baseline's peak is censored by the cap: it is not a measurement of
the memory required for a successful original build. The final initial build
used an already populated Babel cache; the earlier first successful transformed
build took 42.5 s with that cache less populated. No successful unmodified
baseline duration exists. Do not interpret these runs as a controlled speedup
benchmark or assign an exact saving to each individual setting.

Four controlled TypeScript edits (renaming a local variable in `theme.ts`, then
restoring the original bytes) rebuilt successfully in **14.807, 4.372, 3.200,
and 3.336 seconds**. The restoration generated one extra watch notification,
also successful (4.708 s). RSS at the four controlled build completions was
**3.29, 3.10, 3.26, and 3.28 GiB**. The first rebuild refreshes Angular's
incremental program and costs more. The initial build used about 50.9 CPU
seconds (roughly 145% CPU over 35 seconds); the first rebuild used about
25.4 CPU seconds, and subsequent controlled rebuilds about 7.3, 5.5 and
5.3 CPU seconds. No swap or cgroup OOM events occurred in the final run.

The measured success covers this initial build and edit sequence, not an
hours-long session or an exhaustive browser regression test. Existing Angular
unused-variable/spec warnings remain. Cross-process webpack caching is traded
for avoiding the measured serialization spike; a restart still performs Angular
analysis. No production build was run or released for these tooling changes.

The final watch stayed alive for over four minutes, including approximately
two idle minutes after the last rebuild, and was then stopped successfully.
Its raw records are in
`/home/user/.cache/tutor-web-vault/tutor-web-vault-22945/`; investigation traces
are `/tmp/tutor-webpack-*.jsonl` and `/tmp/tutor-wasm-parse.log` (temporary).
The final sampled V8 heap peak was 2.70 GiB. Launcher checks also passed for
Python/JavaScript syntax, rejection of a concurrent invocation, one-shot exit,
watch shutdown and retention of systemd resource counters, using a tiny webpack
fixture rather than repeating the application build.

## Focused fallback verification

`verify-fallback.cjs` bundles the real SDK through the development loader,
checks that its generated body is unchanged and all **418 named exports** match
the original module, initializes the SDK and exercises random bytes, a v4 UUID,
and a fixed random range. It also checks that unfamiliar generator output is
rejected. It passed in a separate 1 GiB cgroup:

```sh
systemd-run --user --scope -p MemoryMax=1G -p MemorySwapMax=0 \
  /home/user/.local/node24/bin/node --max-old-space-size=768 \
  scripts/web-vault/verify-fallback.cjs /home/user/webvault-build
```

The split follows webpack's documented restriction that [noParse modules must
not contain imports/require dependencies](https://webpack.js.org/configuration/module/#modulenoparse).
The original ESM module cannot safely be marked `noParse` without the boundary.
Cache generation settings are documented in [webpack's cache reference](https://webpack.js.org/configuration/cache/).
