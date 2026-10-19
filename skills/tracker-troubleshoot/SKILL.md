---
name: tracker-troubleshoot
description: "Use this when a user reports that easylife-ai / git-ai is not reporting their AI token usage or their code commits to the tracker, or when the dashboard shows missing data. Starts from the `easylife-ai doctor` diagnosis when one is supplied, then diagnoses the two reporting pipelines (token usage and code stats) end to end and produces a fix plus a verification, without losing or duplicating already-reported data."
---

# Tracker 上报排查 Skill

排查"客户端装了但服务端收不到数据"的问题。两条上报链路互相独立，**必须分开定位**，不要一次改一堆东西。

## 你是谁、在什么环境下工作

这份 skill 由 easylife-ai 客户端随安装脚本一起放到用户的 `~/.easylife-ai/skills/tracker-troubleshoot/SKILL.md`，用户通过下面的命令把它交给你：

```bash
easylife-ai ops-check            # 打印一段 prompt，用户复制到任意智能体工具里
easylife-ai ops-check <问题>      # 同上，可带上具体问题
easylife-ai ops-check no-doctor <问题>   # 不带 doctor 诊断段
```

所以：**你运行在用户自己的机器上，用户就是当事人。** 你需要的信息（配置文件、hook、日志、状态文件）都在这台机器上，可以直接读。不要假设你只能靠用户转述——先自己看，再问用户确认你看到的和实际现象是否一致。

prompt 里有两段内容，**先读诊断，再看问题**：

- **`easylife-ai doctor诊断情况如下：`** —— 用户最近一次跑 `easylife-ai doctor` 的完整输出。这是已经发生过的真实上报测试结果，见下面的「第 0 步预备：读 doctor 诊断」。
- **`用户问题如下：`** —— 用户的原始描述。如果他填的还是 `<输入自己的问题>`，说明他没写具体问题，先问他：**token 上报和代码上报，哪一个没有数据？**

## 第 0 步预备：读 doctor 诊断

`easylife-ai doctor` 会**真跑一遍两条上报链路**（真的建临时仓库、真的 `git push`、真的生成 3 轮 Claude Code 会话），并把结果原样存到 `~/.easylife-ai/doctor-last-report.txt`。`ops-check` 会自动把这份输出贴进 prompt 的"诊断情况如下"里。

**如果 prompt 里没有"doctor诊断情况如下"这一段**，说明用户用了 `ops-check no-doctor`，是有意不给诊断结果——**不要因此认定链路正常，也不要要求用户必须先跑 doctor**，直接按本 skill 的手工流程查即可。`no-doctor` 只是省掉这段文本，不改排查方法。

**这比你自己逐条查日志快得多，先把它读完再决定查什么。** 输出分 7 步：

| 步 | 标题 | 通过时的含义 |
|---|---|---|
| 1 | 环境自检 | 版本、`git` 可用、PATH shim 就位（`~/.easylife-ai/bin/git` 在 PATH 里赢过真 git） |
| 2 | 配置校验 | `tracker-config.json` 四个字段齐全且非空 |
| 3 | hook 与插件检查 | 13 个 AI 工具的 hook/插件已配置且最新；会自动修复缺失项 |
| 4 | 连通性 | 只做 TCP 探测，**不写任何数据到服务端** |
| 5 | 代码上报 | 真推送到临时仓库，返回 `recordId` = 服务端确实入库了 |
| 6 | token 上报（3 轮） | 真的生成 3 轮会话并上报，返回 `recordId` |
| 7 | 结论 | 汇总 |

### 怎么读这份诊断

**看标记，不只看结论那一行。** 每项前面有四种标记：

- `✓` 通过，且确实执行了
- `⚠` 警告，非阻断
- `✗` 失败（阻断项会让 doctor 以退出码 1 结束）
- `⊘` **跳过 = 没验证**，既不是通过也不是失败

**最关键的一条：`⊘` 不等于通过。** 例如：

