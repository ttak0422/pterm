local M = {}

--- Configuration
M.config = {
	-- Default shell command
	shell = vim.env.SHELL or "/bin/sh",
	-- Socket directory (nil = let daemon decide)
	socket_dir = nil,
	auto_redraw = true,
	auto_redraw_delay_ms = 1000,
}

--- Active connections: session_name -> { buf, job_id, session_name }
local connections = {}
local redraw_timers = {}
local cached_binary = nil
local editor_focused = true
local focus_autocmds_initialized = false

--- Find the pterm binary (result is cached after the first successful lookup).
local function find_binary()
	if cached_binary then
		return cached_binary
	end

	-- Look relative to plugin root directory (lua/pterm/init.lua -> repo root)
	local script_path = debug.getinfo(1, "S").source:sub(2)
	local repo_root = vim.fn.fnamemodify(script_path, ":h:h:h")

	-- Prefer release build in development worktrees.
	local release_bin = repo_root .. "/target/release/pterm"
	if vim.fn.executable(release_bin) == 1 then
		cached_binary = release_bin
		return cached_binary
	end

	-- Nix build output
	local nix_bin = repo_root .. "/result/bin/pterm"
	if vim.fn.executable(nix_bin) == 1 then
		cached_binary = nix_bin
		return cached_binary
	end

	-- Fall back to PATH
	if vim.fn.executable("pterm") == 1 then
		cached_binary = "pterm"
		return cached_binary
	end

	error("pterm binary not found. Install pterm with Nix or build it in this repository.")
end

local function command_env()
	return { PTERM_SOCKET_DIR = M.config.socket_dir, SHELL = M.config.shell }
end

local function trigger_redraw(session_name)
	local conn = connections[session_name]
	if conn and conn.job_id then
		local bin = find_binary()
		vim.fn.jobstart({ bin, "redraw", session_name }, { env = command_env() })
	end
end

local function configure_views(buf)
	-- A buffer may be displayed in a pre-existing, unfocused window. Apply
	-- clipping to every mirror without granting it session resize authority.
	for _, win in ipairs(vim.api.nvim_list_wins()) do
		if vim.api.nvim_win_is_valid(win) and vim.api.nvim_win_get_buf(win) == buf then
			vim.api.nvim_set_option_value("wrap", false, { win = win, scope = "local" })
		end
	end
end

local function window_size(win)
	-- Window width includes number/sign/fold columns. Match Neovim's terminal
	-- text area even when another window has different local options.
	local info = vim.fn.getwininfo(win)[1]
	local cols = math.min(512, math.max(1, vim.api.nvim_win_get_width(win) - (info and info.textoff or 0)))
	local rows = math.min(256, math.max(1, vim.api.nvim_win_get_height(win)))
	-- Respect the daemon's fixed dimension/cell budgets even on very large UIs.
	return { cols = cols, rows = math.min(rows, math.floor(65536 / cols)) }
end

-- Neovim sizes native terminal jobs to the largest window displaying their
-- buffer. That PTY size is a rendering detail, not session resize authority.
-- The plugin's --no-resize bridge leaves sizing to these explicit requests.
local function flush_resize(session_name, conn)
	if connections[session_name] ~= conn then
		return
	end
	local win = vim.api.nvim_get_current_win()
	if not editor_focused or vim.api.nvim_win_get_buf(win) ~= conn.buf then
		conn.pending_size = nil
		return
	end
	if conn.resize_job or not conn.pending_size then
		return
	end
	-- A queued command may outlive a focus/layout change. Never dispatch an
	-- old window's dimensions after another window or buffer became active.
	local size = window_size(win)
	conn.pending_size = nil

	local resize_job
	resize_job = vim.fn.jobstart({
		find_binary(),
		"resize",
		session_name,
		"--cols",
		tostring(size.cols),
		"--rows",
		tostring(size.rows),
	}, {
		env = command_env(),
		on_exit = function(_, exit_code, _)
			vim.schedule(function()
				if connections[session_name] ~= conn or conn.resize_job ~= resize_job then
					return
				end
				conn.resize_job = nil
				if exit_code ~= 0 then
					conn.last_size = nil
					conn.resize_failures = (conn.resize_failures or 0) + 1
					-- The daemon socket may not exist yet during `pterm open`.
					-- Retry briefly without requiring another focus/layout event.
					if conn.resize_failures <= 3 then
						vim.defer_fn(function()
							if connections[session_name] ~= conn or conn.resize_job then
								return
							end
							local current = vim.api.nvim_get_current_win()
							if editor_focused and vim.api.nvim_win_get_buf(current) == conn.buf then
								conn.pending_size = window_size(current)
								flush_resize(session_name, conn)
							end
						end, 50 * 2 ^ (conn.resize_failures - 1))
						return
					end
				else
					conn.resize_failures = 0
				end
				flush_resize(session_name, conn)
			end)
		end,
	})
	if resize_job > 0 then
		conn.resize_job = resize_job
		conn.last_size = size
	else
		conn.last_size = nil
	end
