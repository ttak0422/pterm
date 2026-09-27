-- Run from the repository root:
--   cargo build --release
--   nvim --headless -u NONE -l tests/neovim_live_terminal_repro.lua
-- Exercises the real plugin jobstart(term=true) path; unlike
-- neovim_regressions.lua, this does not mock jobstart.
-- Manual investigation harness, not a reproduction of the reported long-run
-- exit yet. OpenCode startup is optional and sends no prompt.
local root = vim.fn.getcwd()
vim.opt.runtimepath:prepend(root)

local temp_root = vim.fn.tempname()
vim.fn.mkdir(temp_root, "p")
for _, dir in ipairs({ "sockets", "config", "data", "state", "cache", "project" }) do
	vim.fn.mkdir(temp_root .. "/" .. dir, "p")
end
vim.env.XDG_CONFIG_HOME = temp_root .. "/config"
vim.env.XDG_DATA_HOME = temp_root .. "/data"
vim.env.XDG_STATE_HOME = temp_root .. "/state"
vim.env.XDG_CACHE_HOME = temp_root .. "/cache"

local pterm = require("pterm")
pterm.setup({
	socket_dir = temp_root .. "/sockets",
	shell = vim.fn.exepath("sh"),
	auto_redraw = true,
	auto_redraw_delay_ms = 100,
})

local real_jobstart = vim.fn.jobstart
local real_jobresize = vim.fn.jobresize
local terminal_jobs = {}
local job_buffers = {}
local exits = {}
local resizes = {}

local function buffer_text(buf)
	if not buf or not vim.api.nvim_buf_is_valid(buf) then
		return "<buffer deleted>"
	end
	return table.concat(vim.api.nvim_buf_get_lines(buf, 0, -1, false), "\n")
end

-- selene: allow(incorrect_standard_library_use)
vim.fn.jobstart = function(command, opts)
	if opts and opts.term then
		local on_exit = opts.on_exit
		opts.on_exit = function(job, code, event)
			exits[job] = {
				code = code,
				event = event,
				output = buffer_text(job_buffers[job]),
			}
			if on_exit then
				on_exit(job, code, event)
			end
		end
	end
	local job = real_jobstart(command, opts)
	if opts and opts.term and job > 0 then
		table.insert(terminal_jobs, job)
		job_buffers[job] = vim.api.nvim_get_current_buf()
	end
	return job
end

-- selene: allow(incorrect_standard_library_use)
vim.fn.jobresize = function(job, cols, rows)
	table.insert(resizes, { job = job, cols = cols, rows = rows })
	return real_jobresize(job, cols, rows)
end

local sessions = {}

local function assert_running(name, job, buf)
	-- selene: allow(incorrect_standard_library_use)
	local status = vim.fn.jobwait({ job }, 0)[1]
	assert(status == -1, ("%s terminal job exited: status=%s output=%s"):format(name, status, buffer_text(buf)))
	assert(pterm.is_connected(name), name .. " plugin connection was torn down")
	assert(vim.api.nvim_buf_is_valid(buf), name .. " terminal buffer was deleted")
end

local function screen_size(name)
	local dump, err = pterm.dump(name)
	assert(dump, err or (name .. " dump failed"))
	-- selene: allow(incorrect_standard_library_use)
	return vim.json.decode(dump).screen.size
end

