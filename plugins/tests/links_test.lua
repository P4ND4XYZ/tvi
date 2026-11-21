local links = require("plugins.links")

local function open_picker(candidates)
  local view, float = {}, {}
  local picker = { current_view = view }
  local on_key

  function view:links()
    return candidates
  end

  function view:resolve_link(id)
    if picker.resolve_error then
      return nil, picker.resolve_error
    end
    return "/docs/" .. id .. ".md"
  end

  function view:open_file(path)
    if picker.open_error then
      return false, picker.open_error
    end
    picker.opened_file = path
    return true
  end

  function view:open_float(opts)
    on_key = opts.on_key
    float:update(opts)
    return float
  end

  function float:update(opts)
    picker.header = opts.header
  end

  function float:close()
    picker.closed = true
  end

  function picker:press(name, text)
    on_key(float, { name = name, text = text })
  end

  _G.tvi = { ui = { current_view = function() return picker.current_view end } }
  links.open()
  return picker
end

local function opens_the_filtered_link_in_the_original_view()
  local query = "猫" .. string.rep("a", 74)
  local picker = open_picker({
    { id = 8, label = "Dog", target = "dog.md" },
    { id = 7, label = query, target = "cat.md" },
  })
  picker.current_view = {}
  for _, point in utf8.codes(query) do
    local ch = utf8.char(point)
    picker:press(ch, ch)
  end
  assert(picker.header == "> " .. query, "expected the complete query in the header")

  picker.open_error = "missing target"
  picker:press("Enter")
  assert(picker.header == "missing target  |  > " .. query, "expected the error in the header")
  assert(not picker.closed, "failed to open a link, but the picker closed")

  picker.open_error = nil
  picker:press("Enter")
  assert(picker.opened_file == "/docs/7.md", "expected the matching link to open in the original view")
  assert(picker.closed, "expected the picker to close after opening the link")
end

local function leaves_the_picker_open_if_a_link_cannot_be_resolved()
  local picker = open_picker({ { id = 7, label = "Cat", target = "cat.md" } })
  picker.resolve_error = "stale link"
  picker:press("Enter")
  assert(picker.header == "stale link  |  > ", "expected the resolution error in the header")
  assert(not picker.closed and not picker.opened_file, "an unresolved link must not open a file")
end

local function moves_selection_with_control_n_and_control_p()
  local candidates = {
    { id = 7, label = "Cat", target = "cat.md" },
    { id = 8, label = "Dog", target = "dog.md" },
  }
  local picker = open_picker(candidates)
  picker:press("C-n")
  picker:press("Enter")
  assert(picker.opened_file == "/docs/8.md", "Ctrl+n should select the next link")

  picker = open_picker(candidates)
  picker:press("C-n")
  picker:press("C-p")
  picker:press("Enter")
  assert(picker.opened_file == "/docs/7.md", "Ctrl+p should select the previous link")
end

return function()
  opens_the_filtered_link_in_the_original_view()
  leaves_the_picker_open_if_a_link_cannot_be_resolved()
  moves_selection_with_control_n_and_control_p()
end