```
[6/7] token 上报（3 轮）
      ⊘ 已禁用：token_report=false，跳过 token 上报验证（未验证）
[7/7] 结论
      ✓ 已执行的检查全部通过
      ⚠ 1 项未验证，不能据此认为两条链路都正常
```

看到 `token_report=false` 就说明 `~/.easylife-ai/tracker-config.json` 里的 `token_report` 被关掉了，**token 链路根本没被测过**。这时不要继续查 hook 和日志——先问用户是不是有意关的，要开启就把它改成 `true`（或删掉该字段，缺省即开启），然后重跑 doctor。

### 按诊断结果直接分流

| doctor 显示 | 去哪一步 |
|---|---|
| 第 4 步 `✗ TCP 不可达` | **第 3 步（网络与服务端层）** |
| 第 5 步 `✗` 或没有 `recordId` | **第 4 步（"上报成功但数据没入库"）** 的代码上报小节 |
| 第 6 步 `✗` 或只有部分轮次入库 | **第 4 步** 的 Token 上报小节 |
| 第 3 步 `✗` / 有 `⚠ 自动修复失败` | **第 2 步（Hook 与二进制层）** |
| 第 2 步 `✗` | **第 1 步（配置层）** |
| 全部 `✓` 但用户仍说没数据 | **第 4 步**——很可能是去重、"没有新轮次"等设计行为 |
| 第 6 步是 `⊘` | 先确认 `token_report` 开关，见上 |

### 注意事项

- **诊断里的 `recordId` 是真实入库记录**（每次 doctor 会在服务端留下 1 条代码记录 + 3 条 token 记录）。这是验证手段，不是脏数据；不要因为这个让用户"清理"。
- **诊断结果可能是旧的。** 看一眼文件时间：`ls -l ~/.easylife-ai/doctor-last-report.txt`。如果用户在跑完 doctor 之后又改过配置或 hook，让用户重跑一次 `easylife-ai doctor` 再继续。
- **不要让用户为了排查反复重跑 doctor。** 每一次都会往服务端写数据。诊断够用就停。
- **doctor 会自动修复 hook。** 所以"第 3 步全部 ✓"可能意味着它刚替你修好了——那么用户原来的问题可能已经解决，直接进**第 5 步验证**即可。
- 用户没跑过 doctor 时，prompt 里会写"尚未运行过"。这时可以建议他先跑 `easylife-ai doctor`（说明它会真实上报），或者直接按本 skill 的手工流程查。

## 链路全景

| 链路 | 触发时机 | 接口 | 客户端入口 |
|---|---|---|---|
| Token 用量上报 | Claude Code / Codex 的 Stop hook、OpenCode 插件 `session.idle` | `POST {tracker_url}/ai-code-boost/open/report/token/usage` | `easylife-ai report-token-usage <platform>` |
| 代码统计上报 | `git push` 成功后 | `POST {tracker_url}/ai-code-boost/open/report/stats` | `git push` → pre-push hook → `report_pushed_commits` |

两条链路共用 `~/.easylife-ai/tracker-config.json`，都用 `X-Team-Key` 头鉴权。

## 铁律：先取证，再改

在动任何文件之前，先按下面顺序收集信息。**任何一步都不要删除用户的状态文件**。

1. 问清楚现象：是"token 没有"还是"代码没有"还是"都没有"？最后一次成功是什么时候？
2. 区分是**单机问题**还是**服务端/配置问题**：如果同一团队其他人都正常，问题在本地；如果所有人都没有，先怀疑 `tracker_url`、`team_key` 或服务端。
3. 全程只读，除非到了"修复"环节。

## 第 0 步：三分钟体检

**如果 prompt 里已经有 doctor 诊断结果，这一步只用来补充 doctor 看不到的东西**——doctor 验证的是"此刻能不能通"，体检看的是"历史上通没通过、用户的实际配置是什么样"。两者一起看。

doctor 已覆盖（有诊断结果就不必重复跑）：版本、`git` 可用性、PATH shim、配置字段是否齐全、hook/插件安装状态、TCP 可达性、两条链路的端到端实测。

