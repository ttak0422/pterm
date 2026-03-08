local telescope = require("telescope")
local pickers = require("telescope.pickers")
local finders = require("telescope.finders")
local conf = require("telescope.config").values
local actions = require("telescope.actions")
local action_state = require("telescope.actions.state")

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
	if #session_names == 0 then
		vim.notify("No active pterm sessions", vim.log.levels.INFO)
		return
	end

	local entries = {}
	for _, name in ipairs(session_names) do
		local connected = pterm.connections[name] ~= nil
		table.insert(entries, {
			value = name,
			ordinal = name,
			display = connected and ("[connected] " .. name) or name,
		})
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
			attach_mappings = function(prompt_bufnr)
				actions.select_default:replace(function()
					actions.close(prompt_bufnr)

					local selection = action_state.get_selected_entry()
					if not selection or not selection.value then
						return
					end

					local session_name = selection.value
					if not session_exists(pterm, session_name) then
						vim.notify("Session '" .. session_name .. "' not found", vim.log.levels.ERROR)
						return
					end

					local open_ok, err = pcall(pterm.open, session_name)
					if not open_ok then
						vim.notify("Failed to open session '" .. session_name .. "': " .. tostring(err), vim.log.levels.ERROR)
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
