--- ANSI SGR handling for pterm previews.
---
--- Turns the SGR-annotated text produced by `pterm snapshot-ansi` into runs
--- (text + attributes) and resolves those attributes to Neovim highlight
--- groups that are rendered as extmarks in the preview buffer.

local M = {}

--- Attributes carried by a run. Colors are `nil` (default), `{ mode = "idx", n = <0-255> }`
--- or `{ mode = "rgb", r =, g =, b = }`.
M.default_attrs = {
	fg = nil,
	bg = nil,
	bold = false,
	dim = false,
	italic = false,
	underline = false,
	inverse = false,
}

local function clone_attrs(attrs)
	return {
		fg = attrs.fg,
		bg = attrs.bg,
		bold = attrs.bold,
		dim = attrs.dim,
		italic = attrs.italic,
		underline = attrs.underline,
		inverse = attrs.inverse,
	}
end

local function attrs_equal(a, b)
	return a.fg == b.fg
		and a.bg == b.bg
		and a.bold == b.bold
		and a.dim == b.dim
		and a.italic == b.italic
		and a.underline == b.underline
		and a.inverse == b.inverse
end

local function attrs_is_default(attrs)
	return attrs_equal(attrs, M.default_attrs)
end

--- xterm 256-color palette as hex strings, indexed 0..255.
local palette = {}
do
	local base = {
		"#000000",
		"#800000",
		"#008000",
		"#808000",
		"#000080",
		"#800080",
		"#008080",
		"#c0c0c0",
		"#808080",
		"#ff0000",
		"#00ff00",
		"#ffff00",
		"#0000ff",
		"#ff00ff",
		"#00ffff",
		"#ffffff",
	}
	for i = 0, 15 do
		palette[i] = base[i + 1]
	end

	local function cube_level(v)
		if v == 0 then
			return 0
		end
		return 55 + (v - 1) * 40
	end
	for i = 16, 231 do
		local n = i - 16
		local r = math.floor(n / 36)
		local g = math.floor((n % 36) / 6)
		local b = n % 6
		palette[i] = string.format("#%02x%02x%02x", cube_level(r), cube_level(g), cube_level(b))
	end
	for i = 232, 255 do
		local v = 8 + (i - 232) * 10
		palette[i] = string.format("#%02x%02x%02x", v, v, v)
	end
end

local function color_hex(color)
	if color == nil then
		return nil
	end
	if color.mode == "idx" then
		return palette[color.n]
	end
	if color.mode == "rgb" then
		return string.format("#%02x%02x%02x", color.r, color.g, color.b)
	end
	return nil
end

local function blend(hex1, hex2, t)
	local function channel(s)
		return tonumber(s, 16)
	end
	local r1, g1, b1 = channel(hex1:sub(2, 3)), channel(hex1:sub(4, 5)), channel(hex1:sub(6, 7))
	local r2, g2, b2 = channel(hex2:sub(2, 3)), channel(hex2:sub(4, 5)), channel(hex2:sub(6, 7))
	local function mix(a, b)
		return math.floor(a + (b - a) * t)
	end
	return string.format("#%02x%02x%02x", mix(r1, r2, t), mix(g1, g2, t), mix(b1, b2, t))
end

local function hl_color(attr)
	local value = attr
	if type(value) == "number" then
		return string.format("#%06x", value)
	end
	return nil
end

local hl_cache = {}
local hl_seq = 0