doctor 不覆盖、必须自己查的是 **第 4 条（历史上报记录）和第 5 条（token 增量状态）**：doctor 只证明"此刻能通"，证明不了"用户平时的那次上报有没有成功"。

先跑这组命令，一次拿到 80% 的答案。**逐条看输出，不要跳。**

```bash
# 1. 客户端版本与二进制位置（两个路径不一致本身就是问题，见第 2 步）
easylife-ai --version
git-ai --version
command -v easylife-ai git-ai git

# 2. 配置是否齐全（tracker_url / team_id / team_key / username 四项都必须在且非空）
cat ~/.easylife-ai/tracker-config.json

# 3. 网络是否可达（只探 TCP，不写任何数据到服务端）
nc -z -G 5 <服务端主机> <端口> && echo "port open"

# 4. 代码上报历史（只看最后 20 行）
easylife-ai tracker log -n 20

# 5. token 上报的增量状态（文件存在且有条目 = 这条链路历史上通过）
cat ~/.easylife-ai/reported-turns.json
```

判读：

- `nc` 不通 → 走 **第 3 步（网络与服务端）**。
- `nc` 通、但 `tracker log` 全是 `failed` → 走 **第 3 步**。
- `nc` 通、`tracker log` 正常、token 状态文件是空的或没有本机会话 → 走 **第 2 步（hook / 二进制）**。
- 一切正常但用户说没数据 → 走 **第 4 步**（去重、"没有新轮次"等设计行为）。

> **不要用"POST 一个空 body 看 HTTP 码"来探活。** 实测该服务端的服务端异常兜底会把**鉴权失败、参数缺失、方法不匹配**全部包成同一种 `HTTP 500` + `{"code":500,"msg":"服务器内部错误"}`，无法区分故障类型；而 POST 一个格式合法的 body 会**真的往服务端写入一条记录**。探活只用 TCP 探测和本地日志。

## 第 1 步：配置层

配置文件路径固定为 `~/.easylife-ai/tracker-config.json`（Windows 是 `%USERPROFILE%\.easylife-ai\tracker-config.json`）。这个文件由安装脚本写入，**只有 `TRACKER_URL`、`TEAM_ID`、`TEAM_KEY`、`USER_NAME` 四个环境变量同时非空时才会生成**。

### 常见故障

**配置文件根本不存在。** 安装时 `USER_NAME` 为空或全是空白字符，安装脚本会打印 `USER_NAME is missing or blank; tracker configuration was not written.` 然后什么都不写。注意：`TRACKER_URL`/`TEAM_ID`/`TEAM_KEY` 都在但 `USER_NAME` 缺失时，同样不会写入——这是设计如此，不要试图手工补一个只有三个字段的文件。

**`team_id` 不是数字。** token 上报会静默跳过：`report-token-usage: invalid team_id '...': ... Skipping report.`（只在 debug 日志里可见）。代码上报则直接失败并返回 `invalid team_id`。

**`username` 是身份主键，写错了不会报错，只会把数据记到别人名下或新建一个员工。** 服务端按 `(team_id, normalized_name)` 查找或创建员工，`normalized_name` = `trim → 删除空格 → 转小写`。所以 `Eric team` 和 `ericteam` 是同一个人，而 `Eric-team` 不是。让用户确认这里的名字和后台看到的员工名一致。

### 修复

重新安装并显式传全四个变量（不要只补 `USER_NAME`）：

```bash
curl -fsSL <安装脚本地址> | TRACKER_URL=http://<服务端>:<端口> \
  TEAM_ID=<团队ID> TEAM_KEY=<团队密钥> USER_NAME="<员工姓名>" bash
```

Windows PowerShell 用 `$env:TRACKER_URL="..."` 形式预先设置，再跑 `install.ps1`。

手工修也不禁止，但四个字段必须齐全，`team_id` 必须是数字字符串：

