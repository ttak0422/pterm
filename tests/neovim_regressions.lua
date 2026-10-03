-- Run with: nvim --headless -u NONE -l tests/neovim_regressions.lua
vim.opt.runtimepath:prepend(vim.fn.getcwd())

-- Keep Neovim's real buffers and autocmds; replace only external processes.
local next_job = 0
local stopped_jobs = {}
local expected_socket_dir
local expected_shell
local last_command
local resize_requests = {}
local complete_resizes = true
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
	local job = next_job
	if cmd[2] == "resize" then
		resize_requests[#resize_requests + 1] = {
			job = job,
			session = cmd[3],
			cols = tonumber(cmd[5]),
			rows = tonumber(cmd[7]),
			on_exit = opts.on_exit,
		}
		if complete_resizes then
			vim.schedule(function()
				opts.on_exit(job, 0, "exit")
			end)
		end
	end
	return job
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
		last_command[4] == "--no-resize"
			and last_command[9] == "--"
			and last_command[10] == "/bin/echo"
			and last_command[11] == "hello",
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

local function settle_resizes()
	vim.wait(30)
end

local function expect_current_size(session)
	settle_resizes()
	local win = vim.api.nvim_get_current_win()
	local latest = resize_requests[#resize_requests]
	local cols = vim.api.nvim_win_get_width(win) - vim.fn.getwininfo(win)[1].textoff
	assert(latest and latest.session == session, "focused session did not request a resize")
	assert(latest.cols == cols, "resize did not use the active window's text width")
	assert(latest.rows == vim.api.nvim_win_get_height(win), "resize did not use the active window's height")
end

check("focus chooses the active window rather than an arbitrary mirror", function()
	pterm.setup({ auto_redraw = false })
	pterm.open("active-window")
	local first_win = vim.api.nvim_get_current_win()
	local initial = last_command
	assert(initial[4] == "--no-resize", "native SIGWINCH still controls the shared session")
	assert(tonumber(initial[6]) == vim.api.nvim_win_get_width(first_win), "initial width is not explicit")
	assert(tonumber(initial[8]) == vim.api.nvim_win_get_height(first_win), "initial height is not explicit")

	vim.cmd("vsplit")
	vim.cmd("vertical resize 24")
	local small_win = vim.api.nvim_get_current_win()
	vim.api.nvim_exec_autocmds("WinResized", { modeline = false })
	expect_current_size("active-window")
	local small_cols = resize_requests[#resize_requests].cols
	local buf = vim.api.nvim_get_current_buf()
	vim.api.nvim_win_set_buf(small_win, vim.api.nvim_create_buf(false, true))
	vim.api.nvim_set_option_value("wrap", true, { win = small_win, scope = "local" })
	vim.api.nvim_set_current_win(first_win)
	vim.api.nvim_exec_autocmds("FocusLost", { modeline = false })
	vim.api.nvim_win_set_buf(small_win, buf)
	assert(
		not vim.api.nvim_get_option_value("wrap", { win = small_win }),
		"unfocused existing mirror wraps terminal rows"
	)
	vim.api.nvim_exec_autocmds("FocusGained", { modeline = false })
	vim.api.nvim_set_current_win(small_win)
	expect_current_size("active-window")

	-- Both windows show the same buffer, so BufEnter alone misses this change.
	vim.api.nvim_set_current_win(first_win)
	expect_current_size("active-window")
	assert(resize_requests[#resize_requests].cols > small_cols, "larger active window never became authoritative")

	-- The active text area may contain window-local decorations.
	vim.api.nvim_set_option_value("number", true, { win = first_win })
	vim.api.nvim_exec_autocmds("VimResized", { modeline = false })
	expect_current_size("active-window")

	-- A notification about another window does not give that mirror authority.
	local count = #resize_requests
	vim.api.nvim_exec_autocmds("WinResized", { modeline = false })
	settle_resizes()
	assert(#resize_requests == count, "unchanged active window issued a redundant layout resize")

	vim.api.nvim_set_current_win(small_win)
	expect_current_size("active-window")
	vim.cmd("close")
	expect_current_size("active-window")
end)
pterm.detach("active-window")
settle_resizes()
vim.cmd("silent! only")

check("background editors and inactive session buffers cannot take resize authority", function()
	pterm.open("focus-owner")
	vim.api.nvim_exec_autocmds("FocusGained", { modeline = false })
	expect_current_size("focus-owner")
	local count = #resize_requests
	vim.api.nvim_exec_autocmds("FocusLost", { modeline = false })
	vim.api.nvim_exec_autocmds("VimResized", { modeline = false })
	vim.api.nvim_exec_autocmds("WinResized", { modeline = false })
	settle_resizes()
	assert(#resize_requests == count, "background editor stole resize authority")

	vim.api.nvim_exec_autocmds("FocusGained", { modeline = false })
	expect_current_size("focus-owner")
	assert(#resize_requests == count + 1, "same-sized focused editor did not reclaim authority")
	local buf = vim.api.nvim_get_current_buf()
	vim.api.nvim_set_current_buf(vim.api.nvim_create_buf(false, true))
	count = #resize_requests
	vim.api.nvim_exec_autocmds("VimResized", { modeline = false })
	settle_resizes()
	assert(#resize_requests == count, "hidden terminal buffer stole resize authority")
	vim.api.nvim_set_option_value("wrap", true, { win = vim.api.nvim_get_current_win(), scope = "local" })
	vim.api.nvim_set_current_buf(buf)
	expect_current_size("focus-owner")
	assert(
		not vim.api.nvim_get_option_value("wrap", { win = vim.api.nvim_get_current_win() }),
		"mirror wraps canonical rows"
	)
end)
pterm.detach("focus-owner")
settle_resizes()

check("editor focus is tracked while there are no attached sessions", function()
	vim.api.nvim_exec_autocmds("FocusLost", { modeline = false })
	vim.api.nvim_exec_autocmds("FocusGained", { modeline = false })
	pterm.open("focus-after-detach")
	vim.api.nvim_exec_autocmds("WinEnter", { buffer = vim.api.nvim_get_current_buf() })
	expect_current_size("focus-after-detach")
end)
pterm.detach("focus-after-detach")
settle_resizes()

check("rapid focus resizes are serialized and keep only the newest pending size", function()
	complete_resizes = false
	pterm.open("resize-order")
	vim.api.nvim_exec_autocmds("FocusGained", { modeline = false })
	settle_resizes()
	local first = resize_requests[#resize_requests]
	local count = #resize_requests
	vim.cmd("vsplit")
	vim.cmd("vertical resize 21")
	vim.api.nvim_exec_autocmds("WinResized", { modeline = false })
	settle_resizes()
	vim.cmd("vertical resize 29")
	vim.api.nvim_exec_autocmds("WinResized", { modeline = false })
	settle_resizes()
	assert(#resize_requests == count, "concurrent resize commands can finish out of focus order")
	first.on_exit(first.job, 0, "exit")
	expect_current_size("resize-order")
	assert(#resize_requests == count + 1, "superseded intermediate window sizes were not coalesced")
	local pending = resize_requests[#resize_requests]
	pterm.detach("resize-order")
	assert(stopped_jobs[pending.job], "detaching left a resize command running")
	pending.on_exit(pending.job, 0, "exit")
	settle_resizes()
	assert(#resize_requests == count + 1, "old resize callback revived a detached connection")
end)
complete_resizes = true
pterm.detach("resize-order")
settle_resizes()
vim.cmd("silent! only")

check("queued resizes cannot take authority after focus leaves their buffer", function()
	complete_resizes = false
	pterm.open("stale-size")
	local buf = vim.api.nvim_get_current_buf()
	vim.api.nvim_exec_autocmds("FocusGained", { modeline = false })
	settle_resizes()
	local first = resize_requests[#resize_requests]
	local count = #resize_requests
	vim.cmd("vsplit")
	vim.cmd("vertical resize 25")
	vim.api.nvim_exec_autocmds("WinResized", { modeline = false })
	settle_resizes()
	vim.api.nvim_set_current_buf(vim.api.nvim_create_buf(false, true))
	first.on_exit(first.job, 0, "exit")
	settle_resizes()
	assert(#resize_requests == count, "a completed command dispatched a stale hidden-buffer resize")

	vim.api.nvim_set_current_buf(buf)
	-- Losing editor focus before the scheduled dispatch must discard the request.
	vim.api.nvim_exec_autocmds("FocusLost", { modeline = false })
	settle_resizes()
	assert(#resize_requests == count, "an unfocused scheduled resize stole authority")
	vim.api.nvim_exec_autocmds("FocusGained", { modeline = false })
	expect_current_size("stale-size")
end)
complete_resizes = true
pterm.detach("stale-size")
settle_resizes()
vim.cmd("silent! only")

check("large window sizes stay inside the daemon resource limits", function()
	local columns = vim.api.nvim_get_option_value("columns", {})
	local lines = vim.api.nvim_get_option_value("lines", {})
	vim.api.nvim_set_option_value("columns", 1000, {})
	vim.api.nvim_set_option_value("lines", 400, {})
	pterm.open("large-size")
	assert(tonumber(last_command[6]) == 512, "initial width exceeds the daemon maximum")
	assert(tonumber(last_command[8]) == 128, "initial dimensions exceed the cell budget")
	vim.api.nvim_exec_autocmds("FocusGained", { modeline = false })
	settle_resizes()
	local resize = resize_requests[#resize_requests]
	assert(resize.cols == 512 and resize.rows == 128, "focused resize exceeds the daemon budget")
	pterm.detach("large-size")
	vim.api.nvim_set_option_value("columns", columns, {})
	vim.api.nvim_set_option_value("lines", lines, {})
end)
pterm.detach("large-size")
settle_resizes()

check("managed history reset restores scrollback before acknowledging the bridge", function()
	pterm.open("history-reset")
	local buf = vim.api.nvim_get_current_buf()
	local job = next_job
	vim.api.nvim_set_option_value("scrollback", 1234, { buf = buf })
	local chansend = vim.fn.chansend
	local acknowledgements = 0
	-- selene: allow(incorrect_standard_library_use)
	vim.fn.chansend = function(channel, text)
		assert(channel == job, "history acknowledgement reached another bridge")
		assert(text == "\27]51;pterm-history-ready\7", "unexpected history acknowledgement")
		assert(vim.api.nvim_get_option_value("scrollback", { buf = buf }) == 1234, "ACK preceded option restoration")
		acknowledgements = acknowledgements + 1
		return #text
	end
	vim.api.nvim_exec_autocmds("TermRequest", {
		buffer = buf,
		data = { sequence = "\27]51;pterm-reset-history" },
	})
	assert(acknowledgements == 0, "history reset did not defer out of the terminal callback")
	settle_resizes()
	assert(acknowledgements == 1, "bridge was not released after clearing history")
	vim.api.nvim_exec_autocmds("TermRequest", {
		buffer = buf,
		data = { sequence = "\27]51;unrelated-request" },
	})
	settle_resizes()
	assert(acknowledgements == 1, "unrelated terminal request cleared history")
	vim.api.nvim_exec_autocmds("TermRequest", {
		buffer = buf,
		data = { sequence = "\27]51;pterm-reset-history" },
	})
	pterm.detach("history-reset")
	settle_resizes()
	assert(acknowledgements == 1, "late reset acknowledged a detached bridge")
	-- selene: allow(incorrect_standard_library_use)
	vim.fn.chansend = chansend
end)
pterm.detach("history-reset")
settle_resizes()

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
