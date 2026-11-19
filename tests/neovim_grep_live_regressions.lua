-- Real daemon + real Telescope test, also run by the Nix integration check.
-- For manual runs, build target/release/pterm first.
-- PTERM_TELESCOPE_RTP and PTERM_PLENARY_RTP must point to disposable checkouts.
vim.opt.runtimepath:prepend(vim.fn.getcwd())
for _, name in ipairs({ "PTERM_TELESCOPE_RTP", "PTERM_PLENARY_RTP" }) do
	assert(vim.env[name], name .. " is required")
	vim.opt.runtimepath:prepend(vim.env[name])
end
vim.o.columns, vim.o.lines = 120, 40
local pterm = require("pterm")
local temp = assert(vim.uv.fs_mkdtemp("/tmp/pterm-grep-XXXXXX"))
pterm.setup({ socket_dir = temp, auto_redraw = false, shell = vim.fn.exepath("sh") })
require("telescope").setup({ defaults = { history = false, sorting_strategy = "ascending" } })
require("telescope").load_extension("pterm")
local state, actions = require("telescope.actions.state"), require("telescope.actions")
local sessions = {}
local terminal_error
local jobstart = vim.fn.jobstart
-- selene: allow(incorrect_standard_library_use)
vim.fn.jobstart = function(command, opts)
	if opts and opts.term then
		local terminal_buf = vim.api.nvim_get_current_buf()
		local on_exit = opts.on_exit
		opts.on_exit = function(...)
			if vim.api.nvim_buf_is_valid(terminal_buf) then
				terminal_error = table.concat(vim.api.nvim_buf_get_lines(terminal_buf, 0, -1, false), "\n")
			end
			if on_exit then
				on_exit(...)
			end
		end
	end
	return jobstart(command, opts)
end
local function check(name, output, wanted, occurrence, native_target)
	sessions[#sessions + 1] = name
	local original = vim.api.nvim_get_current_win()
	local win = vim.api.nvim_open_win(
		vim.api.nvim_create_buf(false, true),
		true,
		{ relative = "editor", row = 0, col = 0, width = 40, height = 12, style = "minimal" }
	)
	-- Fixed synthetic output; no user shell startup, input or sessions.
	pterm.open(name, { name, "sh", "-c", [[printf '%s\n' "$1"; exec sleep 30]], "pterm-grep-fixture", output })
	local buf = vim.api.nvim_get_current_buf()
	assert(
		vim.wait(4000, function()
			return vim.api.nvim_buf_is_valid(buf)
				and table.concat(vim.api.nvim_buf_get_lines(buf, 0, -1, false), "\n"):find("SENTINEL", 1, true)
				and pterm._grep_ready(name)
		end, 20),
		"managed replay did not become ready: " .. tostring(terminal_error)
	)
	vim.cmd("stopinsert")
	local snapshot = pterm.full_text(name)
	assert(snapshot and snapshot:find(wanted, 1, true), "daemon did not rejoin logical target")
	local logical, count
	count = 0
	for i, line in ipairs(vim.split(snapshot, "\n", { plain = true })) do
		if line == wanted then
			count = count + 1
			if count == occurrence then
				logical = i
			end
		end
	end
	assert(logical, "logical target not found")
	local endrow, expected, native_count = 1, nil, 0
	for i, line in ipairs(vim.api.nvim_buf_get_lines(buf, 0, -1, false)) do
		if line == "SENTINEL" then
			endrow = i
		end
		if line == (native_target or wanted) then
			native_count = native_count + 1
			if native_count == occurrence then
				expected = i
			end
		end
	end
	assert(expected, "known native target row was not rendered")
	vim.api.nvim_win_set_cursor(win, { endrow, 0 })
	require("telescope").extensions.pterm.grep({ default_text = "needle" })
	local prompt = vim.api.nvim_get_current_buf()
	local picker
	assert(
		vim.wait(3000, function()
			picker = state.get_current_picker(prompt)
			local entry = state.get_selected_entry()
			return picker
				and type(picker.manager) == "table"
				and picker.manager:num_results() > 0
				and entry
				and entry.value == name
		end, 10),
		"new picker did not finish populating"
	)
	local found = false
	for i = 0, picker.manager:num_results() - 1 do
		picker:set_selection(i)
		local e = state.get_selected_entry()
		if e.value == name and e.lnum == logical then
			found = true
			break
		end
	end
	assert(found, "Telescope selected target not found")
	actions.select_default(prompt)
	assert(
		vim.wait(4000, function()
			return vim.api.nvim_get_current_buf() == buf and vim.api.nvim_win_get_cursor(win)[1] == expected
		end, 10),
		"real grep jump failed: " .. name .. "\n" .. vim.inspect(vim.api.nvim_buf_get_lines(buf, 0, -1, false))
	)
	assert(vim.api.nvim_win_get_cursor(win)[2] == 0, "row-start column changed")
	print("Real daemon + Telescope grep passed: " .. name)
	pterm.kill(name)
	table.remove(sessions)
	vim.api.nvim_win_close(win, true)
	vim.api.nvim_set_current_win(original)
end
local ok, err = xpcall(function()
	check("duplicate", "before\nneedle\nbetween\nneedle\nafter\nSENTINEL", "needle", 2)
	check("substring", "prefix needle suffix\nbetween\nneedle\nafter\nSENTINEL", "needle", 1)
	local long = "prefix-" .. string.rep("x", 50) .. "-needle-" .. string.rep("y", 30) .. "-end"
	check("wrap", "before\n" .. long .. "\nafter\nSENTINEL", long, 1, long:sub(1, 40))
	local wide = string.rep("日", 24) .. " needle tail"
	check("unicode", "before\n" .. wide .. "\nafter\nSENTINEL", wide, 1, string.rep("日", 20))
end, debug.traceback)
for _, name in ipairs(sessions) do
	pcall(pterm.kill, name)
end
-- selene: allow(incorrect_standard_library_use)
vim.fn.jobstart = jobstart
vim.fn.delete(temp, "rf")
assert(ok, err)
print("Real daemon + Telescope grep regressions passed")