local function open_session(name, command)
	table.insert(sessions, name)
	local args = { name }
	vim.list_extend(args, command)
	pterm.open(name, args)
	local job = terminal_jobs[#terminal_jobs]
	assert(job and job > 0, "plugin did not start a real terminal job")
	local buf = job_buffers[job]
	local socket = temp_root .. "/sockets/" .. name .. "/socket"
	assert(
		vim.wait(3000, function()
			return vim.uv.fs_stat(socket) ~= nil or exits[job] ~= nil
		end, 20),
		name .. " session socket did not appear"
	)
	assert_running(name, job, buf)
	return job, buf
end

local function hide_show_and_resize(name, job, buf)
	local scratch = vim.api.nvim_create_buf(false, true)
	vim.api.nvim_set_current_buf(scratch)
	vim.wait(150)
	vim.api.nvim_set_current_buf(buf)
	vim.wait(150)
	assert_running(name, job, buf)

	local resize_start = #resizes
	vim.cmd("vsplit")
	vim.wait(250)
	local used_fallback_resize = false
	if #resizes == resize_start then
		-- Headless Neovim may not emit WinResized for a programmatic split;
		-- trigger the plugin's normal VimResized callback against the new layout.
		used_fallback_resize = true
		vim.api.nvim_exec_autocmds("VimResized", { modeline = false })
		vim.wait(150)
	end
	assert(#resizes > resize_start, name .. " did not issue jobresize after the split")
	local resize
	for i = #resizes, resize_start + 1, -1 do
		if resizes[i].job == job then
			resize = resizes[i]
			break
		end
	end
	assert(resize, name .. " resize was sent for a different job")
	local resized_screen = screen_size(name)
	assert(
		resized_screen.cols == resize.cols and resized_screen.rows == resize.rows,
		("daemon size %dx%d did not follow jobresize %dx%d while split was open"):format(
			resized_screen.cols,
			resized_screen.rows,
			resize.cols,
			resize.rows
		)
	)
	vim.cmd("close")
	vim.wait(250)
	assert_running(name, job, buf)
	return resize, resized_screen, used_fallback_resize
end

local function cleanup()
	for i = #sessions, 1, -1 do
		pcall(pterm.kill, sessions[i])
	end
	-- selene: allow(incorrect_standard_library_use)
	vim.fn.jobstart = real_jobstart
	-- selene: allow(incorrect_standard_library_use)
	vim.fn.jobresize = real_jobresize
	vim.fn.delete(temp_root, "rf")
end

local ok, err = xpcall(function()
	local pterm_bin = root .. "/target/release/pterm"
	assert(vim.fn.executable(pterm_bin) == 1, "build target/release/pterm first")

	local fixture = [=[
sleep 0.5
printf '\033[?1049h\033[?25l'
i=0
while [ "$i" -lt 100 ]; do
  printf '\033[?2026h\033[H\033[2J\033[38;5;42mPTERM-LIVE-TICK-%03d\033[0m\033[?2026l' "$i"
  sleep 0.1
  i=$((i + 1))
done
exec sleep 30
]=]
	local fixture_job, fixture_buf = open_session("live-fixture", { "sh", "-c", fixture })
	assert(
		vim.wait(3000, function()
			return buffer_text(fixture_buf):find("PTERM%-LIVE%-TICK%-") ~= nil
		end, 25),
		"live TUI updates did not reach Neovim's terminal buffer"
	)
	local fixture_resize, fixture_size, fixture_resize_fallback =
		hide_show_and_resize("live-fixture", fixture_job, fixture_buf)
	vim.wait(3000)
	assert_running("live-fixture", fixture_job, fixture_buf)
	local fixture_screen = pterm.snapshot_text("live-fixture")
	local fixture_tick = tonumber(fixture_screen:match("PTERM%-LIVE%-TICK%-(%d+)"))
	assert(fixture_tick and fixture_tick >= 20, "daemon snapshot did not advance through repeated TUI updates")
	print(
		("Synthetic Neovim TUI: job=%d running, tick=%d, screen=%dx%d, split resize=%dx%d (%s)"):format(
			fixture_job,
			fixture_tick,
			fixture_size.cols,
			fixture_size.rows,
			fixture_resize.cols,
			fixture_resize.rows,
			fixture_resize_fallback and "VimResized fallback" or "WinResized"
		)
	)
	pterm.kill("live-fixture")
	for i = #sessions, 1, -1 do
		if sessions[i] == "live-fixture" then
			table.remove(sessions, i)
		end
	end

	local opencode = vim.fn.exepath("opencode")
	if opencode == "" then
		print("OpenCode TUI startup: skipped (opencode not on PATH)")
		return
	end
	local opencode_job, opencode_buf = open_session("live-opencode", {
		opencode,
		"--pure",
		temp_root .. "/project",
	})
	local did_exit = vim.wait(8000, function()
		return exits[opencode_job] ~= nil
	end, 50)
	if did_exit then
		local result = exits[opencode_job]
		error(
			("OpenCode TUI exited during startup: code=%s event=%s\n%s"):format(
				tostring(result.code),
				tostring(result.event),
				result.output
			)
		)
	end
	assert_running("live-opencode", opencode_job, opencode_buf)
	local opencode_resize, _, opencode_resize_fallback =
		hide_show_and_resize("live-opencode", opencode_job, opencode_buf)
	vim.wait(1500)
	assert_running("live-opencode", opencode_job, opencode_buf)
	print(
		("OpenCode TUI startup: job=%d running after startup/focus/resize; jobresize=%dx%d (%s)\n%s"):format(
			opencode_job,
			opencode_resize.cols,
			opencode_resize.rows,
			opencode_resize_fallback and "VimResized fallback" or "WinResized",
			buffer_text(opencode_buf)
		)
	)
end, debug.traceback)

cleanup()
if not ok then
	print(err)
	vim.cmd("cquit 1")
else
	print("Live Neovim terminal reproduction passed")
end
