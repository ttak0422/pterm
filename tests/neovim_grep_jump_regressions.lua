-- Native terminal buffers and the real picker callbacks; only the daemon and
-- Telescope UI are mocked. No user configuration, sessions or history is used.
vim.opt.runtimepath:prepend(vim.fn.getcwd())
local target = require("pterm.grep")
local function locate(native, snapshot, lnum, expected)
	local row = target.locate(native, target.snapshot(snapshot), lnum)
	assert(
		row == expected,
		vim.inspect({ native = native, snapshot = snapshot, selected = lnum, got = row, expected = expected })
	)
end
locate({ "before", "needle", "between", "needle", "after" }, { "before", "needle", "between", "needle", "after" }, 4, 4)
locate(
	{ "prefix needle suffix", "between", "needle", "after" },
	{ "prefix needle suffix", "between", "needle", "after" },
	3,
	3
)
locate({ "before", "abc", "日本", "needle", "after" }, { "before", "abc日本needle", "after" }, 2, 2)
locate({ "before", "日本 needle needle", "after" }, { "before", "日本 needle needle", "after" }, 2, 2)
locate({ "", "", "before", "", "needle", "after", "" }, { "", "before", "", "needle", "after", "", "" }, 4, 5)
locate({ "needle", "after" }, { "before", "needle", "after" }, 2, nil)
locate({ "before", "needle new", "after" }, { "before", "needle old", "after" }, 2, nil)
locate({ "before", "needle", "after", "before", "needle", "after" }, { "before", "needle", "after" }, 2, nil)
-- Identical concatenated bytes with different logical boundaries are not equal.
locate({ "ab", "cd", "ef" }, { "abc", "def" }, 2, nil)

