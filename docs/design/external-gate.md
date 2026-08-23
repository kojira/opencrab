# 外部 gateway 実装者向け入口

この文書は、任意の言語で独自の gateway kind を実装し、opencrab と接続する人のための入口である。
例では合成名 `kind_id=example` を使う。
特定の外部サービス、運用者、稼働中のエージェントを前提にしない。

wire の正本は [Discord gate 設計](./discord-gate.md) §2 の protocol 2 である。
全体像は [ゲートウェイ仕様概要](./gateway-overview.md)、kind 固有設計の例は
[Discord gate 設計](./discord-gate.md) と [Nostr gate 設計](./nostr-gate.md) を参照する。
この文書は wire を再定義せず、外部実装者が最初に必要とする共通契約と未決事項を示す。

## 最初に押さえる境界

gateway は外部サービスとの I/O と protocol 変換を担当する独立プロセスである。
会話履歴、場、参加者、policy、route、turn、権限、delivery の正本は core/store に置く。
gateway は「誰に考えさせるか」「返答してよいか」「再送するか」を独自判断しない。

外部 kind を追加するときも、Discord や Nostr の内部実装を複製する必要はない。
必要なのは、共通の kind / instance / revision、place / binding / route、protocol 2 に沿うことである。
ただし、現行には任意 kind の登録から起動までを完結する公開管理 API はまだない。

## B1: kind と instance を登録する

### 二つの宣言がそろって初めて接続できる

永続設定の正本は次の DB entity である。

- `gate_kinds`: kind の protocol major、origin scope、ingress discovery
- `gate_instances`: credential とプロセスの単位、および active revision
- `gate_instance_revisions`: immutable な schema-bound config と desired enabled state
- `secret_sets` / `secret_values`: revision が使う secret
- `gate_connections`: 接続 epoch と観測状態。設定ではない

これとは別に、gateway は接続直後の protocol 2 `hello` で live spec を宣言する。
`hello` は `kind_id`、`instance_id`、`revision` に加え、`origin_scope`、`address_form`、
`ingress_discovery`、tools、effects、capabilities、actions を含む。

core は `hello` を登録要求として扱わない。
DB に instance、active revision、kind 宣言が先に存在し、present かつ enabled でなければ拒否する。
さらに kind、revision、protocol major、origin scope、ingress discovery が DB 宣言と一致しなければ拒否する。
同じ kind の active instance 間では、address form を含む live spec 全体も同一でなければならない。

`kind_id` は小文字英字で始まり、小文字英数字、`_`、`-` からなる最大 64 byte の識別子である。
`instance_id` は canonical lowercase UUID、revision は DB の active revision と一致させる。
一つの credential を一つの instance、一つの gateway process として登録する。

protocol 1 の compatibility seed は既存 web / Nostr を移行するための経路である。
新しい外部 kind を protocol 1 名で接続して自動作成する仕組みではない。
protocol 2 の `hello` も DB row を作成・修復しない。

現行の protocol parser、kind registry、store の接続検査は kind 非依存である。
したがって、登録済み kind を接続するたびに core の protocol 分岐を追加する必要はない。
一方、現行の管理設定と launcher は任意 kind を設定だけで追加する公開 surface を提供していない。
外部 kind は、現時点では DB 登録と launcher 統合を行う実装作業なしに配備できない。

> **未決（issue 化）:** 任意 kind の `gate_kinds` / instance / revision を作成・更新する公開管理面、
> config schema の登録方法、secret 名と gateway への安全な受け渡し、launcher への executable 宣言を定める。
> 手動 SQL を正式な登録経路とはみなさず、`hello` による暗黙作成も導入しない。

## B2: プロセスモデルと transport

現行契約は Unix domain socket 上の UTF-8 LF-JSON である。
一つの JSON object を一行にし、LF で終端する。一行の上限は 1 MiB である。
gateway が client として core の socket に接続し、最初の request として `hello` を送る。
protocol 2 では `hello` 成功後、外部サービス側の準備が完了してから `ready` を送る。

gateway は core と同じプロセス内の plugin ではない。
起動、PID 所有、停止、監督は launcher の責務であり、core は gateway を spawn しない。
core は接続状態を記録するが、それを根拠にプロセスを勝手に再起動しない。

