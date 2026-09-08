import tsParser from "@typescript-eslint/parser";
import tsPlugin from "@typescript-eslint/eslint-plugin";
import reactHooks from "eslint-plugin-react-hooks";
import reactRefresh from "eslint-plugin-react-refresh";

export default [
  {
    ignores: [
      "dist/**",
      "node_modules/**",
      "src-tauri/**",
      "src/lib/bindings.ts",
      "src/lib/next-navigation-stub.ts",
      "*.config.js",
      "*.config.ts",
    ],
  },
  // The compatibility gate itself was outside every lint the repo has, which is a poor
  // place for a script whose whole job is to fail the build correctly. Node globals only;
  // no TypeScript rules, because these are plain ES modules.
  {
    files: ["scripts/**/*.mjs"],
    languageOptions: {
      ecmaVersion: "latest",
      sourceType: "module",
      globals: { console: "readonly", process: "readonly", URL: "readonly" },
    },
  },
  {
    files: ["src/**/*.{ts,tsx}"],
    languageOptions: {
      parser: tsParser,
      ecmaVersion: "latest",
      sourceType: "module",
      // Type-aware linting. It roughly doubles `pnpm lint` (3.2 s -> 6.7 s here) and is
      // what `no-floating-promises` needs -- in a codebase whose every backend call is an
      // async Tauri command, that is the single most valuable rule available.
      parserOptions: { ecmaFeatures: { jsx: true }, project: "./tsconfig.json" },
    },
    plugins: {
      "@typescript-eslint": tsPlugin,
      "react-hooks": reactHooks,
      "react-refresh": reactRefresh,
    },
    rules: {
      ...tsPlugin.configs.recommended.rules,
      ...reactHooks.configs.recommended.rules,
      "react-hooks/rules-of-hooks": "error",
      "react-hooks/exhaustive-deps": "warn",
      "react-refresh/only-export-components": ["warn", { allowConstantExport: true }],
      "@typescript-eslint/no-explicit-any": "warn",
      // A warning, not an error: the eighteen existing hits are mostly formik's
      // `setFieldValue`, which cannot reject, and triaging them is its own change.
      "@typescript-eslint/no-floating-promises": "warn",
      "@typescript-eslint/no-unused-vars": [
        "warn",
        { argsIgnorePattern: "^_", varsIgnorePattern: "^_" },
      ],
    },
  },
];
