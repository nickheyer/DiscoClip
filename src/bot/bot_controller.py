import os
from sys import platform
import subprocess
from subprocess import Popen

from lib.utils import (
    get_venv_python_dict
)

from models.models import (
    ErrLog
)

# Global subproc
bot_proc = None

def start_bot(host, token):
    global bot_proc
    if bot_proc:
        raise Exception('Bot already on')
    op_call = None
    venv = get_venv_python_dict()
    if venv and os.path.exists(venv):
        op_call = os.path.join(venv, 'bin', 'python')
    elif platform in ["linux", "linux2", "darwin"]:
        op_call = "python3"
    elif platform == "win32":
        op_call = "python.exe"
    args = [op_call, f"bot/bot.py", '--host', host, '--token', token]
    err_log = ErrLog(entry='')
    bot_proc = Popen(args, stderr=subprocess.PIPE, start_new_session=True)
    err_log.entry = bot_proc.stderr.read().decode('utf-8')
    err_log.save()

def kill_bot():
    global bot_proc
    try:
        bot_proc.kill()
        bot_proc.wait()
        bot_proc.close()
    except:
        pass
    bot_proc = None


def restart_bot():
    kill_bot()
    start_bot()