```json
{
  "tracker_url": "http://<服务端>:<端口>",
  "team_id": "38",
  "team_key": "<团队密钥>",
  "username": "<员工姓名>",
  "blacklist": []
}
```

改完**必须重新验证**（回到第 5 步，用真实会话 / 真实 push 验证，不要用 curl 探针）。

## 第 2 步：Hook 与二进制层

这一层是"配置对了但数据还是没上来"的头号原因。每个平台的 hook 是**安装时**由客户端写进各平台配置文件的，记录的是**绝对路径**。路径变了（重装到别的目录、切换 debug 版和正式版、二进制被挪走），hook 就会指向一个不存在或错误的二进制，**失败是静默的**。

### 各平台 hook 应该长什么样

**Claude Code** —— `~/.claude/settings.json` 的 `hooks.Stop`，以及（装了 ECC 插件时）`~/.claude/hooks/hooks.json`：

```json
{"type": "command", "command": "<二进制绝对路径> report-token-usage claude-code",
 "description": "Report AI session token usage to tracker", "id": "stop:report-token-usage"}
```

**Codex** —— `~/.codex/hooks.json` 的 `hooks.Stop`：`<二进制绝对路径> report-token-usage codex`。注意 Codex 的 Stop hook 里应该**只有** report-token-usage，PreToolUse/PostToolUse 才是 `checkpoint codex --hook-input stdin`。

**OpenCode** —— `~/.config/opencode/plugins/git-ai.ts`，脚本顶部有 `const GIT_AI_BIN = "<二进制绝对路径>"`，`session.idle` 事件触发上报。这个文件是生成物，**不要手工编辑**，重装会覆盖。

### 判定命令

```bash
# Claude Code：确认 Stop 里有 report-token-usage，且路径真实存在
python3 -c "
import json,os
p=os.path.expanduser('~/.claude/settings.json')
h=json.load(open(p)).get('hooks',{}).get('Stop',[])
for b in h:
    for k in b.get('hooks',[]):
        c=k.get('command','')
        if 'report-token-usage' in c:
            exe=c.split()[0]
            print('OK ' if os.path.exists(exe) else 'MISSING ', c)
"

# Codex
python3 -c "
import json,os
p=os.path.expanduser('~/.codex/hooks.json')
h=json.load(open(p)).get('hooks',{}).get('Stop',[])
for b in h:
    for k in b.get('hooks',[]):
        c=k.get('command','')
        if 'report-token-usage' in c:
            exe=c.split()[0]
            print('OK ' if os.path.exists(exe) else 'MISSING ', c)
"

# OpenCode
grep -n 'GIT_AI_BIN' ~/.config/opencode/plugins/git-ai.ts
```

### 修复

**统一走重装，不要手工改 hook。** 客户端的安装流程会自己清理重复项和过期路径：

```bash
easylife-ai install-hooks          # 重新注入全部平台 hook
```

装完再跑一次上面的判定命令确认。如果同机存在多个客户端二进制（例如 `~/.easylife-ai/bin/easylife-ai` 和 `~/.git-ai/bin/git-ai` 并存），**先问清楚哪个是要用的那个**，把 hook 统一到同一个——两套 hook 同时存在会导致重复上报和状态文件互相打架。

### 代码上报特有的二进制问题

`git push` 的上报依赖用户实际调用的 `git` 就是客户端包装过的 git。验证：

```bash
command -v git          # 应该指向客户端的 git 包装器，不是 /usr/bin/git
git --version           # 应能正常返回
```

如果用户用 IDE 内置 Git、或某个绝对路径的 git 来推送，pre-push 那一步根本不会经过客户端，自然没有上报。这种情况不是 bug，要告诉用户改用命令行推送，或让 IDE 指向包装后的 git。

## 第 3 步：网络与服务端层

### 网络

