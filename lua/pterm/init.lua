local M = {}

--- Configuration
M.config = {
	-- Path to pterm binary (auto-detected if nil)
	binary = nil,
	-- Default shell command
	shell = vim.env.SHELL or "/bin/sh",
	-- Default terminal size
	cols = 80,
	rows = 24,
	-- Socket directory (nil = let daemon decide)
	socket_dir = nil,
}

--- Active connections: session_name -> { buf, job_id, session_name }
M.connections = {}

--- Find the pterm binary.
local function find_binary()
	if M.config.binary then
		return M.config.binary
	end

	-- Look relative to plugin root directory (lua/pterm/init.lua -> repo root)
	local script_path = debug.getinfo(1, "S").source:sub(2)
	local repo_root = vim.fn.fnamemodify(script_path, ":h:h:h")

	-- Prefer release build in development worktrees.
	local release_bin = repo_root .. "/target/release/pterm"
	if vim.fn.executable(release_bin) == 1 then
		return release_bin
	end

	-- Nix build output
	local nix_bin = repo_root .. "/result/bin/pterm"
	if vim.fn.executable(nix_bin) == 1 then
		return nix_bin
	end

	-- Fall back to PATH
	if vim.fn.executable("pterm") == 1 then
		return "pterm"
	end

	error("pterm binary not found. Build with: cargo build --release")
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

--- Kill a session.
function M.kill(session_name)
	if not session_name then
		vim.notify("Session name required", vim.log.levels.ERROR)
		return
	end

	-- Detach if connected
	local conn = M.connections[session_name]
	if conn then
		M.detach(session_name)
	end

	local bin = find_binary()
	vim.fn.system({ bin, "kill", session_name })
	vim.notify("Killed session: " .. session_name, vim.log.levels.INFO)
end

