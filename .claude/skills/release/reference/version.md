# 前提の確認と version の確定の詳細

[SKILL.md](../SKILL.md) の 1 と 2 のコマンド・読み方・登録の例・semver の目安。

## 1. 前提の確認

```sh
git fetch origin
git switch main && git pull --ff-only
git status --porcelain            # 何も出ないこと（clean）
git rev-parse HEAD origin/main    # 2 行が同じこと
gh run list --branch main --workflow ci.yml --limit 5   # 最新の main の run が success
cargo publish --dry-run --locked -p dagq  # dagq の package の作成と build が通ること
```

- `gh run list` の先頭が main の HEAD の commit で `completed success` でなければリリースしない（`gh run view <run-id> --log-failed` で原因を見る）
- `cargo publish --dry-run --locked -p dagq` は PATH の `dagq`（`~/.local/bin/dagq`）とは無関係に、package の中身（`build.rs`・`src/`・`migrations/`・`Cargo.toml`・`Cargo.lock`・`README.md`・`LICENSE`）だけで build できるかを見る。`cargo package --list --locked` で中身を確かめられる

## 2. `-dev` を外して version を確定する

worker が task の worktree で行う:

```sh
# version を書き換えた後
cargo update --workspace   # Cargo.lock の dagq の version を更新
# 確認
grep -n '^version' Cargo.toml
grep -n '"version"' plugins/claude-dagq/.claude-plugin/plugin.json
git diff --stat                                                  # Cargo.toml / Cargo.lock / plugin.json だけ
```

この変更を登録する例（planner の session から、固定バイナリ `~/.local/bin/dagq` で）:

```sh
dagq add 'release: version を X.Y.Z に上げる' \
  --description 'Cargo.toml と plugins/claude-dagq/.claude-plugin/plugin.json の version を X.Y.Z にし、Cargo.lock を更新する' \
  --acceptance 'Cargo.toml・plugin.json・Cargo.lock の dagq の version が X.Y.Z で、cargo publish --dry-run --locked が通る' \
  --paths Cargo.toml --paths Cargo.lock --paths 'plugins/claude-dagq/.claude-plugin/plugin.json' \
  --verify 'cargo publish --dry-run --locked' --verify 'cargo test --locked --test plugin'
```

登録の書式は dagq skill に従う。着地後に 1 の前提の確認をやり直す。

semver の目安（1.0 未満なので minor が互換を切る単位）:

- migration で queue の schema が変わる、CLI のコマンド・flag・出力の非互換、receipt や `dagq.toml` の書式の非互換 → minor 以上（`0.3.x` → `0.4.0`）
- 互換を保つ修正・機能追加だけ → patch（`0.3.0` → `0.3.1`）
- 迷ったら `git log vPREV..origin/main -- migrations/` で migration の有無を見る。`vPREV` は前のリリースの tag（`git tag --list 'v*'`）。0.3.0 は手で publish したので `v0.3.0` の tag は無い。0.3.0 からの差分を見るときは `v0.2.0` か、version を 0.3.0 にした commit（`git log -S 'version = "0.3.0"' --oneline -- Cargo.toml`）を起点にする
