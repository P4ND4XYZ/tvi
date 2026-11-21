# カラースキーム

[目次に戻る](../index.md)

本文、Markdownの装飾、検索結果、UIの色や文字装飾を変更できます。

## 設定と切り替え

[Lua設定](lua.md)の `init.lua` やキー・フロートのコールバックから `tvi.colorscheme.set(styles)` を呼びます。キーは下表のグループ名、値は属性のテーブルです。

```lua
tvi.colorscheme.set({
  ["document.normal"] = { fg = "bright_white", bg = "#18212a" },
  ["markdown.heading1"] = { fg = "#80d8ff", bg = "default", underline = false },
  ["search.current"] = { fg = "black", bg = "bright_yellow" },
  ["ui.status"] = { fg = "cyan", reverse = false },
})

tvi.keymap.set("normal", "gr", function()
  tvi.colorscheme.set({})
end, { desc = "既定の配色に戻す" })
```

呼び出すたびに、組み込みの配色を出発点として全体を置き換えます。前回の指定は引き継がず、空のテーブルで既定に戻ります。未定義のグループ名・属性名や不正な値は Lua エラーになり、適用中の配色は変わりません。

開いている文書やフロートと、その後に開く文書やフロートに、次の描画から適用します。閲覧位置・検索の選択・入力中の文字は変わりません。変更は保存されません。次の起動でも適用するには `init.lua` に記述します。

## Dracula風プリセット

暗い背景に紫やピンクの見出しを使い、選択中の検索結果を黄色で強調します。

[plugins/colorscheme/dracula.lua](../../plugins/colorscheme/dracula.lua) を、`init.lua` と同じディレクトリの `plugins/colorscheme/dracula.lua` にコピーし、`init.lua` に次を追加します。

```lua
tvi.colorscheme.set(require("plugins.colorscheme.dracula"))
```

## グループ

| グループ | 対象 |
|---|---|
| `document.normal` | 本文の基準のスタイル、余白、ヘルプ |
| `markdown.heading1` ～ `markdown.heading6` | 見出しレベル1～6 |
| `markdown.link` | リンク |
| `markdown.quote` | 引用と GFM アラートのラベル。指定していない属性は元の装飾を維持 |
| `markdown.code` | インラインコードのみ |
| `search.match` | 検索の一致箇所（既定は白文字に青背景） |
| `search.current` | 選択中の一致箇所（既定は黒文字に黄背景） |
| `ui.status` | 状態欄と通知 |
| `ui.prompt` | コマンド・検索の入力欄 |
| `float.normal` | フロートの本文、ヘッダー、選択行 |
| `float.border` | フロートの枠とタイトル |

本文の基準の上に Markdown の装飾、最後に検索の強調を重ねます。一致が重なる箇所は選択中の一致が優先されます。コードブロック、画像（代替表示を含む）、Mermaid の図は配色の対象外です。ただしコードの文字にも検索の強調は適用されます。

## 属性と色

- `fg`（文字色）、`bg`（背景色）: 下記の色名、`#RRGGBB`、または `"default"`。省略した場合は元の色を使い、`"default"` は端末の既定色に戻します。
- `bold`（太字）、`italic`（斜体）、`underline`（下線）、`reverse`（反転）: `true` で付け、`false` で外します。たとえば状態欄・フロートの選択行は既定で反転し、`reverse = false` で解除できます。

ANSI 16色の名前は次のとおりです。

| 通常色 | 明色 |
|---|---|
| `black`、`red`、`green`、`yellow`、`blue`、`magenta`、`cyan`、`white` | `bright_black`、`bright_red`、`bright_green`、`bright_yellow`、`bright_blue`、`bright_magenta`、`bright_cyan`、`bright_white` |

色名の表示は端末のパレット設定によって変わります。`#RRGGBB` の表示には24ビットカラー（True Color）対応の端末が必要です。非対応の端末では指定どおりの色にならない場合があり、tvi は ANSI 16色への変換を行いません。
