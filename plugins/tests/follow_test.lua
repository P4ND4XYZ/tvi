local follow = require("plugins.follow")

local function with_follow(run)
  local env = { HERDR_WORKSPACE_ID = "work-資料" }
  local state = { path = "/docs/Current.md", calls = 0, closed = 0, opened = {}, scrolls = {} }
  local view = {}
  function view:document()
    local path, kind = state.path, state.kind or "md"
    local doc = {}
    function doc:path() return path end
    function doc:type() return kind end
    if kind == "md" then
      function doc:scroll_to_line(line)
        assert(path == state.path, "follow must use the newly displayed document")
        state.scrolls[#state.scrolls + 1] = { path = path, line = line }
      end
    end
    return doc
  end
  function view:open_file(path)
    state.opened[#state.opened + 1] = path
    if state.open_error then return false, state.open_error end
    state.path, state.kind = path, "md"
    return true
  end
  ---@diagnostic disable-next-line: missing-fields
  _G.tvi = {
    ui = {
      current_view = function() return view end,
      notify = function(message) state.notice = message end,
    },
    net = {
      listen_local = function(port, on_line)
        state.calls = state.calls + 1
        if state.bind_error then return nil, state.bind_error end
        state.port = port
        state.on_line = on_line
        return { close = function() state.closed = state.closed + 1 end }
      end,
    },
  }
  local getenv = os.getenv
  os.getenv = function(name) return env[name] end
  local ok, err = pcall(run, state, env)
  os.getenv = getenv
  follow.stop()
  assert(ok, err)
end

local function start_and_stop_are_opt_in(state, env)
  assert(state.calls == 0)
  env.HERDR_WORKSPACE_ID = nil
  assert(follow.start() == nil and state.calls == 0)
  env.HERDR_WORKSPACE_ID = "work-資料"
  assert(follow.start())
  assert(follow.start() and state.calls == 1 and state.port == 23511)
  follow.stop()
  follow.stop()
  assert(state.closed == 1)

  for _, invalid in ipairs({ "", "1.0", " 80", "-1", "0", "65536", "0x50" }) do
    env.TVI_FOLLOW_PORT = invalid
    assert(follow.start() == nil and state.calls == 1)
    assert(state.notice:find("TVI_FOLLOW_PORT", 1, true))
  end
  env.TVI_FOLLOW_PORT = "65535"
  state.bind_error = "address in use"
  local ok, err = follow.start()
  assert(ok == nil and err:find(state.bind_error, 1, true))
  state.bind_error = nil
  assert(follow.start() and state.port == 65535)
end

local function ignores_invalid_frames_and_non_markdown_switches(state)
  assert(follow.start())
  local send = state.on_line
  for _, payload in ipairs({
    "other\t12\t/docs/Other.md",
    "work-資料\t12\t/docs/Other.txt",
    "work-資料\t12\t",
    "work-資料\t0\t/docs/Current.md",
    "work-資料\t+12\t/docs/Current.md",
    "work-資料\t9223372036854775808\t/docs/Current.md",
    "work-資料\t12\t/docs/Current.md\r",
    "work-資料\t12\t/docs/Current.md\n",
    "work-資料\t12\t/docs/Current.md\0",
    "work-資料\t12\t/docs/Current.md\255",
  }) do
    assert(send(payload) == false and #state.opened == 0 and #state.scrolls == 0)
  end
end

local function switches_documents_before_scrolling(state)
  assert(follow.start())
  local send = state.on_line
  send("work-資料\t0012\t/docs/Current.md")
  assert(#state.opened == 0 and state.scrolls[1].line == 12)

  local target = "/docs/A\tB.MARKDOWN"
  send("work-資料\t14\t" .. target)
  send("work-資料\t15\t" .. target)
  assert(#state.opened == 1 and state.opened[1] == target)
  assert(#state.scrolls == 3 and state.scrolls[2].path == target and state.scrolls[2].line == 14)
  assert(state.scrolls[3].path == target and state.scrolls[3].line == 15)
end

local function does_not_scroll_when_open_file_fails(state)
  assert(follow.start())
  state.open_error = "file not found"
  state.on_line("work-資料\t14\t/docs/Missing.md")
  assert(#state.opened == 1 and state.opened[1] == "/docs/Missing.md")
  assert(state.path == "/docs/Current.md" and #state.scrolls == 0)
end

return function()
  with_follow(start_and_stop_are_opt_in)
  with_follow(ignores_invalid_frames_and_non_markdown_switches)
  with_follow(switches_documents_before_scrolling)
  with_follow(does_not_scroll_when_open_file_fails)
  with_follow(function(state)
    assert(follow.start())
    state.path, state.kind = "/docs/Current.pdf", "pdf"
    assert(state.on_line("work-資料\t12\t/docs/Current.pdf") == false)
    assert(#state.opened == 0 and #state.scrolls == 0)
    state.on_line("work-資料\t15\t/docs/Other.md")
    assert(state.scrolls[1].path == "/docs/Other.md" and state.scrolls[1].line == 15)
  end)
end
