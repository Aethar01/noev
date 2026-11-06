# tree-sitter-noev

A [tree-sitter](https://tree-sitter.github.io) grammar for Noev, for editors: highlighting,
indentation, folding and text objects. It accepts the programs of `spec/grammar.md`, but it
is not the compiler's parser and does not report errors the way `noev check` does.

- `grammar.js` is the grammar; `src/scanner.c` handles the one rule tree-sitter can't
  express on its own (a line starting with `<` starts a new statement).
- `src/` holds the generated parser, committed so editors can build it without the
  tree-sitter CLI. Regenerate it with `tree-sitter generate` after changing `grammar.js`.
- The queries live in [`../nvim/queries/noev`](../nvim/queries/noev), with Neovim's capture
  names.

```
tree-sitter generate                  # after editing grammar.js
tree-sitter test                      # test/corpus
tree-sitter parse -p . ../../examples/*.nv
```

CI checks that `src/` is up to date, that the corpus passes, and that every `.nv` file in the
repository parses without errors.
