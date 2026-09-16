# 子代理模型映射 / Subagent model mapping

在运行 cursor-byok 服务的用户目录创建 `~/.cursor-byok-v3/subagent-models.yaml`。桌面应用和独立服务读取相同文件。配置只影响子代理，不改变主代理选模。

Create `~/.cursor-byok-v3/subagent-models.yaml` in the home directory of the user running cursor-byok. Desktop and standalone server use the same file. Only subagents are affected.

## 配置 / Configuration

```yaml
# 所有未命中 type 或 mapping 的子代理使用该目标。
# Replace these example IDs with real configured model IDs.
fallback: { model: "plugin:your-plugin/your-provider/your-model", effort: high }

types:
  explore: { model: "plugin:your-plugin/your-provider/fast-model", effort: high }
  generalPurpose: { model: "composer-2.5" }
  my-reviewer: { model: "plugin:your-plugin/your-provider/review-model", effort: medium }

mapping:
  composer-2.5: { model: "another-official-model-id", effort: high }
  cursor-grok-4.5-high: { model: "0123456789abcdef", effort: high }
```

每个目标是对象：必填 `model`；非 Composer 目标必填 `effort`，Composer 目标（`composer-*`）禁止写 `effort`。字符串形式的旧目标不再接受。三个字段都可以省略。优先级固定为 `types[type] > mapping[请求模型] > fallback > 原请求模型`。命中规则后取整个目标对象，不会从低优先级规则合并 effort。因此，配置了 type 或 fallback 时，显式选中的模型也可能被覆盖。`fallback` 不是请求失败后的重试。

Each target is an object with required `model`. Non-Composer targets require `effort`; Composer targets (`composer-*`) must omit `effort`. Legacy string targets are rejected. All fields are optional. Priority is `types[type] > mapping[requested model] > fallback > original requested model`. A hit takes the whole target object; effort is not merged from lower-priority rules. A type rule or fallback can override an explicit selection. Fallback is a default, not error recovery.

映射只执行一次：同时存在 `A: B` 和 `B: C` 时，请求 A 使用 B。自定义 type 使用子代理的原始名称，大小写敏感。YAML 是唯一映射来源；Cursor 界面传来的旧模型 override 不再用来补救选模，禁用子代理仍然生效。

Mappings are single-pass: with both `A: B` and `B: C`, requesting A selects B. Type names are exact and case-sensitive, including custom subagent names. YAML is the sole mapping policy; legacy UI model override repair is removed, while disabled subagents remain disabled.

## Effort

- 非 Composer 的 YAML 目标必须写 `effort`；Composer 目标必须省略 `effort`。启动/热更新时校验；无效配置保留上一份有效快照。
- 是否要求 effort 看**最终目标**模型：例如 `composer-2.5 -> official-B` 必须带 effort；`official-A -> composer-2.5` 禁止 effort。
- 显式 `effort` 覆盖请求中的 `effort` / `reasoning` 别名，并阻止本地已保存默认 effort 回填。
- Composer 命中：清除冲突的 `effort`/`reasoning`，且不注入请求里的 effort。
- 未命中任何规则（也无 fallback）：原样透传请求字节与参数，不做 effort 强制或默认注入。
- 同模型仅改 effort 也会改写官方 protobuf 请求体。
- YAML **不允许** `model: inherit`（types / mapping / fallback 目标均拒绝）。Task.model=`inherit` 仍表示父模型身份，先规范化为父模型，再应用 YAML。
- 当前拒绝空字符串和 `none`。不对供应商做统一能力白名单；真实是否接受由上游决定。不存在 default-high 注入。

Non-Composer YAML targets require `effort`; Composer targets must omit it. Validation runs at startup/reload; invalid reloads keep the last valid snapshot. Effort rules apply to the **final target** model. Explicit effort overrides request aliases and blocks saved local defaults. Composer hits clear conflicting `effort`/`reasoning` and do not inject request effort. Unmatched requests (no type/mapping/fallback) pass through unchanged—no mandatory effort and no defaulting. Same-model effort-only changes still rewrite the official protobuf body. YAML targets must not use `model: inherit`; Task.model `inherit` still resolves to the parent model before YAML applies. Empty effort and `none` are rejected. There is no speculative capability whitelist and no default-high injection.

