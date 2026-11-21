local M = { root_markers = { ".git" } }
local separator = package.config:sub(1, 1)

local function list_files(root)
  local opts = {
    cwd = root,
    args = { "--type", "f", "--extension", "md", "--color", "never", "--print0" },
  }
  local program = "fd"
  local result, err = tvi.process.run(program, opts)
  if not result and err.kind == "not_found" then
    program = "fdfind"
    result, err = tvi.process.run(program, opts)
    if not result and err.kind == "not_found" then
      return nil, "not found fd."
    end
  end
  if not result then
    return nil, program .. ": " .. err.message
  end
  if not result.ok then
    local reason = result.code and ("exit code " .. result.code) or "terminated without an exit code"
    return nil, program .. ": " .. reason
  end
  local files = {}
  for path in result.stdout:gmatch("([^%z]+)%z") do
    if separator == "\\" then
      path = path:gsub("\\", "/")
    end
    -- --print0 の出力には先頭の ./ が付くことがある。
    path = path:gsub("^%./", "")
    if path ~= "" and path:sub(1, 1) ~= "/" and (separator ~= "\\" or not path:find(":"))
        and utf8.len(path) then
      local relative = true
      for part in path:gmatch("[^/]+") do
        if part == "." or part == ".." then
          relative = false
          break
        end
      end
      if relative then
        files[#files + 1] = path
      end
    end
  end
  table.sort(files)
  return files
end

local function score(path, points)
  local matched, position, first = 0, 0, 0
  for _, point in utf8.codes(path:lower()) do
    position = position + 1
    if point == points[matched + 1] then
      matched = matched + 1
      if matched == 1 then
        first = position
      end
      if matched == #points then
        return first - 1 + 2 * (position - first + 1 - matched)
      end
    end
  end
end

local function filter(files, query)
  if query == "" then
    return files
  end
  local points, matches = {}, {}
  for _, point in utf8.codes(query:lower()) do
    points[#points + 1] = point
  end
  for index, path in ipairs(files) do
    local rank = score(path, points)
    if rank then
      matches[#matches + 1] = { index = index, rank = rank }
    end
  end
  table.sort(matches, function(a, b)
    if a.rank == b.rank then
      return a.index < b.index
    end
    return a.rank < b.rank
  end)
  local selected = {}
  for index, match in ipairs(matches) do
    selected[index] = files[match.index]
  end
  return selected
end

function M.open()
  local view = tvi.ui.current_view()
  local root, err = view:project_root({ root_markers = M.root_markers })
  if not root then
    tvi.ui.notify(err)
    return
  end
  local files
  files, err = list_files(root)
  if not files then
    tvi.ui.notify(err)
    return
  end
  local query, candidates, selected = "", files, 1

  local function refilter()
    candidates = filter(files, query)
    selected = 1
  end

  local function display()
    return {
      header = "> " .. query,
      lines = candidates,
      focus_line = #candidates > 0 and selected or false,
    }
  end

  local opts = display()
  opts.title = "Files"
  opts.on_key = function(float, key)
    if key.name == "Enter" then
      local path = candidates[selected]
      if path then
        if separator ~= "/" then
          path = path:gsub("/", separator)
        end
        local join = root:sub(-1) == separator and "" or separator
        local ok, reason = view:open_file(root .. join .. path)
        if ok then
          float:close()
        else
          tvi.ui.notify(reason)
        end
      end
      return
    elseif key.name == "C-p" then
      selected = math.max(1, selected - 1)
    elseif key.name == "C-n" then
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
  return view:open_float(opts)
end

return M
