const path = require("node:path");

module.exports = () => {
  if (process.env.NODE_ENV === "production") {
    throw new Error("Tutor's development config must not be used for production builds.");
  }
  process.env.NODE_ENV = "development";
  process.env.ENV ??= "selfhosted";
  const source = path.resolve(process.env.WEB_VAULT_SOURCE || process.cwd());
  const web = path.join(source, "apps/web");
  // Upstream resolves tsconfig.build.json relative to the working directory.
  process.chdir(web);
  const config = require(path.join(web, "webpack.config.js"))();
  config.devtool = "cheap-module-source-map";
  config.parallelism = 2;
  config.module.rules.unshift({
    test: /[\\/]@bitwarden[\\/]sdk-internal[\\/]bitwarden_wasm_internal_bg\.wasm\.js$/,
    type: "javascript/auto",
    use: [{ loader: path.join(__dirname, "wasm-fallback-loader.cjs") }],
  });
  config.module.noParse = /[\\/]@bitwarden[\\/]sdk-internal[\\/]bitwarden_wasm_internal_bg\.wasm\.js\?tutor-wasm-body$/;
  // Serializing the full source/map pack after the initial build creates a
  // second memory spike. Watch mode already reuses Angular and webpack state;
  // Babel keeps its own disk cache for dependency transforms across restarts.
  config.cache = { type: "memory", maxGenerations: 1 };
  return config;
};
