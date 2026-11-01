import sys
import os

sys.path.insert(0, os.path.dirname(__file__))

if __name__ == "__main__":
    import subprocess
    import socket as _socket

    # 端口与监听地址可由环境变量覆盖：
    # - Docker/既有部署不设这些变量，行为与原先完全一致（12401 + 0.0.0.0）
    # - 桌面打包（Tauri 壳）由编排器分配空闲端口并注入，且强制只监听回环
    # 变量名沿用 app/core/config.py 的 Settings.server_port（SERVER_PORT），
    # 这样 /api/v1/system/info 上报的端口与真实监听端口不会互相矛盾。
    _port = int(os.environ.get("SERVER_PORT") or 12401)
    _host = os.environ.get("SERVER_HOST") or "0.0.0.0"

    # 启动前清理目标端口上的旧进程。
    # 默认关闭：这段逻辑会 taskkill /F 掉「任何」监听该端口的进程，在桌面环境
    # 下可能杀掉用户无关的程序（端口由 OS 动态分配，无法预知占用者是谁）。
    # 桌面打包由 Tauri 编排器统一管理子进程生命周期，不需要这个兜底。
    # 需要旧行为时显式设 AUTOMATION_KILL_PORT_OWNER=1。
    if os.environ.get("AUTOMATION_KILL_PORT_OWNER") == "1":
        try:
            result = subprocess.run(
                ["netstat", "-ano"], capture_output=True, text=True, shell=True
            )
            for line in result.stdout.splitlines():
                if f":{_port}" in line and "LISTENING" in line:
                    parts = line.strip().split()
                    if parts:
                        pid = parts[-1]
                        try:
                            subprocess.run(["taskkill", "/F", "/PID", pid],
                                         capture_output=True, text=True, shell=True)
                        except Exception:
                            pass
        except Exception:
            pass

    # Patch socket.socket.__init__ to always set SO_REUSEADDR
    # Workaround for Windows zombie TCP port entries
    _orig_init = _socket.socket.__init__

    def _patched_init(self, *args, **kwargs):
        _orig_init(self, *args, **kwargs)
        try:
            self.setsockopt(_socket.SOL_SOCKET, _socket.SO_REUSEADDR, 1)
        except OSError:
            pass

    _socket.socket.__init__ = _patched_init

    import uvicorn
    uvicorn.run(
        "app.main:app",
        host=_host,
        port=_port,
        reload=False,
        access_log=False,
        log_level="info"
    )

    # Restore original __init__ after uvicorn exits
    _socket.socket.__init__ = _orig_init
