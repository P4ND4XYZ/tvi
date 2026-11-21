return function()
  -- tvi のテストでは初回の読み込みから実際の API で検証する。
  if tvi then
    tvi.colorscheme.set(require("plugins.colorscheme.dracula"))
  end

  local styles = require("plugins.colorscheme.dracula")
  local document = styles["document.normal"]
  assert(document.fg == "#F8F8F2" and document.bg == "#282A36", "expected the Dracula base colors")
  assert(styles["search.match"].bg == "#BD93F9", "expected a purple search match")
  assert(styles["search.current"].bg == "#F1FA8C", "expected a yellow current match")
end
