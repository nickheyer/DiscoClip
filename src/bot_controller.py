import os
from sys import platform
from subprocess import Popen

from src.utils import (
    set_values,
    get_err_logs_path,
    get_src_path,
    gen_err_log_file_path,
    get_project_root
)

# Subprocesses List
bot_proc = None

def start_bot():
    global bot_proc
    op_call = None
    if platform in ["linux", "linux2", "darwin"]:
        op_call = "python3"
    elif platform == "win32":
        op_call = "python.exe"
    args = [op_call, f"bot.py"]
    log_path = get_err_logs_path()
    os.makedirs(log_path, exist_ok=True)
    err_log = open(
        gen_err_log_file_path(),
        "a",
    )
    bot_proc = Popen(args, stderr=err_log, start_new_session=True, cwd=get_src_path())
    set_values("statemachine", "botState", True)
    set_values('activity', 'currently', 'waiting for links')


def kill_bot():
    global bot_proc
    set_values("statemachine", "botState", False)
    set_values('activity', 'currently', 'offline')
    try:
        bot_proc.kill()
        bot_proc.wait()
        bot_proc.close()
    except:
        pass

def restart_bot():
    kill_bot()
    start_bot()