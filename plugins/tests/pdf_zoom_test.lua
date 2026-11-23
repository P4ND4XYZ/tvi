return function()
  local keys, doc, notice = {}, nil, nil
  local view = { document = function() return doc end }
  _G.tvi = {
    ui = {
      current_view = function() return view end,
      notify = function(message) notice = message end,
    },
    keymap = { set = function(mode, key, callback)
      assert(mode == "normal")
      keys[key] = callback
    end },
  }
  require("plugins.pdf_zoom").setup()

  local scale = 0.5
  doc = {
    type = function() return "pdf" end,
    zoom = function() return scale end,
    set_zoom = function(_, value) scale = value; return true end,
  }
  keys["+"]()
  assert(scale == 0.625)
  keys["-"]()
  assert(scale == 0.5)

  doc.set_zoom = function() return false, "display limit" end
  keys["+"]()
  assert(notice == "display limit")

  doc = { type = function() return "md" end }
  keys["+"]()
  doc = nil
  keys["-"]()
end
