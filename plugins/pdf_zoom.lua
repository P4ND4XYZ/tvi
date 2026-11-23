local M = {}
local step = 1.25

local function zoom(factor)
  local doc = tvi.ui.current_view():document()
  if not doc or doc:type() ~= "pdf" then
    return
  end
  ---@cast doc Tvi.PdfDocument
  local ok, err = doc:set_zoom(doc:zoom() * factor)
  if not ok and err then
    tvi.ui.notify(err)
  end
end

function M.setup()
  tvi.keymap.set("normal", "+", function() zoom(step) end, { desc = "PDFを拡大" })
  tvi.keymap.set("normal", "-", function() zoom(1 / step) end, { desc = "PDFを縮小" })
end

return M
