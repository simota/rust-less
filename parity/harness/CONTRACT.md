# Parity Harness Contract(capture / verify 共有 — hub 発行、変更は hub 承認必須)

同一ハーネスで `/usr/bin/less`(baseline)と `target/release/rless`(candidate)を駆動し、
canonicalize 済み画面状態を差分することでパリティを証明する。オラクルは captured(black-box)。

## 実行環境(canonicalization 契約)

- tmux 3.7b、専用ソケット(`tmux -S "$TMPDIR/rust-less-parity.sock"`)。**tmux を伴う実行
  (run.sh / diff 対象の capture 生成)はハブが行う** — サンドボックスの Unix ソケット制限のため。
  サブエージェントは成果物(fixture/シナリオ/スクリプト)の作成・解析のみを担当し tmux を直接実行しない
- ペイン固定 80x24(シナリオで明示指定がある場合のみ変更可。リサイズシナリオは resize-window を使う)
- 環境: `env -u LESS -u LESSOPEN -u LESSCLOSE LESSHISTFILE=- LANG=C.UTF-8 <binary> <args>`
- fixture は `parity/fixtures/` の合成ファイルのみ(タイムスタンプ・乱数・PII なし → マスク不要)
- 各 step 後に `wait_ms`(既定 400ms)待ってから capture(settle 待ち)

## シナリオスキーマ(parity/scenarios/*.json)

```json
{
  "id": "nav-basic",
  "feature_ids": ["NAV-J", "NAV-G"],
  "fixture": "parity/fixtures/lines200.txt",
  "args": ["-N"],
  "stdin": false,
  "steps": [
    {"keys": "jjj", "capture": "after-3j"},
    {"key": "Enter"},
    {"keys": "/line 50", "key_after": "Enter", "capture": "after-search"}
  ],
  "quit_keys": "q",
  "expect_exit": 0
}
```

- `keys` = リテラル送出(`tmux send-keys -l`)、`key` = 名前付きキー(`Enter`, `Space`, `Up`, `Down`, `PPage`, `NPage`)
- `capture` があれば その時点の画面を記録
- `stdin: true` → `cat <fixture> | <binary>` で起動

## Runner IF

- `parity/harness/run.sh <binary> <scenarios-dir|scenario.json> <outdir>`
- 出力(scenario ごと): `<outdir>/<id>/<capture-name>.txt`(`capture-pane -p`)、
  `<capture-name>.attrs.txt`(`capture-pane -p -e`)、`exit_code`、`steps.log`(interaction log)
- `parity/harness/diff.sh <baseline-dir> <candidate-dir>` → シナリオ×capture 毎の PASS/FAIL と
  `matrix.json`(feature_id → シナリオ → 結果)。差分本文は `diff -u` で保存

## Conformant adapter 4成果物の対応

state screenshots = `.txt`/`.attrs.txt` · interaction log = `steps.log` ·
state/navigation graph = シナリオ集合 + feature_ids マッピング · provenance = `parity/provenance.json`

## 比較規則

- Visual+Behavioral: capture 毎に `.txt` 完全一致が必須。`.attrs.txt` は検索ハイライト・反転
  プロンプト等「属性が仕様である」capture のみ比較対象(シナリオに `"compare_attrs": true`)
- 終了コード完全一致
- Feature: matrix.json で inventory の全 feature_id が ≥1 シナリオでカバーされること
