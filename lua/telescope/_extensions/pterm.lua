local telescope = require("telescope")
local pickers = require("telescope.pickers")
local finders = require("telescope.finders")
local conf = require("telescope.config").values
local actions = require("telescope.actions")
local action_state = require("telescope.actions.state")
local previewers = require("telescope.previewers")

local function trim_right_to_width(line, width)
	if width <= 0 then
		return ""
	end
	if vim.fn.strdisplaywidth(line) <= width then
		return line
	end

	local suffix = "..."
	if width <= #suffix then
		return suffix:sub(1, width)
	end

	local target_width = width - #suffix
	local low = 0
	local high = vim.fn.strchars(line)
	while low < high do
		local mid = math.ceil((low + high) / 2)
		local head = vim.fn.strcharpart(line, 0, mid)
		if vim.fn.strdisplaywidth(head) <= target_width then
			low = mid
		else
			high = mid - 1
		end
	end

	return vim.fn.strcharpart(line, 0, low) .. suffix
end

local function tail_preview_lines(text, width, height)
	local lines = vim.split(text or "", "\n", { plain = true })
	while #lines > 0 and lines[#lines] == "" do
		table.remove(lines)
	end
	if #lines == 0 then
		return { "(empty)" }
	end

	local first = math.max(1, #lines - height + 1)
	local preview = {}
	for i = first, #lines do
		table.insert(preview, trim_right_to_width(lines[i], width))
	end
	return preview
end

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

local function make_previewer(pterm)
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

			self.state.pterm_preview_job = pterm.snapshot_text_async(session_name, function(text, err)
				if not self.state or self.state.pterm_preview_request_id ~= request_id then
					return
				end
				self.state.pterm_preview_job = nil

				local lines
				if text then
					lines = tail_preview_lines(text, width, height)
				else
					lines = { "Failed to load preview", vim.trim(tostring(err or "")) }
				end

				set_preview_lines(bufnr, lines)
			end)
		end,
	})
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

return telescope.register_extension({
	exports = {
		sessions = sessions,
		pterm = sessions,
	},
})