## 模型 ID / Model IDs

- **官方模型 / Official:** 使用真实官方 model ID，不是显示名称。目标仍受 Cursor 账号权限及上游可用性限制。
- **Composer:** 以 `composer-` 前缀识别（如 `composer-2.5`）。
- **普通 BYOK 模型 / Configured BYOK:** 使用 `model_hash`，不是 provider 的原始 `model_id`。可以从应用模型列表接口 `GET /__byok-api__/api/models` 的响应中读取 `model_hash`，或从对应本地运行的诊断记录读取模型 ID。
- **插件模型 / Plugin:** 使用完整 `plugin:<plugin-id>/<provider-id>/<upstream-model-id>`。可以从实际本地请求或运行诊断的模型 ID 复制，不要使用显示名称。示例中的 plugin 和 hash 均为占位符。

配置文件不需要 API key。凭据继续由既有模型配置或插件管理。

This file contains model references only, never API keys. Existing provider/plugin configuration owns credentials.

## 怎样调用 / Calling subagents

**官方主代理：**要求它选择 Cursor 的 Task 工具允许的官方模型名，再用 `mapping` 转到实际目标。例如，把 `composer-2.5` 映射为插件模型后，要求主代理“使用 composer-2.5 子代理完成这个任务”。如果该 type 有规则，则 type 规则优先。YAML 不会改变官方主代理由 Cursor 提供的模型名单，也不能令它直接认识自定义模型名。

**Official parent:** request an official model name already accepted by Cursor's Task tool, then map it to your actual target. YAML does not change Cursor's official tool description or grant access to unsupported official models.

**自己的主代理：**正常上下文中会列出 YAML 引用的模型和 mapping 入口名。可以要求它在 Task 的 `model` 参数中填写完整模型 ID，或使用 `inherit`。`inherit` 先规范化为父模型，再在子代理启动时应用 YAML；它不绕过 type、mapping 或 fallback。YAML 目标不能写 `inherit`。列表只包含配置引用，并不是所有已安装模型的目录。

**BYOK parent:** the request context lists IDs referenced by YAML and mapping keys. Task may select an exact listed ID or `inherit`. Inherit resolves to the parent model before the child applies YAML policy; it does not bypass the policy. YAML configured targets cannot be `inherit`.

## 热更新 / Hot reload

- 启动时文件不存在：使用空策略；文件存在但无效：明确报错。
- 保存有效配置后无需重启，轮询检测并校验后原子替换当前策略。
- 新启动的子代理使用最新有效策略；已经启动的同一子代理不切换模型或 effort（生命周期内钉住选中的 model+effort）。
- 无效 YAML、读取错误、临时删除文件：保留上一份有效策略并记录错误。建议编辑器使用原子保存；清空策略请写入 `{}`，不要删除文件或留空。
- 本地主代理在下一次正常上下文编译时看到更新的模型列表，不中断正在进行的生成。工具定义不因更新改变，旧历史不会被改写。
- 路由诊断区分策略候选值与接纳后的实际选择：策略日志记录 type、命中规则、配置版本和 `candidate_*`；接纳日志记录 `selected_model`、`selected_effort_action` 及是否与候选值不同。effort 操作中 `Set("high")` 表示强制 high，`Clear` 表示清除 effort，`Unchanged` 表示保留请求参数、并非关闭 effort。热更新后的旧生命周期可能继续使用已锁定值，应以接纳日志为准。

Valid changes reload without restarting. Invalid/deleted files retain the last valid policy. Write `{}` to clear it. New subagents use the latest policy; already-started subagents keep their selected model and effort for the lifecycle. BYOK parents see updated model context on their next normal context compilation, without rewriting earlier history or changing tool schemas. Policy logs show candidates; admission logs show the selected model and effort action. `Set` overrides effort, `Clear` removes it, and `Unchanged` preserves request parameters. When a lifecycle retains an earlier selection after reload, use the admission log rather than the candidate log.