別コンテナで動かす場合も transport は変わらない。
core が作る socket の親ディレクトリを volume として両コンテナへ共有し、
gateway コンテナから同じ socket pathname を開けるようにする。
volume mount だけでなく、Unix socket を開ける UID/GID と directory permission も揃える。
TCP への置換や socket proxy は、現行契約には含まれない。

gateway が落ちた場合に上げ直す主体は launcher、service manager、またはコンテナ orchestrator である。
現行の同梱 launcher は gateway を自動 restart せず、異常終了を degraded として扱う。
復旧は明示的な restart、または外側の supervisor policy で新しいプロセスを起動して行う。
再接続時は新しい connection epoch で `hello` と `ready` をやり直す。

## B3: 一つの instance を複数エージェントで共有する

core の route モデルは kind 非依存であり、1 instance : N subject を表現できる。
Discord の shared instance と同じ構造を、外部 kind にも使う。

正本の関係は次のとおりである。

1. `places` に、会話の単位となる場を一つ作る。
2. `gate_bindings` に、その place と instance の外部 address を結ぶ binding を作る。
3. 各エージェント subject を place の participant として扱う。
4. 各 subject について `subject_routes(subject, place, kind, purpose)` を作る。
5. 複数 subject の route の `binding_id` を同じ binding に向ける。

route purpose は `inbound`、`outbound`、`timed`、`tool:<name>` である。
同じ binding を共有しても、subject ごとの policy、tool visibility、turn、履歴は core 側で分離される。
送信開始時には選択済み route の instance、revision、connection epoch、address を snapshot する。
接続不能になっても同 kind の別 instance へ自動 fallback しない。

`prebound` kind では、core が登録済み binding を `bind` で gateway に知らせる。
`membership` kind では gateway が観測した address を `event.discovery` で提示し、
core が admission 後に place / binding / membership / route を実体化する。
どちらを採るかは外部サービスの発見モデルから kind spec で宣言し、実装者の都合で接続中に変えない。

> **未決（issue 化）:** 外部 kind について、共有 instance の participant 集合、place、binding、
> subject route と purpose を宣言・更新する汎用 admin/config surface を定める。
> DB model は表現できるが、外部連携者向けの安定した宣言手順はまだない。

## B4: 切断中の配送保証

共通原則は「gateway や core link に未完データを buffer しない、自動再送しない」である。
失敗を成功に見せず、結果を確定できない時は不定として残す。

outbound delivery の状態は `prepared -> sending -> delivered | failed | indeterminate` である。
active connection epoch が無いまま送信を始められなければ `failed` になる。
外部 API へ渡す前後を区別できない timeout、socket drop、process crash は `indeterminate` になる。
terminal state を後から成功へ推測せず、同じ delivery を自動再送しない。

したがって gateway が 5 分停止している間に core が作った返信は、黙って消えない。
未接続が確定していれば delivery は `failed`、送信開始後に結果が分からなくなれば `indeterminate` として残る。
自動復旧後にその delivery が勝手に再投稿されることはない。
再送が必要なら、運用者または上位の明示操作が新しい effect / delivery を作る必要がある。

反対方向、すなわち停止中に外部サービスへ到着した入力は、core がまだ観測していない。
外部サービスが durable な一覧・履歴 API を持つ kind は、gateway が復帰後に source を read し、
安定した origin を付けた通常の `event` として catch-up する。
live 配信と catch-up が重なる場合も同じ origin にして、core の dedup に委ねる。
gateway 独自 DB を会話履歴や dedup の正本にしない。
source read の失敗や cursor gap は診断可能な失敗として表に出し、catch-up 済みと報告しない。

source に読戻し API が無い、cursor を保存していない、または履歴範囲外なら、その 5 分の入力は回収できない。
現行 protocol の `m=read` は prebound gateway が core の場ログを読む request であり、
外部サービスの欠落入力を復元する命令ではない。membership kind ではこの `read` 自体を使えない。
外部入力の catch-up 可否、cursor、pagination、同一 origin の作り方は kind 固有契約に明記する。

> **未決（issue 化）:** 独自 kind 共通の source-side catch-up 管理面と cursor 永続化契約はない。
> 外部 API が持つ読戻し能力に基づき、kind ごとの issue で範囲と非保証を固定する。

## B5: place と address を設計する

