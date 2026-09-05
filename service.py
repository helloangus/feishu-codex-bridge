"""Termux service supervisor with flock ownership and rotating sanitized logs."""
import fcntl
import logging
from logging.handlers import RotatingFileHandler
import os
from pathlib import Path
import re
import signal
import subprocess
import sys
import time

BASE = Path(__file__).resolve().parent
STATE = BASE / '.runtime'
LOCK = STATE / 'service.lock'


def redact(text):
    text = re.sub(r'wss?://\S+', '[WebSocket endpoint]', text)
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
        child = subprocess.Popen([sys.executable, '-u', str(BASE / 'bridge.py')],
                                 stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                                 text=True, start_new_session=True)
        stopping = False

        def shutdown(*_):
            nonlocal stopping
            stopping = True
            try:
                os.killpg(child.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
        signal.signal(signal.SIGTERM, shutdown)
        signal.signal(signal.SIGINT, shutdown)
        try:
            for line in child.stdout:
                safe = redact(line.rstrip())
                logger.info(safe)
                if foreground:
                    print(safe, flush=True)
            code = child.wait()
            logger.info('Bridge exited: %s; requested=%s', code, stopping)
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
        print('服务运行中' if running() else '服务未运行')
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
