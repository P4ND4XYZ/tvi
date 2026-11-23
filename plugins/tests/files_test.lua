local function setup(responses)
  local calls, notices, opened = {}, {}, {}
  local separator = package.config:sub(1, 1)
  local root = separator == "\\" and "C:\\資料 project" or "/資料 project"
  local model = { calls = calls, notices = notices, opened = opened, root = root }
  local view, doc = {}, {}
  function view:document() return doc end
  function doc:project_root(opts)
    model.markers = opts.root_markers
    return root
  end
  function view:open_float(opts)
    model.display, model.on_key = opts, opts.on_key
    local float = {}
    function float:update(updated)
      model.display = updated
    end
    function float:close()
      model.display = nil
    end
    model.float = float
    return float
  end
  function view:open_file(path)
    opened[#opened + 1] = path
    return true, nil, model.pdf_pending
  end
  model.press = function(name, input)
    model.on_key(model.float, { name = name, text = input })
  end
  -- テストで使うAPIだけを差し替える。
  ---@diagnostic disable-next-line: missing-fields
  _G.tvi = {
    ui = {
      current_view = function() return view end,
      supports_pdf = function() return model.pdf_supported == true end,
      notify = function(message) notices[#notices + 1] = message end,
    },
    process = {
      run = function(program, opts)
        assert(not model.display, "enumeration must finish before opening the float")
        assert(opts.cwd == root)
        calls[#calls + 1] = { program = program, args = opts.args }
        local response = assert(responses[#calls], "unexpected process invocation")
        return response.result, response.err
      end,
    },
  }
  return model
end

return function()
  local files = require("plugins.files")
  local model = setup({ { result = {
    stdout = "./z.md\0./notes/資料 a.md\0./a.md\0",
    ok = true,
    code = 0,
  } } })
  assert(files.open())
  assert(model.markers[1] == ".git")
  assert(model.calls[1].program == "fd" and #model.calls == 1)
  assert(table.concat(model.display.lines, "|") == "a.md|notes/資料 a.md|z.md")
  model.press("C-p")
  assert(model.display.focus_line == 1)
  model.press("C-n")
  assert(model.display.focus_line == 2)
  model.press("C-n")
  model.press("C-n")
  assert(model.display.focus_line == 3)
  model.press("C-p")
  assert(model.display.focus_line == 2)
  model.press("Char", "n資a")
  assert(#model.display.lines == 1 and model.display.lines[1] == "notes/資料 a.md")
  model.press("Char", "!")
  assert(#model.display.lines == 0 and model.display.focus_line == false)
  model.press("Enter")
  assert(#model.opened == 0 and #model.notices == 0)
  model.press("Backspace")
  model.press("Enter")
  local sep = package.config:sub(1, 1)
  assert(model.opened[1] == model.root .. sep .. "notes" .. sep .. "資料 a.md")
  assert(model.display == nil)

  assert(table.concat(model.calls[1].args, " "):find("--extension md", 1, true))
  assert(not table.concat(model.calls[1].args, " "):find("--extension pdf", 1, true))
  model = setup({ { result = { ok = true, stdout = "./manual.PDF\0" } } })
  model.pdf_supported, model.pdf_pending = true, true
  assert(files.open())
  assert(table.concat(model.calls[1].args, " "):find("--extension pdf", 1, true))
  model.press("Enter")
  assert(model.opened[1] == model.root .. sep .. "manual.PDF")
  assert(model.display.focus_line == 1 and #model.notices == 0)

  local missing = { err = { kind = "not_found", message = "missing" } }
  model = setup({ missing, { result = { ok = true, code = 0, stdout = "" } } })
  local defaults = files.root_markers
  files.root_markers = { ".workspace" }
  assert(files.open())
  files.root_markers = defaults
  assert(model.markers[1] == ".workspace")
  assert(model.calls[1].program == "fd" and model.calls[2].program == "fdfind")
  assert(#model.notices == 0 and #model.display.lines == 0 and model.display.focus_line == false)

  model = setup({ missing, missing })
  files.open()
  assert(#model.calls == 2 and model.display == nil)
  assert(#model.notices == 1 and model.notices[1] == "not found fd.")

  model = setup({ { result = { ok = false, code = 7, stdout = "ignored\0" } } })
  files.open()
  assert(#model.calls == 1 and model.display == nil)
  assert(#model.notices == 1 and model.notices[1] == "fd: exit code 7")

  model = setup({ { err = { kind = "spawn", message = "permission denied" } } })
  files.open()
  assert(#model.calls == 1 and model.display == nil)
  assert(#model.notices == 1 and model.notices[1] == "fd: permission denied")
end
