-- Requires git, tig, and a built pterm on PATH (or target/release/pterm):
--   nvim --headless -u NONE -l tests/neovim_tig_regressions.lua
-- Run real ncurses/tig in unequal same-buffer windows, not a synthetic TUI.
vim.opt.runtimepath:prepend(vim.fn.getcwd())
vim.api.nvim_set_option_value("columns", 160, {})
vim.api.nvim_set_option_value("lines", 48, {})

local temp_root = vim.fn.tempname()
local repo = temp_root .. "/repo"
local size_file = temp_root .. "/pty-size"
local exit_file = temp_root .. "/tig-exit"
local session = "tig-resize-live"
local commit_count = 16
local pterm = require("pterm")
pterm.setup({ socket_dir = temp_root .. "/sockets", auto_redraw = false, shell = vim.fn.exepath("sh") })
local child = [=[
cd "$1" || exit 1
size_file=$2
exit_file=$3
tig=$4
# Observe the same terminal as tig without sending it commands or forcing redraws.
# The limit is a backstop if the test runner is interrupted before cleanup.
(
  i=0
  while [ "$i" -lt 600 ]; do
    stty size < /dev/tty > "$size_file" || exit
    sleep 0.1
    i=$((i + 1))
  done
) &
observer=$!
trap 'kill "$observer" 2>/dev/null || :; wait "$observer" 2>/dev/null || :' EXIT
trap 'exit 1' HUP TERM
"$tig"
status=$?
printf '%s\n' "$status" > "$exit_file"
exit "$status"
]=]

local function read_file(path)
	local file = io.open(path, "r")
	if not file then
		return ""
	end
	local text = file:read("*a")
	file:close()
	return text
end

local function trim_right(text)
	return text:gsub(" +$", "")
end

local function marker(index)
	return ("TIG-%04d-"):format(index)
end

local function window_size(win)
	return vim.api.nvim_win_get_width(win) - vim.fn.getwininfo(win)[1].textoff, vim.api.nvim_win_get_height(win)
end

local function git(args)
	local command = { vim.fn.exepath("git"), "-C", repo }
	vim.list_extend(command, args)
	local result = vim.system(command, { text = true }):wait(4000)
	assert(result.code == 0, "fixture git failed: " .. (result.stderr or ""))
end

local function expect_tig(buf, stage)
	local cols, rows = window_size(vim.api.nvim_get_current_win())
	local last_reason = "tig has not started"
	local screen
	local native_top
	local function check()
		local dump = pterm.dump(session)
		if not dump then
			last_reason = "daemon dump unavailable; tig exit=" .. read_file(exit_file)
			return false
		end
		-- selene: allow(incorrect_standard_library_use)
		screen = vim.json.decode(dump).screen
		local actual_rows, actual_cols = read_file(size_file):match("(%d+)%s+(%d+)")
		if
			screen.size.cols ~= cols
			or screen.size.rows ~= rows
			or tonumber(actual_cols) ~= cols
			or tonumber(actual_rows) ~= rows
		then
			last_reason = "daemon or tig PTY size does not match the active window"
			return false
		end
		if not screen.modes.alternate_screen then
			last_reason = "tig did not enter the alternate screen"
			return false
		end
		-- Tig reserves the penultimate row for its title. The percentage ends
		-- at the right edge, proving ncurses observed BOTH new dimensions.
		local title = screen.rows[rows - 1].text
		if not title:find("[main]", 1, true) or #title ~= cols or title:sub(-1) ~= "%" then
			last_reason = "tig title is not at the active window's bottom/right edge"
			return false
		end
		for row = 1, math.min(commit_count, rows - 2) do
			if not screen.rows[row].text:find(marker(commit_count - row + 1), 1, true) then
				last_reason = "tig commit rows were wrapped, lost, or reordered at row " .. row
				return false
			end
		end
		if #screen.rows[1].text < cols - 1 then
			last_reason = "long commit title did not reach the active window's right edge"
			return false
		end
		if not vim.api.nvim_buf_is_valid(buf) then
			last_reason = "tig terminal buffer was removed"
			return false
		end
		local lines = vim.api.nvim_buf_get_lines(buf, 0, -1, false)
		local top
		for i, line in ipairs(lines) do
			if line:find(marker(commit_count), 1, true) then
				if top then
					last_reason = "native terminal duplicated tig's first row"
					return false
				end
				top = i
			end
		end
		if not top or #lines < top + rows - 1 then
			last_reason = "native terminal has not received the complete tig screen"
			return false
		end
		for row = 1, rows do
			if trim_right(lines[top + row - 1]) ~= trim_right(screen.rows[row].text) then
				last_reason = "native terminal differs from tig's canonical row " .. row
				return false
			end
		end
		-- Terminal mode follows the native buffer tail. Canonical rows must end
		-- there, with the larger mirror's unused physical rows blank ABOVE them.
		if top + rows - 1 ~= #lines then
			last_reason = "tig screen does not end at the native terminal tail"
			return false
		end
		for i, line in ipairs(lines) do
			if (i < top or i >= top + rows) and line:find("%S") then
				last_reason = "native terminal retained stale tig text outside the canonical screen"
				return false
			end
		end
		native_top = top
		return true
	end
	local matched = vim.wait(5000, check, 25)
	assert(matched, ("%s (%dx%d): %s"):format(stage, cols, rows, last_reason))
	-- `-l` does not reliably enter Terminal mode while its Lua script runs.
	-- Exercise a real Neovim window anchored at the same buffer tail instead;
	-- this must show tig, not a short viewport full of mirror padding.
	vim.cmd("stopinsert")
	vim.cmd("normal! Gzb")
	vim.cmd("redraw")
	assert(
		vim.fn.getwininfo(vim.api.nvim_get_current_win())[1].topline == native_top,
		stage .. ": the active tail-following window does not start at tig's first row"
	)
	for _, win in ipairs(vim.api.nvim_list_wins()) do
		if vim.api.nvim_win_get_buf(win) == buf then
			assert(not vim.api.nvim_get_option_value("wrap", { win = win }), "tig mirror wraps instead of clipping")
		end
	end
	return screen
