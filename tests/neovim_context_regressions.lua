-- Run with: nvim --headless -u NONE -i NONE -l tests/neovim_context_regressions.lua
vim.opt.runtimepath:prepend(vim.fn.getcwd())

-- Use real Neovim windows and mocked handles; the cloud can run without
-- creating Unix sockets or launching a bridge.
-- selene: allow(incorrect_standard_library_use)
vim.fn.executable = function()
	return 1
end
local job = 0
-- selene: allow(incorrect_standard_library_use)
vim.fn.jobstart = function(command, opts)
	job = job + 1
	if command[2] == "resize" then
		local id = job
		vim.schedule(function()
			opts.on_exit(id, 0, "exit")
		end)
	end
	return job
end
-- selene: allow(incorrect_standard_library_use)
vim.fn.jobstop = function()
	return 1
end

local root = vim.fn.tempname()
local socket_dir = root .. "/session"
local workdir = root .. "/ spaces %# | tail "
vim.fn.mkdir(socket_dir, "p")
vim.fn.mkdir(workdir, "p")
local socket = socket_dir .. "/socket"
local fs_stat = vim.uv.fs_stat
-- selene: allow(incorrect_standard_library_use)
vim.uv.fs_stat = function(path)
	if path == socket then
		return { type = "socket" }
	end
	return fs_stat(path)
end

local real_context = require("pterm.context")
local on_context, closed
package.loaded["pterm.context"] = {
	subscribe = function(path, fields, callback)
		assert(path == socket and fields == 3, "wrong subscription path or field mask")
		on_context = callback
		return function()
			closed = true
		end
	end,
}

local pterm = require("pterm")
pterm.setup({ socket_dir = root, sync_cwd = true, track_foreground = true, auto_redraw = false })
local function check()
	local cwd = vim.fn.getcwd()
	vim.cmd("vsplit")
	local other = vim.api.nvim_get_current_win()
	vim.cmd("wincmd p")
	local owner = vim.api.nvim_get_current_win()
	pterm.open("session")
	local buf = vim.api.nvim_get_current_buf()
	local ft = vim.api.nvim_get_option_value("filetype", { buf = buf })

	on_context({ cwd = workdir, foreground = "tig" })
	local context = pterm.buffer_context(buf)
	assert(context and context.foreground == "tig" and context.cwd == workdir, "context not cached")
	assert(vim.api.nvim_get_option_value("filetype", { buf = buf }) == ft, "pterm must not change filetype")
	assert(vim.fn.getcwd() == workdir, vim.inspect({ vim.fn.getcwd(), workdir }))
	assert(vim.fn.getcwd(-1, 0) == cwd, "tab cwd changed")
	assert(vim.fn.getcwd(vim.api.nvim_win_get_number(other), 0) == cwd, "unrelated window cwd changed")
	assert(vim.api.nvim_get_current_win() == owner, "owner window lost focus")

	vim.api.nvim_set_current_win(other)
	on_context({ cwd = cwd, foreground = "bash" })
	assert(pterm.buffer_context(buf).foreground == "bash", "foreground transition not cached")
	assert(vim.fn.getcwd() == cwd, "unrelated focused window cwd changed")
	assert(vim.fn.getcwd(vim.api.nvim_win_get_number(owner), 0) == workdir, "inactive owner cwd changed")
	on_context(nil)
	assert(pterm.buffer_context(buf) == nil, "context must clear on EOF")
	pterm.detach("session")
	assert(closed, "metadata subscription leaked")
	on_context({ cwd = workdir, foreground = "tig" })
	assert(pterm.buffer_context(buf) == nil, "stale callback revived detached buffer")
end

local ok, err = pcall(check)
vim.fn.delete(root, "rf")
assert(ok, err)

local read, closed_pipe
local pipe = {
	connect = function(_, path, callback)
		assert(path == socket, "IPC did not use the dedicated socket")
		callback()
	end,
	read_start = function(_, callback)
		read = callback
	end,
	write = function(_, data, callback)
		assert(data == string.char(0x0d, 1, 0, 0, 0, 3), "incorrect subscription frame")
		callback()
	end,
	is_closing = function()
		return closed_pipe
	end,
	close = function()
		closed_pipe = true
	end,
}
-- selene: allow(incorrect_standard_library_use)
vim.uv.new_pipe = function()
	return pipe
end
local received
real_context.subscribe(socket, 3, function(data)
	received = data
end)
local payload = '{"cwd":"/synthetic","foreground":"tig"}'
-- Discard one initial snapshot in pieces, then decode a fragmented context
-- frame on the same connection without interpreting snapshot bytes.
read(nil, string.char(0x80, 3, 0, 0, 0) .. "a")
read(nil, "bc" .. string.char(0x8a, #payload, 0))
read(nil, string.char(0, 0) .. payload)
assert(
	vim.wait(500, function()
		return received ~= nil
	end),
	"fragmented context response was not received"
)
assert(received.foreground == "tig", "unexpected context data")
read("fixture EOF")
assert(
	vim.wait(500, function()
		return closed_pipe and received == nil
	end),
	"EOF must clear metadata"
)

for _, invalid in ipairs({
	string.char(0x8a, 1, 64, 0, 0), -- > 16 KiB: fail without waiting for more data
	string.char(0x8a, 1, 0, 0, 0) .. "{",
}) do
	closed_pipe = false
	real_context.subscribe(socket, 3, function(data)
		received = data
	end)
	read(nil, invalid)
	assert(
		vim.wait(500, function()
			return closed_pipe and received == nil
		end),
		"invalid IPC response was accepted"
	)
end
