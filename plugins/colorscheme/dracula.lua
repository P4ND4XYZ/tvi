-- Dracula のパレットを tvi の表示グループに割り当てる。
-- https://draculatheme.com/contribute
local background = "#282A36"
local foreground = "#F8F8F2"
local selection = "#44475A"
local purple = "#BD93F9"
local pink = "#FF79C6"
local cyan = "#8BE9FD"
local green = "#50FA7B"
local orange = "#FFB86C"
local yellow = "#F1FA8C"

return {
  ["document.normal"] = { fg = foreground, bg = background },
  ["markdown.heading1"] = { fg = purple, bold = true },
  ["markdown.heading2"] = { fg = pink, bold = true },
  ["markdown.heading3"] = { fg = green, bold = true },
  ["markdown.heading4"] = { fg = cyan, bold = true },
  ["markdown.heading5"] = { fg = orange, bold = true },
  ["markdown.heading6"] = { fg = yellow, bold = true },
  ["markdown.link"] = { fg = cyan, underline = true },
  ["markdown.quote"] = { fg = purple },
  ["markdown.code"] = { fg = green },
  ["search.match"] = { fg = background, bg = purple },
  ["search.current"] = { fg = background, bg = yellow, bold = true },
  ["ui.status"] = { fg = foreground, bg = selection, reverse = false },
  ["ui.prompt"] = { fg = cyan, bg = selection },
  ["float.normal"] = { fg = foreground, bg = selection },
  ["float.border"] = { fg = purple },
}