end

local ok, err = xpcall(function()
	assert(vim.fn.executable("git") == 1, "real tig regressions require git")
	assert(vim.fn.executable("tig") == 1, "real tig regressions require tig")
	vim.fn.mkdir(repo, "p")
	vim.fn.mkdir(temp_root .. "/home", "p")
	vim.fn.mkdir(temp_root .. "/sockets", "p")
	-- Do not depend on developer Git/Tig configuration, hooks, dates or locale.
	vim.env.HOME = temp_root .. "/home"
	vim.env.XDG_CONFIG_HOME = temp_root .. "/home"
	vim.env.GIT_CONFIG_NOSYSTEM = "1"
	vim.env.GIT_CONFIG_GLOBAL = "/dev/null"
	vim.env.GIT_AUTHOR_NAME = "Pterm Fixture"
	vim.env.GIT_AUTHOR_EMAIL = "pterm-fixture@example.invalid"
	vim.env.GIT_COMMITTER_NAME = vim.env.GIT_AUTHOR_NAME
	vim.env.GIT_COMMITTER_EMAIL = vim.env.GIT_AUTHOR_EMAIL
	vim.env.GIT_AUTHOR_DATE = "2000-01-01T00:00:00+0000"
	vim.env.GIT_COMMITTER_DATE = vim.env.GIT_AUTHOR_DATE
	vim.env.LC_ALL = "C"
	vim.env.TIGRC_SYSTEM = ""
	vim.env.TIGRC_USER = temp_root .. "/tigrc"
	vim.env.TIG_SCRIPT = nil
	vim.env.TIG_NO_DISPLAY = nil
	vim.fn.writefile({
		"set main-view = commit-title:graph=no,refs=no",
		"set show-changes = no",
		"set line-graphics = ascii",
		"set history-size = 0",
		"bind generic q quit",
	}, vim.env.TIGRC_USER)
	git({ "init", "--initial-branch=fixture", "--template=" })
	for i = 1, commit_count do
		git({ "commit", "--allow-empty", "-m", marker(i) .. string.rep("abcdefghij", 20) .. "-END" })
	end
	pterm.open(session, {
		session,
		"sh",
		"-c",
		child,
		"tig-fixture",
		repo,
		size_file,
		exit_file,
		vim.fn.exepath("tig"),
	})
	local buf = vim.api.nvim_get_current_buf()
	local job = vim.api.nvim_get_option_value("channel", { buf = buf })
	local large = vim.api.nvim_get_current_win()
	expect_tig(buf, "initial tig screen")

	vim.cmd("vsplit")
	vim.cmd("vertical resize 43")
	vim.cmd("split")
	vim.cmd("resize 11")
	local small = vim.api.nvim_get_current_win()
	vim.api.nvim_exec_autocmds("WinResized", { modeline = false })
	local small_cols, small_rows = window_size(small)
	local large_cols, large_rows = window_size(large)
	assert(large_cols > small_cols and large_rows > small_rows, "fixture needs a wider and taller passive mirror")
	expect_tig(buf, "small active window with larger mirror")
	for _ = 1, 2 do
		vim.api.nvim_set_current_win(large)
		local screen = expect_tig(buf, "large active window with clipped smaller mirror")
		assert(#screen.rows[1].text > small_cols, "fixture did not exercise horizontal clipping")
		vim.api.nvim_set_current_win(small)
		expect_tig(buf, "small active window after focus switch")
	end
	vim.cmd("vertical resize 35")
	vim.cmd("resize 9")
	vim.api.nvim_exec_autocmds("WinResized", { modeline = false })
	expect_tig(buf, "shrunken active window")
	vim.cmd("close")
	expect_tig(buf, "remaining mirror after small window closes")

	vim.fn.chansend(job, "q")
	assert(
		vim.wait(5000, function()
			return read_file(exit_file):match("^0%s*$") and not pterm.is_connected(session)
		end, 25),
		"tig did not quit cleanly or its bridge stayed connected"
	)
end, debug.traceback)

if vim.uv.fs_stat(temp_root .. "/sockets/" .. session .. "/socket") then
	pcall(pterm.kill, session)
end
vim.fn.delete(temp_root, "rf")
if not ok then
	print(err)
	vim.cmd("cquit 1")
else
	print("Real tig resize regressions passed")
end