- `tracker_url` 必须是客户端能直连的地址。**不要写 `localhost`**，除非服务端和客户端同机。
- 很多公司内网需要走代理。客户端的 HTTP 客户端（ureq，编译时未启用 `proxy-from-env`）**不读 `HTTP_PROXY`/`HTTPS_PROXY` 环境变量**，所以"我浏览器能打开"不能证明客户端能通，设了 `export HTTPS_PROXY=...` 也不会生效。只能用第 0 步的 `nc` 在**同一台机器、同一个网络**上验证 TCP 可达性；如果确实需要代理，要让网络层（透明代理 / 路由）解决，而不是靠环境变量。
- 注意协议和端口：`tracker_url` 写 `http://192.168.x.x:39527`，最终请求是 `http://192.168.x.x:39527/ai-code-boost/open/report/token/usage`。少写端口或写成 https 都会失败。

### 服务端返回码对照

**这个服务端的 HTTP 码区分度很低**：任何未捕获异常都被兜底成 `HTTP 500` + `{"code":500,"msg":"服务器内部错误","data":null}`。实测以下情况**全部返回这个 500**：

- `team_key` 错误 / 缺失
- 请求体字段缺失（例如少了 `username`）
- 用 GET/HEAD 请求这两个 POST 接口
- 端口通但打到了错误的路径

所以**不要靠 HTTP 码判断根因**。正确的判读顺序是：

| 现象 | 含义 | 处理 |
|---|---|---|
| TCP 连不上 | 地址错、端口错、防火墙、服务端没起 | 确认地址端口；服务端侧查服务存活 |
| 500 + `code:500` + 空 body/错 key 也复现 | 服务端兜底异常，无法细分 | 用完整且正确的请求体复现，对照 `username`/`team_id`/`team_key` |
| 500 + 完整正确请求体 | 服务端校验仍然拒绝 | 抓服务端日志定位；大概率是必填字段或授权问题 |
| 200 + `data.success: true` | 真成功 | — |
| 200 + `data.inserted: 0` | 接受了但没入库 | 授权失效；见第 4 步 |

代码上报成功时响应体形如：

```json
{"code":200,"msg":"操作成功","data":{"success":true,"total":1,"inserted":1,
 "duplicated":0,"failed":0,"results":[{"commitSha":"...","recordId":608,"status":"inserted"}]}}
```

**判成功的依据是 `data.inserted`，不是 HTTP 200。**

### HTTP 500 的经典根因

服务端把顶层 `pusher_email` 换成了 `username`（带 `@NotBlank` 强制校验），客户端如果不发 `username`，Spring 校验失败被兜底 catch 成 500。**老版本客户端有这个问题**，先确认 `easylife-ai --version` 再考虑升级。

排障时**不要**为了验证而手工 POST 到这两个接口——`tracker-config.json` 里的 key 是真实可用的，手工发的数据会被服务端当真入库。要验证就用真实客户端跑真实流程，看第 5 步。

## 第 4 步："上报成功但数据没入库"

这是最容易被误判的一类。**HTTP 200 不等于入库。**

### 代码上报

```bash
easylife-ai tracker log -n 50     # 看状态列
```

日志格式：`时间 \t 状态 \t sha \t remote/branch \t 仓库路径 \t [原因]`，状态有 `uploaded` / `skipped` / `failed` / `retry_ok`。常见 `skipped` 原因（**这些是设计行为，不是故障**）：

| 原因 | 含义 |
|---|---|
| `already_reported` | 已上报过（git notes `refs/notes/ai-tracker` 里有记录） |
| `blacklisted` | 命中 `tracker-config.json` 的 `blacklist` |
| `merge_commit` | 合并提交不上报 |
| `synthetic_message` | 提交信息以 `merge `/`revert `/`cherry-pick `/`rebase ` 开头，或含 `cherry picked from commit` |
| `copy_paste_threshold` | 手工新增行数超过阈值（当前 1000000） |

另外要检查服务端响应体里的 `inserted` / `duplicated` / `failed`，不能只看 200。授权失效时服务端会返回 200 但 `inserted=0`。

