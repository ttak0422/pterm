local telescope = require("telescope")
local pickers = require("telescope.pickers")
local finders = require("telescope.finders")
local conf = require("telescope.config").values
local actions = require("telescope.actions")
local action_state = require("telescope.actions.state")
local previewers = require("telescope.previewers")
local ansi = require("pterm.ansi")

local function set_preview_lines(bufnr, lines)
	if not bufnr or not vim.api.nvim_buf_is_valid(bufnr) then
		return
	end

	local ok = pcall(function()
		vim.api.nvim_set_option_value("modifiable", true, { buf = bufnr })
		vim.api.nvim_buf_set_lines(bufnr, 0, -1, false, lines)
		vim.api.nvim_set_option_value("modifiable", false, { buf = bufnr })
	end)
	if not ok and vim.api.nvim_buf_is_valid(bufnr) then
		pcall(vim.api.nvim_set_option_value, "modifiable", false, { buf = bufnr })
	end
end

local function apply_highlights(bufnr, ns, preview)
	vim.api.nvim_buf_clear_namespace(bufnr, ns, 0, -1)
	for i, item in ipairs(preview) do
		local col = 0
		for _, run in ipairs(item.runs) do
			local group = ansi.hl_group(run.attrs)
			local len = #run.text
			if group and len > 0 then
				pcall(vim.api.nvim_buf_set_extmark, bufnr, ns, i - 1, col, {
					end_col = col + len,
					hl_group = group,
				})
			end
			col = col + len
		end
	end
end

