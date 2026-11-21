# Lua設定とプラグイン

[目次に戻る](../index.md)

利用者がLuaでプラグインを読み込み、キーを割り当てるための機能です。

## 起動時の設定

| OS | 設定ファイル |
|---|---|
| Windows | `%LOCALAPPDATA%\tvi\init.lua` |
| Linux | `$XDG_CONFIG_HOME/tvi/init.lua`。未設定・空・相対パスの場合は `~/.config/tvi/init.lua` |

起動時に該当する `init.lua` を読み込みます。`--config` は設けません。設定ファイルがなければ通常どおり起動し、エラーがあればファイル名と理由を通知して、既定の操作で閲覧を続けられるようにします。

[plugins/links.lua](../../plugins/links.lua) を、`init.lua` と同じディレクトリの `plugins/links.lua` にコピーします。`require` は設定ディレクトリを基準に解決します。プラグインの読み込みだけではキーを登録しません。

```lua
local links = require("plugins.links")
tvi.keymap.set("normal", "gl", links.open, { desc = "リンク一覧を開く" })
```

旧版の `plugins/links.lua` をすでに設定ディレクトリにコピーしている場合は、更新時に[新しいファイル](../../plugins/links.lua)へ差し替えてください。旧APIの `view:open_link(id)` は使えません。自作Luaでは `view:resolve_link(id)` で絶対パスを取得し、成功した場合に `view:open_file(path)` へ渡してください。

[ファイルピッカー](file-picker.md)は同梱されており、`require("plugins.files")` にコピーは不要です。NORMALの `ff` で起動し、`tvi.keymap.set` で同じキー列に別の操作を登録すると上書きできます。

## 公開API

| API | 動作 |
|---|---|
| `tvi.keymap.set(mode, keys, callback, opts)` | モード別にキーを登録する。`opts.desc` は操作の説明 |
| `tvi.colorscheme.set(styles)` | [カラースキーム](colorscheme.md)全体を置き換える |
| `tvi.ui.current_view()` | フロートではなく現在の本文ビューを取得する |
| `tvi.ui.notify(message)` | 状態欄の通知だけを更新する |
| `view:project_root(opts?)` | 文書のディレクトリから上へ、`opts.root_markers` のいずれかを含む最も近いディレクトリを探す。未指定のマーカーは `{ ".git" }`。該当なしなら文書のディレクトリ。成功時は絶対パス、失敗時は `nil, err` |
| `tvi.process.run(program, opts)` | 指定のプログラムをシェルを介さず同期実行する。`opts.args` は文字列の配列、`opts.cwd` は作業ディレクトリ（いずれも省略可） |
| `view:links()` | 表示文書の対象リンクを出現順に返す。各候補は `id`、`label`、`target` を持つ |
| `view:resolve_link(id)` | リンク先の絶対パスを `path, nil`、失敗時は `nil, err` として返す。リンク先の存在は確認しない |
| `view:open_file(path)` | 絶対パスを指定し、UTF-8の通常ファイルをそのビューでMarkdownとして開く。成功時は `true, nil`、失敗時は `false, err` を返す |
| `view:open_float(opts)` | ビューに紐づくフロートを開き、ハンドルを返す |
| `float:update(opts)` / `float:close()` | そのフロートの表示を更新・終了する |

キーマップの `mode` は `normal`・`command`・`search` です。
同じモードで、上書き可能な同梱プラグインを除き、既定操作や登録済みの操作と同じキー列は登録できません。`gg` と `gl` のように先頭キーを共有することはできます。
キー列が操作に一致しても、より長いキー列の先頭なら、次のキーを最大300ミリ秒待ちます。長いキー列が成立しなければ、すでに一致した操作を実行します。
COMMANDとSEARCHでは文字入力・編集・確定・キャンセル用のキーを変更せず、未使用のショートカットを登録できます。
キー列には `gl` のように文字を並べます。修飾キーや特殊キーは `<C-x>`（Ctrl-x）、`<F2>` のように書きます。`<` 自体を割り当てる場合は `<lt>` と書きます。たとえば `tvi.keymap.set("search", "<C-x>", callback)` はSEARCHのショートカットを登録します。

取得したビューハンドルはフォーカスが移っても対象が変わりません。`resolve_link` は、ビューが閉じている場合や、リンクIDが現在の文書に属さない場合は失敗します。解決した絶対パスは、別の文書に移動した後も `open_file` に渡せます。`open_file` は失敗しても文書や閲覧履歴を変更しません。

### 外部コマンドの結果

`tvi.process.run` は起動したコマンドの終了を待ち、`{ stdout, ok, code }` を返します。`stdout` はNULを含む出力をそのまま保持するLua文字列です。`ok` は終了成功時だけ `true`、`code` は終了コードがない場合は `nil` です。実行できなかった場合は `nil, err` を返し、実行ファイルが見つからなければ `err.kind == "not_found"` です。`err.message` に理由が入ります。標準エラーは取り込みません。Windowsでは `.exe` / `.com` を指定でき、拡張子を省略すると `.exe` を使います。ファイル名だけの指定は `PATH` の絶対ディレクトリから探し、相対項目は無視します。相対パスを明示した場合は、tvi自身の作業ディレクトリを基準に解決します。`opts.cwd` は子プロセスの作業ディレクトリだけを指定し、実行ファイルの検索には使いません。

### フロートの表示項目

| 項目 | 内容 |
|---|---|
| `title` | 枠に表示するタイトル |
| `header` | 本文をスクロールしても残る任意の1行 |
| `lines` | 本文に表示する文字列の配列 |
| `focus_line` | `lines` 内の1始まりの強調行。見える位置までスクロールし、`false` で強調を解除する |
| `on_key(float, key)` | 作成時に指定する入力コールバック。`key.name` はキー名、`key.text` は入力文字 |

`float:update` は指定された表示項目だけを更新し、`lines` を渡した場合は本文全体を置き換えます。位置とサイズは自動です。
初版はフロート1枚までとし、表示中に2枚目を開く操作は拒否します。閉じたハンドルで別のフロートは操作できません。
入力やエラー時の動作は[動作モードと入力](modes.md)を参照してください。
