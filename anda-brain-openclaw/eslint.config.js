import js from '@eslint/js'
import tseslint from 'typescript-eslint'
import importPlugin from 'eslint-plugin-import'
import prettierRecommended from 'eslint-plugin-prettier/recommended'

export default tseslint.config(
  { ignores: ['dist', 'node_modules', 'examples', 'scripts'] },
  js.configs.recommended,
  tseslint.configs.recommended,
  importPlugin.flatConfigs.recommended,
  {
    // Type-aware rules are limited to files covered by tsconfig.json.
    files: ['src/**/*.ts'],
    languageOptions: {
      parserOptions: {
        project: 'tsconfig.json',
        tsconfigRootDir: import.meta.dirname
      }
    },
    rules: {
      '@typescript-eslint/consistent-type-exports': [
        'error',
        { fixMixedExportsWithInlineTypeSpecifier: true }
      ],
      '@typescript-eslint/consistent-type-imports': [
        'error',
        { fixStyle: 'inline-type-imports' }
      ]
    }
  },
  {
    rules: {
      '@typescript-eslint/no-empty-function': 'off',
      '@typescript-eslint/no-empty-interface': 'off',
      '@typescript-eslint/no-unused-vars': 'off',
      'import/named': 'off',
      'import/newline-after-import': 'error',
      'import/no-unresolved': 'off',
      'import/order': [
        'error',
        {
          groups: [
            ['builtin', 'external', 'internal'],
            'parent',
            ['sibling', 'index']
          ],
          'newlines-between': 'never',
          alphabetize: { order: 'ignore' }
        }
      ],
      'no-console': 'warn',
      'no-restricted-imports': [
        'error',
        {
          'paths': []
        }
      ],
      'no-useless-rename': 'error',
      'object-shorthand': ['error', 'always']
    },
    settings: {
      'import/internal-regex': '^#'
    }
  },
  prettierRecommended
)
