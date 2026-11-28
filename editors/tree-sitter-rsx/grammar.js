/**
 * Tree-sitter grammar for Yawn `.rsx`.
 *
 * Rust fragments are opaque tokens produced by the external scanner (src/scanner.c), which
 * balances delimiters and skips strings and comments; editors inject the Rust grammar into
 * them (queries/injections.scm). The markup is parsed here. Tags are flat (open, close and
 * self-closing tags are separate nodes), which is enough for highlighting and tolerant of
 * void elements and unfinished code.
 */
module.exports = grammar({
  name: 'rsx',

  externals: $ => [
    $.rust_toplevel,
    $.rust_setup,
    $.rust_expr,
    $.rust_head,
    $.rust_for_head,
    $.css_content,
    $._string_text,
    $.rust_type,
  ],

  extras: $ => [/\s/, $.comment],

  rules: {
    source_file: $ => repeat(choice($.rust_toplevel, $.component)),

    comment: _ => token(choice(seq('//', /[^\n]*/), seq('/*', /[^*]*\*+([^/*][^*]*\*+)*/, '/'))),

    component: $ => seq(
      optional('pub'),
      'component',
      field('name', $.component_name),
      optional($.type_parameters),
      optional($.params),
      optional($.where_clause),
      '{',
      optional($.rust_setup),
      optional($.style_block),
      $.view_block,
      optional($.style_block),
      '}',
    ),

    component_name: _ => /[A-Z][A-Za-z0-9_]*/,

    // `<T: Clone, F: Fn(T) -> View>`, each parameter an opaque token like a prop's type
    type_parameters: $ => seq('<', $.rust_type, repeat(seq(',', $.rust_type)), optional(','), '>'),

    where_clause: $ => seq('where', $.rust_type, repeat(seq(',', $.rust_type)), optional(',')),

    params: $ => seq('(', optional(seq($.param, repeat(seq(',', $.param)), optional(','))), ')'),

    // `name: Type`. The type is an opaque token (it can hold commas inside `<>` and `()`).
    param: $ => seq(optional('mut'), field('name', $.param_name), ':', field('type', $.rust_type)),

    param_name: _ => /[A-Za-z_][A-Za-z0-9_]*/,

    style_block: $ => seq('style', '{', optional($.css_content), '}'),

    view_block: $ => seq('view', '{', repeat($._node), '}'),

    _node: $ => choice(
      $.start_tag,
      $.end_tag,
      $.self_closing_tag,
      $.string,
      $.expression,
      $.if_node,
      $.for_node,
    ),

    start_tag: $ => seq('<', $._tag_name, repeat($.attribute), '>'),
    self_closing_tag: $ => seq('<', $._tag_name, repeat($.attribute), '/>'),
    end_tag: $ => seq('</', $._tag_name, '>'),

    _tag_name: $ => choice($.component_tag, $.tag_name),
    tag_name: _ => /[a-z][a-z0-9-]*/,
    component_tag: _ => /[A-Z][A-Za-z0-9_]*/,

    attribute: $ => seq(
      field('name', choice($.directive, $.attribute_name)),
      optional(seq('=', field('value', choice($.string, $.expression)))),
    ),

    directive: _ => token(/(on|class|prop):[A-Za-z_][A-Za-z0-9_-]*/),
    attribute_name: _ => /[A-Za-z_][A-Za-z0-9_-]*/,

    expression: $ => seq('{', optional($.rust_expr), '}'),

    string: $ => seq('"', repeat(choice($.string_content, $.interpolation, $.escape)), token.immediate('"')),
    string_content: $ => $._string_text,
    escape: _ => token.immediate(/\\./),
    interpolation: $ => seq(token.immediate('{'), optional($.rust_expr), '}'),

    if_node: $ => seq('if', $.rust_head, '{', repeat($._node), '}', optional($.else_clause)),
    else_clause: $ => seq('else', choice($.if_node_nested, seq('{', repeat($._node), '}'))),
    if_node_nested: $ => seq('if', $.rust_head, '{', repeat($._node), '}', optional($.else_clause)),

    for_node: $ => seq('for', $.rust_for_head, 'key', $.rust_head, '{', repeat($._node), '}'),
  },
});
