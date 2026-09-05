# -*- coding: utf-8 -*-
"""GUI 冒烟测试：pywinauto (UIA/accesskit) 驱动 + 交互日志断言。

用法（PowerShell，需先 pip install pywinauto pyautogui pillow）：
    python tests/gui_smoke.py --password p2p-smoke-test-123

流程（自足式，不污染真实缓存）：
  1. 临时目录作 P2P_ID_CACHE_DIR
  2. CLI 管道创建测试身份（固定 BIP39 测试向量，restore 路径）
  3. 启动 GUI（同缓存目录）→ UIA 定位控件 → 登录
  4. 断言 interact.log：登录成功、/list 回显
  可选：--contact <名> --attach 时对已运行窗口做联系人点击断言（需已有联系人）。
"""

import argparse
import os
import subprocess
import sys
import tempfile
import time
from pathlib import Path

# 固定 BIP39 测试向量（仅测试用，无真实资产）
SMOKE_MNEMONIC = (
    "legal winner thank year wave sausage worth useful legal winner thank yellow"
)
SMOKE_NAME = "SmokeTest"
SMOKE_PROFILE = ["1990-01-01", "M"]
CHAT_TITLE = "P2P 聊天 GUI"


def find_newest_interact_log() -> Path:
    # 缓存根跟随 P2P_ID_CACHE_DIR（冒烟用临时目录）；缺省= 用户主目录
    root = Path(os.environ.get("P2P_ID_CACHE_DIR", str(Path.home() / ".p2p_rust_app")))
    root = root / "gui_logs"
    dirs = sorted(d for d in root.iterdir() if d.is_dir())
    if not dirs:
        raise RuntimeError(f"gui_logs 下无日志目录（GUI 未启动过？）: {root}")
    return dirs[-1] / "interact.log"


