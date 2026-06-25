/**
 * tree-sitter grammar for asdf/mise `.tool-versions` files.
 *
 * Format: one entry per line — `<tool> <version> [version...]` — plus `#`
 * comments and blank lines. Examples:
 *
 *   nodejs 20.11.1
 *   python 3.12.2 3.11.8   # multiple versions
 *   ruby   system
 *   # a comment line
 */
module.exports = grammar({
  name: 'tool_versions',

  // Spaces and tabs separate tokens; newlines are significant (handled below).
  extras: _ => [/[ \t]+/],

  rules: {
    source_file: $ => seq(repeat($._line), optional($._content)),

    // Every line ends in a newline; a line may be blank, an entry, or a comment.
    // `optional($._content)` in `source_file` allows a final line with no newline.
    _line: $ => seq(optional($._content), /\r?\n/),

    _content: $ => choice(
      seq($.entry, optional($.comment)),
      $.comment,
    ),

    entry: $ => seq(
      field('tool', $.word),
      repeat1(field('version', $.word)),
    ),

    // A bare token: not starting with whitespace or `#`.
    word: _ => /[^\s#]\S*/,

    comment: _ => /#[^\n]*/,
  },
});
