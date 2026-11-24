local M = {}
local listener

local function fail(message)
  tvi.ui.notify(message)
  return nil, message
end

local function follow_port(tab_id)
  local override = os.getenv("TVI_FOLLOW_PORT")
  if override ~= nil then
    local port = override:match("^[0-9]+$") and math.tointeger(tonumber(override))
    if port and port >= 1 and port <= 65535 then
      return port
    end
    return nil
  end
  local hash = 5381
  for i = 1, #tab_id do
    hash = (hash * 33 + tab_id:byte(i)) % 4294967296
  end
  return 20000 + hash % 40001
end

function M.start()
  if listener then
    return true
  end
  local tab_id = os.getenv("HERDR_TAB_ID")
  if not tab_id or tab_id == "" or tab_id:find("[\t\r\n%z]") or not utf8.len(tab_id) then
    return fail("follow: HERDR_TAB_ID is missing or invalid")
  end
  local port = follow_port(tab_id)
  if not port then
    return fail("follow: TVI_FOLLOW_PORT must be a decimal integer from 1 to 65535")
  end
  local handle, err = tvi.net.listen_local(port, function(payload)
    if #payload > 16384 or payload:find("[\r\n%z]") or not utf8.len(payload) then
      return false
    end
    local id, line_text, path = payload:match("^([^\t]+)\t([0-9]+)\t(.+)$")
    if id ~= tab_id then
      return false
    end
    local line = math.tointeger(tonumber(line_text))
    if not line or line < 1 then
      return false
    end
    local view = tvi.ui.current_view()
    local doc = view:document()
    if not doc or path ~= doc:path() then
      local extension = path:lower():match("%.([a-z]+)$")
      if extension ~= "md" and extension ~= "markdown" then
        return false
      end
      if not view:open_file(path) then
        return false
      end
      doc = view:document()
    end
    if not doc or doc:type() ~= "md" then
      return false
    end
    ---@cast doc Tvi.MarkdownDocument
    doc:scroll_to_line(line)
  end)
  if not handle then
    return fail("follow: " .. err)
  end
  listener = handle
  return true
end

function M.stop()
  if listener then
    listener:close()
    listener = nil
  end
end

return M
