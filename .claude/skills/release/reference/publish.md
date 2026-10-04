# 公開の確認と失敗したときの見どころ

[SKILL.md](../SKILL.md) の 4 と 5 のコマンド・読み方と、失敗したときの見どころ。

## 4. 確認

```sh
gh run list --workflow release.yml --limit 3     # tag vX.Y.Z の run を探す
gh run watch <run-id> --exit-status              # 完了まで待つ。失敗なら非 0
gh run view <run-id>                             # すべての step が成功していること
gh release view vX.Y.Z --json assets --jq '.assets[].name'
# dagq-vX.Y.Z-aarch64-apple-darwin.tar.gz、dagq-broker-client-vX.Y.Z-aarch64-apple-darwin.tar.gz と SHA256SUMS の 3 つが出ること
curl -sS -A 'dagq-release (https://github.com/hisamekms/dagq)' \
  https://crates.io/api/v1/crates/dagq | jq -r '.crate.max_version'
# X.Y.Z が出ること
```

crates.io の API は User-Agent の無い request を拒むので `-A` を付ける。`dagq-broker-protocol`・`dagq-broker-client`・`dagq-broker` も同じ URL の形で `max_version` を見る。

**broker の crate の最初の publish**: Trusted Publisher は crates.io に既にある crate にしか登録できないので、`dagq-broker-protocol`・`dagq-broker-client`・`dagq-broker` を含む最初のリリースでは、tag の push の前に人が API token でこの 3 つを手で publish し（`cargo publish --locked -p dagq-broker-protocol` を先に）、それぞれに `dagq` と同じ Trusted Publisher を登録する。そうしないと `Publish to crates.io` が `dagq-broker-protocol` で落ちる（GitHub Release は出る。登録してから `gh run rerun <run-id> --failed`）。

## 5. 初回の自動 publish の確認

0.3.0 は手で publish し、Trusted Publisher（owner `hisamekms`、repository `dagq`、workflow `release.yml`、environment なし）は登録済み。tag による自動 publish はまだ一度も走っていないので、最初のリリースでだけ次を確かめる。

```sh
gh run view <run-id> --log | grep -E 'Authenticate to crates.io|Publish to crates.io|already on crates.io'
```

1. `Authenticate to crates.io` の step（`rust-lang/crates-io-auth-action@v1`）が成功している（token は log で mask されるので、見えるのは step の成功だけ。失敗なら OIDC の交換のエラーが出る）
2. `Publish to crates.io` の step で `cargo publish --locked` が成功し、`Uploaded dagq vX.Y.Z` / `Published dagq vX.Y.Z` が出ている。4 の `max_version` も `X.Y.Z`
3. workflow を rerun すると publish が skip される:

   ```sh
   gh run rerun <run-id>
   gh run watch <run-id> --exit-status
   gh run view <run-id> --log | grep 'already on crates.io'
   # "dagq X.Y.Z is already on crates.io; skipping publish" が出て、
   # Authenticate / Publish の step が skipped、run は success
   ```

確認できたら、dagq の planner にその旨（run の URL、`max_version`、rerun で skip されたこと）を伝え、goal 18 を閉じてもらう（goal を閉じるのは planner）。

## 失敗したときの見どころ

- **tag と version の不一致**: `Check that the tag matches the crate version` が `tag vX.Y.Z declares version ... but Cargo.toml has ...` で落ちる。build も publish もされない。tag を消して（`git push origin :refs/tags/vX.Y.Z` と `git tag -d vX.Y.Z`、ユーザーの確認を取ってから）version を上げた commit に打ち直す
- **Trusted Publisher の不一致**: `Authenticate to crates.io` が失敗する。crates.io の `dagq` の Settings → Trusted Publishing の owner `hisamekms` / repository `dagq` / workflow filename `release.yml` / environment（空）が、実際の repository と workflow のファイル名に一致しているかを見る。登録を直したら `gh run rerun <run-id> --failed`（GitHub Release は `--clobber` で上書きされる）
- **id-token 権限**: OIDC token が取れないエラー（`ACTIONS_ID_TOKEN_REQUEST_URL` が無い等）は、`release.yml` の `permissions:` に `id-token: write` が無いか、fork からの run。workflow の変更は dagq の task にする
- **crates.io の API の失敗**: `crates.io answered HTTP <status>` で止まったら、時間を置いて rerun する
- GitHub Release が出た後に crates.io だけ失敗しても、原因を直して rerun すれば Release は上書き、crates.io は publish される（ADR-0030 の Consequences）
