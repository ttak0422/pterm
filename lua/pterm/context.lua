local M = {}

-- pterm-proto SUBSCRIBE_CONTEXT / CONTEXT. Keep this local control channel
-- separate from the PTY stream so terminal escape sequences cannot set context.
local request_type, response_type = 0x0d, 0x8a
local max_payload = 16 * 1024

function M.subscribe(path, fields, update)
	local pipe = vim.uv.new_pipe(false)
	local buffer, established = "", false
	local function close()
		if not pipe:is_closing() then
			pipe:close()
		end
	end
	local function fail()
		close()
		vim.schedule(function()
			update(nil)
		end)
	end
	local function read(err, bytes)
		if err or not bytes then
			fail()
			return
		end
		buffer = buffer .. bytes
		while #buffer >= 5 do
			local kind, a, b, c, d = buffer:byte(1, 5)
			local size = a + b * 256 + c * 65536 + d * 16777216
			if kind ~= response_type or size > max_payload then
				fail()
				return
			end
			if #buffer < size + 5 then
				return
			end
			local ok, data = pcall(vim.json.decode, buffer:sub(6, 5 + size))
			if not ok or type(data) ~= "table" then
				fail()
				return
			end
			established = true
			buffer = buffer:sub(6 + size)
			vim.schedule(function()
				if not pipe:is_closing() then
					update(data)
				end
			end)
		end
	end
	pipe:connect(path, function(err)
		if pipe:is_closing() then
			return
		end
		if err then
			fail()
			return
		end
		pipe:read_start(read)
		pipe:write(string.char(request_type, 1, 0, 0, 0, fields), function(write_err)
			if write_err then
				fail()
			end
		end)
	end)
	vim.defer_fn(function()
		if not established and not pipe:is_closing() then
			fail()
		end
	end, 500)
	return close
end

return M
