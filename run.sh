#!/bin/sh
PORT=7600
cd src
gunicorn -k geventwebsocket.gunicorn.workers.GeventWebSocketWorker -w 1 --bind 0.0.0.0:$PORT app:app
echo "Shutting down Gunicorn Server"