// Rock syntax for Monaco: a small Monarch grammar plus brackets and comments.
// Keywords follow crates/rock-tokenizer (keyword_or_identifier).

export const rockLanguageConfiguration = {
  comments: { lineComment: '//', blockComment: ['/*', '*/'] },
  brackets: [['{', '}'], ['[', ']'], ['(', ')']],
  autoClosingPairs: [
    { open: '{', close: '}' },
    { open: '[', close: ']' },
    { open: '(', close: ')' },
    { open: '"', close: '"', notIn: ['string'] },
  ],
};

export const rockMonarch = {
  defaultToken: '',
  keywords: [
    'fn', 'operator', 'table', 'type', 'as', 'yield', 'await', 'if', 'let', 'return', 'defer',
    'struct', 'newtype', 'value', 'else', 'for', 'in', 'while', 'loop', 'break', 'continue',
    'enum', 'match', 'extern', 'global', 'const', 'deco', 'macro',
  ],
  constants: ['true', 'false', 'null'],
  builtinTypes: ['int', 'float', 'bool', 'string', 'void', 'any'],

  tokenizer: {
    root: [
      [/@[a-zA-Z_]\w*/, 'tag'],
      [/#[a-zA-Z_]\w*/, 'keyword.directive'],
      [/[A-Z]\w*/, 'type.identifier'],
      [/[a-z_]\w*/, {
        cases: {
          '@keywords': 'keyword',
          '@constants': 'constant',
          '@builtinTypes': 'type',
          '@default': 'identifier',
        },
      }],
      { include: '@whitespace' },
      [/[{}()[\]]/, '@brackets'],
      [/\d+\.\d+([eE][-+]?\d+)?/, 'number.float'],
      [/0x[0-9a-fA-F_]+/, 'number.hex'],
      [/\d[\d_]*/, 'number'],
      [/"/, 'string', '@string'],
      [/[=<>!~?:&|+\-*/^%.,;]+/, 'operator'],
    ],
    whitespace: [
      [/\s+/, 'white'],
      [/\/\*/, 'comment', '@comment'],
      [/\/\/.*$/, 'comment'],
    ],
    comment: [
      [/[^/*]+/, 'comment'],
      [/\*\//, 'comment', '@pop'],
      [/[/*]/, 'comment'],
    ],
    // "text {expr} text": interpolations are highlighted as code.
    string: [
      [/[^\\"{]+/, 'string'],
      [/\\./, 'string.escape'],
      [/\{/, 'delimiter.bracket', '@interpolation'],
      [/"/, 'string', '@pop'],
    ],
    interpolation: [
      [/\}/, 'delimiter.bracket', '@pop'],
      { include: '@root' },
    ],
  },
};
