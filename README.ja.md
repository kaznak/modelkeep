# ModelKeep

<p align="right"><a href="README.md">English</a> · <a href="README.ja.md">日本語</a></p>

ModelKeep は、Hugging Face モデルリポジトリ向けの persistent pull-through mirror です。
取得済み revision を永続ストレージ上の通常ファイルとして保存し、`HF_ENDPOINT` を設定した
既存の `hf` / `huggingface_hub` クライアントへ HTTP で再配信します。

QNAP の archive が永続データの正本です。クライアントキャッシュ、index、サーバープロセス、
コンテナイメージを交換・再構築しても、保存済みモデルを再ダウンロードする必要はありません。

## MVP の実装状況

現在の MVP は以下を提供します。

- commit 単位の immutable revision と分離された mutable ref
- atomic publish、クラッシュ復旧、SHA-256 manifest
- HTTP `GET`、`HEAD`、byte range、conditional request、Hub model metadata
- 公式 Hugging Face client を利用した single-flight pull-through acquisition
- 既存 Hugging Face Hub cache の import
- structured operational logging
- Nix による再現可能な amd64 / arm64 OCI image

ModelKeep 自身は Xet/CAS protocol を実装しません。upstream の取得は公式 Hugging Face
client に委譲します。クライアントへの配信は ModelKeep 自身が通常の HTTP ファイルとして行い、
upstream へリダイレクトすることはありません。

## クイックスタート

Hugging Face client の endpoint を ModelKeep に向けます。

```sh
export HF_ENDPOINT=http://modelkeep:8090
export HF_HUB_DISABLE_XET=1
hf download Qwen/example-model
```

保存済み revision は Hugging Face へ接続せず配信されます。公式 fetch helper が設定された
サーバーでは、未保存の ref やファイルへのアクセスが upstream 取得を開始できます。

## QNAP へのデプロイ

Container Station 用の [`compose.yaml`](compose.yaml) を用意しています。GitHub Actions は
Nix flake から amd64 / arm64 image をビルドし、`v*` tag が push されたときに GHCR へ multi-architecture
image を公開します。

```sh
git tag v0.1.0
git push origin v0.1.0
```

QNAP 側では以下のように起動します。

```sh
export MODELKEEP_IMAGE_REPOSITORY=ghcr.io/OWNER/REPOSITORY
export MODELKEEP_IMAGE_TAG=0.1.0

docker login ghcr.io
docker compose pull
docker compose up -d
```

QNAP の archive share が別の場所にある場合は、`compose.yaml` の `/share/LLM/modelkeep` を変更してください。
コンテナは UID/GID `10001:10001`、read-only root filesystem、capability drop で動作し、永続データは `/data` のみに書き込みます。HTTP port は QNAP host の loopback のみに公開されます。host の公式 Tailscale app で tailnet 限定 HTTPS endpoint を構成し、port 8090 を LAN に直接公開しないでください。
ホスト側の UID/GID と権限 preflight は [`docs/deployment/qnap-permissions.md`](docs/deployment/qnap-permissions.md) を参照してください。
Tailscale Serve の設定と境界確認は [`docs/deployment/qnap-tailscale-serve.md`](docs/deployment/qnap-tailscale-serve.md) を参照してください。

private / gated repository を使う場合は、deploy 環境から `HF_TOKEN` を渡します。認証情報を URL、manifest、log に保存しないでください。

## 開発

ホスト側の前提は Nix と Linux 実行環境だけです。Rust や Hugging Face のツールを使う前に、
固定された開発環境へ入ります。

```sh
nix develop
```

標準チェックはその環境内で実行します。

```sh
nix develop -c cargo fmt --check
nix develop -c cargo clippy --all-targets --all-features -- -D warnings
nix develop -c cargo test --all-features
nix flake check
```

Cargo、Rustc、Rustfmt、Clippy、Python、`hf` client、Git、CA証明書はすべて
`flake.lock` が固定するrevisionから提供されます。flakeはformat、Clippy、unit test、
Hugging Face client環境を個別のcheckとして公開します。実際のHugging Faceへ接続する
テストには引き続きnetwork accessが必要です。

正式な build 定義は Nix flake にあります。

```sh
nix build .#packages.x86_64-linux.modelkeep
nix build .#packages.x86_64-linux.modelkeep-image
```

Pull Request と `main` への push では amd64 / arm64 の image build が実行されますが、公開はしません。
GHCR へ push し multi-architecture manifest を作成するのは `v*` tag の場合だけです。

## CLI

```sh
modelkeep serve [archive-root] [bind-address]
modelkeep health
modelkeep ready
modelkeep audit [archive-root]
modelkeep refresh [archive-root] <repo-id> <ref> [--dry-run]
modelkeep list [archive-root] <repo-id>
modelkeep show [archive-root] <repo-id> <commit>
modelkeep verify [archive-root] <repo-id> <commit>
modelkeep remove [archive-root] <repo-id> <commit> [--dry-run]
modelkeep import-hf-cache <cache-path> [archive-root]
```

compute node 上の既存 cache を削除する前に import と verify を行います。

```sh
modelkeep import-hf-cache ~/.cache/huggingface/hub /data
modelkeep verify /data Qwen/ExampleModel <commit>
```

`GET /healthz` は軽量な liveness check です。QNAP 用 Compose 設定では、コンテナの
healthcheck に `modelkeep health` を使用します。

## Durable archive

```text
/data/
  models/<namespace>/<name>/
    revisions/<commit>/       通常のモデルファイルと manifest
    refs/main                  commit を指す mutable ref
  tmp/                         作業中データのみ
```

publish 済み revision は immutable です。`main` を更新しても別 commit を追加または選択するだけで、
旧 revision は削除しません。ダウンロードは staging、検証、flush、atomic publish の順に行われ、
不完全な staging は完成済み object として配信されません。archive の自動 GC は行いません。

永続設計の判断は [`docs/adr/`](docs/adr/) に記録しています。通常ファイルによる archive、immutable
revision、Xet 境界、自動 GC なし、公式 upstream acquisition、derived metadata などを定義しています。

## ライセンス

ModelKeep は [MIT License](LICENSE) のもとで公開します。