local function make_previewer(pterm)
	local ns = vim.api.nvim_create_namespace("pterm_preview")
	return previewers.new_buffer_previewer({
		title = "pterm preview",
		define_preview = function(self, entry, status)
			status = status or {}
			self.state = self.state or {}

			local session_name = entry and entry.value
			if not session_name then
				return
			end

			local request_id = (self.state.pterm_preview_request_id or 0) + 1
			self.state.pterm_preview_request_id = request_id

			local previous_job = self.state.pterm_preview_job
			if previous_job and previous_job.kill then
				pcall(previous_job.kill, previous_job, 15)
			end
			self.state.pterm_preview_job = nil

			local width = 80
			if status.preview_win and vim.api.nvim_win_is_valid(status.preview_win) then
				width = vim.api.nvim_win_get_width(status.preview_win)
			end
			local height = 20
			if status.preview_win and vim.api.nvim_win_is_valid(status.preview_win) then
				height = vim.api.nvim_win_get_height(status.preview_win)
			end

			local bufnr = self.state.bufnr
			set_preview_lines(bufnr, { "Loading preview..." })

			self.state.pterm_preview_job = pterm.snapshot_ansi_async(session_name, function(text, err)
				if not self.state or self.state.pterm_preview_request_id ~= request_id then
					return
				end
				self.state.pterm_preview_job = nil

				local preview
				if text then
					preview = ansi.preview_lines(text, width, height)
				else
					preview = {
						{
							text = "Failed to load preview",
							runs = {},
						},
						{
							text = vim.trim(tostring(err or "")),
							runs = {},
						},
					}
				end

				local lines = {}
				for _, item in ipairs(preview) do
					lines[#lines + 1] = item.text
				end
				set_preview_lines(bufnr, lines)
				apply_highlights(bufnr, ns, preview)
			end)
		end,
	})
end

--- Preview the matched line in context. `session_lines` is the same
--- `full_text` split that produced the entries, so no extra process runs here.
local function make_grep_previewer(session_lines)
	local ns = vim.api.nvim_create_namespace("pterm_grep_preview")
	return previewers.new_buffer_previewer({
		title = "pterm grep preview",
		define_preview = function(self, entry, status)
			status = status or {}
			self.state = self.state or {}

			local all_lines = entry and session_lines[entry.value]
			local match_index = entry and entry.lnum
			if not all_lines or not match_index then
				return
			end

			local height = 20
			if status.preview_win and vim.api.nvim_win_is_valid(status.preview_win) then
				height = vim.api.nvim_win_get_height(status.preview_win)
			end
			local context = math.max(0, math.floor((height - 1) / 2))
			local first = math.max(1, match_index - context)
			local last = math.min(#all_lines, match_index + context)
			local cursor_row = match_index - first + 1

			local bufnr = self.state.bufnr
			set_preview_lines(bufnr, vim.list_slice(all_lines, first, last))
			if not bufnr or not vim.api.nvim_buf_is_valid(bufnr) then
				return
			end

			vim.api.nvim_buf_clear_namespace(bufnr, ns, 0, -1)
			pcall(vim.api.nvim_buf_set_extmark, bufnr, ns, cursor_row - 1, 0, {
				end_row = cursor_row,
				hl_group = "Search",
				hl_eol = true,
			})
			if status.preview_win and vim.api.nvim_win_is_valid(status.preview_win) then
				pcall(vim.api.nvim_win_set_cursor, status.preview_win, { cursor_row, 0 })
			end
		end,
	})
end

--- Move the cursor onto the matched line inside the session's terminal buffer.
--- The buffer is filled by an asynchronous history replay, so the lookup is
--- retried for a short while before giving up.
local function jump_to_grep_line(session_name, match_line, attempts)
	if not match_line or match_line == "" then
		return
	end

	attempts = attempts or 10
	vim.defer_fn(function()
		local bufnr = vim.api.nvim_get_current_buf()
		if not vim.api.nvim_buf_is_valid(bufnr) or vim.api.nvim_buf_get_name(bufnr) ~= "pterm://" .. session_name then
			return
		end

		for i, line in ipairs(vim.api.nvim_buf_get_lines(bufnr, 0, -1, false)) do
			if line == match_line or line:find(match_line, 1, true) then
				-- terminal-mode pins the cursor to the PTY: normal mode is the only
				-- mode where the window cursor can be moved into the scrollback.
				vim.cmd("stopinsert")
				pcall(vim.api.nvim_win_set_cursor, 0, { i, 0 })
				return
			end
		end

		if attempts > 1 then
			jump_to_grep_line(session_name, match_line, attempts - 1)
		end
	end, 50)
end

local function session_exists(pterm, session_name)
	for _, name in ipairs(pterm.list()) do
		if name == session_name then
			return true
		end
	end
	return false
end

local function sessions(opts)
	opts = opts or {}

	local ok, pterm = pcall(require, "pterm")
	if not ok then
		vim.notify("Failed to load pterm module", vim.log.levels.ERROR)
		return
	end

	local session_names = pterm.list()

	local entries = {}
	for _, name in ipairs(session_names) do
		local connected = pterm.is_connected(name)
		local label = pterm.display_name(name)
		-- ordinal stays the raw name: matching on the cwd-basename suffix of
		-- the label would break the Enter-creates-new-session flow whenever
		-- the typed name collides with an existing session's directory name.
		table.insert(entries, {
			value = name,
			ordinal = name,
			display = connected and ("✔ " .. label) or ("  " .. label),
		})
	end

	local previewer = opts.previewer
	if previewer == nil then
		previewer = make_previewer(pterm)
	end

	pickers
		.new(opts, {
			prompt_title = "pterm sessions",
			finder = finders.new_table({
				results = entries,
				entry_maker = function(entry)
					return {
						value = entry.value,
						ordinal = entry.ordinal,
						display = entry.display,
					}
				end,
			}),
			sorter = conf.generic_sorter(opts),
			previewer = previewer,
			attach_mappings = function(prompt_bufnr)
				actions.select_default:replace(function()
					actions.close(prompt_bufnr)

					local selection = action_state.get_selected_entry()
					if not selection or not selection.value then
						local session_name = vim.trim(action_state.get_current_line())
						if session_name == "" then
							return
						end

						local open_ok, err = pcall(pterm.open, session_name)
						if not open_ok then
							vim.notify(
								"Failed to open session '" .. session_name .. "': " .. tostring(err),
								vim.log.levels.ERROR
							)
						end
						return
					end

					local session_name = selection.value
					if not session_exists(pterm, session_name) then
						vim.notify("Session '" .. session_name .. "' not found", vim.log.levels.ERROR)
						return
					end

					local open_ok, err = pcall(pterm.open, session_name)
					if not open_ok then
						vim.notify(
							"Failed to open session '" .. session_name .. "': " .. tostring(err),
							vim.log.levels.ERROR
						)
					end
				end)
				return true
			end,
		})
		:find()
end

--- Full-text search across every session's contents (scrollback + screen).
--- Each matching line becomes an entry; selecting one opens its session.
local function grep(opts)
	opts = opts or {}

	local ok, pterm = pcall(require, "pterm")
	if not ok then
		vim.notify("Failed to load pterm module", vim.log.levels.ERROR)
		return
	end

	local results = {}
	local session_lines = {}
	for _, name in ipairs(pterm.list()) do
		local text = pterm.full_text(name)
		if text then
			local lines = vim.split(text, "\n", { plain = true })
			session_lines[name] = lines
			for lnum, line in ipairs(lines) do
				if vim.trim(line) ~= "" then
					table.insert(results, { session = name, line = line, lnum = lnum })
				end
			end
		end
	end

	local previewer = opts.previewer
	if previewer == nil then
		previewer = make_grep_previewer(session_lines)
	end

	pickers
		.new(opts, {
			prompt_title = "pterm grep",
			finder = finders.new_table({
				results = results,
				entry_maker = function(entry)
					local text = entry.session .. ": " .. entry.line
					return {
						value = entry.session,
						grep_line = entry.line,
						lnum = entry.lnum,
						ordinal = text,
						display = text,
					}
				end,
			}),
			sorter = conf.generic_sorter(opts),
			previewer = previewer,
			attach_mappings = function(prompt_bufnr)
				actions.select_default:replace(function()
					actions.close(prompt_bufnr)

					local selection = action_state.get_selected_entry()
					if not selection or not selection.value then
						return
					end

					local session_name = selection.value
					local match_line = selection.grep_line
					if not session_exists(pterm, session_name) then
						vim.notify("Session '" .. session_name .. "' not found", vim.log.levels.ERROR)
						return
					end

					local open_ok, err = pcall(pterm.open, session_name)
					if not open_ok then
						vim.notify(
							"Failed to open session '" .. session_name .. "': " .. tostring(err),
							vim.log.levels.ERROR
						)
					else
						jump_to_grep_line(session_name, match_line)
					end
				end)
				return true
			end,
		})
		:find()
end

return telescope.register_extension({
	exports = {
		sessions = sessions,
		grep = grep,
		pterm = sessions,
	},
})