`address_form` は gateway が `hello` で宣言する正規表現で、address 文字列全体に適用される。
同じ kind の全 active instance は同じ form を宣言する。
address は外部 API の locator であり、表示名や推測可能な別名を identity の代用にしない。

place の粒度は「一続きの会話として履歴と policy を共有する単位」である。
Discord ならチャンネル、必要な kind ではスレッド、Nostr の現行設計なら設定済み timeline が相当する。
relay、HTTP endpoint、credential、process といった接続上の都合だけで place を分割しない。

既知の場は provision により place / binding を先に作れる。
外部 membership が場を発見する kind は、検証済み `event.discovery` の観測を core が admission し、
eligible subject がいる場合だけ place / binding / membership / route を実体化できる。
gateway は観測したという理由だけで policy や participant を決めない。

> **未決（issue 化）:** 外部側の会話が削除、archive、退出、address 再利用になった時の
> place / binding / membership / route の閉鎖・保持・再 provision 契約は共通仕様として未決である。
> kind ごとの消滅イベントと core の管理操作を定めるまで、gateway が物理削除を推測してはならない。

## B6: 添付

現行の共通 wire が受け付ける添付は URL 参照の画像だけである。
`event.attachments` の各要素は `kind=image` と `url` を持ち、必要なら由来作者を付ける。
画像 byte、base64、multipart body、ローカル file path を LF-JSON に載せない。
core が必要時に URL を取得し、content type と実 byte を検査する。

画像以外の file、動画、音声、inline byte、upload/spool/transfer protocol は現行契約にない。
未知の attachment kind を画像へ近似せず、kind 固有 field を共通 payload に追加しない。
添付拡張は #745 系の後続 issue で共通 wire と取得境界を更新してから利用する。

## C1〜C4: core 共通機能との関係

### C1: #747 と #748

#747 の NO_REPLY fail-closed と #748 の settled context は engine/core の契約であり、kind 非依存である。
外部 gateway は NO_REPLY を外部投稿へ補完せず、settled turn の文脈を独自に再構成しない。
登録済み外部 kind にも追加の wire や kind 固有実装なしで、そのまま効く。

### C2: activity の語彙

`activity` は core から gateway への応答なしの表示通知である。
共通 field は `m=activity`、`address`、文字列の `activity_id`、`state` である。

- `state=started`: `kind=turn|background` を持ち、`label` は任意
- `state=progress`: `label` を持つ
- `state=ended`: 終了を示す

これは typing や進捗表示のための best-effort 通知で、effect や delivery ではない。
route 不在、未接続、queue 満杯、gateway 側の表示失敗で delivery を作らず、会話結果も変えない。

### C3: 外部 API token

外部 API token は instance revision に結び付く `secret_sets` / `secret_values` に置く想定である。
同じ kind でも instance ごとに secret を分け、config bytes、argv、通常ログ、status、report に値を出さない。
launcher/runner が active revision の secret を解決し、gateway process だけへ渡す境界を使う。

> **未決（issue 化）:** 任意 kind の secret name、必須/任意 schema、rotation、runner からの注入形式は未決である。
> 現行 runner の既知 kind 用規則を外部 kind が流用できるとはみなさない。

### C4: 即応とまとめ入力

即応と debounce の二層入力は place policy と core inbox/turn の責務であり、kind 非依存である。
gateway は event と安定した origin、author、address を運び、即応判定や window を持たない。
同じ外部 kind でも place または authority layer ごとの core policy として宣言できる構造である。

> **未決（issue 化）:** 任意 kind 用 place policy schema と、その作成・更新を行う汎用管理 surface は未決である。
> gateway の private config に debounce 値を置いて core policy を迂回してはならない。

## 実装前チェックリスト

- kind、instance、revision が DB に事前登録され、`hello` と一致している
- launcher が gateway を独立 process として起動し、core socket だけを transport に使う
- place、binding、各 subject の route purpose が宣言されている
- address と origin が安定し、display name から identity を推測しない
- failed と indeterminate を区別し、自動再送しない
- source-side catch-up の可否と非保証を kind 固有文書に書く
- 添付は現行の URL-based image 契約だけを送る
- activity、NO_REPLY、settled context、二層入力を gateway 側へ再実装しない
- secret を instance 単位で隔離し、通常の設定・ログ・引数へ出さない
- 未決マーカーの項目を推測で埋めず、対応 issue が確定してから実装する