**关于 `diff_gz`：** 只收集 `.rs .py .js .ts .tsx .jsx .go .java .c .cpp .h .hpp .rb .php .swift .kt .scala .sh .sql .css .html .vue .svelte` 这些后缀的文件，超过 2MB 会被截断。**如果一次提交只改了 `.md`、`.json`、`.yml` 之类，diff 为空，`diff_gz` 就是 `null`，服务端的代码审核不会有内容——但 commit 统计仍然会上报。** 用户说"代码没上去"时要先分清他指的是统计还是 diff。

### 补报失败的历史提交

失败会进重试队列 `~/.easylife-ai/tracker-retry-queue.json`，最多重试 3 次，超过就从队列里丢弃。手动触发重试：

```bash
easylife-ai tracker retry
```

如果队列文件很大（几百 KB 甚至 MB 级，正常只应该有几条），说明长期失败积压，先解决根因（通常是网络或 team_key）再 retry，否则只是再失败一轮。

### Token 上报

Token 上报**没有人类可读的日志文件**——`tracker-upload.log` 只记代码上报。它的结果只走 debug 输出。所以排查 token 问题时：

```bash
GIT_AI_DEBUG=1 <二进制绝对路径> report-token-usage claude-code
```

注意 `report-token-usage` 接受 `<platform> [--session-id <id>]`，平台是 `claude-code`、`codex`、`opencode`（OpenCode 需要 `--session-id`）。输出形如 `[git-ai token-report] claude-code reported: 3 turns (up to turn 12)` 表示成功。**没有任何输出通常意味着"没有新的轮次可报"**，而不是失败——见下一条。

数据源位置（用户说"没数据"时按平台去这里确认源数据本身存在）：

| 平台 | 数据源 |
|---|---|
| Claude Code | `~/.claude/projects/<项目>/<session>.jsonl`（也认 `~/.config/claude/projects/`） |
| Codex | `~/.codex/sessions/**/rollout-*.jsonl`，按文件名时间戳取最新 |
| OpenCode | OpenCode 的 SQLite 库（`opencode.db`），表结构由 OpenCode 管理 |

增量状态在 `~/.easylife-ai/reported-turns.json`，键是 JSONL 路径（OpenCode 是 `opencode:<session_id>`），值是最后成功上报的轮次序号。**每次只上报序号大于该值的轮次。**

由此产生三条必须向用户讲清楚的结论：

1. **"重启一下重新上报"不会重传旧数据。** 已经成功上报的轮次不会因为客户端升级、重启、解析器修复而自动补全。
2. **绝对不要为了"补数据"清空 `reported-turns.json`。** 清空会让所有轮次重新上报，服务端按 `(session_id, turn_index)` UPSERT，编号变了就会产生重复记录。文档明确写了"不得直接清空状态文件来尝试补数据"。
3. **同一 `session_id` + `turn_index` 且四个 token 值都没变化时，服务端会跳过并返回"数据未增长，已跳过"。** 这是去重，不是故障。

验证 token 链路能否工作，正确做法是**让用户开一个新会话跑一轮**，然后：

```bash
GIT_AI_DEBUG=1 <二进制绝对路径> report-token-usage claude-code 2>&1 | tail -20
cat ~/.easylife-ai/reported-turns.json
```

`reported-turns.json` 里对应 JSONL 那条的值增长 = 链路通。

Codex 有个编号模式的坑：首次成功上报时会把 `codex-index-mode:<jsonl路径>` 写进同一个状态文件（`1` 连续编号 / `2` 旧编号），后续不再切换。如果用户在编号修复的过渡期上报过，可能出现轮次号与服务端既有记录对不上，**这种需要服务端侧人工核对，客户端不要动**。

## 第 5 步：修复后必须验证

收尾时**必须**给出可复现的验证，不能只说"改好了"：

**最省事的验证：改完直接让用户重跑 `easylife-ai doctor`。** 它会自己走完两条链路的端到端实测并给出 `recordId`，比下面这些手工步骤更彻底。只有 doctor 无法覆盖的场景（比如要验证的就是用户真实仓库/真实会话，而不是 doctor 的临时仓库）才用下面 1–6。

