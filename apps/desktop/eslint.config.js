import js from "@eslint/js";
import globals from "globals";
import tseslint from "typescript-eslint";

export default tseslint.config(
  { ignores: ["dist", "release"] },
  js.configs.recommended,
  ...tseslint.configs.recommended,
  { files: ["**/*.{ts,mjs}"], languageOptions: { globals: globals.node } },
  { files: ["src/pages/**/*.ts"], languageOptions: { globals: globals.browser } },
);
