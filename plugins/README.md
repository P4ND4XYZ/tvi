# Luaプラグインと配色プリセット

tviの[Lua設定](../docs/features/lua.md#起動時の設定)から読み込む追加機能です。

| 機能 | 説明 |
|---|---|
| [ファイル選択](#ファイル選択) | プロジェクト内の閲覧可能なファイルを探し、同じビューで開きます。 |
| [リンク一覧](#リンク一覧) | 現在の文書にあるローカルMarkdownリンクを一覧から選び、別の文書へ移動します。 |
| [PDFの拡大・縮小](#pdfの拡大縮小) | `+` と `-` でPDFの表示倍率を変更します。 |
| [カーソル追従](cursor-follow.md) | 同じHerdrワークスペースのNeovimが開いているMarkdownファイルとカーソル行に追従します。 |
| [Dracula風プリセット](#dracula風プリセット) | 暗い背景を使った配色に切り替えます。 |

## ファイル選択

同梱の [files.lua](files.lua) はNORMALモードの `ff` で開きます。`fd` または `fdfind` が必要です。Markdownと、[表示に対応した環境](../docs/features/pdf.md)ではPDFも候補になります。文字入力で候補を絞り、`Ctrl-n` / `Ctrl-p` で選び、`Enter` で開きます。開けなければ候補を残し、`Esc` で閉じます。

## PDFの拡大・縮小

[pdf_zoom.lua](pdf_zoom.lua) を `init.lua` と同じディレクトリの `plugins/pdf_zoom.lua` にコピーし、`init.lua` に次を追加します。

```lua
require("plugins.pdf_zoom").setup()
```

NORMALモードで `+` を押すと表示中の倍率を1.25倍に、`-` を押すと0.8倍にします。PDF以外では何もしません。倍率は描画後に反映されるため、連打した回数分だけ変わるとは限りません。

## リンク一覧

[links.lua](links.lua) を、`init.lua` と同じディレクトリの `plugins/links.lua` にコピーし、起動キーを割り当てます。読み込みだけではキー登録されません。

```lua
local links = require("plugins.links")
tvi.keymap.set("normal", "gl", links.open, { desc = "リンク一覧を開く" })
```

旧版の `plugins/links.lua` を設定ディレクトリにコピーしている場合は、[新しいファイル](links.lua)に差し替えてください。自作Luaでは `view:document()` で [Markdownの文書ハンドル](../docs/features/lua.md#文書ハンドル)を取得し、そのハンドルでリンクを列挙・解決してから、得られた絶対パスを `view:open_file(path)` に渡してください。

### 操作

| キー | 動作 |
|---|---|
| `gl`（設定例） | NORMALモードでリンク一覧を開く |
| 文字入力 / `Backspace` | リンク名とパスをファジー検索で絞り込む。検索語が空なら文書の出現順に表示 |
| `↑` / `↓` | 候補を選ぶ |
| `Enter` | 選んだリンク先の先頭を同じ本文ビューで開き、一覧を閉じる |
| `Esc` | 一覧だけを閉じ、元の閲覧位置を保つ |

一覧の各行にはリンク名とパスを表示します。対象リンクや検索に一致する候補がない場合はその旨を表示し、`Enter` では何も開きません。リンク先を開けない場合は理由を一覧に表示し、元の文書・履歴・候補の選択を保ちます。

対象リンクと履歴の動作は[文書間の移動と復帰](../docs/features/links.md)を参照してください。

## Dracula風プリセット

暗い背景に紫やピンクの見出しを使い、選択中の検索結果を黄色で強調します。

[colorscheme/dracula.lua](colorscheme/dracula.lua) を、`init.lua` と同じディレクトリの `plugins/colorscheme/dracula.lua` にコピーし、`init.lua` に次を追加します。

```lua
tvi.colorscheme.set(require("plugins.colorscheme.dracula"))
```
