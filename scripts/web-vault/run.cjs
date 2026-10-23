const fs = require("node:fs");
const path = require("node:path");

const source = process.env.WEB_VAULT_SOURCE;
const config = require("./webpack.dev.cjs")();
const webpack = require(path.join(source, "node_modules/webpack"));
const compiler = webpack(config);
const started = performance.now();
let builds = 0;
function record(event, extra = {}) {
  const data = { event, seconds: (performance.now() - started) / 1000,
    ...process.memoryUsage(), cpu: process.cpuUsage(), ...extra };
  fs.appendFileSync(process.env.TUTOR_METRICS, JSON.stringify(data) + "\n");
}
const timer = setInterval(() => record("memory"), 2000);
timer.unref();
compiler.hooks.invalid.tap("Tutor", (file) => record("invalid", { file }));
compiler.hooks.done.tap("Tutor", (stats) => {
  const result = { build: ++builds, durationMs: stats.endTime - stats.startTime,
    errors: stats.hasErrors(), warnings: stats.compilation.warnings.length };
  record("build", result);
  console.log(`Tutor build ${builds}: ${result.durationMs} ms, errors=${result.errors}`);
});
const report = (error, stats) => {
  if (error) { console.error(error); process.exitCode = 1; }
  if (stats) {
    console.log(stats.toString({ all: false, errors: true, warnings: true, timings: true }));
    if (stats.hasErrors() && process.argv[2] === "once") process.exitCode = 1;
  }
};

let watcher;
let server;
async function stop() {
  clearInterval(timer);
  if (server) await server.stop();
  if (watcher) await new Promise((resolve) => watcher.close(resolve));
  await new Promise((resolve) => compiler.close(resolve));
}
process.once("SIGTERM", () => stop().catch((error) => { console.error(error); process.exitCode = 1; }));
process.once("SIGINT", () => stop().catch((error) => { console.error(error); process.exitCode = 1; }));
if (process.argv[2] === "once") {
  compiler.run((error, stats) => {
    report(error, stats);
    compiler.close((error) => { clearInterval(timer); if (error) report(error); record("closed"); });
  });
} else if (process.argv[2] === "serve") {
  const WebpackDevServer = require(path.join(source, "node_modules/webpack-dev-server"));
  server = new WebpackDevServer(config.devServer, compiler);
  server.start().catch((error) => { report(error); stop(); });
} else {
  watcher = compiler.watch({}, report);
}