end

local function resize_active_window(session_name, force)
	local conn = connections[session_name]
	if not conn or not editor_focused then
		return
	end
	local win = vim.api.nvim_get_current_win()
	if vim.api.nvim_win_get_buf(win) ~= conn.buf then
		return
	end
	configure_views(conn.buf)

	local size = window_size(win)
	local last = conn.pending_size or conn.last_size
	if not force and last and last.cols == size.cols and last.rows == size.rows then
		return
	end
	conn.pending_size = size
	conn.resize_failures = 0
	if conn.resize_scheduled then
		return
	end
	conn.resize_scheduled = true
	vim.schedule(function()
		conn.resize_scheduled = nil
		flush_resize(session_name, conn)
	end)
end

local function initialize_focus_autocmds()
	if focus_autocmds_initialized then
		return
	end
	focus_autocmds_initialized = true
	-- Keep these handlers even when the last connection closes while the editor
	-- is unfocused. A subsequent FocusGained must not leave new sessions frozen.
	local group = vim.api.nvim_create_augroup("pterm_focus", { clear = true })
	vim.api.nvim_create_autocmd("FocusLost", {
		group = group,
		callback = function()
			editor_focused = false
			for _, conn in pairs(connections) do
				conn.pending_size = nil
				local resize_job = conn.resize_job
				conn.resize_job = nil
				if resize_job then
					pcall(vim.fn.jobstop, resize_job)
				end
			end
		end,
	})
	vim.api.nvim_create_autocmd("FocusGained", {
		group = group,
		callback = function()
			editor_focused = true
			for session_name in pairs(connections) do
				resize_active_window(session_name, true)
			end
		end,
	})
end

