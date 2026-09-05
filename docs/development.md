# 开发指南

## 本地工作流

```sh
cp .env.example .env
chmod 600 .env
python -m unittest discover -s tests -v
python -m py_compile bridge.py service.py
./setup.sh --check
./start.sh foreground
```

日常运行请使用 `start.sh`，不要长期直接运行 `python bridge.py`；后者会绕过服务锁、全局 App 锁、日志轮转和异常重启。

| 文件 | 修改它的典型场景 |
| --- | --- |
| `bridge.py` | 新命令、卡片、app-server 事件、附件/交付物。 |
| `service.py` | 生命周期、锁、健康状态、日志和重启策略。 |
| `start.sh` | 环境加载或服务入口参数。 |
| `setup.sh` | 首次部署引导；不得覆盖已有 `.env`。 |
| `tests/` | 为每个无网络可验证行为增加回归。 |
| `docs/` | 用户行为、架构边界或部署步骤变更时同步更新。 |

## 增加命令或卡片

1. 在 `Bridge.command()` 添加 `/your-command` 分支。
2. 耗时或 RPC 操作必须捕获异常，用 `card_or_text()` 反馈；不要让异常逃出飞书回调线程。
3. 要出现在控制面板时，在 `help_buttons()` 添加短标签和回传 `value`；状态型开关必须使用显式目标状态，并通过 `update_help_card()` 更新来源卡片。
4. 有参数的按钮，在 `on_card_action()` 转换为同一文本命令参数。
5. 更新 README 命令表、添加测试，并做真实飞书按钮验收。

```python
elif command == "/example":
    try:
        result = self.server.request("some/method", {"key": key})
        self.feishu.card_or_text(chat_id, "示例", format_result(result), "green")
    except Exception as exc:
        self.feishu.card_or_text(chat_id, "示例失败", str(exc), "red")
```

不要把用户输入直接拼入 shell 命令。需要文件或 shell 操作时，让 Codex 走既有审批协议。

需要用户及时操作的卡片必须显示超时后果，并在可更新的原卡上反映超时终态；当前审批、Codex 选择题和 Plan 后续操作默认均为 600 秒，可分别通过 `CODEX_APPROVAL_TIMEOUT_SECONDS`、`CODEX_QUESTION_TIMEOUT_SECONDS` 和 `CODEX_PLAN_ACTION_TIMEOUT_SECONDS` 配置。

`Feishu.make_card()` 统一生成 Card JSON 2.0。按钮使用 `behaviors: [{"type": "callback", "value": ...}]`，不要使用旧版 `action` 容器。长说明使用垂直满宽布局；短控制项可用两列。

## 修改 app-server 适配层

`CodexServer` 是唯一可触碰 JSON-RPC stdio 的层：请求-响应使用 `request()`，通知/审批响应使用 `send()`。新增通知在 `handle_event()` 识别后经 `self.event(kind, value)` 交给 `Bridge.codex_event()`；不得直接读 stdout，也不能误唤醒其他 thread 的 completion。

先在真实 CLI 版本验证 JSON-RPC schema，不能仅依赖旧日志或记忆。Plan 模式和 `item/tool/requestUserInput` 当前属于实验性协议：服务端请求必须用原 JSON-RPC ID 回复，选择题答案格式为题目 ID 到标签数组的映射；不能从其他线程读取 stdout。

## 测试与验收

`tests/test_bridge.py` 使用 fake outbox 和 `Bridge.__new__`，不启动真实 app-server；`tests/test_service.py` 使用临时目录。测试不得读取真实 `.env`、访问网络、启动长期进程或修改用户工作目录。

提交前：

- [ ] `python -m unittest discover -s tests -v`
- [ ] `python -m py_compile bridge.py service.py`
- [ ] `git diff --check`
- [ ] 未提交 `.env`、日志、会话、附件或 token
- [ ] README、docs、PLAN 已同步
- [ ] 修改飞书或 Codex 接口后完成真实飞书验收
- [ ] 修改 Plan 或交付物展示后，真实点击控制面板开关，并确认 Plan Markdown 与 diff 卡片显示。