--- Resolve a run's attributes to a Neovim highlight group name.
--- Returns nil for fully-default runs (caller should skip them).
function M.hl_group(attrs)
	if attrs_is_default(attrs) then
		return nil
	end

	local key = (
		attrs.fg and attrs.fg.mode .. ":" .. (attrs.fg.n or (attrs.fg.r .. ";" .. attrs.fg.g .. ";" .. attrs.fg.b))
		or "d"
	)
		.. "|"
		.. (attrs.bg and attrs.bg.mode .. ":" .. (attrs.bg.n or (attrs.bg.r .. ";" .. attrs.bg.g .. ";" .. attrs.bg.b)) or "d")
		.. (attrs.bold and "|b" or "")
		.. (attrs.dim and "|d" or "")
		.. (attrs.italic and "|i" or "")
		.. (attrs.underline and "|u" or "")
		.. (attrs.inverse and "|r" or "")
	if hl_cache[key] then
		return hl_cache[key]
	end

	local normal = vim.api.nvim_get_hl(0, { name = "Normal" }) or {}
	local normal_fg = hl_color(normal.fg)
	local normal_bg = hl_color(normal.bg)

	local fg = color_hex(attrs.fg)
	local bg = color_hex(attrs.bg)
	if attrs.inverse then
		fg, bg = bg, fg
	end

	local spec = {}
	if fg then
		spec.fg = fg
	end
	if bg then
		spec.bg = bg
	end
	if attrs.bold then
		spec.bold = true
	end
	if attrs.italic then
		spec.italic = true
	end
	if attrs.underline then
		spec.underline = true
	end
	if attrs.inverse and not fg and not bg then
		spec.reverse = true
	end
	if attrs.dim then
		local base = fg or normal_fg
		local base_bg = bg or normal_bg
		if base and base_bg then
			spec.fg = blend(base, base_bg, 0.5)
		elseif base then
			spec.fg = blend(base, "#000000", 0.5)
		elseif normal_fg then
			spec.fg = blend(normal_fg, normal_bg or "#000000", 0.5)
		end
	end

	-- The cache key contains ':', '|' and ';', which are invalid in a highlight
	-- group name, so groups are numbered instead.
	hl_seq = hl_seq + 1
	local name = "PtermHl" .. hl_seq
	vim.api.nvim_set_hl(0, name, spec)
	hl_cache[key] = name
	return name
end

local function parse_params(seq)
	local params = {}
	for p in (seq .. ";"):gmatch("([^;]*);") do
		table.insert(params, tonumber(p) or 0)
	end
	return params
end

local function apply_sgr(attrs, seq)
	local params = parse_params(seq)
	local i = 1
	while i <= #params do
		local p = params[i]
		if p == 0 then
			attrs.fg, attrs.bg = nil, nil
			attrs.bold, attrs.dim = false, false
			attrs.italic, attrs.underline, attrs.inverse = false, false, false
		elseif p == 1 then
			attrs.bold = true
		elseif p == 2 then
			attrs.dim = true
		elseif p == 22 then
			attrs.bold, attrs.dim = false, false
		elseif p == 3 then
			attrs.italic = true
		elseif p == 23 then
			attrs.italic = false
		elseif p == 4 then
			attrs.underline = true
		elseif p == 24 then
			attrs.underline = false
		elseif p == 7 then
			attrs.inverse = true
		elseif p == 27 then
			attrs.inverse = false
		elseif p == 39 then
			attrs.fg = nil
		elseif p == 49 then
			attrs.bg = nil
		elseif p >= 30 and p <= 37 then
			attrs.fg = { mode = "idx", n = p - 30 }
		elseif p >= 40 and p <= 47 then
			attrs.bg = { mode = "idx", n = p - 40 }
		elseif p >= 90 and p <= 97 then
			attrs.fg = { mode = "idx", n = p - 90 + 8 }
		elseif p >= 100 and p <= 107 then
			attrs.bg = { mode = "idx", n = p - 100 + 8 }
		elseif p == 38 or p == 48 then
			local kind = params[i + 1]
			if kind == 5 then
				local n = params[i + 2]
				if n then
					if p == 38 then
						attrs.fg = { mode = "idx", n = n }
					else
						attrs.bg = { mode = "idx", n = n }
					end
				end
				i = i + 2
			elseif kind == 2 then
				local r, g, b = params[i + 2], params[i + 3], params[i + 4]
				if r and g and b then
					if p == 38 then
						attrs.fg = { mode = "rgb", r = r, g = g, b = b }
					else
						attrs.bg = { mode = "rgb", r = r, g = g, b = b }
					end
				end
				i = i + 4
			end
		end
		i = i + 1
	end
