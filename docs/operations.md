# 运行手册

## 状态与日志

```sh
./start.sh status
./start.sh logs
tail -f .runtime/bridge.log
```

| 状态 | 含义与动作 |
| --- | --- |
| 服务未运行 | 执行 `./start.sh start`。 |
| 桥接正在启动 | 等待数秒。 |
| 已初始化，等待飞书连接 | 检查网络、长连接配置和代理。 |
| 已连接飞书 | 可发送 `/status` 验证业务侧。 |
| 异常退出，等待自动重启 | 查看日志；不要反复手工启动。 |

常见日志：`message_received` 表示新消息，`message_ignored` 多为 `duplicate`/`unauthorized`，`user_paired` 表示用户已通过配对码加入本机白名单，`command_received` 与 `card_action_received` 表示命令/按钮已进入处理器，`worker_error` 表示任务失败，`bridge_crash restart_in=N` 表示退避重启。日志不记录正文，但可能含路径和异常类型；对外求助时仅分享必要片段。

## 停止与重启

```sh
./start.sh stop
./start.sh restart
```

不要使用宽泛的 `pkill python` 或 `killall node`。服务会验证 PID 归属，且项目锁与 App ID 全局锁能防止同设备重复运行。提示同一机器人已有其他实例时，应从旧项目执行其 `start.sh stop`，不要直接删除锁文件。

## 故障处置

| 症状 | 优先检查 |
| --- | --- |
| 一直“开始处理” | 用 `/stop` 中断；检查审批卡和 `worker_error`。 |
| 没有交付物 | 工程文本只发差异；仅已知成果格式自动上传，需是本轮改动且不在缓存/编译目录，附件总数和大小有限制。 |
| 重启后上下文丢失 | 检查 `.feishu-codex-session` 是否存在且 thread 仍可恢复。 |
| 审批缺少细节 | 拒绝或要求 Codex 解释，不要盲批。 |
| 重复回复 | `ps -ef | rg 'service.py|bridge.py'`，然后比对日志消息短哈希；若本机一次、飞书多次，排查其他设备实例。 |

bridge 不会自动重跑失败任务，因为任务可能已经改文件或运行过命令。若怀疑 App Secret 泄露，应在飞书后台轮换密钥，更新 `.env` 后重启服务。

### 飞书专用代理

默认使用启动服务时继承的 Linux 代理环境变量。HTTPS 接口和安全 WebSocket 可通过 `https_proxy` / `HTTPS_PROXY` 配置，并遵循 `no_proxy` / `NO_PROXY`；没有代理变量时直连。WebSocket 还支持 `wss_proxy`。需要 `websockets>=15` 及带 `_ws_connect_kwargs` 的 lark-oapi。

更换代理环境后，在新环境的终端执行 `./start.sh restart` 即可，无需修改项目配置。运行中的进程不会自动获得终端后来修改的环境变量；桌面设置中的代理也需要由启动环境导出。若 `.env` 自己设置了代理变量，应删除固定值，以免覆盖终端环境。

`FEISHU_PROXY_URL` 仅用于显式覆盖，例如 `http://127.0.0.1:7890`；通常无需配置。依赖清单已包含 HTTP 与 SOCKS 代理支持；升级时执行 `python -m pip install -r requirements.txt`。

## 对话归档与差异验收

Rust 开发版归档恢复整批待验证：若日志报告归档核对失败，先检查 Codex 连接、协议兼容和状态目录写入问题；`archive_reconciled` 只记录清除的绑定数量，不记录会话内容。核对成功后才连接飞书，核对失败会关闭本次 Codex 进程。远端操作结果不确定时不要直接重发归档或任务；排除存储/连接故障后启动，让归档核对清理确认已归档的绑定。若超过 100 页或 60 秒，需先排查会话规模或响应速度，不应绕过核对删除本地状态。

部署代码后使用 `./start.sh restart` 加载，再用 `./start.sh status` 检查连接。归档保留历史记录；`/archived` 或普通列表底部“已归档”可进入归档列表，再点“取消归档”。当前列表只显示最近 8 个，已知完整 ID 可直接用命令操作。

真实飞书验收需人工在手机完成：

1. 发送 `/resume`，检查同名对话的完整 ID 和当前对话标记；归档一个空闲测试对话，确认从普通列表消失。
2. 从“已归档”取消归档，再恢复；重复点旧按钮、切换目录后点旧按钮应被拒绝。
3. 归档当前测试对话，下一次提问应创建不同 ID；运行中及排队时归档应提示等待。
4. 在测试目录修改 `.py`、Markdown、JSON 并运行编译检查，同时生成图片/PDF。确认文本只发差异，`.pyc` 完全不出现，成果正常上传；检查新增、删除、长差异及超长单行在手机上的显示。
5. 保存不含凭据或私有内容的卡片截图作为验收记录。离线测试和 schema 核对不能替代这一步。

## Rust 迁移工具运行边界

Rust 已提供原生运行和两层服务监督。使用 `bash start-rust.sh start|stop|restart|status`（选其中一个操作）；前台监督使用 `bash start-rust.sh foreground`。配置、凭据、打包及恢复边界见 [Rust 部署指南](rust-deployment.md)，新增脚本待统一验证。导入/导出演练请使用合成数据或停止服务后的状态备份，遵循 [迁移指南](migration.md)。不要同时启动新旧版本连接同一 App ID。