1. 配置改了 → 文件四个字段齐全非空，且 `team_id` 能解析成数字。
2. hook 改了 → 重跑第 2 步的判定命令，期望全部 `OK`。
3. 网络改了 → `nc -z` 通。
4. token 链路 → **让用户开一个新会话跑一轮**，然后 `GIT_AI_DEBUG=1 <二进制绝对路径> report-token-usage claude-code 2>&1 | tail -20` 有 `reported: N turns` 输出，且 `reported-turns.json` 里对应条目数值增长。
5. 代码链路 → `git push` 一个小提交（**提交里要含一个代码文件，不能只改 `.md`/`.json`**，否则 `diff_gz` 为空），`easylife-ai tracker log -n 5` 出现 `uploaded`（不是 `failed`）。
6. 最终以**服务端后台看到数据**为准，本地日志只是中间证据。

**验证用的数据会成为真实记录。** 如果为了验证造了测试提交或测试会话，验证完要提醒用户：这些会算进他的统计里，需要的话得在服务端侧清理，客户端无法撤回。

## 升级与兜底

以上都排查完还是不行，或者怀疑是客户端本身的 bug：

```bash
easylife-ai upgrade                 # 升级到最新
easylife-ai backup                  # 升级/重装前先备份（配置和历史都在里面）
```

升级不会重传旧数据（见第 4 步第 1 条），要向用户说清楚这个预期，避免"升完还是没数据"被当成没修好。

如果最终定位到是客户端代码问题（例如某个平台解析器读不到新的会话格式），**收集这些再提 issue**：`easylife-ai --version`、操作系统、平台版本、`GIT_AI_DEBUG=1` 的完整输出、`reported-turns.json` 里相关的那一行、以及一段脱敏后的源数据文件头部。**不要直接上传用户的完整会话记录**，里面可能有代码和密钥。

## 你应该输出什么

用户要的是"能照着做完"的结论，不是排查过程流水账。按这个结构给：

1. **结论**：哪条链路断了、断在哪一层（配置 / hook / 网络 / 服务端）。
2. **证据**：具体哪个命令的哪行输出支持这个结论。
3. **修复步骤**：可以直接复制的命令；需要用户提供信息的（如 `team_key`）明确标出来。
4. **验证方法**：改完怎么确认好了，见第 5 步。
5. **不确定的地方**：说清楚哪些结论是推测、还需要什么信息。**不要为了显得确定而编造服务端行为或字段名。**

如果排查到一半发现信息不够（例如用户不知道 `team_key`、或者服务端日志拿不到），直接说要什么，不要空转。

## 禁止事项

- 不要清空 `reported-turns.json`、`tracker-retry-queue.json` 或 `refs/notes/ai-tracker` 来"重新开始"——会造成重复入库或漏报。
- 不要把 `tracker-config.json` 的 `team_key` 贴到公开渠道。
- 不要手工编辑 `~/.config/opencode/plugins/git-ai.ts`（生成物，重装即覆盖）。
- 不要只看 HTTP 200 就判定"上报成功"（要看响应体 `data.inserted` / `data.success`）。
- 不要用 POST 探针测接口可用性——`tracker-config.json` 里的 key 是真 key，探针数据会真的入库。
- 不要在没定位到链路层之前就重装——重装会掩盖证据，让问题下次复现时更难查。
- **不要把 doctor 诊断里的 `⊘`（跳过）当成通过。** 跳过 = 这一项根本没测，用它下结论会得出错误的"都正常"。
- **不要让用户反复重跑 `easylife-ai doctor` 来"再确认一下"。** 每次运行都会往服务端写入 1 条代码记录 + 3 条 token 记录；诊断够用就停，别把它当无副作用的探针。
- 不要因为 doctor 在服务端留下了测试记录就建议用户"清库"——那是正常验证数据，客户端也撤不回。
