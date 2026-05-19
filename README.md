# diskanalyzer

Rust製のTUIディスク使用量アナライザです。現在のディレクトリやディスク全体をスキャンして、ツリー表示・保存・読み込みができます。

## 起動

```bash
cargo run
```

既存のJSON結果を開く場合:

```bash
cargo run -- open ./diskanalyzer-result.json
```

## ビルド

```bash
cargo build --release
```

## 操作

- `s` : 現在のディレクトリをスキャン
- `a` : 現在のディスクをフルスキャン
- `p` : `./diskanalyzer-result.json` に保存
- `o` : `./diskanalyzer-result.json` を読み込み
- `q` : 終了
- `j` / `k` : 選択移動
- `Enter` / `l` : ディレクトリを開く
- `u` / `h` : 親へ戻る
- `b` / `[` : 履歴を戻る
- `f` / `]` : 履歴を進める
- `d` : 削除確認

## Release

`.github/workflows/release.yml` は手動実行専用です。

1. GitHub の Actions タブから `Release` を選ぶ
2. `tag_name` にリリースしたいタグを入力する
3. 実行すると Linux / Windows / macOS 向けのバイナリを1つずつ作成して Release に添付する

Rust のセットアップはキャッシュ有効で行われます。
