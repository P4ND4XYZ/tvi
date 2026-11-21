local suites = {
  "plugins.tests.links_test",
  "plugins.tests.files_test",
  "plugins.tests.dracula_test",
}

for _, name in ipairs(suites) do
  local run = require(name)
  local previous_tvi = _G.tvi
  local ok, err = pcall(run)
  _G.tvi = previous_tvi
  if not ok then
    error(name .. ": " .. tostring(err), 0)
  end
end
