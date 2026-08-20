# ModelKeep 開発計画書

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

- request
- local hit
- upstream miss
- fetch start/finish/failure
- verify failure
- archive publish
- disk full
- recovery of incomplete download

Prometheus metrics は MVP 後でもよいが、追加しやすい構造にする。

候補 metrics:

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

- 小型 public model
- safetensors model
- sharded model
- revision 指定
- `HEAD`
- Range request
- redirect
- Xet-backed file

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

```text
ModelKeep empty
 -> GX10 request
 -> upstream fetch
 -> archive publish
 -> client success
```

#### Warm hit / offline

```text
GX10 cache empty
ModelKeep archive populated
upstream network blocked
 -> hf download succeeds
```

これを ModelKeep の最重要受入試験とする。

#### Concurrency

同一 shard への複数 request に対して upstream fetch が一回だけであること。

#### Crash

大きな file の fetch 中に ModelKeep を SIGKILL し、再起動後に incomplete file を完成品として配信しないこと。

#### Revision retention

`main` が commit A から B へ変化した後も A を commit SHA 指定で取得できること。

#### Upstream deletion

repository/file が upstream に存在しない状態を模擬しても、archive 済み revision を取得できること。

#### Upgrade

ModelKeep server version を更新しても archive 済み model files の migration/re-download が不要であること。

---

## 22. CI

CI matrix には最低限以下を含める。

- Rust unit/integration tests
- `aarch64-linux` Nix build
- OCI image build
- 複数 `huggingface_hub` version に対する compatibility test

Hugging Face client の更新によって HTTP behavior が変わった場合、CI で検出する。

upstream を使う online test と、fixture/archive だけを使う deterministic offline test を分離する。

---

## 23. 開発フェーズ

### Phase 0 — Protocol observation

- 現行 `hf download` の HTTP trace
- Xet 使用時の挙動確認
- 必要 endpoint/header の確定
- compatibility test fixture 作成

### Phase 1 — Read-only mirror

- materialized archive を手動配置
- `HEAD` / `GET`
- Range serving
- revision path
- 実 `hf download` で取得成功

### Phase 2 — Durable archive

- manifest
- commit-based immutable revision
- verify
- `.part` / fsync / atomic rename
- crash recovery

### Phase 3 — Pull-through

- upstream fetch worker
- cache miss detection
- single-flight
- archive publish 後の配信
- upstream failure handling

### Phase 4 — GX10 migration

- `import-hf-cache`
- 既存大型モデルの import
- GX10 cache 削除
- ModelKeep から再取得

### Phase 5 — Nix / QNAP hardening

- reproducible OCI image
- non-root
- read-only rootfs
- Container Station deployment
- restart/reboot tests
- QNAP snapshot integration policy

### Phase 6 — Operations

- list/show/verify
- metrics
- disk capacity alerting
- upgrade procedure
- backup/restore documentation

---

## 24. MVP 完了条件

以下をすべて満たした時点を MVP 完了とする。

1. QNAP Container Station 上で ModelKeep が常駐する。
2. GX10 から `HF_ENDPOINT=ModelKeep` で標準 `hf download` が動作する。
3. 未保存 public model が upstream から取得され、QNAP に永続保存される。
4. GX10 のローカル cache を削除しても同モデルを QNAP から再取得できる。
5. upstream への通信を遮断しても archive 済みモデルを取得できる。
6. Range request が正しく動作する。
7. 同一 file の同時 miss が一回の upstream fetch に集約される。
8. fetch 中の強制停止で壊れた完成ファイルが残らない。
9. `main` 更新後も旧 commit が保存される。
10. GX10 の既存 HF cache から大型モデルを import できる。
11. ModelKeep version update で archive の再取得が不要である。
12. QNAP reboot / Container Station restart 後に自動復旧する。

---

## 25. 将来拡張

MVP 後の候補:

- datasets 対応
- gated/private repository の認証 policy
- Web/API management interface
- repository pin/unpin policy
- explicit archive deletion workflow
- multi-QNAP replication
- S3/object storage backend
- OCI/model registry backend
- ModelArk 等との archive export/import interoperability
- upstream sources の Hugging Face 以外への拡張

ModelKeep という名称は Hugging Face 専用に限定しないため、将来的に model artifact 全般の persistent pull-through mirror へ拡張できる。

---

## 26. 実装開始時の優先タスク

1. Rust workspace と Nix flake を作成する。
2. 現行 `huggingface_hub` / `hf download` の protocol trace を取得する。
3. 小型モデルを materialize した fixture を作る。
4. read-only `HEAD` / `GET` / Range server を実装する。
5. 実 `hf download` を ModelKeep endpoint に向けて通す。
6. archive path/manifest/commit revision model を実装する。
7. atomic publish と crash test を実装する。
8. official Hugging Face fetch worker を統合する。
9. single-flight を実装する。
10. offline warm-hit test を CI に入れる。
11. `import-hf-cache` を実装する。
12. Nix OCI image を aarch64-linux 向けに生成する。
13. QNAP Container Station で長時間運転・再起動試験を行う。

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
