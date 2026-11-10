-- Run with: nvim --headless -u NONE -l tests/neovim_grep_regressions.lua
vim.opt.runtimepath:prepend(vim.fn.getcwd())

-- Keep real buffers, highlight groups and extmarks; mock only Telescope and
-- session queries so the suite does not require a running daemon or plugins.
local picker
local query_count = 0
local expected_query_count = 3
local fixture = {
	colored = "before\nlead \27[1;31mred\27[0m plain\n\n\27[34m日本\27[0m tail\nafter",
	legacy = "plain old daemon\nsecond line",
}
package.preload["pterm"] = function()
	return {
		list = function()
			return { "colored", "legacy", "gone" }
		end,
		full_text = function(name)
			query_count = query_count + 1
			return fixture[name] and (fixture[name]:gsub("\27%[[%d;]*m", ""))
		end,
		full_text_ansi = function(name)
			query_count = query_count + 1
			return fixture[name]
		end,
	}
end
package.preload["telescope"] = function()
	return {
		register_extension = function(spec)
			return spec
		end,
	}
end
package.preload["telescope.pickers"] = function()
	return {
		new = function(_, spec)
			picker = spec
			return { find = function() end }
		end,
	}
end
package.preload["telescope.finders"] = function()
	return {
		new_table = function(spec)
			return spec
		end,
	}
end
package.preload["telescope.config"] = function()
	return {
		values = {
			generic_sorter = function(opts)
				return opts
			end,
		},
	}
end
package.preload["telescope.actions"] = function()
	return {}
end
package.preload["telescope.actions.state"] = function()
	return {}
end
package.preload["telescope.previewers"] = function()
	return {
		new_buffer_previewer = function(spec)
			return spec
		end,
	}
end

vim.g.terminal_color_1 = "#cc1122"
vim.g.terminal_color_4 = "#1122cc"
vim.api.nvim_set_hl(0, "Search", { bg = "#444444" })
vim.api.nvim_set_hl(0, "TelescopeMatching", { fg = "#ffffff", bold = true })
local extension = require("telescope._extensions.pterm")
local ansi = require("pterm.ansi")
local parse = ansi.parse
local parsed_lines = 0
ansi.parse = function(text)
	parsed_lines = parsed_lines + 1
	return parse(text)
end
extension.exports.grep()
assert(parsed_lines == 0, "grep eagerly parsed styles outside the preview")
assert(query_count == 3, "grep must query each session only once")
assert(#picker.finder.results == 6, "empty or failed session lines leaked into results")
local entry
for _, item in ipairs(picker.finder.results) do
	local result = picker.finder.entry_maker(item)
	assert(not result.display:find("\27", 1, true), "ANSI leaked into the result list")
	assert(result.ordinal == result.display, "search text differs from visible result")
	if item.line == "lead red plain" then
		entry = result
	end
end
assert(entry and entry.lnum == 2 and entry.grep_line == "lead red plain", "plain jump target changed")

local bufnr = vim.api.nvim_create_buf(false, true)
local self = { state = { bufnr = bufnr } }
local ns = vim.api.nvim_create_namespace("pterm_grep_preview")
local prompt = "red 日"
local status = {
	picker = {
		_get_prompt = function()
			return prompt
		end,
	},
}
local function render(selected)
	picker.previewer.define_preview(self, selected, status)
	assert(query_count == expected_query_count, "preview fetched another snapshot")
	assert(not vim.api.nvim_get_option_value("modifiable", { buf = bufnr }), "preview left editable")
	return vim.api.nvim_buf_get_extmarks(bufnr, ns, 0, -1, { details = true })
end
local marks = render(entry)
assert(
	table.concat(vim.api.nvim_buf_get_lines(bufnr, 0, -1, false), "\n") == "before\nlead red plain\n\n日本 tail\nafter"
)
local red, blue, match, selected
for _, mark in ipairs(marks) do
	local row, col, details = mark[2], mark[3], mark[4]
	if details.hl_group == "TelescopeMatching" then
		if row == 1 then
			assert(col == 5 and details.end_col == 8, "match range shifted by ANSI")
			match = details.priority
		else
			assert(row == 3 and col == 0 and details.end_col == #"日", "match range is not UTF-8 byte based")
		end
	elseif details.hl_group == "Search" then
		assert(row == 1, "wrong selected row")
		selected = details.priority
	else
		local hl = vim.api.nvim_get_hl(0, { name = details.hl_group })
		if row == 1 then
			assert(col == 5 and details.end_col == 8 and hl.fg == 0xcc1122 and hl.bold, "red/bold run lost")
			red = details.priority
		elseif row == 3 then
			assert(col == 0 and details.end_col == #"日本" and hl.fg == 0x1122cc, "Unicode run lost")
			blue = details.priority
		else
			error("style leaked to an unstyled line")
		end
	end
end
assert(red and blue and match and selected, "missing terminal, match or selection highlights")
assert(match > red and red > selected, "highlight layers hide original styles or matching words")

-- Repeated preview updates must replace, not accumulate, decorations.
assert(#render(entry) == #marks, "repeated selection accumulated extmarks")
assert(parsed_lines == 5, "unchanged preview was parsed again")
prompt = "tail"
marks = render({ value = "colored", lnum = 4 })
local matches = 0
for _, mark in ipairs(marks) do
	if mark[4].hl_group == "TelescopeMatching" then
		matches = matches + 1
		assert(mark[2] == 3 and mark[3] == #"日本 ", "stale prompt highlight survived")
	end
end
assert(matches == 1, "updated prompt has wrong number of highlights")

-- An older daemon's plain response remains usable; session switches clear ANSI.
prompt = ""
marks = render({ value = "legacy", lnum = 2 })
assert(#marks == 1 and marks[1][4].hl_group == "Search", "stale ANSI highlights survived session switch")
assert(vim.api.nvim_buf_get_lines(bufnr, 0, -1, false)[2] == "second line")

-- Context boundaries retain the logical line number and selected row.
local many = {}
for i = 1, 30 do
	many[i] = "\27[31mline " .. i .. "\27[0m"
end
fixture.colored = table.concat(many, "\n")
extension.exports.grep()
expected_query_count = 6
parsed_lines = 0
render({ value = "colored", lnum = 30 })
local lines = vim.api.nvim_buf_get_lines(bufnr, 0, -1, false)
assert(#lines == 10 and lines[1] == "line 21" and lines[10] == "line 30", "last-line context shifted")
assert(parsed_lines == 10, "styles were parsed outside the displayed context")

vim.api.nvim_buf_delete(bufnr, { force = true })
picker.previewer.define_preview(self, { value = "colored", lnum = 1 }, status)
print("Neovim grep regressions passed")
