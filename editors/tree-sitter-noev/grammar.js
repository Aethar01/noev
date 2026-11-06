/**
 * @file Tree-sitter grammar for Noev (spec/grammar.md), for editors: highlighting, folding,
 * indentation and text objects. The compiler's own parser is the authority; this grammar
 * accepts the same programs, but treats line breaks as whitespace.
 */

/* eslint-disable no-undef */

const PREC = {
  or: 1,
  and: 2,
  not: 3,
  compare: 4,
  bitor: 5,
  bitxor: 6,
  bitand: 7,
  shift: 8,
  add: 9,
  mul: 10,
  unary: 11,
  postfix: 12,
  path: 13,
};

const sepBy = (sep, rule) => optional(seq(rule, repeat(seq(sep, rule)), optional(sep)));

module.exports = grammar({
  name: 'noev',

  extras: ($) => [/\s/, $.line_comment, $.block_comment],

  // A `<` starting a line (src/scanner.c).
  externals: ($) => [$._line_start_angle],

  word: ($) => $.identifier,

  // `if ready { … }`: `ready { … }` could start a struct literal; the parser keeps both
  // readings until one fails.
  conflicts: ($) => [
    [$._primary, $.struct_expression],
    [$._contextual_name, $.ownership],
    // `<s| x`: a measurement test, unless a ket follows and makes it an expectation value.
    [$.measurement_test, $._observable],
    [$._contextual_name, $.with_expression],
    [$._contextual_name, $.ctrl_expression],
  ],

  rules: {
    source_file: ($) => repeat($._item),

    _item: ($) => choice($.struct_item, $.function_item, $.trait_item, $.impl_item, $.extern_item),

    // Items

    struct_item: ($) =>
      seq(
        'struct',
        field('name', alias($.identifier, $.type_identifier)),
        '{',
        repeat(choice($.field_declaration, $.function_item, ',')),
        '}',
      ),

    field_declaration: ($) =>
      seq(field('name', alias($.identifier, $.field_identifier)), ':', field('type', $._type)),

    function_item: ($) => seq($._function_signature, field('body', $.block)),

    _function_signature: ($) =>
      seq(
        optional(alias('unsafe', $.modifier)),
        optional(alias('rev', $.modifier)),
        optional(alias('circuit', $.modifier)),
        'fn',
        field('name', $.identifier),
        field('parameters', $.parameters),
        optional(seq('->', field('return_type', $._type))),
      ),

    function_signature_item: ($) => $._function_signature,

    parameters: ($) => seq('(', sepBy(',', choice($.self_parameter, $.parameter)), ')'),

    self_parameter: ($) => seq($.ownership, $.self),

    parameter: ($) => seq(field('name', $.identifier), ':', field('type', $._type)),

    trait_item: ($) =>
      seq(
        'trait',
        field('name', alias($.identifier, $.type_identifier)),
        '{',
        repeat(choice($.function_item, $.function_signature_item, seq('type', $.identifier), ';')),
        '}',
      ),

    impl_item: ($) =>
      seq(
        'impl',
        field('trait', $.path),
        'for',
        field('type', $._type),
        '{',
        repeat(choice($.function_item, seq('type', $.identifier, '=', $._type), ';')),
        '}',
      ),

    extern_item: ($) =>
      seq(
        'extern',
        choice(
          $.function_signature_item,
          seq($.string, '{', repeat(choice($.function_signature_item, ';')), '}'),
        ),
      ),

    // Types

    _type: ($) =>
      choice($.type_path, $.generic_type, $.reference_type, $.unit_type, $.tuple_type, $.array_type),

    type_path: ($) =>
      prec.right(
        seq(
          choice(alias($.identifier, $.type_identifier), alias('Self', $.type_identifier)),
          repeat(seq('::', alias($.identifier, $.type_identifier))),
        ),
      ),

    generic_type: ($) => seq($.type_path, '<', sepBy(',', $._type), '>'),

    reference_type: ($) => prec.right(seq(choice('ref', 'mut'), $._type)),

    unit_type: (_) => seq('(', ')'),

    tuple_type: ($) => seq('(', $._type, ',', sepBy(',', $._type), ')'),

    array_type: ($) => seq('[', $._type, ';', $.integer, ']'),

    // Statements

    block: ($) => seq('{', repeat(choice($._statement, ';')), '}'),

    _statement: ($) =>
      choice(
        $.let_declaration,
        $.assignment,
        $.return_statement,
        $.break_statement,
        $.continue_statement,
        $.assert_statement,
        $._expression,
      ),

    let_declaration: ($) =>
      seq(
        'let',
        optional(alias('mut', $.mutable)),
        field('pattern', $._pattern),
        optional(seq(':', field('type', $._type))),
        '=',
        field('value', $._expression),
      ),

    assignment: ($) =>
      prec.right(
        seq(
          field('left', $._expression),
          field('operator', choice('=', '+=', '-=', '*=', '/=', '%=', '&=', '|=', '^=', '<<=', '>>=')),
          field('right', $._expression),
        ),
      ),

    return_statement: ($) => prec.right(seq('return', optional($._expression))),

    break_statement: (_) => 'break',

    continue_statement: (_) => 'continue',

    assert_statement: ($) => seq('assert', $._expression),

    _pattern: ($) =>
      choice($.identifier, $._contextual_name, $.wildcard, $.tuple_pattern, $.array_pattern),

    // Contextual keywords are ordinary names everywhere else (`let adj = …`).
    _contextual_name: ($) =>
      alias(choice('mv', 'rev', 'circuit', 'adj', 'ctrl', 'with', 'do', 'within'), $.identifier),

    wildcard: (_) => '_',

    tuple_pattern: ($) => seq('(', sepBy(',', $._pattern), ')'),

    array_pattern: ($) => seq('[', sepBy(',', $._pattern), ']'),

    // Expressions

    _expression: ($) =>
      choice(
        $.binary_expression,
        $.not_expression,
        $._unary_like,
      ),

    _unary_like: ($) =>
      choice(
        $.unary_expression,
        $.ownership_expression,
        $.adj_expression,
        $.measurement_test,
        $._postfix,
      ),

    binary_expression: ($) => {
      const table = [
        [PREC.or, 'or'],
        [PREC.and, 'and'],
        [PREC.compare, choice('==', '!=', '<', '<=', '>', '>=')],
        [PREC.bitor, '|'],
        [PREC.bitxor, '^'],
        [PREC.bitand, '&'],
        [PREC.shift, choice('<<', '>>')],
        [PREC.add, choice('+', '-')],
        [PREC.mul, choice('*', '/', '%')],
      ];
      return choice(
        ...table.map(([p, op]) =>
          prec.left(
            p,
            seq(field('left', $._expression), field('operator', op), field('right', $._expression)),
          ),
        ),
      );
    },

    not_expression: ($) => prec(PREC.not, seq('not', $._expression)),

    unary_expression: ($) => prec(PREC.unary, seq(choice('-', '~'), $._expression)),

    ownership: (_) => choice('mv', 'ref', 'mut'),

    ownership_expression: ($) => prec.dynamic(1, prec(PREC.unary, seq($.ownership, $._postfix))),

    adj_expression: ($) => prec(PREC.unary, seq('adj', $._postfix)),

    // The operand is unary, so `<1| mv q and ok` is `(<1| mv q) and ok`.
    measurement_test: ($) => seq($.bra, $._unary_like),

    _postfix: ($) =>
      choice(
        $.field_expression,
        $.index_expression,
        $.call_expression,
        $.method_call_expression,
        $._primary,
      ),

    field_expression: ($) =>
      prec(
        PREC.postfix,
        seq(
          field('value', $._postfix),
          '.',
          field('field', choice(alias($.identifier, $.field_identifier), $.integer)),
        ),
      ),

    index_expression: ($) =>
      prec(PREC.postfix, seq(field('value', $._postfix), '[', field('index', $._expression), ']')),

    call_expression: ($) =>
      prec(PREC.postfix, seq(field('function', $._postfix), field('arguments', $.arguments))),

    method_call_expression: ($) =>
      prec(
        PREC.postfix + 1,
        seq(
          field('receiver', $._postfix),
          '.',
          field('method', alias($.identifier, $.method_identifier)),
          field('arguments', $.arguments),
        ),
      ),

    arguments: ($) => seq('(', sepBy(',', $._expression), ')'),

    _primary: ($) =>
      choice(
        $.integer,
        $.float,
        $.boolean,
        $.string,
        $.ket,
        $.identifier,
        $._contextual_name,
        $.path,
        $.self,
        $.struct_expression,
        $.parenthesized_expression,
        $.unit,
        $.tuple_expression,
        $.array_expression,
        $.array_repeat_expression,
        $.expectation,
        $.bra_expectation,
        $.if_expression,
        $.while_expression,
        $.for_expression,
        $.sample_expression,
        $.ctrl_expression,
        $.with_expression,
        $.unsafe_block,
        $.block,
      ),

    path: ($) =>
      prec.right(
        PREC.path,
        seq(
          choice($.identifier, alias('Self', $.type_identifier)),
          repeat1(seq('::', $.identifier)),
        ),
      ),

    struct_expression: ($) =>
      seq(
        field('name', alias($.identifier, $.type_identifier)),
        '{',
        sepBy(',', $.field_initializer),
        '}',
      ),

    field_initializer: ($) =>
      seq(field('field', alias($.identifier, $.field_identifier)), optional(seq(':', $._expression))),

    parenthesized_expression: ($) => seq('(', $._expression, ')'),

    unit: (_) => seq('(', ')'),

    tuple_expression: ($) => seq('(', $._expression, ',', sepBy(',', $._expression), ')'),

    array_expression: ($) => seq('[', sepBy(',', $._expression), ']'),

    array_repeat_expression: ($) => seq('[', $._expression, ';', $._expression, ']'),

    // `<ψ| H |ψ>`, and `<+| H |+>` with a bra and a ket.
    expectation: ($) =>
      prec.right(
        seq(
          choice('<', $._line_start_angle),
          field('preparation', $._postfix),
          '|',
          field('observable', $._observable),
          '|',
          $._postfix,
          '>',
          optional($.within),
        ),
      ),

    // A ket after `<s| …` closes an expectation value: it never starts a new statement.
    bra_expectation: ($) =>
      prec.dynamic(
        1,
        prec.right(seq($.bra, field('observable', $._observable), $.ket, optional($.within))),
      ),

    within: ($) => seq('within', $._unary_like),

    // An observable has no `|` or comparison at its top level.
    _observable: ($) => choice($.observable_expression, $._unary_like),

    observable_expression: ($) => {
      const table = [
        [PREC.bitxor, '^'],
        [PREC.bitand, '&'],
        [PREC.shift, choice('<<', '>>')],
        [PREC.add, choice('+', '-')],
        [PREC.mul, choice('*', '/', '%')],
      ];
      return choice(
        ...table.map(([p, op]) =>
          prec.left(p, seq(field('left', $._observable), field('operator', op), field('right', $._observable))),
        ),
      );
    },

    if_expression: ($) =>
      prec.right(
        seq(
          'if',
          field('condition', $._expression),
          field('consequence', $.block),
          optional(seq('else', field('alternative', choice($.block, $.if_expression)))),
        ),
      ),

    while_expression: ($) => seq('while', field('condition', $._expression), field('body', $.block)),

    for_expression: ($) =>
      seq(
        'for',
        field('variable', $.identifier),
        'in',
        field('start', $._expression),
        '..',
        field('end', $._expression),
        field('body', $.block),
      ),

    sample_expression: ($) => seq('sample', field('shots', $._expression), field('body', $.block)),

    // Contextual keywords are preferred to the same word as a name when both parse.
    ctrl_expression: ($) =>
      prec.dynamic(1, seq('ctrl', $._place, repeat(seq(',', $._place)), field('body', $.block))),

    // A variable, field or element: what `ctrl` takes.
    _place: ($) => choice($.identifier, $.self, $.place_field, $.place_index),

    place_field: ($) =>
      prec(
        PREC.postfix,
        seq(
          field('value', $._place),
          '.',
          field('field', choice(alias($.identifier, $.field_identifier), $.integer)),
        ),
      ),

    place_index: ($) =>
      prec(PREC.postfix, seq(field('value', $._place), '[', field('index', $._expression), ']')),

    with_expression: ($) => prec.dynamic(1, seq('with', $.block, 'do', $.block)),

    unsafe_block: ($) => seq('unsafe', $.block),

    // Tokens

    identifier: (_) => /[A-Za-z_][A-Za-z0-9_]*/,

    self: (_) => 'self',

    boolean: (_) => choice('true', 'false'),

    integer: (_) => token(choice(/0x[0-9a-fA-F_]+/, /0b[01_]+/, /[0-9][0-9_]*/)),

    float: (_) => token(choice(/[0-9][0-9_]*\.[0-9][0-9_]*([eE][+-]?[0-9]+)?/, /[0-9][0-9_]*[eE][+-]?[0-9]+/)),

    ket: (_) => token(seq('|', /[01+\-]+/, '>')),


    bra: (_) => token(seq('<', /[01+\-]+/, '|')),

    string: ($) =>
      seq(
        '"',
        repeat(choice($.string_content, $.escape_sequence, alias('{{', $.escape_sequence), $.interpolation)),
        token.immediate('"'),
      ),

    string_content: (_) => token.immediate(prec(1, /[^"\\{]+/)),

    escape_sequence: (_) => token.immediate(seq('\\', choice(/[ntr0"\\]/, /u\{[0-9a-fA-F]+\}/))),

    interpolation: ($) =>
      seq(
        token.immediate('{'),
        $._expression,
        optional(seq(':', alias(token.immediate(/[^}]*/), $.format_spec))),
        '}',
      ),

    line_comment: (_) => token(seq('//', /[^\n]*/)),

    block_comment: (_) => token(seq('/*', /[^*]*\*+([^/*][^*]*\*+)*/, '/')),
  },
});