--- Get socket directory (must match daemon's socket_dir() logic).
local function socket_dir()
	if M.config.socket_dir then
		return M.config.socket_dir
	end
	local pterm_dir = vim.env.PTERM_SOCKET_DIR
	if pterm_dir then
		return pterm_dir
	end
	local runtime_dir = vim.env.XDG_RUNTIME_DIR
	if runtime_dir then
		return (runtime_dir .. "/pterm"):gsub("//+", "/")
	end
	local uid = vim.uv.os_get_passwd().uid
	return "/tmp/pterm-" .. uid
end

--- Get socket path for a session.
--- Session names may contain '/' for hierarchical sessions.
local function socket_path(session_name)
	return socket_dir() .. "/" .. session_name .. "/socket"
end

--- Recursively scan the socket directory for active sessions (pure Lua).
--- Mirrors the Rust `find_sessions()` logic without spawning a subprocess,
--- which avoids instability when called during command-line completion.
local function scan_sessions(base, prefix)
	local sessions = {}
	local handle = vim.uv.fs_scandir(base)
	if not handle then
		return sessions
	end
	while true do
		local name, typ = vim.uv.fs_scandir_next(handle)
		if not name then
			break
		end
		if name ~= "socket" and typ == "directory" then
			local full_name = prefix == "" and name or (prefix .. "/" .. name)
			local child_dir = base .. "/" .. name
			if vim.uv.fs_stat(child_dir .. "/socket") then
				table.insert(sessions, full_name)
			end
			local children = scan_sessions(child_dir, full_name)
			vim.list_extend(sessions, children)
		end
	end
	return sessions
end

--- List active sessions.
function M.list()
	local dir = socket_dir()
	local sessions = scan_sessions(dir, "")
	table.sort(sessions)
	return sessions
end

--- Return the session shell's current working directory, or nil if unknown.
--- The daemon records this in `<socket_dir>/<name>/cwd` and refreshes it as the
--- shell changes directory.
function M.session_cwd(session_name)
	if not session_name or session_name == "" then
		return nil
	end
	local path = socket_dir() .. "/" .. session_name .. "/cwd"
	local fd = io.open(path, "r")
	if not fd then
		return nil
	end
	local content = fd:read("*a")
	fd:close()
	if not content then
		return nil
	end
	content = vim.trim(content)
	if content == "" then
		return nil
	end
	return content
end

--- Format a session name with its directory basename appended, e.g.
--- "session_name (pterm)". Falls back to the bare name when the directory
--- is unknown.
function M.display_name(session_name)
	local cwd = M.session_cwd(session_name)
	if not cwd then
		return session_name
	end
	local tail = vim.fn.fnamemodify(cwd, ":t")
	if tail == "" then
		return session_name
	end
	return session_name .. " (" .. tail .. ")"
end

function M.is_connected(session_name)
	return connections[session_name] ~= nil
end

--- Kill a session.
function M.kill(session_name)
	if not session_name then
		vim.notify("Session name required", vim.log.levels.ERROR)
		return
	end

	local bin = find_binary()
	local result = vim.system({ bin, "kill", session_name }, { text = true, env = command_env() }):wait()
	if result.code ~= 0 then
		vim.notify(
			"Failed to kill session '"
				.. session_name
				.. "': "
				.. vim.trim((result.stderr or "") .. (result.stdout or "")),
			vim.log.levels.ERROR
		)
		return
	end
	M.detach(session_name)
	vim.notify("Killed session: " .. session_name, vim.log.levels.INFO)
end

local function augroup_name(buf)
	return "pterm_" .. buf
end

local function teardown_connection(session_name, opts)
	opts = opts or {}

	local conn = connections[session_name]
	if not conn then
		return
	end

	local timer = redraw_timers[session_name]
	if timer then
		timer:stop()
		timer:close()
		redraw_timers[session_name] = nil
	end

	connections[session_name] = nil

	if conn.resize_job then
		pcall(vim.fn.jobstop, conn.resize_job)
	end

	if opts.stop_job ~= false and conn.job_id then
		pcall(vim.fn.jobstop, conn.job_id)
	end

	pcall(function()
		vim.api.nvim_del_augroup_by_name(augroup_name(conn.buf))
	end)

	if conn.buf and vim.api.nvim_buf_is_valid(conn.buf) then
		local buf = conn.buf
		vim.schedule(function()
			if vim.api.nvim_buf_is_valid(buf) then
				pcall(vim.api.nvim_buf_delete, buf, { force = true })
			end
		end)
	end

	if opts.exit_code ~= nil then
		vim.notify("Session '" .. session_name .. "' exited (" .. opts.exit_code .. ")", vim.log.levels.INFO)
	end
end

local function schedule_redraw(session_name, delay_ms)
	if not M.config.auto_redraw then
		return
	end
	delay_ms = delay_ms or M.config.auto_redraw_delay_ms
	local existing = redraw_timers[session_name]
	if existing then
		return
	end

	trigger_redraw(session_name)

	local timer = vim.uv.new_timer()
	redraw_timers[session_name] = timer
	timer:start(
		delay_ms,
		0,
		vim.schedule_wrap(function()
			if redraw_timers[session_name] ~= timer then
				return
			end
			redraw_timers[session_name] = nil
			timer:stop()
			timer:close()
		end)
	)
end

local function with_preserved_global_options(option_names, callback)
	local saved = {}
	for _, name in ipairs(option_names) do
		saved[name] = vim.go[name]
	end

	local ok, err = pcall(callback)

	for _, name in ipairs(option_names) do
		vim.go[name] = saved[name]
	end

	if not ok then
		error(err)
	end
end

--- Internal: create a terminal buffer and start a pterm bridge process.
--- `cmd` is the full argv for jobstart (e.g. {"pterm","open","main"}).
local function start_terminal(session_name, cmd)
	initialize_focus_autocmds()
	teardown_connection(session_name)

	-- Clean up any stale buffer with the same name from a previous connection
	local buf_name = "pterm://" .. session_name
	local existing = vim.fn.bufnr(buf_name)
	if existing ~= -1 then
		pcall(vim.api.nvim_buf_delete, existing, { force = true })
	end

	-- `jobstart(..., {term=true})` requires the current buffer to be unmodified.
	-- Always use a fresh buffer so attach is deterministic regardless of the
	-- user's currently focused buffer state.
	local buf = vim.api.nvim_create_buf(true, false)
	vim.api.nvim_set_current_buf(buf)

	-- Set window-local options appropriate for a terminal buffer.
	local win = vim.api.nvim_get_current_win()
	vim.api.nvim_set_option_value("number", false, { win = win })
	vim.api.nvim_set_option_value("relativenumber", false, { win = win })
	with_preserved_global_options({ "signcolumn", "foldcolumn", "statuscolumn", "wrap" }, function()
		vim.api.nvim_set_option_value("signcolumn", "no", { win = win })
		vim.api.nvim_set_option_value("foldcolumn", "0", { win = win })
		vim.api.nvim_set_option_value("statuscolumn", "", { win = win })
		vim.api.nvim_set_option_value("wrap", false, { win = win })
	end)

	-- Pass the active window size before any child command separator. Native
	-- terminal resizes may occur before the bridge starts; they must not choose
	-- the shared session's initial size either.
	local size = window_size(win)
	table.insert(cmd, 5, "--cols")
	table.insert(cmd, 6, tostring(size.cols))
	table.insert(cmd, 7, "--rows")
	table.insert(cmd, 8, tostring(size.rows))
	local job_id
	job_id = vim.fn.jobstart(cmd, {
		term = true,
		env = command_env(),
		on_exit = function(_, exit_code, _)
			vim.schedule(function()
				local conn = connections[session_name]
				if conn and conn.job_id == job_id then
					teardown_connection(session_name, {
						stop_job = false,
						exit_code = exit_code,
					})
				end
			end)
		end,
	})

	if job_id <= 0 then
		vim.notify("Failed to start pterm for '" .. session_name .. "'", vim.log.levels.ERROR)
		if vim.api.nvim_buf_is_valid(buf) then
			pcall(vim.api.nvim_buf_delete, buf, { force = true })
		end
		return
	end

	vim.api.nvim_buf_set_name(buf, buf_name)

	-- Store connection
	connections[session_name] = {
		buf = buf,
		job_id = job_id,
		session_name = session_name,
		last_size = size,
	}

	local augroup = vim.api.nvim_create_augroup(augroup_name(buf), { clear = true })

	vim.api.nvim_create_autocmd({ "BufWinEnter", "WinEnter" }, {
		group = augroup,
		buffer = buf,
		callback = function()
			configure_views(buf)
		end,
	})

	vim.api.nvim_create_autocmd("TermRequest", {
		group = augroup,
		buffer = buf,
		callback = function(event)
			local sequence = event.data and event.data.sequence or vim.v.termrequest
			if sequence ~= "\27]51;pterm-reset-history" then
				return
			end
			local conn = connections[session_name]
			vim.schedule(function()
				if not conn or connections[session_name] ~= conn or not vim.api.nvim_buf_is_valid(buf) then
					return
				end
				-- Neovim <=0.11 has no CSI 3 J scrollback-clear callback, and a
				-- scrollback value below 1 means unlimited. The bridge first
				-- scrolls a blank row into history. Keep that one sentinel row
				-- while dropping all old content, then restore the user's limit.
				-- Only numeric decreases synchronously refresh native capacity.
				-- Restoring 1 -> limit alone leaves capacity at one until a timer,
				-- which would truncate the replay sent immediately after the ACK.
				-- Setting -1 forces a refresh that expands to Neovim's unlimited
				-- sentinel; restoring the saved limit then refreshes immediately.
				local scrollback = vim.api.nvim_get_option_value("scrollback", { buf = buf })
				vim.api.nvim_set_option_value("scrollback", 1, { buf = buf })
				vim.api.nvim_set_option_value("scrollback", -1, { buf = buf })
				vim.api.nvim_set_option_value("scrollback", scrollback, { buf = buf })
				vim.fn.chansend(conn.job_id, "\27]51;pterm-history-ready\7")
			end)
		end,
	})

	-- Clean up on buffer delete.
	-- BufDelete fires both when a buffer is truly deleted (:bdelete/:bwipeout)
	-- and when a plugin merely sets buflisted=false (e.g. scope-nvim scopes
	-- buffers per tab page).  Defer the check so we can distinguish the two:
	-- a merely-unlisted buffer remains valid and loaded.
	vim.api.nvim_create_autocmd("BufDelete", {
		group = augroup,
		buffer = buf,
		callback = function()
			vim.schedule(function()
				local conn = connections[session_name]
				if not conn or conn.buf ~= buf then
					return
				end
				if vim.api.nvim_buf_is_valid(buf) and vim.api.nvim_buf_is_loaded(buf) then
					return
				end
				teardown_connection(session_name)
			end)
		end,
	})

	-- Only the focused terminal window may update the shared session size.
	-- Resizing an inactive mirror must not win over the user's current window.
	vim.api.nvim_create_autocmd("VimResized", {
		group = augroup,
		callback = function()
			resize_active_window(session_name, false)
		end,
	})
	if vim.fn.exists("##WinResized") == 1 then
		vim.api.nvim_create_autocmd("WinResized", {
			group = augroup,
			callback = function()
				resize_active_window(session_name, false)
			end,
		})
	end

	vim.api.nvim_create_autocmd({ "BufEnter", "WinEnter", "TermEnter" }, {
		group = augroup,
		buffer = buf,
		callback = function()
			resize_active_window(session_name, true)
			schedule_redraw(session_name)
		end,
	})

	-- Explicit control requests are the only source of managed size authority,
	-- including first attach. The bridge must never race this with stale sizes.
	resize_active_window(session_name, true)
	vim.cmd("startinsert")
end

--- Open or attach to a session.
--- If session exists, attach. Otherwise create new.
--- Uses `pterm open` which handles both creation and attachment in a single
--- process, eliminating the timing gap between daemon creation and bridge
--- connection that caused wrong-size snapshot delivery.
function M.open(session_name, args)
	args = args or {}

	if not session_name or session_name == "" then
		vim.notify("Session name required", vim.log.levels.ERROR)
		return
	end

	-- Already connected?
	if connections[session_name] then
		-- Switch to existing buffer
		local conn = connections[session_name]
		if vim.api.nvim_buf_is_valid(conn.buf) then
			vim.api.nvim_set_current_buf(conn.buf)
			configure_views(conn.buf)
			resize_active_window(session_name, true)
			vim.cmd("startinsert")
			return
		else
			-- Buffer was closed, clean up
			teardown_connection(session_name)
		end
	end

	local bin = find_binary()

	-- Build `pterm open` command with optional child command arguments.
	local cmd = { bin, "open", session_name, "--no-resize" }

	local cmd_parts = {}
	local found_name = false
	for _, arg in ipairs(args) do
		if not found_name and arg == session_name then
			found_name = true
		elseif found_name then
			table.insert(cmd_parts, arg)
		end
	end

	if #cmd_parts > 0 then
		table.insert(cmd, "--")
		for _, part in ipairs(cmd_parts) do
			table.insert(cmd, part)
		end
	end

	start_terminal(session_name, cmd)
end

--- Attach to an existing session.
function M.attach(session_name)
	local sock = socket_path(session_name)

	if vim.uv.fs_stat(sock) == nil then
		vim.notify("Session '" .. session_name .. "' not found", vim.log.levels.ERROR)
		return
	end

	local bin = find_binary()
	start_terminal(session_name, { bin, "attach", session_name, "--no-resize" })
end

--- Detach from a session (does not kill the daemon).
function M.detach(session_name)
	teardown_connection(session_name)
end

--- Redraw a session (resend terminal snapshot via the daemon).
--- Stateless: no buffer or window changes. The daemon sends a STATE_SYNC
--- message through the existing bridge, which writes it to Neovim's terminal.
function M.redraw(session_name)
	if not session_name then
		vim.notify("Session name required", vim.log.levels.ERROR)
		return
	end

	local bin = find_binary()
	local result = vim.system({ bin, "redraw", session_name }, { text = true, env = command_env() }):wait()
	if result.code ~= 0 then
		vim.notify("Failed to redraw session '" .. session_name .. "'", vim.log.levels.ERROR)
	end
end

--- Run a pterm subcommand that prints text for a session.
local function run_text_command(subcommand, session_name)
	if not session_name or session_name == "" then
		return nil, "Session name required"
	end

	local bin = find_binary()
	local result = vim.system({ bin, subcommand, session_name }, { text = true, env = command_env() }):wait()
	local output = (result.stdout or "") .. (result.stderr or "")
	if result.code ~= 0 then
		return nil, output
	end

	return (output:gsub("\n$", ""))
end

--- Return a plain-text snapshot of the visible terminal screen.
function M.snapshot_text(session_name)
	return run_text_command("snapshot-text", session_name)
end

--- Return the full plain-text contents of a session
--- (scrollback history + visible screen).
function M.full_text(session_name)
	return run_text_command("full-text", session_name)
end

--- Return an ANSI-colored snapshot of the visible terminal screen.
--- Rows carry SGR escape sequences so colors/attributes survive extraction.
function M.snapshot_ansi(session_name)
	return run_text_command("snapshot-ansi", session_name)
end

--- Asynchronously return a plain-text snapshot of the visible terminal screen.
function M.snapshot_text_async(session_name, callback)
	if type(callback) ~= "function" then
		error("callback required")
	end

	if not session_name or session_name == "" then
		vim.schedule(function()
			callback(nil, "Session name required")
		end)
		return nil
	end

	local ok, bin = pcall(find_binary)
	if not ok then
		vim.schedule(function()
			callback(nil, bin)
		end)
		return nil
	end

	local system_ok, job = pcall(
		vim.system,
		{ bin, "snapshot-text", session_name },
		{ text = true, env = command_env() },
		vim.schedule_wrap(function(result)
			if result.code == 0 then
				callback((result.stdout or ""):gsub("\n$", ""))
				return
			end

			local err = result.stderr or result.stdout or ""
			if err == "" then
				err = "pterm snapshot-text exited with code " .. tostring(result.code)
			end
			callback(nil, err)
		end)
	)
	if not system_ok then
		vim.schedule(function()
			callback(nil, job)
		end)
		return nil
	end

	return job
end

--- Asynchronously return an ANSI-colored snapshot of the visible terminal screen.
function M.snapshot_ansi_async(session_name, callback)
	if type(callback) ~= "function" then
		error("callback required")
	end

	if not session_name or session_name == "" then
		vim.schedule(function()
			callback(nil, "Session name required")
		end)
		return nil
	end

	local ok, bin = pcall(find_binary)
	if not ok then
		vim.schedule(function()
			callback(nil, bin)
		end)
		return nil
	end

	local system_ok, job = pcall(
		vim.system,
		{ bin, "snapshot-ansi", session_name },
		{ text = true, env = command_env() },
		vim.schedule_wrap(function(result)
			if result.code == 0 then
				callback((result.stdout or ""):gsub("\n$", ""))
				return
			end

			local err = result.stderr or result.stdout or ""
			if err == "" then
				err = "pterm snapshot-ansi exited with code " .. tostring(result.code)
			end
			callback(nil, err)
		end)
	)
	if not system_ok then
		vim.schedule(function()
			callback(nil, job)
		end)
		return nil
	end

	return job
end

--- Return a diagnostic state dump as JSON.
function M.dump(session_name)
	return run_text_command("dump", session_name)
end

local function open_dump_buffer(session_name, dump)
	local buf_name = "pterm-dump://" .. session_name
	local existing = vim.fn.bufnr(buf_name)
	if existing ~= -1 then
		pcall(vim.api.nvim_buf_delete, existing, { force = true })
	end

	local buf = vim.api.nvim_create_buf(true, true)
	vim.api.nvim_buf_set_name(buf, buf_name)
	vim.api.nvim_set_option_value("buftype", "nofile", { buf = buf })
	vim.api.nvim_set_option_value("bufhidden", "wipe", { buf = buf })
	vim.api.nvim_set_option_value("swapfile", false, { buf = buf })
	vim.api.nvim_set_option_value("filetype", "json", { buf = buf })
	vim.api.nvim_buf_set_lines(buf, 0, -1, false, vim.split(dump, "\n", { plain = true }))
	vim.api.nvim_set_current_buf(buf)
end

--- Setup function for lazy.nvim / packer etc.
function M.setup(opts)
	M.config = vim.tbl_deep_extend("force", M.config, opts or {})

	local pterm_subcommands = { "new", "list", "redraw", "dump", "kill" }

	local function complete_values(values, arg_lead)
		if not arg_lead or arg_lead == "" then
			return values
		end
		return vim.tbl_filter(function(value)
			return value:find(arg_lead, 1, true) == 1
		end, values)
	end

	local function complete_sessions(arg_lead)
		local ok, sessions = pcall(M.list)
		if not ok then
			return {}
		end
		return complete_values(sessions, arg_lead)
	end

	local function complete_pterm(arg_lead, cmd_line, cursor_pos)
		local before_cursor = cmd_line:sub(1, cursor_pos)
		local words = vim.split(vim.trim(before_cursor), "%s+", { trimempty = true })
		if words[1] == "Pterm" then
			table.remove(words, 1)
		end

		local arg_index = #words
		if before_cursor:match("%s$") then
			arg_index = arg_index + 1
		end

		if arg_index <= 1 then
			return complete_values(pterm_subcommands, arg_lead)
		end

		local subcommand = words[1]
		local session_subcommand = subcommand == "new"
			or subcommand == "redraw"
			or subcommand == "dump"
			or subcommand == "kill"
		if arg_index == 2 and session_subcommand then
			return complete_sessions(arg_lead)
		end

		return {}
	end

	local function list_sessions()
		local sessions = M.list()
		if #sessions == 0 then
			vim.notify("No active pterm sessions", vim.log.levels.INFO)
		else
			for _, name in ipairs(sessions) do
				vim.notify(M.display_name(name), vim.log.levels.INFO)
			end
		end
	end

	local function dump_session(session_name)
		local dump, err = M.dump(session_name)
		if not dump then
			vim.notify(
				"Failed to dump session '" .. session_name .. "': " .. vim.trim(tostring(err or "")),
				vim.log.levels.ERROR
			)
			return
		end

		open_dump_buffer(session_name, dump)
	end

	local function require_session(subcommand, session_name)
		if session_name and session_name ~= "" then
			return true
		end
		vim.notify("Usage: :Pterm " .. subcommand .. " <session>", vim.log.levels.ERROR)
		return false
	end

	vim.api.nvim_create_user_command("Pterm", function(cmd_opts)
		local subcommand = cmd_opts.fargs[1]
		if not subcommand or subcommand == "" then
			vim.notify("Usage: :Pterm <new|list|redraw|dump|kill> ...", vim.log.levels.ERROR)
			return
		end

		if subcommand == "new" then
			local session_name = cmd_opts.fargs[2]
			if not require_session("new", session_name) then
				return
			end
			M.open(session_name, vim.list_slice(cmd_opts.fargs, 2))
		elseif subcommand == "list" then
			list_sessions()
		elseif subcommand == "redraw" then
			local session_name = cmd_opts.fargs[2]
			if require_session("redraw", session_name) then
				M.redraw(session_name)
			end
		elseif subcommand == "dump" then
			local session_name = cmd_opts.fargs[2]
			if require_session("dump", session_name) then
				dump_session(session_name)
			end
		elseif subcommand == "kill" then
			local session_name = cmd_opts.fargs[2]
			if require_session("kill", session_name) then
				M.kill(session_name)
			end
		else
			vim.notify("Unknown Pterm subcommand: " .. subcommand, vim.log.levels.ERROR)
		end
	end, {
		nargs = "*",
		complete = complete_pterm,
		desc = "Manage persistent terminal sessions",
	})
end

return M
