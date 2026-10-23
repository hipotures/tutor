// Focused runtime check for the only source transformation in webpack.dev.cjs.
// Run with Node 24: node verify-fallback.cjs /path/to/vw_web_builds
const assert = require("node:assert/strict");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const { pathToFileURL } = require("node:url");

async function main() {
  const source = path.resolve(process.argv[2]);
  const sdk = path.join(source, "node_modules/@bitwarden/sdk-internal");
  const fallback = path.join(sdk, "bitwarden_wasm_internal_bg.wasm.js");
  const input = fs.readFileSync(fallback, "utf8");
  const loader = require("./wasm-fallback-loader.cjs");
  const body = loader.call({ resourcePath: fallback, resourceQuery: "?tutor-wasm-body" }, input);
  const prefix = /^import\*as ([$\w]+) from"(\.\/bitwarden_wasm_internal_bg\.js)";/.exec(input)[0];
  assert(body.includes(input.slice(prefix.length, input.indexOf("export var "))));
  assert.throws(() => loader.call({ resourcePath: fallback }, "unexpected SDK format"));
  assert.throws(() => loader.call({ resourcePath: fallback }, input + "export var changed=other;"));

  process.env.WEB_VAULT_SOURCE = source;
  const config = require("./webpack.dev.cjs")();
  const webpack = require(path.join(source, "node_modules/webpack"));
  const temporary = fs.mkdtempSync(path.join(os.tmpdir(), "tutor-fallback-"));
  try {
    const entry = path.join(temporary, "entry.cjs");
    fs.writeFileSync(entry, `
      const sdk = require(${JSON.stringify(path.join(sdk, "index.js"))});
      exports.verify = async () => {
        const fallback = await import(${JSON.stringify(fallback)});
        sdk.init(fallback);
        sdk.init_sdk(undefined, undefined, 0);
        const random = new sdk.SdkRandomNumberClient();
        const bytes = random.gen_bytes(32);
        const uuid = random.gen_uuid();
        const fixed = random.gen_range(7, 7);
        random.free();
        return { names: Object.keys(fallback).filter(x => x !== "default").sort(), bytes, uuid, fixed };
      };
    `);
    const compiler = webpack({
      mode: "development", target: "node", devtool: false, entry,
      output: { path: temporary, filename: "test.cjs", library: { type: "commonjs2" } },
      module: { rules: [config.module.rules[0]], noParse: config.module.noParse },
      optimization: { minimize: false },
    });
    try {
      await new Promise((resolve, reject) => compiler.run((error, stats) => {
        if (error) reject(error);
        else if (stats.hasErrors()) reject(new Error(stats.toString({ all: false, errors: true })));
        else resolve();
      }));
    } finally {
      await new Promise((resolve, reject) => compiler.close(error => error ? reject(error) : resolve()));
    }
    const actual = await require(path.join(temporary, "test.cjs")).verify();
    const original = await import(pathToFileURL(fallback).href);
    assert.deepEqual(actual.names, Object.keys(original).sort());
    assert.equal(actual.bytes.length, 32);
    assert(actual.bytes.some(byte => byte !== 0));
    assert.match(actual.uuid, /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/);
    assert.equal(actual.fixed, 7);
    console.log(`PASS: unchanged generated body, ${actual.names.length} exports, SDK init, random bytes, UUID, fixed range; invalid formats rejected.`);
  } finally {
    fs.rmSync(temporary, { recursive: true, force: true });
  }
}

main().catch(error => { console.error(error); process.exitCode = 1; });
