import sys, os
import requests
from flask import Flask, render_template, request
from flask_socketio import SocketIO
import atexit
import signal

from models.utils import (
    update_config,
    update_state,
    get_logs,
    get_dict,
    get_verbose_dict,
    add_log
)

from lib.utils import initialize_dirs

from bot.bot_controller import (
    start_bot,
    kill_bot
)

from models.models import initialize_db

# --- FLASK/SOCKET INSTANTIATION

app = Flask(__name__)
socketio = SocketIO(app)
app.config["TEMPLATES_AUTO_RELOAD"] = True


# --- MISC HELPERS ---

def refresh_logs():
    socketio.emit('bot_log_added', {
        'log': get_logs(100)
    })

def add_refresh_log(log):
    add_log(log)
    refresh_logs()

def check_for_null(data):
    nulls = []
    for k, v in data.items():
        if v in [None, '']:
            nulls.append(k)
    return nulls

def validate_disc_token(token):
    response = requests.get(
        'https://discord.com/api/v9/users/@me',
        headers={'Authorization': f'Bot {token}'})
    return response.status_code == 200

# --- STARTUP ---

@app.before_first_request
def startup():
    initialize_dirs()
    initialize_db()
    add_refresh_log('Server started!')
    update_state({
        'app_state': True,
        'current_activity': 'Offline'
    })

# --- SHUTDOWN ---

@atexit.register
def exit_shutdown():
    add_refresh_log('Server killed!')
    update_state({
        'bot_state': False,
        'app_state': False
    })
    kill_bot()

# --- HTTP ROUTES ---

@app.route("/")
def index():
    return render_template("/index.html")

# --- WEBSOCKET ROUTES ---

# - CLIENT SOCKETS

@socketio.on('client_connect')
def socket_on_connect(data):
    socketio.emit('client_info', {
        'config': get_verbose_dict('Configuration'),
        'state': get_dict('State'),
        'log': get_logs(100)
    })

@socketio.on('get_config')
def socket_get_config(data):
    config =  get_dict('Configuration')
    socketio.emit('config', config)

@socketio.on('update_config')
def socket_update_config(data):
    config = update_config(data)
    socketio.emit('config_updated', config)

@socketio.on('server_off')
def socket_turn_server_off(data):
    kill_bot()
    if sys.platform in ["linux", "linux2"]:
        os.system("pkill -f gunicorn")
    elif sys.platform == "win32":
        sig = getattr(signal, "SIGKILL", signal.SIGTERM)
        os.kill(os.getpid(), sig)
    try:
        shutdown_func = request.environ.get("werkzeug.server.shutdown")
        shutdown_func()
    except:
        pass
    sys.exit()

@socketio.on('bot_on')
def socket_turn_bot_on(data):
    bot_state = get_dict('State')['bot_state']
    config = get_dict('Configuration')
    try:
        if bot_state:
            raise Exception("Bot already on!")
        null_fields = check_for_null(config)
        if len(null_fields) > 0:
            return {
                'success': False,
                'error': f'Missing required configuration fields: {", ".join(null_fields)}'
            }
        if not validate_disc_token(config['token']):
            return {
                'success': False,
                'error': 'Invalid Discord Token'
            }
        start_bot(request.host, config['token'])
        return {
            'success': True
        }
    except Exception as e:
        socketio.emit('bot_on_finished', { 'error': str(e) })

@socketio.on('bot_off')
def socket_turn_bot_on(data):
    bot_state = get_dict('State')['bot_state']
    try:
        if not bot_state:
            raise Exception("Bot already off!")
        kill_bot()
        socketio.emit('bot_off_finished', { 'success': True })
        update_state({
            'bot_state': False
        })
        add_refresh_log('Bot killed!')
        refresh_logs()
    except Exception as e:
        socketio.emit('bot_off_finished', { 'error': str(e) })

# - BOT SOCKETS

@socketio.on('bot_connect')
def socket_on_connect(data):
    return get_dict('Configuration')

@socketio.on('bot_started')
def socket_bot_started(data):
    # Forwarding the emit to the client from bot subproc
    socketio.emit('bot_on_finished', data)
    update_state({
        'bot_state': True
    })
    add_refresh_log('Bot started!')

@socketio.on('bot_change_presence')
def socket_bot_change_presence(data):
    status = {
        'current_activity': data['presence']
    }
    update_state(status)
    socketio.emit('update_status', status)

@socketio.on('bot_add_log')
def socket_bot_log_added(data):
    add_refresh_log(data['log'])

# --- SOCKET/FLASK APP LOOP ---

if __name__ == "__main__":
    socketio.run(app)
