// wasm2js produces a large, dependency-free body. Keep its module boundary
// visible to webpack, but do not build a second AST for the generated body.
const BODY_QUERY = "?tutor-wasm-body";

module.exports = function wasmFallbackLoader(source) {
  const prefix = /^import\*as ([$\w]+) from"(\.\/bitwarden_wasm_internal_bg\.js)";/;
  const imported = prefix.exec(source);
  const boundary = source.indexOf("export var ");
  if (!imported || boundary < imported[0].length) {
    throw new Error("Unrecognized SDK wasm2js format; review the development loader after upgrading SDK.");
  }

  const suffix = source.slice(boundary).trim();
  const declaration = /export var ([$\w]+)=retasmFunc\.([$\w]+);/gy;
  const names = [];
  let offset = 0;
  while (offset < suffix.length) {
    declaration.lastIndex = offset;
    const match = declaration.exec(suffix);
    if (!match || match[1] !== match[2]) {
      throw new Error("Unrecognized SDK wasm2js exports; refusing to change fallback behavior.");
    }
    names.push(match[1]);
    offset = declaration.lastIndex;
  }

  if (this.resourceQuery === BODY_QUERY) {
    return `module.exports = function(${imported[1]}) {\n"use strict";\n${source.slice(imported[0].length, boundary)}\nreturn retasmFunc;\n};\n`;
  }

  // The factory receives the very same bindings namespace as the original
  // module. No eval, new Function, extra network dependency, or SDK stub.
  return [
    `import * as bindings from ${JSON.stringify(imported[2])};`,
    `import instantiate from ${JSON.stringify("./" + require("node:path").basename(this.resourcePath) + BODY_QUERY)};`,
    "const result = instantiate(bindings);",
    ...names.map((name) => `export const ${name} = result.${name};`),
  ].join("\n");
};
