; Highlighting for Noev, with Neovim's capture names.

; Names, most general first: later patterns win.

(identifier) @variable

(parameter
  name: (identifier) @variable.parameter)

((identifier) @constant
  (#match? @constant "^[A-Z][A-Z0-9_]*$"))

(type_identifier) @type

((type_identifier) @type.builtin
  (#any-of? @type.builtin
    "Int" "Float" "Bool" "Bit" "String" "Qubit" "Observable" "Distribution"))

(self) @variable.builtin

(field_identifier) @variable.member

(field_expression
  field: (integer) @variable.member)

(place_field
  field: (integer) @variable.member)

; Paths: `Float::PI`, `Experiment::new`.
(path
  (identifier) @type
  (#match? @type "^[A-Z]"))

(path
  (identifier) @constant .
  (#match? @constant "^[A-Z][A-Z0-9_]*$"))

; Functions

(function_item
  name: (identifier) @function)

(function_signature_item
  name: (identifier) @function)

(call_expression
  function: (identifier) @function.call)

(call_expression
  function: (path
    (identifier) @function.call .))

; Conversions and observables look like calls to types: `Float(n)`, `Z(0)`.
(call_expression
  function: (identifier) @type.builtin
  (#any-of? @type.builtin "Int" "Float" "Bool" "Bit"))

(call_expression
  function: (identifier) @function.builtin
  (#any-of? @function.builtin "X" "Y" "Z" "I" "P" "PX" "print" "eprint" "measure" "swap"))

(method_identifier) @function.method.call

; Quantum literals

(ket) @string.special

(bra) @string.special

; Literals

(integer) @number

(float) @number.float

(boolean) @boolean

(string) @string

(escape_sequence) @string.escape

(interpolation
  "{" @punctuation.special
  "}" @punctuation.special)

(format_spec) @string.special

(line_comment) @comment

(block_comment) @comment

; Keywords

[
  "let"
  "struct"
  "trait"
  "impl"
  "type"
  "extern"
  "assert"
] @keyword

"fn" @keyword.function

[
  "if"
  "else"
] @keyword.conditional

[
  "while"
  "for"
  "in"
] @keyword.repeat

[
  "return"
  (break_statement)
  (continue_statement)
] @keyword.return

[
  "and"
  "or"
  "not"
] @keyword.operator

; Ownership, and the quantum words.
(ownership) @keyword.modifier

(mutable) @keyword.modifier

(reference_type
  [
    "ref"
    "mut"
  ] @keyword.modifier)

(modifier) @keyword.modifier

[
  "sample"
  "ctrl"
  "adj"
  "within"
  "with"
  "do"
  "unsafe"
] @keyword

; Operators and punctuation

[
  "+"
  "-"
  "*"
  "/"
  "%"
  "&"
  "|"
  "^"
  "~"
  "<<"
  ">>"
  "=="
  "!="
  "<"
  "<="
  ">"
  ">="
  "="
  "+="
  "-="
  "*="
  "/="
  "%="
  "&="
  "|="
  "^="
  "<<="
  ">>="
  "->"
  ".."
] @operator

[
  "("
  ")"
  "["
  "]"
  "{"
  "}"
] @punctuation.bracket

[
  ","
  ";"
  ":"
  "::"
  "."
] @punctuation.delimiter

(wildcard) @character.special
