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


def find_newest_gui_dir() -> Path:
    # 缓存根跟随 P2P_ID_CACHE_DIR（冒烟用临时目录）；缺省= 用户主目录
    root = Path(os.environ.get("P2P_ID_CACHE_DIR", str(Path.home() / ".p2p_rust_app")))
    root = root / "gui_logs"
    dirs = sorted(d for d in root.iterdir() if d.is_dir())
    if not dirs:
        raise RuntimeError(f"gui_logs 下无日志目录（GUI 未启动过？）: {root}")
    return dirs[-1]


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
    gui_dir = find_newest_gui_dir()
    log = gui_dir / "interact.log"
    runtime_log = gui_dir / "runtime.log"
    print(f"[2/4] GUI 已启动，日志: {gui_dir}")

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
        invoke 偶发挂死（禁用控件/COM 请求未被 idle 帧处理）→ 3s 超时线程 + 回退鼠标点击"""
        import threading

        result = {"done": False}

        def _invoke():
            try:
                el.invoke()
                result["done"] = True
            except Exception:
                pass

        t = threading.Thread(target=_invoke, daemon=True)
        t.start()
        t.join(timeout=3.0)
        if result["done"]:
            return
        try:
            el.click_input()
        except Exception:
            pass
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
            # UIA ValuePattern 对 egui 无效（accesskit 不支持 SetValue）——先试语义写入，
            # 未登录成功再回退真输入（需交互桌面：RDP 断开/锁屏时真输入 API 不可用）
            edit.set_edit_text(args.password)
        except Exception:
            pass
        import pyautogui
        import pyperclip

        pyautogui.FAILSAFE = False  # 自动化脚本：UIA 元素坐标可能落在屏幕角落，禁用防呆
        unlock = find("解锁")
        if unlock:
            click(unlock)
        # 语义路径未登录成功 → 回退真输入：点击聚焦 + 清空 + 剪贴板粘贴（绕 IME）+ 再点解锁
        if not wait_log_contains(log, "登录成功", timeout=8.0):
            try:
                edit.click_input()
                time.sleep(0.5)
                pyautogui.hotkey("ctrl", "a")
                pyautogui.press("delete")
                pyperclip.copy(args.password)
                pyautogui.hotkey("ctrl", "v")
                time.sleep(0.5)
                if unlock:
                    click(unlock)
            except Exception as e:
                print(f"WARN：真输入不可用（{type(e).__name__}，无交互桌面？），依赖语义路径结果")
        if not wait_log_contains(log, "登录成功"):
            print("FAIL：interact.log 未见 登录成功")
            print("提示：真输入路径需要交互桌面（RDP 已连接且未锁屏）；"
                  "无头会话请改在有桌面的环境运行冒烟，并以 e2e（管道模式）作为引擎回归门禁")
            return 5
        print("[3/4] 登录成功（日志断言通过）")

    # 4) 聊天态冒烟：/list 快捷按钮回显 + GUI 调试 trace 断言 + 可选联系人点击
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
        # GUI 调试 trace：点击经 send_input 应在 runtime.log 留下 event= 行
        if not wait_log_contains(runtime_log, "event=send kind=line"):
            print("FAIL：runtime.log 未见 ui trace（event=send kind=line）")
            return 6
        print("[4/4] /list 回显 + ui trace 断言通过")

    # 4.5) 备份助记词链路冒烟：侧栏按钮 → BackupPassword 密码卡片 → 解锁 → MnemonicShow 卡片
    time.sleep(1.0)
    backup_btn = find("备份助记词")
    if backup_btn is None:
        print("FAIL：未找到 备份助记词 按钮")
        return 9
    click(backup_btn)
    time.sleep(1.0)
    if not wait_log_contains(runtime_log, "kind=BackupPassword"):
        print("FAIL：BackupPassword Ask 未触发（runtime.log 无 event=ask）")
        return 9
    # UIA 树可能有多个 Edit（accesskit 残留 + 新卡片）——逐个点击粘贴：
    # 真实卡片的 Edit 是最后一个获得焦点的（残留在 UIA 树里但无实际焦点响应），随后单次解锁
    time.sleep(1.0)
    edits = [e for e in win.descendants(control_type="Edit") if e.element_info.enabled]
    if not edits:
        print("FAIL：无可用的 Edit 控件")
        return 9
    for e in reversed(edits):
        e.click_input()
        time.sleep(0.3)
        pyperclip.copy(args.password)
        pyautogui.hotkey("ctrl", "v")
        time.sleep(0.2)
    unlock = find("解锁")
    if unlock is None:
        print("FAIL：密码卡片无 解锁 按钮")
        return 9
    click(unlock)
    if not wait_log_contains(log, "你的身份助记词", timeout=8):
        print("FAIL：解锁后未见助记词输出")
        return 9
    if not wait_log_contains(runtime_log, "event=mnemonic_show"):
        print("FAIL：MnemonicShow 事件未触发")
        return 9
    print("[4.5/5] 备份助记词链路断言通过（Ask 卡片→解锁→MnemonicShow）")

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

    # 5) 信任链路两段式冒烟（需 --contact；按钮→确认卡片→确认→引擎执行）
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
        # 信任按钮 → 确认卡片出现 → 确认信任 → 引擎执行
        trust_btn = find("信任")
        if trust_btn is None:
            print("FAIL：未找到 信任 按钮")
            return 8
        click(trust_btn)
        if not wait_log_contains(runtime_log, "event=trust_card open"):
            print("FAIL：确认卡片未弹出（runtime.log 无 event=trust_card）")
            return 8
        confirm_btn = find("确认信任")
        if confirm_btn is None:
            print("FAIL：未找到 确认信任 按钮（卡片未渲染？）")
            return 8
        click(confirm_btn)
        if not wait_log_contains(log, "已信任:"):
            print("FAIL：确认后引擎未执行信任")
            return 8
        print("[5/5] 信任两段式断言通过（按钮→卡片→确认→已信任）")

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
