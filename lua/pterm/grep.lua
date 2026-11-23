-- Grep results identify logical lines in one immutable full-text snapshot.
local M = {}

function M.snapshot(lines)
	local text = {}
	for i, line in ipairs(lines) do
		text[i] = type(line) == "table" and line.text or line
	end
	-- Screen padding changes with window height; it is not a search target.
	while text[#text] == "" do
		table.remove(text)
	end
	return text
end

-- Match the complete snapshot, retaining every logical newline, but allow a
-- logical line to span several native rows. Full context disambiguates equal
-- lines; substring matches and partially retained snapshots are never enough.
function M.locate(native, snapshot, lnum)
	if not snapshot[lnum] or snapshot[lnum] == "" then
		return nil
	end
	local first = 1
	while snapshot[first] == "" do
		first = first + 1
	end
	local flat = table.concat(native)
	local needle = table.concat(snapshot, "", first)
	local starts, offset = {}, 1
	for row, line in ipairs(native) do
		if line ~= "" then
			starts[offset] = row
		end
		offset = offset + #line
	end
	local found, from = nil, 1
	while true do
		local at = flat:find(needle, from, true)
		if not at then
			return found
		end
		from = at + 1
		local row = starts[at]
		local target
		if row then
			-- Leading empty logical lines are significant even though the byte
			-- prefilter above cannot represent them.
			row = row - first + 1
			for logical, line in ipairs(snapshot) do
				if logical == lnum then
					target = row
				end
				if line == "" then
					if native[row] ~= "" then
						row = nil
						break
					end
					row = row + 1
				else
					local consumed = 0
					repeat
						local fragment = native[row]
						if
							not fragment
							or fragment == ""
							or line:sub(consumed + 1, consumed + #fragment) ~= fragment
						then
							row = nil
							break
						end
						consumed = consumed + #fragment
						row = row + 1
					until consumed == #line
					if not row then
						break
					end
				end
			end
		end
		if row and target then
			if found and found ~= target then
				return nil, "ambiguous"
			end
			found = target
		end
	end
end

local pending = {}

function M.jump(win, buf, snapshot, lnum, ready)
	if pending[win] then
		pending[win]()
	end
	local active = true
	local ns = vim.api.nvim_create_namespace("pterm_grep_jump_" .. win)
	local group = vim.api.nvim_create_augroup("pterm_grep_jump_" .. win, { clear = true })
	local function cancel()
		if not active then
			return
		end
		active = false
		pending[win] = nil
		vim.on_key(nil, ns)
		vim.api.nvim_del_augroup_by_id(group)
	end
	pending[win] = cancel
	local function valid()
		return active
			and vim.api.nvim_win_is_valid(win)
			and vim.api.nvim_buf_is_valid(buf)
			and vim.api.nvim_get_current_win() == win
			and vim.api.nvim_win_get_buf(win) == buf
	end
	local last_tick
	local function attempt()
		if not valid() then
			cancel()
			return
		end
		if ready and not ready() then
			return
		end
		local tick = vim.api.nvim_buf_get_changedtick(buf)
		if tick == last_tick then
			return
		end
		last_tick = tick
		local row = M.locate(vim.api.nvim_buf_get_lines(buf, 0, -1, false), snapshot, lnum)
		if row then
			cancel()
			vim.cmd("stopinsert")
			vim.api.nvim_win_set_cursor(win, { row, 0 })
			vim.api.nvim_win_call(win, function()
				vim.cmd("normal! zz")
			end)
		end
	end
	vim.on_key(cancel, ns)
	vim.api.nvim_create_autocmd({ "WinLeave", "BufLeave", "WinClosed", "BufWipeout" }, {
		group = group,
		callback = function(event)
			if
				not valid()
				or (event.event == "WinLeave" and vim.api.nvim_get_current_win() == win)
				or (event.event == "BufLeave" and event.buf == buf)
				or (event.event == "WinClosed" and tonumber(event.match) == win)
				or (event.event == "BufWipeout" and event.buf == buf)
			then
				cancel()
			end
		end,
	})
	vim.api.nvim_create_autocmd({ "TextChanged", "TextChangedT" }, {
		group = group,
		buffer = buf,
		callback = function()
			vim.schedule(attempt)
		end,
	})
	-- The OSC ready marker may precede Neovim's throttled native refresh.
	-- Buffer changes drive retries; polling also covers a ready-only change.
	local function poll()
		attempt()
		if active then
			vim.defer_fn(poll, 50)
		end
	end
	vim.defer_fn(poll, 50)
	vim.defer_fn(function()
		if valid() then
			cancel()
			vim.notify("pterm grep: selected snapshot is unavailable or ambiguous; reopen grep", vim.log.levels.WARN)
		else
			cancel()
		end
	end, 5000)
end

return M
