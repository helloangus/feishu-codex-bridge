"""Termux service supervisor with flock ownership and rotating sanitized logs."""
import fcntl
import hashlib
import json
import logging
from logging.handlers import RotatingFileHandler
import os
from pathlib import Path
import re
import signal
import subprocess
import sys
import threading
import time

BASE = Path(__file__).resolve().parent
STATE = BASE / '.runtime'
LOCK = STATE / 'service.lock'
HEALTH = STATE / 'health.json'
APP_LOCK_DIR = Path(os.environ.get('CODEX_SERVICE_GLOBAL_STATE', str(Path.home() / '.feishu-codex-bridge')))
APP_LOCK = APP_LOCK_DIR / (hashlib.sha256(os.environ.get('FEISHU_APP_ID', 'unknown').encode()).hexdigest()[:24] + '.lock')


def redact(text):
    text = re.sub(r'wss?://\S+', '[WebSocket endpoint]', text)
    text = re.sub(r'(?i)bearer\s+[A-Za-z0-9._-]+', 'Bearer [redacted]', text)
    text = re.sub(r'\bt-[A-Za-z0-9_-]{12,}\b', '[redacted]', text)
    for name in ('FEISHU_APP_SECRET', 'FEISHU_APP_ID'):
        secret = os.environ.get(name)
        if secret:
            text = text.replace(secret, '[redacted]')
    return re.sub(r'(?i)((?:access_token|access_key|ticket|authorization|app_secret)[\"\s:=]+)[^\s,\"}]+', r'\1[redacted]', text)


def running():
    with LOCK.open('a+') as stream:
        try:
            fcntl.flock(stream, fcntl.LOCK_EX | fcntl.LOCK_NB)
            return False
        except BlockingIOError:
            return True


def write_health(phase, **fields):
    """Persist a small, credential-free status record for `status`."""
    payload = {'phase': phase, 'updated_at': time.time(), **fields}
    temporary = HEALTH.with_suffix('.tmp')
    temporary.write_text(json.dumps(payload, ensure_ascii=False), encoding='utf-8')
    os.replace(temporary, HEALTH)


def read_health():
    try:
        return json.loads(HEALTH.read_text(encoding='utf-8'))
    except (OSError, ValueError, TypeError):
        return {}


def age_text(timestamp):
    seconds = max(0, int(time.time() - float(timestamp)))
    if seconds < 60:
        return f'{seconds} 秒前'
    if seconds < 3600:
        return f'{seconds // 60} 分 {seconds % 60} 秒前'
    return f'{seconds // 3600} 小时 {(seconds % 3600) // 60} 分前'


def stop():
    if not running():
        print('服务未运行')
        return
    pid = int(LOCK.read_text().strip())
    args = Path(f'/proc/{pid}/cmdline').read_bytes().split(b'\0')
    if str(Path(__file__).resolve()).encode() not in args or b'run' not in args:
        raise RuntimeError('进程归属不匹配，拒绝停止')
    os.kill(pid, signal.SIGTERM)
    for _ in range(100):
        if not running():
            print('服务已停止')
            return
        time.sleep(.1)
    raise RuntimeError('停止超时；请检查日志')


def run(foreground=False):
    APP_LOCK_DIR.mkdir(mode=0o700, parents=True, exist_ok=True)
    app_stream = APP_LOCK.open('a+')
    try:
        fcntl.flock(app_stream, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        app_stream.close()
        print('同一个飞书机器人已有其他项目实例运行')
        return
    try:
        return _run_local(foreground)
    finally:
        fcntl.flock(app_stream, fcntl.LOCK_UN)
        app_stream.close()


def _run_local(foreground=False):
    with LOCK.open('a+') as stream:
        try:
            fcntl.flock(stream, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            print('服务已运行')
            return
        stream.seek(0)
        stream.truncate()
        stream.write(str(os.getpid()))
        stream.flush()
        logger = logging.getLogger('service')
        logger.setLevel(logging.INFO)
        handler = RotatingFileHandler(STATE / 'bridge.log', maxBytes=2*1024*1024, backupCount=3, encoding='utf-8')
        handler.setFormatter(logging.Formatter('%(asctime)s %(message)s'))
        logger.addHandler(handler)
        stopping = False
        child = None
        stop_event = threading.Event()

        def shutdown(*_):
            nonlocal stopping, child
            stopping = True
            stop_event.set()
            if child is None:
                return
            try:
                os.killpg(child.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
        signal.signal(signal.SIGTERM, shutdown)
        signal.signal(signal.SIGINT, shutdown)
        try:
            restart_delay = 2
            while not stopping:
                child = subprocess.Popen([sys.executable, '-u', str(BASE / 'bridge.py')],
                                         stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                                         text=True, start_new_session=True)
                write_health('starting', pid=child.pid)
                logger.info('event=bridge_started pid=%s', child.pid)
                for line in child.stdout:
                    safe = redact(line.rstrip())
                    logger.info(safe)
                    if '"event":"bridge_ready"' in line:
                        write_health('ready', pid=child.pid)
                    elif '[Lark]' in line and ' connected to ' in line:
                        write_health('connected', pid=child.pid)
                    if foreground:
                        print(safe, flush=True)
                code = child.wait()
                logger.info('event=bridge_exit code=%s requested=%s', code, stopping)
                write_health('stopped' if stopping or code == 0 else 'restarting', pid=child.pid, exit_code=code)
                child = None
                if stopping or code == 0:
                    break
                logger.info('event=bridge_crash restart_in=%s', restart_delay)
                # A plain sleep makes `restart` wait for the full backoff
                # interval after a crash. Wake immediately when SIGTERM asks
                # the supervisor to stop.
                stop_event.wait(restart_delay)
                restart_delay = min(restart_delay * 2, 30)
        finally:
            shutdown()
            handler.close()


def main():
    os.umask(0o077)
    STATE.mkdir(mode=0o700, exist_ok=True)
    action = sys.argv[1] if len(sys.argv)>1 else 'start'
    if action == 'run':
        run('--foreground' in sys.argv)
    elif action == 'status':
        active = running()
        print('服务运行中' if active else '服务未运行')
        health = read_health()
        if health:
            labels = {
                'starting': '桥接正在启动',
                'ready': '桥接已初始化，等待飞书连接',
                'connected': '桥接已连接飞书',
                'restarting': '桥接异常退出，等待自动重启',
                'stopped': '桥接已停止',
            }
            detail = labels.get(health.get('phase'), '桥接状态未知')
            pid = health.get('pid')
            updated_at = health.get('updated_at')
            suffix = f'；PID {pid}' if pid else ''
            if updated_at:
                suffix += f'；更新于 {age_text(updated_at)}'
            print(f'{detail}{suffix}')
        print(f'日志：{STATE / "bridge.log"}')
    elif action == 'logs':
        path = STATE / 'bridge.log'
        print(''.join(path.read_text().splitlines(keepends=True)[-60:]) if path.exists() else '暂无日志')
    elif action == 'stop':
        stop()
    elif action in ('start', 'restart'):
        if action == 'restart':
            stop()
        if running():
            print('服务已运行')
            return
        subprocess.Popen([sys.executable, str(Path(__file__).resolve()), 'run'],
                         stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                         stderr=subprocess.DEVNULL, start_new_session=True)
        time.sleep(1)
        if not running():
            raise RuntimeError('服务启动失败，请检查日志')
        print('后台服务已启动；使用 ./start.sh status 或 ./start.sh logs 查看')
    elif action == 'foreground':
        run(True)
    else:
        raise SystemExit('用法：./start.sh [start|stop|restart|status|logs|foreground]')


if __name__ == '__main__':
    main()
