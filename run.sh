#!/bin/sh
PORT=7600
cd src
gunicorn --bind 0.0.0.0:$PORT app:app
echo "Shutting down Gunicorn Server"