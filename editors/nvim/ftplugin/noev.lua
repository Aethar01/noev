-- Editing Noev: comments, indentation, and tree-sitter highlighting once the parser is
-- installed (see editors/nvim/README.md).
vim.bo.commentstring = '// %s'
vim.bo.comments = 's1:/*,mb:*,ex:*/,://'
vim.bo.expandtab = true
vim.bo.shiftwidth = 4
vim.bo.tabstop = 4

if pcall(vim.treesitter.start) then
  vim.bo.indentexpr = "v:lua.require'nvim-treesitter'.indentexpr()"
  vim.wo.foldmethod = 'expr'
  vim.wo.foldexpr = 'v:lua.vim.treesitter.foldexpr()'
  vim.wo.foldlevel = 99
end
