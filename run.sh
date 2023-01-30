#!/bin/sh
PORT=7600
gunicorn --bind 0.0.0.0:$PORT src.app:app
echo "Shutting down Gunicorn Server"