local repeated = { "unique-start" }
for _ = 1, 10000 do
	repeated[#repeated + 1] = "same repeated log line"
end
repeated[#repeated + 1] = "unique-end"
locate(repeated, repeated, 9000, 9000)

local picker, selected, select_default, fixture, buf, chan, win
local notices = {}
-- selene: allow(incorrect_standard_library_use)
vim.notify = function(message)
	notices[#notices + 1] = message
end
package.preload["pterm"] = function()
	return {
		list = function()
			return { "fixture" }
		end,
		full_text_ansi = function()
			return table.concat(fixture.current or fixture.snapshot, "\n")
		end,
		open = function()
			vim.api.nvim_set_current_win(win)
			vim.api.nvim_set_current_buf(buf)
			if fixture.delay then
				vim.defer_fn(function()
					vim.api.nvim_chan_send(chan, fixture.bytes)
				end, fixture.delay)
			end
		end,
		_grep_ready = function()
			return fixture.ready ~= false
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
	return { values = {
		generic_sorter = function(opts)
			return opts
		end,
	} }
end
package.preload["telescope.previewers"] = function()
	return {
		new_buffer_previewer = function(spec)
			return spec
		end,
	}
end
package.preload["telescope.actions"] = function()
	return {
		close = function()
			selected = nil
		end,
		select_default = {
			replace = function(_, fn)
				select_default = fn
			end,
		},
	}
end
package.preload["telescope.actions.state"] = function()
	return {
		get_selected_entry = function()
			return selected
		end,
	}
end
local extension = require("telescope._extensions.pterm")
vim.o.columns, vim.o.lines = 120, 40
local function start(case)
	fixture = case
	local current = case.current
	case.current = nil
	notices = {}
	buf = vim.api.nvim_create_buf(false, true)
	vim.api.nvim_buf_set_name(buf, "pterm://fixture")
	win = vim.api.nvim_open_win(
		buf,
		true,
		{ relative = "editor", row = 0, col = 0, width = 40, height = 12, style = "minimal" }
	)
	chan = vim.api.nvim_open_term(buf, {})
	case.bytes = table.concat(case.native or case.snapshot, "\r\n") .. "\r\nSENTINEL"
	if not case.delay then
		vim.api.nvim_chan_send(chan, case.bytes)
	end
	vim.wait(70, function()
		return false
	end, 10)
	local sentinel = 1
	for i, line in ipairs(vim.api.nvim_buf_get_lines(buf, 0, -1, false)) do
		if line == "SENTINEL" then
			sentinel = i
		end
	end
	vim.api.nvim_win_set_cursor(win, { sentinel, 0 })
	extension.exports.grep()
	for _, entry in ipairs(picker.finder.results) do
		if entry.lnum == case.lnum then
			selected = picker.finder.entry_maker(entry)
		end
	end
	assert(selected, "missing selected result")
	picker.attach_mappings(0)
	fixture.current = current
	select_default()
end
local function finish()
	vim.api.nvim_win_close(win, true)
	vim.api.nvim_buf_delete(buf, { force = true })
end
local function success(case, row)
	start(case)
	assert(
		vim.wait(2000, function()
			return vim.api.nvim_win_get_cursor(win)[1] == row
		end, 10),
		"wrong native row: " .. case.name
	)
	assert(vim.api.nvim_win_get_cursor(win)[2] == 0, "grep navigation must retain row-start semantics")
	assert(#notices == 0, vim.inspect(notices))
	finish()
end
success({ name = "duplicate", snapshot = { "before", "needle", "between", "needle", "after" }, lnum = 4 }, 4)
success({ name = "containing line", snapshot = { "prefix needle suffix", "between", "needle", "after" }, lnum = 3 }, 3)
success({ name = "Unicode", snapshot = { "before", "日本語 needle needle", "after" }, lnum = 2 }, 2)
local long = "prefix-" .. string.rep("x", 50) .. "-needle-" .. string.rep("y", 30) .. "-end"
success({ name = "wrapped ASCII", snapshot = { "before", long, "after" }, lnum = 2 }, 2)
success(
	{ name = "wrapped Unicode", snapshot = { "before", string.rep("日", 24) .. " needle tail", "after" }, lnum = 2 },
	2
)
success({ name = "late replay", snapshot = { "before", "needle", "after" }, delay = 700, lnum = 2 }, 2)

-- A changed snapshot is rejected before opening/jumping to another occurrence.
start({ snapshot = { "before", "needle old", "after" }, current = { "before", "needle new", "after" }, lnum = 2 })
assert(#notices == 1 and notices[1]:find("output changed", 1, true), "stale snapshot was accepted")
finish()

-- Leaving and returning before a retry, replacing the buffer, a key, closing
-- the window, and a superseding selection must all cancel the old request.
for _, action in ipairs({ "leave", "buffer", "key", "close", "supersede" }) do
	start({ snapshot = { "before", "needle", "after" }, lnum = 2, ready = false })
	local oldbuf, oldwin = buf, win
	if action == "leave" then
		local other = vim.api.nvim_open_win(
			vim.api.nvim_create_buf(false, true),
			true,
			{ relative = "editor", row = 1, col = 1, width = 20, height = 4 }
		)
		vim.api.nvim_win_close(other, true)
		vim.api.nvim_set_current_win(win)
	elseif action == "buffer" then
		vim.api.nvim_set_current_buf(vim.api.nvim_create_buf(false, true))
		vim.api.nvim_set_current_buf(buf)
	elseif action == "key" then
		vim.api.nvim_feedkeys("l", "nx", false)
	elseif action == "close" then
		vim.api.nvim_win_close(win, true)
	elseif action == "supersede" then
		target.jump(win, buf, { "before", "needle", "after" }, 3, function()
			return true
		end)
	end
	fixture.ready = true
	vim.wait(200, function()
		return false
	end, 10)
	if action ~= "close" then
		local expected = action == "supersede" and 3 or 4
		assert(vim.api.nvim_win_get_cursor(win)[1] == expected, "cancel failed: " .. action)
		vim.api.nvim_win_close(oldwin, true)
	end
	vim.api.nvim_buf_delete(oldbuf, { force = true })
	assert(#notices == 0, "cancelled request notified")
end

start({ snapshot = { "before", "needle", "after" }, native = { "needle", "after" }, lnum = 2 })
local initial = vim.api.nvim_win_get_cursor(win)
assert(
	vim.wait(5500, function()
		return #notices > 0
	end, 20),
	"missing target did not report timeout"
)
assert(vim.deep_equal(vim.api.nvim_win_get_cursor(win), initial), "timeout moved the cursor")
assert(notices[1]:find("unavailable or ambiguous", 1, true), "timeout was silent")
finish()
print("Neovim grep jump regressions passed")