--- Internal: create a terminal buffer and start a pterm bridge process.
--- `cmd` is the full argv for jobstart (e.g. {"pterm","open","main"}).
local function start_terminal(session_name, cmd)
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

	-- Prevent the buffer from being unloaded or wiped when it leaves a window
	-- (e.g. during :tabnew).  Without this, 'bufhidden' defaults to "" which
	-- follows the global 'hidden' option and may unload the buffer.
	vim.api.nvim_set_option_value("bufhidden", "hide", { buf = buf })

	-- Set window-local options appropriate for a terminal buffer.
	local win = vim.api.nvim_get_current_win()
	vim.api.nvim_set_option_value("number", false, { win = win })
	vim.api.nvim_set_option_value("relativenumber", false, { win = win })
	vim.api.nvim_set_option_value("signcolumn", "no", { win = win })
	vim.api.nvim_set_option_value("foldcolumn", "0", { win = win })
	vim.api.nvim_set_option_value("statuscolumn", "", { win = win })

	-- Let the bridge read the actual PTY size via TIOCGWINSZ instead of
	-- passing --cols/--rows from Lua.  jobstart({term=true}) creates a PTY
	-- sized to the current window, and the bridge's get_winsize(stdout)
	-- will return exactly that size.
	local job_id
	job_id = vim.fn.jobstart(cmd, {
		term = true,
		on_exit = function(_, exit_code, _)
			vim.schedule(function()
				local conn = M.connections[session_name]
				if conn and conn.job_id == job_id then
					M.connections[session_name] = nil
					vim.notify("Session '" .. session_name .. "' exited (" .. exit_code .. ")", vim.log.levels.INFO)
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
	M.connections[session_name] = {
		buf = buf,
		job_id = job_id,
		session_name = session_name,
	}

	local augroup_name = "pterm_" .. session_name:gsub("/", "_")
	local augroup = vim.api.nvim_create_augroup(augroup_name, { clear = true })

	-- Clean up on buffer delete
	vim.api.nvim_create_autocmd("BufDelete", {
		group = augroup,
		buffer = buf,
		callback = function()
			M.detach(session_name)
		end,
	})

	-- Propagate resize events to the bridge process via jobresize().
	-- VimResized fires on SIGWINCH (whole Neovim frame resized).
	vim.api.nvim_create_autocmd("VimResized", {
		group = augroup,
		callback = function()
			local conn = M.connections[session_name]
			if not conn or not conn.job_id then
				return
			end
			-- Find all windows showing this terminal buffer and resize each.
			for _, w in ipairs(vim.api.nvim_list_wins()) do
				if vim.api.nvim_win_is_valid(w) and vim.api.nvim_win_get_buf(w) == conn.buf then
					local cols = vim.api.nvim_win_get_width(w)
					local rows = vim.api.nvim_win_get_height(w)
					pcall(vim.fn.jobresize, conn.job_id, cols, rows)
					break -- one jobresize is enough; the bridge sends RESIZE to daemon
				end
			end
		end,
	})

	-- Re-apply window-local options and refresh terminal content when the
	-- buffer re-enters a window (e.g. after :tabnew → :tabprev).
	-- Skip the very first BufWinEnter (the initial open) to avoid a
	-- redundant redraw while the first snapshot is still in flight.
	local first_buf_win_enter = true
	vim.api.nvim_create_autocmd("BufWinEnter", {
		group = augroup,
		buffer = buf,
		callback = function()
			if first_buf_win_enter then
				first_buf_win_enter = false
				return
			end
			local conn = M.connections[session_name]
			if not conn or not conn.job_id then
				return
			end
			local w = vim.api.nvim_get_current_win()
			vim.api.nvim_set_option_value("number", false, { win = w })
			vim.api.nvim_set_option_value("relativenumber", false, { win = w })
			vim.api.nvim_set_option_value("signcolumn", "no", { win = w })
			vim.api.nvim_set_option_value("foldcolumn", "0", { win = w })
			vim.api.nvim_set_option_value("statuscolumn", "", { win = w })
			-- Sync terminal dimensions and request a full redraw from the
			-- daemon so the display is restored after a tab switch.
			local cols = vim.api.nvim_win_get_width(w)
			local rows = vim.api.nvim_win_get_height(w)
			pcall(vim.fn.jobresize, conn.job_id, cols, rows)
			M.redraw(session_name)
		end,
	})

	-- WinResized fires when individual windows change size (Neovim ≥ 0.9).
	if vim.fn.exists("##WinResized") == 1 then
		vim.api.nvim_create_autocmd("WinResized", {
			group = augroup,
			callback = function()
				local conn = M.connections[session_name]
				if not conn or not conn.job_id then
					return
				end
				local resized_wins = vim.v.event and vim.v.event.windows or {}
				for _, w in ipairs(resized_wins) do
					if vim.api.nvim_win_is_valid(w) and vim.api.nvim_win_get_buf(w) == conn.buf then
						local cols = vim.api.nvim_win_get_width(w)
						local rows = vim.api.nvim_win_get_height(w)
						pcall(vim.fn.jobresize, conn.job_id, cols, rows)
						break
					end
				end
			end,
		})
	end

	vim.cmd("startinsert")
end

--- Open or attach to a session.
--- If session exists, attach. Otherwise create new.
--- Uses `pterm open` which handles both creation and attachment in a single
--- process, eliminating the timing gap between daemon creation and bridge
--- connection that caused wrong-size snapshot delivery.
function M.open(session_name, args)
	args = args or {}

	-- Default session name
	if not session_name or session_name == "" then
		session_name = "main"
	end

	-- Already connected?
	if M.connections[session_name] then
		-- Switch to existing buffer
		local conn = M.connections[session_name]
		if vim.api.nvim_buf_is_valid(conn.buf) then
			vim.api.nvim_set_current_buf(conn.buf)
			vim.cmd("startinsert")
			return
		else
			-- Buffer was closed, clean up
			M.connections[session_name] = nil
		end
	end

	local bin = find_binary()

	-- Build `pterm open` command with optional child command arguments.
	local cmd = { bin, "open", session_name }

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
	start_terminal(session_name, { bin, "attach", session_name })
end

--- Detach from a session (does not kill the daemon).
function M.detach(session_name)
	local conn = M.connections[session_name]
	if not conn then
		return
	end

	-- Remove from connections first to prevent re-entry from BufDelete autocmd
	M.connections[session_name] = nil

	if conn.job_id then
		pcall(vim.fn.jobstop, conn.job_id)
	end

	pcall(function()
		vim.api.nvim_del_augroup_by_name("pterm_" .. session_name:gsub("/", "_"))
	end)

	-- Wipe the associated buffer so it doesn't remain in the buffer list
	-- and doesn't block re-attach.  Use vim.schedule to defer the wipe so
	-- it never runs inside a BufDelete handler (which causes E937).
	if conn.buf and vim.api.nvim_buf_is_valid(conn.buf) then
		local b = conn.buf
		vim.schedule(function()
			if vim.api.nvim_buf_is_valid(b) then
				pcall(vim.api.nvim_buf_delete, b, { force = true })
			end
		end)
	end
end

--- Redraw a session (resend terminal snapshot via the daemon).
--- Stateless: no buffer or window changes. The daemon sends a SCROLLBACK
--- message through the existing bridge, which writes it to Neovim's terminal.
function M.redraw(session_name)
	if not session_name then
		vim.notify("Session name required", vim.log.levels.ERROR)
		return
	end

	local bin = find_binary()
	vim.fn.system({ bin, "redraw", session_name })
	if vim.v.shell_error ~= 0 then
		vim.notify("Failed to redraw session '" .. session_name .. "'", vim.log.levels.ERROR)
	end
end

--- Setup function for lazy.nvim / packer etc.
function M.setup(opts)
	M.config = vim.tbl_deep_extend("force", M.config, opts or {})

	local function complete_sessions(arg_lead)
		local ok, sessions = pcall(M.list)
		if not ok then
			return {}
		end
		if not arg_lead or arg_lead == "" then
			return sessions
		end
		return vim.tbl_filter(function(s)
			return s:find(arg_lead, 1, true) == 1
		end, sessions)
	end

	vim.api.nvim_create_user_command("Pterm", function(cmd_opts)
		M.open(cmd_opts.fargs[1], cmd_opts.fargs)
	end, {
		nargs = "*",
		complete = complete_sessions,
		desc = "Open or attach to a persistent terminal session",
	})

	vim.api.nvim_create_user_command("PtermList", function()
		local sessions = M.list()
		if #sessions == 0 then
			vim.notify("No active pterm sessions", vim.log.levels.INFO)
		else
			for _, name in ipairs(sessions) do
				vim.notify(name, vim.log.levels.INFO)
			end
		end
	end, { desc = "List active pterm sessions" })

	vim.api.nvim_create_user_command("PtermRedraw", function(cmd_opts)
		M.redraw(cmd_opts.fargs[1])
	end, {
		nargs = 1,
		complete = complete_sessions,
		desc = "Redraw a persistent terminal session",
	})

	vim.api.nvim_create_user_command("PtermKill", function(cmd_opts)
		M.kill(cmd_opts.fargs[1])
	end, {
		nargs = 1,
		complete = complete_sessions,
		desc = "Kill a persistent terminal session",
	})
end

return M
