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
| 没有交付物 | 文件必须在工作目录、不超过限制、不在排除目录且是本轮改动。 |
| 重启后上下文丢失 | 检查 `.feishu-codex-session` 是否存在且 thread 仍可恢复。 |
| 审批缺少细节 | 拒绝或要求 Codex 解释，不要盲批。 |
| 重复回复 | `ps -ef | rg 'service.py|bridge.py'`，然后比对日志消息短哈希；若本机一次、飞书多次，排查其他设备实例。 |

bridge 不会自动重跑失败任务，因为任务可能已经改文件或运行过命令。若怀疑 App Secret 泄露，应在飞书后台轮换密钥，更新 `.env` 后重启服务。
