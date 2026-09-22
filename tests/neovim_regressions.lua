-- Run with: nvim --headless -u NONE -l tests/neovim_regressions.lua
vim.opt.runtimepath:prepend(vim.fn.getcwd())

-- Keep Neovim's real buffers and autocmds; replace only external processes.
local next_job = 0
local stopped_jobs = {}
local expected_socket_dir
local expected_shell
local last_command
-- Selene allowances below cover deliberate Neovim function mocks and restoration.
-- selene: allow(incorrect_standard_library_use)
vim.fn.executable = function()
	return 1
end
-- selene: allow(incorrect_standard_library_use)
vim.fn.jobstart = function(cmd, opts)
	last_command = cmd
	if expected_socket_dir then
		assert(opts.env and opts.env.PTERM_SOCKET_DIR == expected_socket_dir, "job uses the wrong socket directory")
		assert(opts.env.SHELL == expected_shell, "job ignores the configured shell")
	end
	next_job = next_job + 1
	return next_job
end
-- selene: allow(incorrect_standard_library_use)
vim.fn.jobstop = function(job)
	stopped_jobs[job] = true
	return 1
end

local pterm = require("pterm")
pterm.setup({ auto_redraw = false })
local failures = {}
local function check(name, callback)
	local ok, err = pcall(callback)
	if not ok then
		failures[#failures + 1] = name .. ": " .. tostring(err)
	end
end

check("distinct session names retain independent cleanup", function()
	pterm.open("a/b")
	local first_buf = vim.api.nvim_get_current_buf()
	pterm.open("a_b")
	vim.api.nvim_buf_delete(first_buf, { force = true })
	assert(
		vim.wait(100, function()
			return not pterm.is_connected("a/b")
		end),
		"deleting a/b left its connection active"
	)
	assert(pterm.is_connected("a_b"), "deleting a/b disconnected a_b")
end)
pterm.detach("a/b")
pterm.detach("a_b")
vim.wait(10)

check("deferred cleanup cannot disconnect a replacement", function()
	pterm.open("replace")
	local old_buf = vim.api.nvim_get_current_buf()
	vim.api.nvim_buf_delete(old_buf, { force = true })
	pterm.open("replace")
	local new_buf = vim.api.nvim_get_current_buf()
	local new_job = next_job
	vim.wait(10)
	assert(pterm.is_connected("replace"), "old buffer cleanup disconnected the replacement")
	assert(vim.api.nvim_buf_is_valid(new_buf), "old buffer cleanup deleted the replacement buffer")
	assert(not stopped_jobs[new_job], "old buffer cleanup stopped the replacement job")
end)
pterm.detach("replace")
vim.wait(10)

local socket_root = vim.fn.tempname()
vim.fn.mkdir(socket_root .. "/custom", "p")
vim.fn.writefile({}, socket_root .. "/custom/socket")
check("configured socket directory and shell reach every subprocess", function()
	local inherited_socket_dir = vim.env.PTERM_SOCKET_DIR
	local inherited_shell = vim.env.SHELL
	expected_socket_dir = socket_root
	expected_shell = vim.fn.exepath("sh")
	pterm.setup({ socket_dir = socket_root, shell = expected_shell, auto_redraw = true })

	-- Run real child processes that report their environment instead of a daemon.
	local system = vim.system
	local calls = 0
	-- selene: allow(incorrect_standard_library_use)
	vim.system = function(_, opts, callback)
		calls = calls + 1
		assert(opts.env and opts.env.PTERM_SOCKET_DIR == socket_root, "command uses the wrong socket directory")
		return system({
			expected_shell,
			"-c",
			'[ "$SHELL" = "$1" ] && printf "%s" "$PTERM_SOCKET_DIR"',
			"pterm-env-test",
			expected_shell,
		}, opts, callback)
	end

	pterm.open("custom", { "custom", "/bin/echo", "hello" })
	assert(
		last_command[4] == "--" and last_command[5] == "/bin/echo" and last_command[6] == "hello",
		"configured shell replaced explicit command arguments"
	)
	vim.api.nvim_exec_autocmds("BufEnter", { buffer = vim.api.nvim_get_current_buf() })
	pterm.attach("custom")
	vim.wait(10)
	assert(pterm.is_connected("custom"), "reattachment lost the new connection")
	for _, query in ipairs({ pterm.snapshot_text, pterm.snapshot_ansi, pterm.full_text, pterm.dump }) do
		assert(query("custom") == socket_root, "query subprocess did not receive configured socket directory")
	end
	pterm.redraw("custom")
	for _, query in ipairs({ pterm.snapshot_text_async, pterm.snapshot_ansi_async }) do
		local output
		query("custom", function(text)
			output = text
		end)
		assert(
			vim.wait(1000, function()
				return output ~= nil
			end),
			"async snapshot callback did not complete"
		)
		assert(output == socket_root, "async subprocess did not receive configured socket directory")
	end
	pterm.kill("custom")
	assert(calls == 8, "a subprocess bypassed the configured environment")
	assert(vim.env.PTERM_SOCKET_DIR == inherited_socket_dir, "setup changed the editor's global environment")
	assert(vim.env.SHELL == inherited_shell, "setup changed the editor's global shell")
end)
pterm.detach("custom")
vim.fn.delete(socket_root, "rf")
vim.wait(10)

local notify = vim.notify
check("failed kills preserve the live connection and report failure", function()
	local result = { code = 1, stdout = "", stderr = "permission denied" }
	-- selene: allow(incorrect_standard_library_use)
	vim.system = function()
		return {
			wait = function()
				return result
			end,
		}
	end
	local notices = {}
	-- selene: allow(incorrect_standard_library_use)
	vim.notify = function(message, level)
		notices[#notices + 1] = { message = message, level = level }
	end
	pterm.open("kill-failure")
	local buf = vim.api.nvim_get_current_buf()
	local job = next_job
	pterm.kill("kill-failure")
	vim.wait(10)
	assert(pterm.is_connected("kill-failure"), "failed kill detached the live session")
	assert(vim.api.nvim_buf_is_valid(buf) and not stopped_jobs[job], "failed kill removed the live terminal")
	assert(#notices == 1 and notices[1].level == vim.log.levels.ERROR, "failed kill reported success")
	assert(notices[1].message:find("permission denied", 1, true), "failed kill hid its cause")
	result.code = 0
	pterm.kill("kill-failure")
	assert(not pterm.is_connected("kill-failure"), "successful kill left a connection active")
	assert(notices[2].level == vim.log.levels.INFO, "successful kill did not report success")
end)
-- selene: allow(incorrect_standard_library_use)
vim.notify = notify
pterm.detach("kill-failure")
vim.wait(10)

check("ANSI preview uses the xterm 256-color cube", function()
	local ansi = require("pterm.ansi")
	for _, color in ipairs({ { 16, 0x000000 }, { 17, 0x00005f }, { 196, 0xff0000 }, { 231, 0xffffff } }) do
		local run = ansi.parse("\27[38;5;" .. color[1] .. "mcolor")[1][1]
		local highlight = vim.api.nvim_get_hl(0, { name = ansi.hl_group(run.attrs) })
		assert(highlight.fg == color[2], "incorrect xterm palette color " .. color[1])
	end
end)

assert(#failures == 0, table.concat(failures, "\n"))
print("Neovim regressions passed")
