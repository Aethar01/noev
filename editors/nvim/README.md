# Noev in Neovim

Highlighting, indentation and folding from the tree-sitter grammar in
[`../tree-sitter-noev`](../tree-sitter-noev), and errors, hover, go to definition and
symbols from the language server built into `noev` (`noev lsp`). Needs Neovim 0.11 or later
and [nvim-treesitter](https://github.com/nvim-treesitter/nvim-treesitter) (`main` branch).

## Setup

With a checkout of this repository at `~/Projects/noev`, and `noev` on your `PATH`
(`cargo install --path crates/noev`):

```lua
-- This directory: filetype, queries, the language server's config.
vim.opt.rtp:append(vim.fn.expand('~/Projects/noev/editors/nvim'))

-- The parser, built by nvim-treesitter from the checkout.
vim.api.nvim_create_autocmd('User', {
  pattern = 'TSUpdate',
  callback = function()
    require('nvim-treesitter.parsers').noev = {
      install_info = {
        path = vim.fn.expand('~/Projects/noev/editors/tree-sitter-noev'),
      },
    }
  end,
})

vim.lsp.enable('noev')
```

Then run `:TSInstall noev` once, and `:TSUpdate noev` after the grammar changes.

## What you get

- **Highlighting:** keywords, ownership words (`mv`, `ref`, `mut`), kets and bras, types,
  functions and methods, fields, constants (`Float::PI`), strings with their interpolations.
- **Indentation and folding** by blocks.
- **Diagnostics as you type**, the same as `noev check`: errors with their codes (`N003`) and
  help, and warnings for runs that go shot by shot.
- **Hover:** the type of a variable or expression, or a function's signature.
- **Go to definition** (`gd`): variables, functions and methods, structs and fields.
- **Document symbols:** functions, structs, fields and methods.
