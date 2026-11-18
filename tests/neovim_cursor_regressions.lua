-- Run after cargo build --release (needs a real PTY and Unix sockets):
--   nvim --headless -u NONE -c 'luafile tests/neovim_cursor_regressions.lua'
-- Use the normal event loop: -l/vim.wait alone do not execute startinsert.
vim.opt.runtimepath:prepend(vim.fn.getcwd())
local pterm = require("pterm")
local root = vim.fn.tempname()
vim.fn.mkdir(root, "p")
pterm.setup({ socket_dir = root, auto_redraw = true, auto_redraw_delay_ms = 1 })
vim.cmd("vnew")
local other = vim.api.nvim_get_current_win()
vim.cmd("wincmd p")
pterm.open("cursor-test", {
	"cursor-test",
	"sh",
	"-c",
	"i=1; while [ $i -le 100 ]; do echo history-$i; i=$((i+1)); done; cat",
})
local win = vim.api.nvim_get_current_win()
local buf = vim.api.nvim_get_current_buf()
local expected
local function assert_view()
	assert(vim.api.nvim_get_mode().mode ~= "t", "navigation unexpectedly entered terminal input")
	local view = vim.fn.winsaveview()
	for _, field in ipairs({ "lnum", "col", "topline", "leftcol" }) do
		assert(
			view[field] == expected[field],
			("navigation changed %s: %s -> %s"):format(field, expected[field], view[field])
		)
	end
end
local steps = {
	function()
		assert(vim.api.nvim_buf_line_count(buf) > 100, "fixture output was not received")
		assert(vim.api.nvim_get_mode().mode == "t", "new sessions must start in input mode")
		vim.cmd("stopinsert")
	end,
	function()
		vim.cmd("normal! 20Gzt")
		vim.api.nvim_win_set_cursor(win, { 20, 3 })
		expected = vim.fn.winsaveview()
		vim.api.nvim_set_current_win(other)
	end,
	function()
		vim.api.nvim_set_current_win(win)
	end,
	function()
		assert_view()
		vim.api.nvim_exec_autocmds("FocusGained", {})
	end,
	function()
		assert_view()
		pterm.open("cursor-test")
	end,
	function()
		assert_view()
		vim.api.nvim_set_current_win(other)
	end,
	function()
		vim.api.nvim_set_current_win(win)
	end,
	function()
		assert_view()
		-- Explicit input intentionally resumes at the application's live cursor.
		vim.cmd("startinsert")
	end,
	function()
		assert(vim.api.nvim_get_mode().mode == "t", "explicit input did not resume")
		assert(vim.fn.winsaveview().lnum > 20, "input retained the scrollback cursor")
		vim.cmd("stopinsert")
	end,
}
local step = 0
local function advance()
	step = step + 1
	local ok, err = pcall(function()
		if steps[step] then
			steps[step]()
		end
	end)
	if not ok or not steps[step] then
		pterm.kill("cursor-test")
		vim.fn.delete(root, "rf")
		if not ok then
			print(tostring(err))
			vim.cmd("cquit 1")
		else
			print("pterm cursor regressions passed")
			vim.cmd("qa!")
		end
		return
	end
	vim.defer_fn(advance, 250)
end
vim.defer_fn(advance, 1500)
