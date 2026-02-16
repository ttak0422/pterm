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
	-- Max wait time for daemon socket creation after `pterm new`
	attach_wait_ms = 3000,
	-- Poll interval while waiting for socket
	attach_poll_ms = 50,
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
	local uid = vim.fn.system("id -u"):gsub("%s+", "")
	return "/tmp/pterm-" .. uid
end

--- Get socket path for a session.
--- Session names may contain '/' for hierarchical sessions.
local function socket_path(session_name)
	return socket_dir() .. "/" .. session_name .. "/socket"
end

local function wait_for_socket(session_name, timeout_ms, poll_ms)
	local sock = socket_path(session_name)
	return vim.wait(timeout_ms, function()
		return vim.uv.fs_stat(sock) ~= nil
	end, poll_ms)
end

--- List active sessions.
function M.list()
	local bin = find_binary()
	local result = vim.fn.systemlist({ bin, "list" })
	return result or {}
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

--- Open or attach to a session.
--- If session exists, attach. Otherwise create new.
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

	local sock = socket_path(session_name)
	local bin = find_binary()

	-- Check if session exists
	if vim.uv.fs_stat(sock) == nil then
		-- Create new session
		local cmd_parts = {}
		-- Skip session name from args, collect rest as command
		local found_name = false
		for _, arg in ipairs(args) do
			if not found_name and arg == session_name then
				found_name = true
			elseif found_name then
				table.insert(cmd_parts, arg)
			end
		end

		local win_cols = vim.api.nvim_win_get_width(0)
		local win_rows = vim.api.nvim_win_get_height(0)

		local create_cmd = {
			bin,
			"new",
			session_name,
			"--cols",
			tostring(win_cols),
			"--rows",
			tostring(win_rows),
		}

		if #cmd_parts > 0 then
			table.insert(create_cmd, "--")
			for _, part in ipairs(cmd_parts) do
				table.insert(create_cmd, part)
			end
		end

		vim.fn.system(create_cmd)
		if vim.v.shell_error ~= 0 then
			vim.notify(
				"Failed to create session '" .. session_name .. "' (pterm new exited " .. vim.v.shell_error .. ")",
				vim.log.levels.ERROR
			)
			return
		end

		-- Wait for daemon socket before attach to avoid new->attach race.
		local ok = wait_for_socket(session_name, M.config.attach_wait_ms, M.config.attach_poll_ms)
		if not ok then
			vim.notify(
				"Session '" .. session_name .. "' was created but socket did not appear in time",
				vim.log.levels.ERROR
			)
			return
		end
	end

	-- Attach to session
	M.attach(session_name)
end

--- Attach to an existing session.
function M.attach(session_name)
	local sock = socket_path(session_name)
	local bin = find_binary()

	if vim.uv.fs_stat(sock) == nil then
		vim.notify("Session '" .. session_name .. "' not found", vim.log.levels.ERROR)
		return
	end

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

	local job_id = vim.fn.jobstart({ bin, "attach", session_name }, {
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
		vim.notify("Failed to start pterm attach for '" .. session_name .. "'", vim.log.levels.ERROR)
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

	vim.cmd("startinsert")
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

--- Setup function for lazy.nvim / packer etc.
function M.setup(opts)
	M.config = vim.tbl_deep_extend("force", M.config, opts or {})

	vim.api.nvim_create_user_command("Pterm", function(cmd_opts)
		M.open(cmd_opts.fargs[1], cmd_opts.fargs)
	end, {
		nargs = "*",
		complete = function()
			return M.list()
		end,
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

	vim.api.nvim_create_user_command("PtermKill", function(cmd_opts)
		M.kill(cmd_opts.fargs[1])
	end, {
		nargs = 1,
		complete = function()
			return M.list()
		end,
		desc = "Kill a persistent terminal session",
	})
end

return M
