-- Run after cargo build --release, or with pterm on PATH:
--   nvim --headless -u NONE -l tests/neovim_resize_regressions.lua
-- Exercises the actual plugin/PTY/native renderer, including retained history.
vim.opt.runtimepath:prepend(vim.fn.getcwd())
vim.api.nvim_set_option_value("columns", 140, {})
vim.api.nvim_set_option_value("lines", 44, {})
local temp_root = vim.fn.tempname()
vim.fn.mkdir(temp_root .. "/sockets", "p")
local size_file = temp_root .. "/pty-size"
local pterm = require("pterm")
pterm.setup({ socket_dir = temp_root .. "/sockets", auto_redraw = false, shell = vim.fn.exepath("sh") })
local session = "resize-live"
local payload = string.rep("abcdefghij", 16) .. "漢字界e\204\129-tail"
local history_lines = 30
local fixture = [=[
stty -echo
alt=0
draw() {
  size=$(stty size)
  printf '%s\n' "$size" > "$1"
  set -- $size
  rows=$1
  cols=$2
  if [ "$alt" = 1 ]; then
    printf '\033[H\033[2Jsize:%sx%s\033[2;1H' "$cols" "$rows"
    i=1
    while [ "$i" -lt "$cols" ]; do printf x; i=$((i + 1)); done
    printf '>'
    printf '\033[%s;1Hbottom:%s' "$rows" "$cols"
  fi
}
size_file=$1
payload=$2
history_lines=$3
trap 'draw "$size_file"' WINCH
trap 'exit' HUP TERM
draw "$size_file"
while :; do
  if IFS= read -r command; then
    case "$command" in
      history)
        i=1
        while [ "$i" -le "$history_lines" ]; do
          printf 'HISTORY-%04d:%s:END-%04d\n' "$i" "$payload" "$i"
          i=$((i + 1))
        done
        printf 'HISTORY-READY\n'
        ;;
      alternate)
        alt=1
        printf '\033[?1049h\033[?25l'
        draw "$size_file"
        ;;
      normal)
        alt=0
        printf '\033[?1049l\033[?25h'
        draw "$size_file"
        ;;
      quit) exit ;;
    esac
  fi
done
]=]

local function current_size()
	local win = vim.api.nvim_get_current_win()
	return vim.api.nvim_win_get_width(win) - vim.fn.getwininfo(win)[1].textoff, vim.api.nvim_win_get_height(win)
end

local function daemon_size()
	local dump = pterm.dump(session)
	if not dump then
		return nil
	end
	-- selene: allow(incorrect_standard_library_use)
	return vim.json.decode(dump).screen.size
end

local function read_file(path)
	local file = io.open(path, "r")
	if not file then
		return ""
	end
	local text = file:read("*a")
	file:close()
	return text
end

local function expect_size()
	local cols, rows = current_size()
	assert(
		vim.wait(4000, function()
			local size = daemon_size()
			local actual_rows, actual_cols = read_file(size_file):match("(%d+)%s+(%d+)")
			return size
				and size.cols == cols
				and size.rows == rows
				and tonumber(actual_cols) == cols
				and tonumber(actual_rows) == rows
		end, 25),
		("daemon/parser or actual child PTY did not adopt active window %dx%d"):format(cols, rows)
	)
end

local function compact(text)
	return text:gsub("[\r\n]", "")
end

local function history_intact(text)
	text = compact(text)
	for i = 1, history_lines do
		local expected = ("HISTORY-%04d:%s:END-%04d"):format(i, payload, i)
		local start, finish = text:find(expected, 1, true)
		local marker = ("HISTORY-%04d:"):format(i)
		local marker_start, marker_end = text:find(marker, 1, true)
		if
			not start
			or text:find(expected, finish + 1, true)
			or not marker_start
			or text:find(marker, marker_end + 1, true)
		then
			return false
		end
	end
	return true
end

local function expect_history(buf, native)
	assert(
		vim.wait(4000, function()
			local text = pterm.full_text(session)
			if not text or not history_intact(text) then
				return false
			end
			return not native or history_intact(table.concat(vim.api.nvim_buf_get_lines(buf, 0, -1, false), "\n"))
		end, 25),
		native and "native history was truncated or duplicated after resize"
			or "daemon history lost content after resize"
	)
end

