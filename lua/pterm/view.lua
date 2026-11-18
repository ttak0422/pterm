-- Reading positions are UI state, independent of the application's PTY cursor.
local M = {}

local function snapshot(win, buf)
	local view = vim.api.nvim_win_call(win, vim.fn.winsaveview)
	local row = view.lnum
	local first = math.max(0, row - 2)
	local lines = vim.api.nvim_buf_get_lines(buf, first, math.min(vim.api.nvim_buf_line_count(buf), row + 1), false)
	local index = row - first
	-- Use an exact three-line anchor. Never guess a logical position after
	-- reflow/truncation or among repeated log lines.
	return { view = view, context = { before = lines[index - 1], text = lines[index], after = lines[index + 1] } }
end

local function locate(lines, anchor)
	local found
	for row = 1, #lines do
		if
			lines[row] == anchor.context.text
			and (not anchor.context.before or lines[row - 1] == anchor.context.before)
			and (not anchor.context.after or lines[row + 1] == anchor.context.after)
		then
			if found then
				return nil
			end
			found = row
		end
	end
	return found
end

local function apply(win, anchor, row)
	local view = vim.deepcopy(anchor.view)
	view.topline = math.max(1, view.topline + row - view.lnum)
	view.lnum = row
	vim.api.nvim_win_call(win, function()
		vim.fn.winrestview(view)
	end)
end

function M.new(buf)
	local state = { buf = buf, pending = nil }
	function state:save()
		self.ready = false
		-- A newer reset supersedes an in-flight replay, but must not replace
		-- the original anchors with the temporarily cleared native buffer.
		if self.pending then
			return
		end
		local saved = {}
		local current = vim.api.nvim_get_current_win()
		local input = vim.api.nvim_get_mode().mode:sub(1, 1) == "t"
		for _, win in ipairs(vim.api.nvim_list_wins()) do
			if vim.api.nvim_win_get_buf(win) == self.buf and not (win == current and input) then
				saved[win] = snapshot(win, self.buf)
			end
		end
		self.pending = saved
		-- A timeout only abandons stale anchors; it never moves a cursor.
		vim.defer_fn(function()
			if self.pending == saved then
				self.pending = nil
			end
		end, 30000)
	end
	function state:cancel()
		if self.pending then
			self.pending[vim.api.nvim_get_current_win()] = nil
			if next(self.pending) == nil then
				self.pending = nil
			end
		end
	end
	function state:restore(from_update)
		self.ready = true
		local saved = self.pending
		if not saved then
			return
		end
		vim.schedule(function()
			if self.pending ~= saved or not vim.api.nvim_buf_is_valid(self.buf) then
				return
			end
			local lines = vim.api.nvim_buf_get_lines(self.buf, 0, -1, false)
			for win, anchor in pairs(saved) do
				if
					vim.api.nvim_win_is_valid(win)
					and vim.api.nvim_win_get_buf(win) == self.buf
					and not (win == vim.api.nvim_get_current_win() and vim.api.nvim_get_mode().mode:sub(1, 1) == "t")
				then
					local found = locate(lines, anchor)
					if found then
						apply(win, anchor, found)
						saved[win] = nil
					elseif from_update then
						-- The completed native refresh has no unique anchor. Do not
						-- reconsider it against unrelated future application output.
						saved[win] = nil
					end
				else
					saved[win] = nil
				end
			end
			if next(saved) == nil then
				self.pending = nil
			end
		end)
	end
	-- TermRequest can precede Neovim's throttled native-buffer refresh.
	-- Retry from the actual buffer update, not an assumed rendering delay.
	vim.api.nvim_buf_attach(buf, false, {
		on_lines = function()
			if state.pending and state.ready then
				state:restore(true)
			end
		end,
	})
	return state
end
return M
