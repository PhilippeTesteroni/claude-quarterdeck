import tseslint from 'typescript-eslint';

// Minimal, CI-ready flat config. The full lint pass lands with the UI work (T4).
export default tseslint.config(
  {
    ignores: ['ui/dist/**', 'node_modules/**', 'target/**', 'src-tauri/target/**'],
  },
  ...tseslint.configs.recommended,
);