local function expect_alt_screen(buf)
	expect_size()
	local cols, rows = current_size()
	local expected_top = ("size:%dx%d"):format(cols, rows)
	local expected_edge = string.rep("x", cols - 1) .. ">"
	local expected_bottom = "bottom:" .. cols
	assert(
		vim.wait(4000, function()
			local lines = vim.api.nvim_buf_get_lines(buf, 0, -1, false)
			local top, edge, bottom
			for i, line in ipairs(lines) do
				line = line:gsub(" +$", "")
				if line == expected_top then
					top = i
				elseif line == expected_edge then
					edge = i
				elseif line == expected_bottom then
					bottom = i
				end
			end
			if not (top and edge == top + 1 and bottom == top + rows - 1) then
				return false
			end
			-- Native libvterm uses the larger mirror's dimensions. Its unused rows
			-- must stay blank rather than contain wrapped or stale application data.
			for i = bottom + 1, #lines do
				if lines[i]:find("%S") then
					return false
				end
			end
			return true
		end, 25),
		("native terminal did not render/pad the canonical %dx%d screen"):format(cols, rows)
	)
end

local ok, err = xpcall(function()
	pterm.open(session, { session, "sh", "-c", fixture, "resize-fixture", size_file, payload, tostring(history_lines) })
	local buf = vim.api.nvim_get_current_buf()
	local job = vim.api.nvim_get_option_value("channel", { buf = buf })
	local first = vim.api.nvim_get_current_win()
	expect_size()
	vim.api.nvim_set_option_value("scrollback", 10000, { buf = buf })
	vim.fn.chansend(job, "history\n")
	expect_history(buf, true)

	vim.cmd("vnew")
	local narrow = vim.api.nvim_get_current_win()
	vim.api.nvim_set_option_value("wrap", true, { win = narrow, scope = "local" })
	vim.api.nvim_set_current_win(first)
	vim.api.nvim_win_set_buf(narrow, buf)
	assert(not vim.api.nvim_get_option_value("wrap", { win = narrow }), "existing passive window rewraps terminal rows")
	vim.api.nvim_set_current_win(narrow)
	for _ = 1, 2 do
		for _, width in ipairs({ 31, 1, 45 }) do
			vim.cmd("vertical resize " .. width)
			vim.api.nvim_exec_autocmds("WinResized", { modeline = false })
			expect_size()
			-- At width one, double-width graphemes need two cells and the local
			-- viewport can clip them. They must remain in the canonical history.
			expect_history(buf, width ~= 1)
		end
		vim.api.nvim_set_current_win(first)
		expect_size()
		expect_history(buf, true)
		vim.api.nvim_set_current_win(narrow)
		expect_size()
		expect_history(buf, true)
	end
	assert(vim.api.nvim_get_option_value("scrollback", { buf = buf }) == 10000, "history reset changed user's limit")

	vim.fn.chansend(job, "alternate\n")
	expect_alt_screen(buf)
	for _ = 1, 3 do
		vim.api.nvim_set_current_win(first)
		expect_alt_screen(buf)
		vim.api.nvim_set_current_win(narrow)
		expect_alt_screen(buf)
	end
	vim.cmd("split")
	vim.cmd("resize 9")
	vim.api.nvim_exec_autocmds("WinResized", { modeline = false })
	expect_alt_screen(buf)
	vim.cmd("close")
	expect_alt_screen(buf)

	vim.api.nvim_exec_autocmds("FocusLost", { modeline = false })
	local held = daemon_size()
	vim.cmd("vertical resize 25")
	vim.api.nvim_exec_autocmds("WinResized", { modeline = false })
	vim.wait(150)
	local unchanged = daemon_size()
	assert(unchanged.cols == held.cols and unchanged.rows == held.rows, "unfocused layout stole the shared size")
	vim.api.nvim_exec_autocmds("FocusGained", { modeline = false })
	expect_alt_screen(buf)
	vim.cmd("close")
	expect_alt_screen(buf)
	vim.fn.chansend(job, "normal\n")
	expect_history(buf, true)
	local first_line = vim.api.nvim_buf_get_lines(buf, 0, 1, false)[1]
	-- ED3-capable Neovim clears the sentinel; older versions retain it.
	assert(
		first_line == "" or (first_line and first_line:find("HISTORY-0001:", 1, true) == 1),
		"history reset retained unexpected stale content before the first history row"
	)
	assert(
		vim.api.nvim_get_option_value("scrollback", { buf = buf }) == 10000,
		"alternate return changed user's history limit"
	)
	assert(pterm.is_connected(session), "focus/resize changes disconnected the persistent session")
end, debug.traceback)

pcall(pterm.kill, session)
vim.fn.delete(temp_root, "rf")
if not ok then
	print(err)
	vim.cmd("cquit 1")
else
	print("Live Neovim resize regressions passed")
end
