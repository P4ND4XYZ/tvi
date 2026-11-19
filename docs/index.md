# tvi 仕様書

## 目的

tviは、端末上でMarkdownファイルを読むための閲覧専用ビューアです。

## 目次

### 機能仕様

| 文書 | 説明 |
|---|---|
| [Markdown表示](features/markdown.md) | UTF-8ファイルの読み込み、折り返し、ローカル画像の表示、起動時のエラー |
| [文書内の移動](features/navigation.md) | 行・ページ単位の移動とリサイズ時の閲覧位置の保持 |
| [動作モードと入力](features/modes.md) | NORMAL、COMMAND、SEARCHの役割と切り替え |
| [検索](features/search.md) | 検索条件、一致箇所への移動、ハイライトの解除と再表示 |
| [コマンドと終了](features/commands.md) | 終了、ヘルプ、検索ハイライトの解除、コマンドエラー |
| [状態表示とヘルプ](features/status-help.md) | モードや閲覧位置の表示とキー一覧 |
| [リンク先への移動](features/links.md) | ローカルリンクの一覧、文書間の移動と復帰 |
| [Lua設定とプラグイン](features/lua.md) | 起動時の設定、キーマップ、公開API |

### 設計方針

- [モジュール構成と依存関係](design/architecture.md): 各モジュールの責務とMarkdown adapterの境界
- [Lua連携の設計](design/lua-integration.md): RustとLuaの責務、入力と状態の管理
