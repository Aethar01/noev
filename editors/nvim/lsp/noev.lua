-- The Noev language server, for `vim.lsp.enable('noev')`. It is part of the `noev` binary.
---@type vim.lsp.Config
return {
  cmd = { 'noev', 'lsp' },
  filetypes = { 'noev' },
  root_markers = { '.git' },
}
