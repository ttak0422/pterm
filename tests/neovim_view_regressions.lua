-- Pure UI-state regressions; no PTY or daemon required.
vim.opt.runtimepath:prepend(vim.fn.getcwd())
local views = require("pterm.view")
local buf = vim.api.nvim_create_buf(false, true)
vim.api.nvim_set_current_buf(buf)
local win = vim.api.nvim_get_current_win()
local previous
local function fixture()
	vim.api.nvim_set_current_buf(buf)
	if previous then
		previous.pending = nil
	end
	local lines = {}
	for i = 1, 60 do
		lines[i] = ("record-%02d"):format(i)
	end
	vim.api.nvim_buf_set_lines(buf, 0, -1, false, lines)
	vim.cmd("normal! 20Gzt")
	vim.api.nvim_win_set_cursor(win, { 20, 3 })
	local state = views.new(buf)
	previous = state
	state:save()
	return state, lines
end
local function settle(state)
	state:restore()
	vim.wait(20)
end
local state, lines = fixture()
vim.api.nvim_buf_set_lines(buf, 0, 0, false, { "new-first" })
vim.api.nvim_win_set_cursor(win, { 1, 0 })
settle(state)
assert(vim.fn.winsaveview().lnum == 21 and vim.fn.winsaveview().col == 3, "unique shifted anchor was not restored")

state, lines = fixture()
vim.list_extend(lines, { "record-19", "record-20", "record-21" })
vim.api.nvim_buf_set_lines(buf, 0, -1, false, lines)
vim.api.nvim_win_set_cursor(win, { 1, 0 })
settle(state)
assert(vim.fn.winsaveview().lnum == 1, "ambiguous duplicate anchor must not be guessed")

state = fixture()
vim.api.nvim_buf_set_lines(buf, 18, 21, false, { "reflowed", "or truncated" })
vim.api.nvim_win_set_cursor(win, { 1, 0 })
settle(state)
assert(vim.fn.winsaveview().lnum == 1, "missing anchor must not restore a stale row number")

state = fixture()
state:cancel()
vim.api.nvim_win_set_cursor(win, { 8, 0 })
settle(state)
assert(vim.fn.winsaveview().lnum == 8, "user input must cancel pending restoration")

state = fixture()
state:save() -- a superseding reset must keep the original anchor
vim.api.nvim_win_set_cursor(win, { 1, 0 })
settle(state)
assert(vim.fn.winsaveview().lnum == 20, "repeated reset replaced the original anchor")

state = fixture()
vim.api.nvim_set_current_buf(vim.api.nvim_create_buf(false, true))
settle(state)
assert(vim.fn.winsaveview().lnum == 1, "restoration touched an unrelated buffer")
state, lines = fixture()
vim.api.nvim_buf_set_lines(buf, 0, -1, false, { "" })
state:restore() -- completion can precede the native buffer refresh
vim.wait(20)
vim.api.nvim_buf_set_lines(buf, 0, -1, false, lines)
vim.wait(20)
assert(vim.fn.winsaveview().lnum == 20, "delayed native refresh lost the anchor")

print("Neovim view regressions passed")
vim.cmd("qa!")
