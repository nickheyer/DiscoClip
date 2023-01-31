from peewee import *
# http://docs.peewee-orm.com/en/latest/peewee/

import datetime
from pathlib import Path

from lib.utils import (
    get_db_file
)

DB_FILE = get_db_file()

db = SqliteDatabase(DB_FILE)

class BaseModel(Model):
    class Meta:
        database = db

class Configuration(BaseModel):
    is_archive = BooleanField(default=True)
    token = CharField()
    upload_size_limit = IntegerField(default=8)

class State(BaseModel):
    bot_state = BooleanField(default=False)
    app_state = BooleanField()
    current_activity = CharField(default='offline')

class ErrLog(BaseModel):
    id = AutoField()
    created = DateTimeField(default=datetime.datetime.now())
    entry = CharField(default="Error Occured")

class File(BaseModel):
    id = AutoField()
    created = DateTimeField(default=datetime.datetime.now())
    status = CharField(default='waiting')

    duration = IntegerField(default=0)
    platform = CharField(default='Invalid')
    size = IntegerField(default=0)
    frames = IntegerField(default=0)
    fps = IntegerField(default=0)
    target_size = IntegerField(default=0)
    bitrate = IntegerField(default=0)
    video_bitrate = IntegerField(default=0)
    audio_bitrate = IntegerField(default=128_000)
    
    file_id = CharField(unique=True)
    url = CharField(unique=True)
    download_url = CharField(unique=True)

    file_name = CharField(default="File doesn't exist")
    full_path = CharField(default="File doesn't exist")


def initialize_db():
    db.connect()
    db.create_tables([
        Configuration,
        State,
        ErrLog,
        File
    ],
    safe=True)

if __name__ == '__main__':
    initialize_db()