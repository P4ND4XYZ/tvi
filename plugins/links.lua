local M = {}

local function query_points(query)
  local points = {}
  for _, point in utf8.codes(query:lower()) do
    points[#points + 1] = point
  end
  return points
end

local function score(text, points)
  local matched, position, first = 0, 0, 0
  for _, point in utf8.codes(text:lower()) do
    position = position + 1
    if point == points[matched + 1] then
      matched = matched + 1
      if matched == 1 then
        first = position
      end
      if matched == #points then
        -- 早い位置から始まり、文字の間隔が短い候補を優先する。
        return first - 1 + 2 * (position - first + 1 - matched)
      end
    end
  end
end

local function filter(links, query)
  if query == "" then
    return links
  end
  local points = query_points(query)
  local matches = {}
  for index, link in ipairs(links) do
    local label = score(link.label, points)
    local target = score(link.target, points)
    if label or target then
      local rank = label and target and math.min(label, target) or label or target
      matches[#matches + 1] = { link = link, rank = rank, index = index }
    end
  end
  table.sort(matches, function(a, b)
    if a.rank == b.rank then
      return a.index < b.index
    end
    return a.rank < b.rank
  end)
  local result = {}
  for index, match in ipairs(matches) do
    result[index] = match.link
  end
  return result
end

function M.open()
  local view = tvi.ui.current_view()
  local doc = view:document()
  if doc and doc:type() ~= "md" then
    doc = nil
  end
  ---@cast doc Tvi.MarkdownDocument?
  local links = doc and doc:links() or {}
  local query, candidates, selected = "", links, 1

  local function refilter()
    candidates = filter(links, query)
    selected = 1
  end

  local function display(problem)
    local lines = {}
    for index, link in ipairs(candidates) do
      lines[index] = link.label .. "  →  " .. link.target
    end
    if #lines == 0 then
      lines[1] = #links == 0 and "No links to open" or "No matching links"
    end
    return {
      header = (problem and problem .. "  |  " or "") .. "> " .. query,
      lines = lines,
      focus_line = #candidates > 0 and selected or false,
    }
  end

  local opts = display()
  opts.title = "Links"
  opts.on_key = function(float, key)
    if key.name == "Enter" then
      local link = candidates[selected]
      if link and doc then
        local path, err = doc:resolve_link(link.id)
        if not path then
          float:update(display(tostring(err or "Could not resolve link")))
          return
        end
        local ok
        ok, err = view:open_file(path)
        if ok then
          float:close()
        else
          float:update(display(tostring(err or "Could not open link")))
        end
      end
      return
    elseif key.name == "Up" or key.name == "C-p" then
      selected = math.max(1, selected - 1)
    elseif key.name == "Down" or key.name == "C-n" then
      selected = math.max(1, math.min(#candidates, selected + 1))
    elseif key.name == "Backspace" then
      local last = utf8.offset(query, -1)
      if not last then
        return
      end
      query = query:sub(1, last - 1)
      refilter()
    elseif key.text and key.text ~= "" then
      query = query .. key.text
      refilter()
    else
      return
    end
    float:update(display())
  end
  return assert(view:open_float(opts))
end

return M
