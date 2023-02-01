import sys, os
from flask import Flask, render_template, request, jsonify
from flask_socketio import SocketIO

import atexit
import signal

from lib.utils import (
    set_values,
    set_file,
    get_data
)

from bot.bot_controller import (
    start_bot,
    kill_bot,
    restart_bot
)

from models.models import initialize_db

app = Flask(__name__)
socketio = SocketIO(app)

# Ensure templates are auto-reloaded
app.config["TEMPLATES_AUTO_RELOAD"] = True

@app.before_first_request #auto-starting based on current state stored in statemachine.json
def startup():
    initialize_db()
    current_status = get_data("statemachine")
    if current_status["botState"]:
        start_bot()

#Function called on exit, similar to shutdown
@atexit.register
def exit_shutdown():
    kill_bot(quiet=True)

# Beginning Routes with default index temp func
@app.route("/")
def index():
    data_list = [get_data("statemachine"), get_data("values")]
    return render_template(
        "/index.html",
        data_list=data_list,
        state=data_list[0],
        values=data_list[1]
    )

@app.route("/stat", methods=["POST"])
def get_status():
    return get_data("activity")


@app.route("/save", methods=["POST"])
def save_credentials():
    req = request.get_json()
    file_name = req["file"]
    json_data = req["data"]
    prev = get_data(file_name)
    dif = list()
    for x in prev:
        if prev[x] != json_data[x]:
            dif.append(x)
    if len(dif) > 0:
        set_file(file_name, json_data)
        if get_data('statemachine')['botState']:
            restart_bot()
            return "Saved and restarted"
        else:
            return "Saved"
    return "No values to save"

@app.route("/data", methods=["POST"])
def return_data():
    return get_data(request.get_json()["document"])


@app.route("/on", methods=["POST"])
def turn_bot_on():
    current_state = get_data("statemachine")
    current_values = get_data("values")
    if current_state["botState"]:
        msg = f"Bot Is Already On"
        return msg
    for x, y in current_values.items():
        if x in ['internalReference']:
            pass
        elif y in [None, ""]:
            return "Missing Values"

    try:
        start_bot()
    except:
        kill_bot()
    msg = f"Turning Bot On"
    return msg


@app.route("/off", methods=["POST"])
def turn_bot_off():
    if get_data("statemachine")[f"botState"] == False:
        msg = "Bot is already off"
        return msg
    kill_bot()
    msg = f"Turning Bot Off"
    return msg


@app.route("/shutdown", methods=["POST"])
def shutdown():
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


if __name__ == "__main__":
    socketio.run(app)
