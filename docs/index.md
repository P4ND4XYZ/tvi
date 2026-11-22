# tvi 仕様書

## 目的

tviは端末上で文書を読むための閲覧専用ビューアです。Markdownと、WindowsのKitty画像表示対応端末ではPDFを扱います。対応形式のファイルを `tvi <file>` で開き、別のファイルは `ff` で選びます。文書の編集は対象外です。ここではtvi本体の機能と公開APIを扱います。

## 目次

### 機能仕様

| 文書 | 説明 |
|---|---|
| [Markdown表示](features/markdown.md) | UTF-8ファイルの読み込み・自動再読み込み、折り返し、ローカル画像・Mermaidの図の表示、起動時のエラー |
| [PDF表示](features/pdf.md) | 対応端末、開き方、ページ内のスクロールとページ送り |
| [文書内の移動](features/navigation.md) | 行・ページ単位の移動とリサイズ時の閲覧位置の保持 |
| [動作モードと入力](features/modes.md) | NORMAL、COMMAND、SEARCHの役割と切り替え |
| [検索](features/search.md) | 検索条件、一致箇所への移動、ハイライトの解除と再表示 |
| [コマンドと終了](features/commands.md) | コマンドによる終了、ヘルプ、検索ハイライトの解除、コマンドエラー |
| [状態表示とヘルプ](features/status-help.md) | モードや閲覧位置の表示とキー一覧 |
| [診断ログ](features/diagnostic-log.md) | 保存先、ローテーションと保持上限、記録するイベント、デバッグログの有効化 |
| [文書間の移動と復帰](features/links.md) | 対象リンク、文書間の移動と閲覧履歴 |
| [Lua設定と公開API](features/lua.md) | 起動時の設定、本文の操作やローカル受信などの公開API |
| [カラースキーム](features/colorscheme.md) | Luaからの配色変更、指定できる色と装飾 |

### 設計方針

- [モジュール構成と依存関係](design/architecture.md): 各モジュールの責務とMarkdown adapterの境界
- [Lua連携の設計](design/lua-integration.md): RustとLuaの責務、入力と状態の管理
- [Mermaid表示の設計](design/mermaid.md): 変換プロセスと既存の画像表示の境界、PNGキャッシュ、検証方針
