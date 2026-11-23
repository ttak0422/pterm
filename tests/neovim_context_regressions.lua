-- Run with: nvim --headless -u NONE -i NONE -l tests/neovim_context_regressions.lua
vim.opt.runtimepath:prepend(vim.fn.getcwd())

-- Use actual Neovim windows/options and on-disk payloads; mock the bridge
-- and socket identity so this check also runs without Unix sockets.
-- selene: allow(incorrect_standard_library_use)
vim.fn.executable = function() return 1 end
local job = 0
-- selene: allow(incorrect_standard_library_use)
vim.fn.jobstart = function(command, opts)
	job = job + 1
	if opts.term then
		assert(opts.env.PTERM_SESSION_CONTEXT == "1")
	end
	if command[2] == "resize" then
		local id = job
		vim.schedule(function() opts.on_exit(id, 0, "exit") end)
	end
	return job
end
-- selene: allow(incorrect_standard_library_use)
vim.fn.jobstop = function() return 1 end

local root = vim.fn.tempname()
local socket_dir = root .. "/session"
local workdir = root .. "/ spaces %# | tail "
vim.fn.mkdir(socket_dir, "p")
vim.fn.mkdir(workdir, "p")
local socket = socket_dir .. "/socket"
local lstat = vim.uv.fs_lstat
local stat = { type = "socket", dev = 1, ino = 2 }
-- selene: allow(incorrect_standard_library_use)
vim.uv.fs_lstat = function(path)
	if path == socket then
		return stat
	end
	return lstat(path)
end
local function write(path, bytes)
	local file = assert(io.open(path, "wb"))
	assert(file:write(bytes))
	assert(file:close())
end

local function snapshot(cwd, foreground, inode)
	write(socket_dir .. "/.pterm-context", vim.json.encode({
		dev = tostring(stat.dev),
		ino = inode or tostring(stat.ino),
		cwd = cwd,
		foreground = foreground,
	}))
end

local pterm = require("pterm")
pterm.setup({ socket_dir = root, sync_cwd = true, foreground_filetypes = { tig = "pterm-tig" }, auto_redraw = false })
local function check()
	local cwd = vim.fn.getcwd()
	vim.cmd("vsplit")
	local other = vim.api.nvim_get_current_win()
	vim.cmd("wincmd p")
	local owner = vim.api.nvim_get_current_win()
	pterm.open("session")
	local buf = vim.api.nvim_get_current_buf()
	vim.api.nvim_set_option_value("filetype", "before-tig", { buf = buf })

	snapshot(workdir, "tig")
	assert(vim.wait(1000, function()
		local context = pterm.buffer_context(buf)
		return context and context.foreground == "tig" and vim.api.nvim_get_option_value("filetype", { buf = buf }) == "pterm-tig"
	end), "foreground did not reach pterm buffer")
	assert(vim.fn.getcwd() == workdir, vim.inspect({ vim.fn.getcwd(), workdir }))
	assert(vim.fn.getcwd(-1, 0) == cwd, "tab cwd changed")
	assert(vim.fn.getcwd(vim.api.nvim_win_get_number(other), 0) == cwd, "unrelated window cwd changed")
	assert(vim.api.nvim_get_current_win() == owner, "owner window lost focus")

	vim.api.nvim_set_current_win(other)
	snapshot(workdir, "bash")
	assert(vim.wait(1000, function() return vim.api.nvim_get_option_value("filetype", { buf = buf }) == "before-tig" end), "original filetype not restored")
	assert(vim.fn.getcwd() == cwd, "unrelated focused window cwd changed")
	snapshot(workdir, "tig", "invalid")
	assert(vim.wait(1000, function() return pterm.buffer_context(buf) == nil end), "stale socket snapshot accepted")
	assert(vim.api.nvim_get_option_value("filetype", { buf = buf }) == "before-tig")
end

local ok, err = pcall(check)
vim.fn.delete(root, "rf")
assert(ok, err)