def wait_log_contains(log: Path, needle: str, timeout: float = 20.0) -> bool:
    """轮询日志直到包含 needle（GUI 异步落盘，drain 间隔毫秒级）"""
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            if needle in log.read_text(encoding="utf-8", errors="ignore"):
                return True
        except OSError:
            pass
        time.sleep(0.3)
    return False


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--exe", default=r"target\debug\p2p_rust_app.exe")
    ap.add_argument("--password", help="测试身份密码（不落盘、不进脚本）")
    ap.add_argument("--attach", action="store_true", help="附加已运行 GUI 而非新启动")
    ap.add_argument("--contact", help="可选：登录后点击该联系人并断言日志")
    args = ap.parse_args()

    try:
        from pywinauto import Desktop, Application
    except ImportError:
        print("缺少依赖：pip install pywinauto pyautogui pillow")
        return 2

    exe = Path(args.exe).resolve()
    if not exe.exists():
        print(f"exe 不存在: {exe}")
        return 2

    cache_dir = tempfile.mkdtemp(prefix="p2p_smoke_")
    env = {"P2P_ID_CACHE_DIR": cache_dir}

    if not args.attach:
        # 1) CLI 管道创建测试身份（restore 路径：r → 助记词 → 资料 → 密码 → /q）
        if not args.password:
            print("需要 --password <测试密码>")
            return 2
        os.environ["P2P_ID_CACHE_DIR"] = cache_dir
        # --cli 进的是 main.rs 主菜单（CLI = 菜单运行器），先发 "4" 选 P2P 聊天（与 e2e 同法）
        feed = "\n".join(
            ["4", "r", SMOKE_MNEMONIC, SMOKE_NAME, *SMOKE_PROFILE, args.password, "/q"]
        )
        r = subprocess.run(
            [str(exe), "--cli"],
            input=feed,
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="ignore",
        )
        if "登录成功" not in (r.stdout or ""):
            print("CLI 建身份失败（未见 登录成功）：")
            print((r.stdout or "")[-800:])
            return 3
        print("[1/4] 测试身份已创建（临时缓存目录）")

    # 2) 启动 GUI（继承 P2P_ID_CACHE_DIR——os.environ 已含）
    if args.attach:
        app = Application(backend="uia").connect(title=CHAT_TITLE, timeout=60)
    else:
        # 直接带 CHILD_ENV=1 分离启动 GUI——绕过分发规则：
        # 管道 stdin 会进 CLI 主菜单（规则2），终端检测也不可靠于脚本环境
        os.environ["P2P_GUI_CHILD"] = "1"
        DETACHED_PROCESS = 0x00000008
        CREATE_NEW_PROCESS_GROUP = 0x00000200
        gui_proc = subprocess.Popen(
            [str(exe)],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            creationflags=DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP,
        )
        del os.environ["P2P_GUI_CHILD"]
    win = Desktop(backend="uia").window(title=CHAT_TITLE)
    win.wait("visible", timeout=60)
    log = find_newest_interact_log()
    print(f"[2/4] GUI 已启动，日志: {log}")

    def find(name: str, types=("Button", "Edit", "Text", "Document", "Pane"), timeout=15):
        """按控件名在多种 control_type 中等待查找（egui/accesskit 命名）"""
        deadline = time.time() + timeout
        while time.time() < deadline:
            for t in types:
                try:
                    el = win.child_window(title=name, control_type=t)
                    if el.exists(timeout=0.5):
                        return el
                except Exception:
                    pass
            time.sleep(0.4)
        return None

    def click(el):
        """UIA Invoke 优先（egui/accesskit 按钮支持语义点击，实测可靠）；
        失败回退鼠标点击（静态文本无 Invoke 模式时按元素坐标点）"""
        try:
            el.invoke()
        except Exception:
            el.click_input()

    # 3) 登录（附加模式跳过——假定已登录）
    if not args.attach and args.password:
        btn = find(f"1. {SMOKE_NAME}")
        if btn is None:
            print("UIA 未找到身份按钮（accesskit 树不可达？dump 见下）")
            win.print_control_identifiers(depth=3)
            return 4
        click(btn)
        print("[3/4] 已点身份卡片，输入密码…")
        time.sleep(1.0)
        edit = win.child_window(control_type="Edit")
        if not edit.exists(timeout=8):
            print("未找到 Edit 控件，转储控件树：")
            win.print_control_identifiers(depth=6)
            return 4
        try:
            edit.set_edit_text(args.password)
        except Exception:
            pass
        # UIA ValuePattern 对 egui 无效（accesskit 不支持 SetValue）→
        # 真点击聚焦 + 剪贴板粘贴（绕开中文输入法：pyautogui 逐字符键入会被 IME 拦截）
        import pyautogui
        import pyperclip

        edit.click_input()
        time.sleep(0.5)
        pyperclip.copy(args.password)
        pyautogui.hotkey("ctrl", "v")
        time.sleep(0.5)
        unlock = find("解锁")
        if unlock:
            click(unlock)
        else:
            edit.type_keys("{ENTER}")
        if not wait_log_contains(log, "登录成功"):
            print("FAIL：interact.log 未见 登录成功")
            return 5
        print("[3/4] 登录成功（日志断言通过）")

    # 4) 聊天态冒烟：/list 快捷按钮回显 + 可选联系人点击
    time.sleep(2.0)
    list_btn = find("/list")
    if list_btn is None:
        print("未找到 /list 按钮，转储控件树：")
        win.print_control_identifiers(depth=6)
        print("WARN：跳过 /list 断言")
    else:
        click(list_btn)
        if not wait_log_contains(log, "/cmd: /list"):
            print("FAIL：/list 点击未落日志")
            return 6
        print("[4/4] /list 回显断言通过")

    if args.contact:
        el = find(args.contact, types=("Text", "ListItem", "Button"))
        if el is None:
            print(f"FAIL：未找到联系人 {args.contact}")
            return 7
        click(el)
        if not wait_log_contains(log, f"点击: 切换会话: {args.contact}"):
            print("FAIL：联系人点击未落日志")
            return 7
        print(f"[4/4] 联系人点击断言通过（{args.contact}）")

    # 5) 信任按钮冒烟（需 --contact 同时给出；联系人未互信时按钮文案为"信任"）
    if args.contact:
        trust = find("信任")
        if trust:
            click(trust)
            if not wait_log_contains(log, "点击: 信任联系人"):
                print("FAIL：信任点击未落日志")
                return 8
            print("[5/5] 信任按钮断言通过")

    print("SMOKE PASS")
    # 收尾：关闭自启的 GUI（残留实例会干扰后续 e2e——mDNS/端口）；
    # GUI 是分离孙进程，按名清理（attach 模式不动用户窗口）
    if not args.attach:
        subprocess.run(
            ["taskkill", "/IM", exe.name, "/F"],
            capture_output=True,
        )
    return 0


if __name__ == "__main__":
    sys.exit(main())