end

--- Parse an SGR-annotated snapshot into lines of runs.
---
--- Each line is a list of `{ text = string, attrs = <attrs> }` where consecutive
--- runs have differing attributes. Attribute state resets at each line break.
--- Returns a flat list of lines.
function M.parse(text)
	local lines = {}
	local current_line = {}
	local text_parts = {}
	local current = clone_attrs(M.default_attrs)
	local run_attrs = clone_attrs(M.default_attrs)

	local function close_run()
		local content = table.concat(text_parts)
		if content ~= "" then
			table.insert(current_line, { text = content, attrs = clone_attrs(run_attrs) })
		end
		text_parts = {}
		run_attrs = clone_attrs(current)
	end

	local i = 1
	local n = #text
	while i <= n do
		local byte = text:byte(i)
		if byte == 27 and text:sub(i + 1, i + 1) == "[" then
			local close = text:find("m", i + 2, true)
			if not close then
				text_parts[#text_parts + 1] = text:sub(i, i)
				i = i + 1
			else
				apply_sgr(current, text:sub(i + 2, close - 1))
				i = close + 1
			end
		elseif byte == 10 then
			close_run()
			table.insert(lines, current_line)
			current_line = {}
			current = clone_attrs(M.default_attrs)
			run_attrs = clone_attrs(current)
			i = i + 1
		elseif attrs_equal(current, run_attrs) then
			text_parts[#text_parts + 1] = text:sub(i, i)
			i = i + 1
		else
			close_run()
			text_parts[#text_parts + 1] = text:sub(i, i)
			i = i + 1
		end
	end
	close_run()
	table.insert(lines, current_line)

	return lines
end

--- Trim a line's runs to fit within a display width. Returns `{ text, runs }`.
function M.trim_runs(runs, width)
	if width <= 0 then
		return { text = "", runs = {} }
	end

	local plain = {}
	for _, run in ipairs(runs) do
		plain[#plain + 1] = run.text
	end
	plain = table.concat(plain)

	if vim.fn.strdisplaywidth(plain) <= width then
		return { text = plain, runs = runs }
	end

	local suffix = "..."
	if width <= #suffix then
		local text = suffix:sub(1, width)
		return { text = text, runs = { { text = text, attrs = clone_attrs(M.default_attrs) } } }
	end

	local target_width = width - #suffix
	local low, high = 0, vim.fn.strchars(plain)
	while low < high do
		local mid = math.ceil((low + high) / 2)
		if vim.fn.strdisplaywidth(vim.fn.strcharpart(plain, 0, mid)) <= target_width then
			low = mid
		else
			high = mid - 1
		end
	end

	local remaining = low
	local trimmed = {}
	for _, run in ipairs(runs) do
		if remaining <= 0 then
			break
		end
		local count = vim.fn.strchars(run.text)
		if count <= remaining then
			table.insert(trimmed, run)
			remaining = remaining - count
		else
			table.insert(trimmed, { text = vim.fn.strcharpart(run.text, 0, remaining), attrs = run.attrs })
			remaining = 0
		end
	end
	table.insert(trimmed, { text = suffix, attrs = clone_attrs(M.default_attrs) })

	local text = {}
	for _, run in ipairs(trimmed) do
		text[#text + 1] = run.text
	end
	return { text = table.concat(text), runs = trimmed }
end

--- Parse an SGR snapshot into preview-ready lines trimmed to `width`.
--- Only the last `height` non-empty lines are kept. Returns a list of
--- `{ text, runs }`.
function M.preview_lines(text, width, height)
	local parsed = M.parse(text or "")

	while #parsed > 0 and #parsed[#parsed] == 0 do
		table.remove(parsed)
	end
	if #parsed == 0 then
		return { { text = "(empty)", runs = {} } }
	end

	local first = math.max(1, #parsed - height + 1)
	local out = {}
	for i = first, #parsed do
		out[#out + 1] = M.trim_runs(parsed[i], width)
	end
	return out
end

return M
