# ModelKeep 開発計画書

この文書は設計意図、MVP要件、将来候補を定義する。作業状態の正本は
[`docs/issues/`](issues/README.md)、永続的な設計判断の正本は
[`docs/adr/`](adr/README.md)とする。

## 0. 要件トレーサビリティ

- 未完了項目はrepository-local Issueへリンクする。
- 完了項目はリンク付きテキストを取り消し線にする。完了Issueのファイルは
  trackerの規約どおり削除されるため、リンク先は完了を記録したcommitとする。
- 説明、原則、非ゴールはIssueではなく、該当ADRまたは本書自身が正本となる。
- 1つのIssueが複数節を満たしてよいが、未検証の実機条件を実装済み機能から
  推論して完了扱いにはしない。

### 節と追跡先

| 節 | 追跡先 |
|---|---|
| 1–6, 27: 目的、非ゴール、責務分離 | 本書および[ADR一覧](adr/README.md) |
| 7: archive形式 | ~~[Issue 0001: incomplete revisionを公開しない](https://github.com/kaznak/modelkeep/commit/70d2c16a156185293ba297281a15b29499b8044d)~~、~~[Issue 0014: resolved commitへ固定](https://github.com/kaznak/modelkeep/commit/0b2c4c5b0966136df2a9eb95189a8feb61f6dc66)~~、~~[Issue 0020: archive audit](https://github.com/kaznak/modelkeep/commit/ef2da9c8a9471bc4ca3b900b6a15472759195480)~~ |
| 8–9: HTTP/Xet compatibility | ~~[Issue 0002: large response streaming](https://github.com/kaznak/modelkeep/commit/3d0cec1c915c9272d28171b3727082a8352c5b2a)~~、~~[Issue 0015: supported HF client suite](https://github.com/kaznak/modelkeep/commit/1b024cc23d148df3640c0290f99f0d25fd5eb4ea)~~、[Issue 0022](issues/0022-complete-protocol-observation-matrix.md) |
| 10: upstream fetcher | ~~[Issue 0014](https://github.com/kaznak/modelkeep/commit/0b2c4c5b0966136df2a9eb95189a8feb61f6dc66)~~、~~[Issue 0017: failure semantics](https://github.com/kaznak/modelkeep/commit/561d511b75455fb49697d99221e74cd73192ffb9)~~ |
| 11: atomicity/concurrency/crash | ~~[Issues 0006–0009: staging/recovery/publication fixes](https://github.com/kaznak/modelkeep/commit/2b8de5e1bdcb191958c2546160ac3949756e30ff)~~、~~[Issue 0018: cross-alias convergence](https://github.com/kaznak/modelkeep/commit/81d6974c825d9c251aa753a433f15d5f5ecc4f0a)~~、[Issue 0024](issues/0024-add-black-box-crash-and-upgrade-tests.md) |
| 12: integrity | ~~[Issue 0020: full archive audit](https://github.com/kaznak/modelkeep/commit/ef2da9c8a9471bc4ca3b900b6a15472759195480)~~ |
| 13: authentication/gated models | [Issue 0028](issues/0028-define-private-gated-credential-policy.md)、[Issue 0021](issues/0021-add-tailnet-identity-aware-authorization.md) |
| 14: Rust server/security | ~~[safe archive resolution](https://github.com/kaznak/modelkeep/commit/220a62b94371cd6a21e623c52a9a5da86b2d30c0)~~、~~[Issue 0015](https://github.com/kaznak/modelkeep/commit/1b024cc23d148df3640c0290f99f0d25fd5eb4ea)~~ |
| 15: Nix/OCI | ~~[aarch64対応OCI CI](https://github.com/kaznak/modelkeep/commit/ad1b222abe5777b08aa9fad67e56b34b76afb38c)~~、~~[Issues 0012–0013: reproducible environment](https://github.com/kaznak/modelkeep/commit/2718a3a4dd54b1daade42d9abfe556192fc333af)~~、[Issue 0023](issues/0023-test-multiple-hf-client-versions.md) |
| 16: QNAP | ~~[Issue 0011: permissions](https://github.com/kaznak/modelkeep/commit/2b8de5e1bdcb191958c2546160ac3949756e30ff)~~、~~[Issue 0019: production runbook](https://github.com/kaznak/modelkeep/commit/e95e1dc62c2187165f2b156406009dad55c1db65)~~、~~[Issue 0005: Tailscale boundary](https://github.com/kaznak/modelkeep/commit/a13d437915dda66392bdc3367137eb1607992e59)~~、[Issue 0026](issues/0026-complete-qnap-gx10-acceptance-testing.md) |
| 17: GX10 cache import | ~~[cache importer実装](https://github.com/kaznak/modelkeep/commit/d017a2d1c9cdd63b830d6426d41ce7e6c61a5ff7)~~、[Issue 0025](issues/0025-validate-large-hf-cache-migration.md) |
| 18: CLI/管理 | ~~[list/show](https://github.com/kaznak/modelkeep/commit/fc34136b181acab80e2b19e674306726e25af214)~~、~~[explicit remove](https://github.com/kaznak/modelkeep/commit/e8aa9cce7aaccf1d9f5b8e701d317b0d678b343b)~~、~~[Issue 0016: refresh](https://github.com/kaznak/modelkeep/commit/ccd518b9fd39db55401bc0c34c9ed3630680eddc)~~、~~[Issue 0020: audit](https://github.com/kaznak/modelkeep/commit/ef2da9c8a9471bc4ca3b900b6a15472759195480)~~ |
| 19: observability | ~~[JSON operational logging基盤](https://github.com/kaznak/modelkeep/commit/8a4d9b2c048d6d72856f54967ad8b05cbb9b4e28)~~、[Issue 0027](issues/0027-complete-structured-operational-events.md)、[Issue 0004](issues/0004-add-storage-observability.md) |
| 20–21: observation/testing | ~~[Issue 0015](https://github.com/kaznak/modelkeep/commit/1b024cc23d148df3640c0290f99f0d25fd5eb4ea)~~、[Issue 0022](issues/0022-complete-protocol-observation-matrix.md)、[Issue 0024](issues/0024-add-black-box-crash-and-upgrade-tests.md)、[Issue 0025](issues/0025-validate-large-hf-cache-migration.md) |
| 22: CI | ~~[native amd64/arm64 image jobs](https://github.com/kaznak/modelkeep/commit/b0a5f24e3d50e50b05b3c6c6ce178b0ed69c39f0)~~、[Issue 0023](issues/0023-test-multiple-hf-client-versions.md) |
| 23–24: phases/MVP acceptance | 下記のphase/MVP対応表、[Issue 0026](issues/0026-complete-qnap-gx10-acceptance-testing.md) |
| 25: 将来拡張 | 下記の各候補Issue |
| 26: 実装優先タスク | 下記のリンク付き一覧 |

## 1. 概要

**ModelKeep** は、Hugging Face Hub を主対象とする **永続型 pull-through model mirror** である。

利用者は既存の Hugging Face クライアント操作を極力変更せず、ModelKeep を `HF_ENDPOINT` として指定する。ModelKeep に保存済みの revision/file は QNAP から配信し、未保存の場合のみ upstream から取得して QNAP に永続保存したうえでクライアントへ配信する。

主な利用環境は以下とする。

- サーバ: QNAP NAS / Container Station
- クライアント: ASUS Ascent GX10 等の推論ノード
- 配布: Nix で構築した aarch64-linux 対応 OCI image
- 主用途: vLLM、SGLang、Transformers、`hf download` 等で利用するモデルのローカル保全・再配信

ModelKeep の中心的な設計原則は次の一文に集約する。

> **サーバ実装は交換可能だが、保存されたモデルは長期資産として独立していなければならない。**

したがって、独自キャッシュ形式をモデル本体の正本とせず、ModelKeep のバージョン更新や実装変更によって保存済みモデルの再取得を要求しない。

---

## 2. 背景と解決したい問題

Hugging Face の標準ローカルキャッシュは単一クライアントでは有効だが、数十〜数百 GB のモデルを複数マシンで利用すると、各ノードの NVMe を圧迫する。また、Hub 上のモデルが削除・非公開化された場合に備えた長期保存用途として、通常の cache lifecycle に依存することは望ましくない。

既存案には次の問題がある。

### 2.1 共有 `HF_HUB_CACHE` / NFS

透過性は高いが、共有領域自体が依然として Hugging Face の「cache」であり、誤った prune/rm 操作等による削除から独立した archive とは言いにくい。複数クライアントからの同時更新や NFS locking も運用上の論点となる。

### 2.2 Olah

`HF_ENDPOINT` を利用する pull-through mirror として UX は ModelKeep の目標に近い。しかし、キャッシュ形式のバージョン互換性・移行性を長期保存の基盤として信頼することは避けたい。数 TB のモデルを保存した後にサーバ更新の都合でキャッシュ再構築が必要になる設計は ModelKeep の目的と合わない。

### 2.3 `hf-mount`

Xet を理解した lazy filesystem として有用だが、remote repository の現在状態を投影する性格が強い。ModelKeep が必要とする「upstream から削除されても保持する正本」とは目的が異なる。

### 2.4 単純な `hf download --local-dir` model store

保存性は良いが、クライアント側が専用コマンドや明示的な NFS path を使う必要があり、`hf download org/model` 等から外れる。利用者が誤って Hugging Face 本体から直接再ダウンロードする経路も残る。

ModelKeep はこれらの間を埋める。

```text
                 Hugging Face Hub
                        |
                  cache miss only
                        v
              +-------------------+
              |     ModelKeep     |
              |       QNAP        |
              |                   |
              | persistent mirror |
              | immutable revs    |
              +---------+---------+
                        |
                   LAN / HTTP
                        v
              +-------------------+
              |       GX10        |
              | standard HF cache |
              | vLLM / SGLang     |
              +-------------------+
```

GX10 のローカル Hugging Face cache は **L1 disposable cache**、QNAP の ModelKeep archive は **L2 persistent source** と位置付ける。

---

## 3. ModelArk 調査から取り入れる設計

ModelArk は Hugging Face モデルの長期保存・災害復旧を主眼とした archive system であり、ModelKeep と問題意識の一部を共有する。一方、ModelArk は pull-through HF mirror を目的とせず、保存データの利用には restore workflow を持つ。

ModelKeep は ModelArk そのものに依存せず、以下の設計思想を取り入れる。

### 採用する考え方

- revision を commit SHA へ解決し、immutable revision として保存する
- download 完了と durable archive 完了を区別する
- size/hash/digest を可能な範囲で検証する
- temporary file から atomic publish する
- crash/restart 後に中途半端なファイルを完成品として扱わない
- 保存済み revision を upstream の更新から独立させる
- metadata/catalog が破損しても model archive 本体から再構築できる設計にする
- Xet transport の失敗や大規模 shard の再取得を障害ケースとして扱う

### 初期版では採用しないもの

- git-annex による複数物理媒体管理
- ZipNN 等による archive 内 weight 圧縮
- SMART/drive planner
- オフライン HDD replica 管理
- restore を前提とした保存形式

QNAP の RAID、snapshot、backup はストレージ層へ委譲する。また ModelKeep は HTTP Range をそのまま効率的に返せることを重視するため、モデルファイルは upstream と byte-identical な通常ファイルとして保存する。

---

## 4. ゴール

### 4.1 UX

GX10 では原則として既存の操作を維持する。

```bash
export HF_ENDPOINT=http://modelkeep:8090
hf download Qwen/example-model
```

あるいは Home Manager で固定する。

```nix
home.sessionVariables = {
  HF_ENDPOINT = "http://modelkeep:8090";
};
```

Transformers 等の `huggingface_hub` 利用アプリケーションも可能な範囲で同じ endpoint を利用できることを目標とする。

### 4.2 保存性

一度完全に取得した revision は、以下の場合にも利用できること。

- upstream repository が削除された
- `main` が別 commit に進んだ
- GX10 のローカル HF cache を削除した
- ModelKeep server software を更新した
- metadata DB を再構築した

### 4.3 運用性

- QNAP Container Station 再起動後に自動復旧する
- root filesystem は read-only とする
- archive 以外への書込みを最小化する
- OCI image は Nix から再現可能に生成する
- 自動 GC で archive を削除しない

---

## 5. 非ゴール

MVP では以下を対象外とする。

- Hugging Face Hub API の完全互換実装
- model upload / push
- Spaces / Inference API
- Web UI
- repository collaboration
- Xet/CAS protocol の独自再実装
- archive の自動容量 GC
- multi-NAS replication
- ModelArk 相当の offline media management

必要な互換面は、実際の `huggingface_hub` / `hf` クライアントを integration test しながら追加する。

---

## 6. アーキテクチャ

```text
                    Hugging Face
                         |
                official HF client
                  / hf_xet internally
                         |
                         v
 +------------------------------------------------+
 | QNAP / ModelKeep                               |
 |                                                |
 |  +----------------------+                      |
 |  | HTTP compatibility   |<---- GX10            |
 |  | frontend             |      HF_ENDPOINT     |
 |  +----------+-----------+                      |
 |             |                                  |
 |      hit    |    miss                          |
 |             |      |                           |
 |             v      v                           |
 |      +-------------------+                     |
 |      | archive manager   |                     |
 |      +---------+---------+                     |
 |                |                               |
 |          +-----+------+                        |
 |          | fetch worker |----> Hugging Face    |
 |          +------------+                        |
 |                                                |
 | /data/models/...       ordinary model files    |
 | /data/metadata/...     reconstructable metadata|
 | /data/tmp/...          incomplete downloads    |
 +------------------------------------------------+
                         |
                         | plain HTTP + Range
                         v
                +-------------------+
                | GX10 local cache  |
                | ~/.cache/HF/...   |
                +---------+---------+
                          |
                     vLLM/SGLang
```

HTTP frontend と upstream fetcher を論理的に分離する。HF/Xet の仕様変化を archive format へ波及させない。

---

## 7. Archive 形式

例:

```text
/data/
  models/
    Qwen/
      ExampleModel/
        revisions/
          aabbccddeeff.../
            config.json
            tokenizer.json
            model-00001-of-00008.safetensors
            model-00002-of-00008.safetensors
            ...
            .modelkeep-manifest.json
        refs/
          main
  metadata/
    modelkeep.sqlite
  tmp/
```

### 原則

1. `main`、tag 等は保存時に commit SHA へ解決する。
2. `revisions/<commit-sha>` は publish 後 immutable とする。
3. `main` 更新時は新 revision を追加し、旧 revision は削除しない。
4. モデルファイルは独自 blob encoding に変換しない。
5. SQLite は index/catalog であり source of truth にしない。
6. manifest は filesystem から独立して読める単純な JSON とする。

Manifest 候補:

```json
{
  "repo_type": "model",
  "repo_id": "Qwen/ExampleModel",
  "requested_revision": "main",
  "commit": "aabbccddeeff...",
  "archived_at": "...",
  "files": [
    {
      "path": "config.json",
      "size": 1234,
      "sha256": "...",
      "etag": "..."
    }
  ]
}
```

manifest schema は versioned にする。ただし schema 更新が model files の再取得を要求してはならない。

---

## 8. HTTP compatibility layer

MVP は `hf download` に必要な subset だけを実装する。

候補:

```text
GET  /{namespace}/{repo}/resolve/{revision}/{path}
HEAD /{namespace}/{repo}/resolve/{revision}/{path}
```

加えて、revision 解決や client metadata 取得に必要な Hub API endpoint を protocol observation の結果に基づいて追加する。

対応すべき HTTP semantics:

- `HEAD`
- `GET`
- `Range`
- `206 Partial Content`
- `Content-Length`
- `Content-Range`
- `Accept-Ranges: bytes`
- ETag
- conditional request が必要なら `If-None-Match` 等
- commit/revision information

upstream の redirect をそのまま client に返して GX10 を Xet/CDN へ逃がさない。保存済みデータは ModelKeep 自身が配信する。

---

## 9. Xet 方針

ModelKeep は Xet protocol を独自実装しない。

```text
Hugging Face -> ModelKeep
    official huggingface_hub / hf_xet を利用可能

ModelKeep -> GX10
    ordinary HTTP file serving
```

これにより Xet の内部変更を ModelKeep の archive format から隔離する。

MVP では必要に応じて GX10 に以下を設定する。

```nix
home.sessionVariables = {
  HF_ENDPOINT = "http://modelkeep:8090";
  HF_HUB_DISABLE_XET = "1";
};
```

最終的には ModelKeep が返す response/header を制御し、クライアントが ModelKeep を迂回して Xet CAS へ直接取得しないことを integration test で保証する。

---

## 10. Upstream fetcher

初期実装では Hugging Face の protocol を Rust で再実装しない。公式 `huggingface_hub` / `hf` を fetch backend として利用する。

候補構成:

```text
Rust server
    |
    +-- fetch request
          |
          v
    Python/HF fetch helper
          |
          v
    huggingface_hub + hf_xet
```

これにより Xet、認証、gated repository、Hub 側仕様変更への追従を公式クライアントへ委譲できる。

MVP 後に、依存サイズや性能上の理由が明確になった場合のみ fetcher の一部を Rust native に置換する。

---

## 11. Atomicity / crash safety

ファイル取得は次の状態遷移とする。

```text
absent
  |
  v
downloading (.part)
  |
  v
verify size/hash
  |
  v
fsync(file)
  |
  v
atomic rename
  |
  v
fsync(directory)
  |
  v
published
```

完成ファイル名へ直接 download しない。

同一 object への同時 request は single-flight 化する。

```text
client A ---+
client B ---+--> one upstream fetch --> archive --> all clients
client C ---+
```

プロセス再起動時には `.part` を検査し、公式 fetcher が安全に resume 可能なら再開、保証できなければ incomplete object として破棄または quarantine する。

archive に ENOSPC が発生した場合、既存モデルを自動削除して空きを作らない。明示的なエラーと monitoring event を返す。

---

## 12. Integrity

ModelArk の設計を参考に、取得したことと保存完了を同一視しない。

検証優先順位:

1. upstream が信頼できる digest/hash を提供する場合は照合
2. size を検証
3. ModelKeep 独自に SHA-256 を記録
4. publish 後の verify コマンドで再検査可能にする

管理コマンド例:

```bash
modelkeep verify Qwen/ExampleModel
modelkeep verify --all
```

verify はデータを書き換えない。

---

## 13. Authentication / gated models

MVP は public model を第一対象とする。

private/gated repository 対応時には以下を検討する。

- client bearer token を upstream へ forward
- server-side service token
- repository ごとの credential policy

最低条件:

- token をログへ出さない
- query string 等へ保存しない
- SQLite/manifest に平文 token を保存しない
- QNAP archive 自体を適切な ACL で保護する

認証方式は archive format と分離する。

---

## 14. Rust server

HTTP frontend は Rust を第一候補とする。

候補 dependency:

- `axum`
- `tokio`
- `tower` / `tower-http`
- `serde` / `serde_json`
- `tracing`
- SQLite が必要なら `rusqlite` または `sqlx`

重要なのは dependency 数の最小化そのものではなく、常駐サーバとしての予測可能性、single binary 化、Nix closure の管理容易性である。

Path traversal を防ぐため、URL の repo/path を直接 filesystem path として連結しない。canonical mapping layer を実装し、`..`、absolute path、invalid encoding 等を拒否する。

---

## 15. Nix / OCI image

flake から以下を生成する。

```text
packages.aarch64-linux.modelkeep
packages.aarch64-linux.modelkeep-image
checks.aarch64-linux.*
```

概念例:

```nix
pkgs.dockerTools.buildLayeredImage {
  name = "modelkeep";
  tag = version;

  contents = [
    modelkeep
    hfFetcher
    pkgs.cacert
  ];

  config = {
    Entrypoint = [ "${modelkeep}/bin/modelkeep" "serve" ];
    User = "10001:10001";
    ExposedPorts."8090/tcp" = {};
  };
}
```

Python/HF fetcher を含むため、MVP では「数十 MB」という image size 目標を優先しない。まず再現性と upstream compatibility を優先する。

fetcher を将来 Rust native 化できれば closure を縮小する。

---

## 16. QNAP Container Station

Compose の基本方針:

```yaml
services:
  modelkeep:
    image: modelkeep:<version>
    restart: unless-stopped
    read_only: true

    ports:
      - "8090:8090"

    volumes:
      - /share/LLM/modelkeep:/data

    tmpfs:
      - /tmp

    cap_drop:
      - ALL

    security_opt:
      - no-new-privileges:true
```

要件:

- non-root
- host Docker socket を渡さない
- `/data` 以外の host filesystem を渡さない
- image tag は version/Git commit で固定
- QNAP 側の model archive は snapshot 対象にする

---

## 17. GX10 既存キャッシュの import

現在 GX10 に存在する数百 GB の Hugging Face cache を再ダウンロードせず ModelKeep archive へ移行できることを MVP 要件に含める。

管理コマンド例:

```bash
modelkeep import-hf-cache ~/.cache/huggingface/hub
```

処理:

```text
HF cache repository discovery
        |
        v
refs / snapshots inspection
        |
        v
revision -> commit SHA
        |
        v
resolve snapshot symlinks
        |
        v
copy into temporary archive revision
        |
        v
size/hash verification
        |
        v
manifest generation
        |
        v
atomic publish
```

snapshot の symlink をそのまま archive の外部 blob path に依存させない。ModelKeep revision directory 単独でモデルを復元・配信できるよう materialize する。

import 完了後、ModelKeep 経由で GX10 の空 cache へ同モデルを取得できることを確認してから GX10 の旧 cache を削除する。

---

## 18. CLI / 管理面

MVP 候補:

```bash
modelkeep serve
modelkeep list
modelkeep show <repo>
modelkeep verify <repo>
modelkeep verify --all
modelkeep import-hf-cache <path>
```

明示的削除を実装する場合でも、初期版では慎重にする。

```bash
modelkeep remove <repo> --revision <commit>
```

`gc` は MVP では実装しないか、実装しても dry-run をデフォルトとする。ModelKeep の価値は「cache miss を減らすこと」だけでなく「消さないこと」にある。

---

## 19. Observability

標準出力へ structured log を出す。

最低限記録するイベント:

- request — [Issue 0027](issues/0027-complete-structured-operational-events.md)
- local hit — [Issue 0027](issues/0027-complete-structured-operational-events.md)
- upstream miss — [Issue 0027](issues/0027-complete-structured-operational-events.md)
- fetch start/finish/failure — [Issue 0027](issues/0027-complete-structured-operational-events.md)
- verify failure — [Issue 0027](issues/0027-complete-structured-operational-events.md)
- archive publish — [Issue 0027](issues/0027-complete-structured-operational-events.md)
- disk full — [Issue 0004](issues/0004-add-storage-observability.md)、[Issue 0027](issues/0027-complete-structured-operational-events.md)
- recovery of incomplete download — [Issue 0027](issues/0027-complete-structured-operational-events.md)

Prometheus metrics は MVP 後でもよいが、追加しやすい構造にする。

候補 metrics:

以下のmetrics候補は[Issue 0004](issues/0004-add-storage-observability.md)で追跡する。

```text
modelkeep_requests_total
modelkeep_archive_hits_total
modelkeep_upstream_fetches_total
modelkeep_upstream_bytes_total
modelkeep_served_bytes_total
modelkeep_fetch_failures_total
modelkeep_archive_bytes
modelkeep_inflight_fetches
```

---

## 20. Protocol observation

実装前に現行 `huggingface_hub` が実際に何を要求するかを固定する。

対象:

- 小型public model — [Issue 0022](issues/0022-complete-protocol-observation-matrix.md)
- safetensors model — [Issue 0022](issues/0022-complete-protocol-observation-matrix.md)
- sharded model — [Issue 0022](issues/0022-complete-protocol-observation-matrix.md)
- revision指定 — [Issue 0022](issues/0022-complete-protocol-observation-matrix.md)
- `HEAD` — [Issue 0022](issues/0022-complete-protocol-observation-matrix.md)
- Range request — [Issue 0022](issues/0022-complete-protocol-observation-matrix.md)
- redirect — [Issue 0022](issues/0022-complete-protocol-observation-matrix.md)
- Xet-backed file — [Issue 0022](issues/0022-complete-protocol-observation-matrix.md)

テスト用 proxy/trace で request path、method、header、response semantics を記録し、ModelKeep が必要な compatibility subset を確定する。

**仕様を推測して完全な Hub clone を作らない。**

---

## 21. テスト戦略

### Unit tests

- archive path mapping
- path traversal rejection
- revision/ref mapping
- Range parsing
- manifest serialization
- state transition
- single-flight

### Integration tests

実際の Hugging Face client を使用する。

```bash
HF_ENDPOINT=http://127.0.0.1:8090 \
  hf download <test-model>
```

### 必須シナリオ

#### Cold miss

~~Fixtureで検証済み — [Issue 0015](https://github.com/kaznak/modelkeep/commit/1b024cc23d148df3640c0290f99f0d25fd5eb4ea)~~。実機は[Issue 0026](issues/0026-complete-qnap-gx10-acceptance-testing.md)。

```text
ModelKeep empty
 -> GX10 request
 -> upstream fetch
 -> archive publish
 -> client success
```

#### Warm hit / offline

~~Fixtureで検証済み — [Issue 0015](https://github.com/kaznak/modelkeep/commit/1b024cc23d148df3640c0290f99f0d25fd5eb4ea)~~。実機は[Issue 0026](issues/0026-complete-qnap-gx10-acceptance-testing.md)。

```text
GX10 cache empty
ModelKeep archive populated
upstream network blocked
 -> hf download succeeds
```

これを ModelKeep の最重要受入試験とする。

#### Concurrency

~~検証済み — [Issue 0018](https://github.com/kaznak/modelkeep/commit/81d6974c825d9c251aa753a433f15d5f5ecc4f0a)~~

同一 shard への複数 request に対して upstream fetch が一回だけであること。

#### Crash

[Issue 0024](issues/0024-add-black-box-crash-and-upgrade-tests.md)

大きな file の fetch 中に ModelKeep を SIGKILL し、再起動後に incomplete file を完成品として配信しないこと。

#### Revision retention

~~検証済み — [Issue 0016](https://github.com/kaznak/modelkeep/commit/ccd518b9fd39db55401bc0c34c9ed3630680eddc)~~

`main` が commit A から B へ変化した後も A を commit SHA 指定で取得できること。

#### Upstream deletion

~~Fixtureで検証済み — [Issue 0015](https://github.com/kaznak/modelkeep/commit/1b024cc23d148df3640c0290f99f0d25fd5eb4ea)~~

repository/file が upstream に存在しない状態を模擬しても、archive 済み revision を取得できること。

#### Upgrade

[Issue 0024](issues/0024-add-black-box-crash-and-upgrade-tests.md)

ModelKeep server version を更新しても archive 済み model files の migration/re-download が不要であること。

---

## 22. CI

CI matrix には最低限以下を含める。

- ~~Rust unit/integration tests — [reproducible checks](https://github.com/kaznak/modelkeep/commit/2718a3a4dd54b1daade42d9abfe556192fc333af)~~
- ~~`aarch64-linux` Nix build — [native arm64 CI](https://github.com/kaznak/modelkeep/commit/b0a5f24e3d50e50b05b3c6c6ce178b0ed69c39f0)~~
- ~~OCI image build — [multi-architecture image workflow](https://github.com/kaznak/modelkeep/commit/ad1b222abe5777b08aa9fad67e56b34b76afb38c)~~
- 複数`huggingface_hub` versionに対するcompatibility test — [Issue 0023](issues/0023-test-multiple-hf-client-versions.md)

Hugging Face client の更新によって HTTP behavior が変わった場合、CI で検出する。

upstream を使う online test と、fixture/archive だけを使う deterministic offline test を分離する。

---

## 23. 開発フェーズ

### Phase 0 — Protocol observation

- 現行`hf download`のHTTP trace — [Issue 0022](issues/0022-complete-protocol-observation-matrix.md)
- Xet使用時の挙動確認 — [Issue 0022](issues/0022-complete-protocol-observation-matrix.md)
- 必要endpoint/headerの確定 — [Issue 0022](issues/0022-complete-protocol-observation-matrix.md)
- ~~compatibility test fixture作成 — [Issue 0015](https://github.com/kaznak/modelkeep/commit/1b024cc23d148df3640c0290f99f0d25fd5eb4ea)~~

### Phase 1 — Read-only mirror

- ~~materialized archiveを手動配置 — [read-only mirror実装](https://github.com/kaznak/modelkeep/commit/22a7d32)~~
- ~~`HEAD` / `GET` — [Issue 0015](https://github.com/kaznak/modelkeep/commit/1b024cc23d148df3640c0290f99f0d25fd5eb4ea)~~
- ~~Range serving — [Issue 0015](https://github.com/kaznak/modelkeep/commit/1b024cc23d148df3640c0290f99f0d25fd5eb4ea)~~
- ~~revision path — [safe archive resolution](https://github.com/kaznak/modelkeep/commit/220a62b94371cd6a21e623c52a9a5da86b2d30c0)~~
- ~~実`hf download`で取得成功 — [Issue 0015](https://github.com/kaznak/modelkeep/commit/1b024cc23d148df3640c0290f99f0d25fd5eb4ea)~~

### Phase 2 — Durable archive

- ~~manifest — [durable archive core](https://github.com/kaznak/modelkeep/commit/eb62623)~~
- ~~commit-based immutable revision — [Issue 0014](https://github.com/kaznak/modelkeep/commit/0b2c4c5b0966136df2a9eb95189a8feb61f6dc66)~~
- ~~verify — [Issue 0020](https://github.com/kaznak/modelkeep/commit/ef2da9c8a9471bc4ca3b900b6a15472759195480)~~
- ~~`.part` / fsync / atomic rename — [Issue 0001](https://github.com/kaznak/modelkeep/commit/70d2c16a156185293ba297281a15b29499b8044d)~~
- ~~state-level crash recovery — [Issues 0006–0009](https://github.com/kaznak/modelkeep/commit/2b8de5e1bdcb191958c2546160ac3949756e30ff)~~
- process-level SIGKILL recovery — [Issue 0024](issues/0024-add-black-box-crash-and-upgrade-tests.md)

### Phase 3 — Pull-through

- ~~upstream fetch worker — [official HF fetcher](https://github.com/kaznak/modelkeep/commit/b7e3a33)~~
- ~~cache miss detection — [pull-through HTTP](https://github.com/kaznak/modelkeep/commit/0f30b4c)~~
- ~~single-flight — [single-flight実装](https://github.com/kaznak/modelkeep/commit/b071528)~~
- ~~archive publish後の配信 — [Issue 0001](https://github.com/kaznak/modelkeep/commit/70d2c16a156185293ba297281a15b29499b8044d)~~
- ~~upstream failure handling — [Issue 0017](https://github.com/kaznak/modelkeep/commit/561d511b75455fb49697d99221e74cd73192ffb9)~~

### Phase 4 — GX10 migration

- ~~`import-hf-cache` — [cache importer実装](https://github.com/kaznak/modelkeep/commit/d017a2d1c9cdd63b830d6426d41ce7e6c61a5ff7)~~
- 既存大型モデルのimport — [Issue 0025](issues/0025-validate-large-hf-cache-migration.md)
- GX10 cache削除後の確認 — [Issue 0025](issues/0025-validate-large-hf-cache-migration.md)
- GX10からModelKeepへ再取得 — [Issue 0025](issues/0025-validate-large-hf-cache-migration.md)

### Phase 5 — Nix / QNAP hardening

- ~~reproducible OCI image — [Issues 0012–0013](https://github.com/kaznak/modelkeep/commit/2718a3a4dd54b1daade42d9abfe556192fc333af)~~
- ~~non-root — [QNAP Compose](https://github.com/kaznak/modelkeep/commit/b441c26)~~
- ~~read-only rootfs — [QNAP Compose](https://github.com/kaznak/modelkeep/commit/b441c26)~~
- ~~Container Station deployment definition — [Issue 0005](https://github.com/kaznak/modelkeep/commit/a13d437915dda66392bdc3367137eb1607992e59)~~
- restart/reboot tests — [Issue 0026](issues/0026-complete-qnap-gx10-acceptance-testing.md)
- ~~QNAP snapshot integration policy — [Issue 0019](https://github.com/kaznak/modelkeep/commit/e95e1dc62c2187165f2b156406009dad55c1db65)~~

### Phase 6 — Operations

- ~~list/show/verify/audit — [Issue 0020](https://github.com/kaznak/modelkeep/commit/ef2da9c8a9471bc4ca3b900b6a15472759195480)~~
- metrics — [Issue 0004](issues/0004-add-storage-observability.md)
- disk capacity alerting — [Issue 0004](issues/0004-add-storage-observability.md)
- ~~upgrade procedure documentation — [Issue 0019](https://github.com/kaznak/modelkeep/commit/e95e1dc62c2187165f2b156406009dad55c1db65)~~
- cross-version executable validation — [Issue 0024](issues/0024-add-black-box-crash-and-upgrade-tests.md)
- ~~backup/restore documentation — [Issue 0019](https://github.com/kaznak/modelkeep/commit/e95e1dc62c2187165f2b156406009dad55c1db65)~~

---

## 24. MVP 完了条件

以下をすべて満たした時点を MVP 完了とする。

1. QNAP Container Station上でModelKeepが常駐する — [Issue 0026](issues/0026-complete-qnap-gx10-acceptance-testing.md)
2. GX10から`HF_ENDPOINT=ModelKeep`で標準`hf download`が動作する — [Issue 0026](issues/0026-complete-qnap-gx10-acceptance-testing.md)
3. ~~未保存public modelがupstreamから取得され、archiveへ完全にpublishされる — [Issue 0015](https://github.com/kaznak/modelkeep/commit/1b024cc23d148df3640c0290f99f0d25fd5eb4ea)~~（QNAP実機の永続性は[Issue 0026](issues/0026-complete-qnap-gx10-acceptance-testing.md)）
4. GX10のローカルcacheを削除しても同モデルをQNAPから再取得できる — [Issue 0026](issues/0026-complete-qnap-gx10-acceptance-testing.md)
5. ~~upstream通信を遮断してもarchive済みモデルを取得できる — [Issue 0015](https://github.com/kaznak/modelkeep/commit/1b024cc23d148df3640c0290f99f0d25fd5eb4ea)~~
6. ~~Range requestが正しく動作する — [Issue 0015](https://github.com/kaznak/modelkeep/commit/1b024cc23d148df3640c0290f99f0d25fd5eb4ea)~~
7. ~~同一fileの同時missが一回のupstream fetchに集約される — [single-flight implementation and tests](https://github.com/kaznak/modelkeep/commit/b071528)~~
8. fetch中の強制停止で壊れた完成ファイルが残らない — [Issue 0024](issues/0024-add-black-box-crash-and-upgrade-tests.md)
9. ~~`main`更新後も旧commitが保存される — [Issue 0016](https://github.com/kaznak/modelkeep/commit/ccd518b9fd39db55401bc0c34c9ed3630680eddc)~~
10. GX10の既存HF cacheから大型モデルをimportできる — [Issue 0025](issues/0025-validate-large-hf-cache-migration.md)
11. ModelKeep version updateでarchiveの再取得が不要である — [Issue 0024](issues/0024-add-black-box-crash-and-upgrade-tests.md)
12. QNAP reboot / Container Station restart後に自動復旧する — [Issue 0026](issues/0026-complete-qnap-gx10-acceptance-testing.md)

---

## 25. 将来拡張

MVP 後の候補:

- datasets対応 — [Issue 0029](issues/0029-add-dataset-repository-support.md)
- gated/private repositoryの認証policy — [Issue 0028](issues/0028-define-private-gated-credential-policy.md)
- tailnet identity-aware authorization — [Issue 0021](issues/0021-add-tailnet-identity-aware-authorization.md)
- Web/API management interface — [Issue 0030](issues/0030-add-management-api.md)
- repository pin/unpin policy — [Issue 0031](issues/0031-define-repository-pinning-policy.md)
- ~~explicit archive deletion workflow — [ADR-0007](adr/0007-explicit-revision-deletion.md)、[implementation](https://github.com/kaznak/modelkeep/commit/e8aa9cce7aaccf1d9f5b8e701d317b0d678b343b)~~
- multi-QNAP replication — [Issue 0032](issues/0032-add-multi-qnap-replication.md)
- S3/object storage backend — [Issue 0033](issues/0033-add-s3-storage-backend.md)
- OCI/model registry backend — [Issue 0034](issues/0034-add-oci-model-registry-backend.md)
- ModelArk等とのarchive export/import interoperability — [Issue 0035](issues/0035-add-archive-interoperability.md)
- upstream sourcesのHugging Face以外への拡張 — [Issue 0036](issues/0036-add-non-hugging-face-upstreams.md)

ModelKeep という名称は Hugging Face 専用に限定しないため、将来的に model artifact 全般の persistent pull-through mirror へ拡張できる。

---

## 26. 実装開始時の優先タスク

1. ~~Rust workspaceとNix flakeを作成する — [Nix package/image](https://github.com/kaznak/modelkeep/commit/4a049b3)~~
2. 現行`huggingface_hub` / `hf download`のprotocol traceを取得する — [Issue 0022](issues/0022-complete-protocol-observation-matrix.md)
3. ~~小型モデルをmaterializeしたfixtureを作る — [Issue 0015](https://github.com/kaznak/modelkeep/commit/1b024cc23d148df3640c0290f99f0d25fd5eb4ea)~~
4. ~~read-only`HEAD` / `GET` / Range serverを実装する — [Issue 0015](https://github.com/kaznak/modelkeep/commit/1b024cc23d148df3640c0290f99f0d25fd5eb4ea)~~
5. ~~実`hf download`をModelKeep endpointに向けて通す — [Issue 0015](https://github.com/kaznak/modelkeep/commit/1b024cc23d148df3640c0290f99f0d25fd5eb4ea)~~
6. ~~archive path/manifest/commit revision modelを実装する — [Issue 0014](https://github.com/kaznak/modelkeep/commit/0b2c4c5b0966136df2a9eb95189a8feb61f6dc66)~~
7. ~~atomic publishとstate-level crash testを実装する — [Issue 0001](https://github.com/kaznak/modelkeep/commit/70d2c16a156185293ba297281a15b29499b8044d)~~。process-level試験は[Issue 0024](issues/0024-add-black-box-crash-and-upgrade-tests.md)
8. ~~official Hugging Face fetch workerを統合する — [official HF fetcher](https://github.com/kaznak/modelkeep/commit/b7e3a33)~~
9. ~~single-flightを実装する — [single-flight実装](https://github.com/kaznak/modelkeep/commit/b071528)~~
10. ~~offline warm-hit testをCIに入れる — [Issue 0015](https://github.com/kaznak/modelkeep/commit/1b024cc23d148df3640c0290f99f0d25fd5eb4ea)~~
11. ~~`import-hf-cache`を実装する — [cache importer](https://github.com/kaznak/modelkeep/commit/d017a2d1c9cdd63b830d6426d41ce7e6c61a5ff7)~~。大型実機検証は[Issue 0025](issues/0025-validate-large-hf-cache-migration.md)
12. ~~Nix OCI imageをaarch64-linux向けに生成する — [native arm64 CI](https://github.com/kaznak/modelkeep/commit/b0a5f24e3d50e50b05b3c6c6ce178b0ed69c39f0)~~
13. QNAP Container Stationで長時間運転・再起動試験を行う — [Issue 0026](issues/0026-complete-qnap-gx10-acceptance-testing.md)

---

## 27. 最終的な設計判断

ModelKeep は「高速なキャッシュサーバ」だけを目指さない。

```text
GX10 local HF cache
    = 高速・小容量・削除可能

ModelKeep / QNAP
    = 永続・大容量・原則削除しない

Hugging Face Hub
    = upstream source
```

この責務分離を維持する。

特に以下を破らないこと。

> **ModelKeep のソフトウェア、DB、manifest schema は将来交換できる。しかし archive 済みモデル本体を ModelKeep の内部実装に人質としてはならない。**

この原則を満たす限り、HF/Xet の仕様変更、ModelKeep の再実装、QNAP の世代更新があっても、蓄積したモデル資産を維持できる。
