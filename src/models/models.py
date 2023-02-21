from peewee import *
from playhouse.sqlite_ext import SqliteDatabase
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
    is_archive = BooleanField(default=True, verbose_name='Archive Videos')
    is_debug = BooleanField(default=False, verbose_name='Send Debug Message On Error')
    is_delete = BooleanField(default=True, verbose_name='Delete Original Message')
    is_mention_user = BooleanField(default=True, verbose_name='Mention Requesting User')
    is_mention_mentioned = BooleanField(default=True, verbose_name='Mention Users Mentioned In Original Message')
    is_include_msg = BooleanField(default=True, verbose_name='Include Original Message In Response')
    is_only_clip = BooleanField(default=False, verbose_name='Only Include Video In Response')
    is_instagram = BooleanField(default=True, verbose_name='Instagram Clips')
    is_tiktok = BooleanField(default=True, verbose_name='TikTok Clips')
    token = CharField(default='', verbose_name='Discord Token')
    upload_size_limit = IntegerField(default=8, verbose_name='Upload Size Limit')

class State(BaseModel):
    bot_state = BooleanField(default=False)
    app_state = BooleanField(default=True)
    current_activity = CharField(default='Offline')

class ErrLog(BaseModel):
    id = AutoField()
    created = DateTimeField(default=datetime.datetime.now)
    entry = CharField(default="Error Occured")

class EventLog(BaseModel):
    id = AutoField()
    created = DateTimeField(default=datetime.datetime.now)
    entry = CharField(default="Event Occured")

class DiscordServer(BaseModel):
    id = AutoField()
    added = DateTimeField(default=datetime.datetime.now)

    server_name = CharField(default='None')
    server_id = CharField(default='None')

class User(BaseModel):
    id = AutoField()
    added = DateTimeField(default=datetime.datetime.now)

    username = TextField()
    upload_count = IntegerField(default=0)
    discord_servers = ManyToManyField(DiscordServer, backref='users')

ServerUsers = User.discord_servers.get_through_model()

class File(BaseModel):
    id = AutoField()
    created = DateTimeField(default=datetime.datetime.now)
    duration = FloatField(default=0)
    original_size = FloatField(default=0)
    size = FloatField(default=0)
    frames = IntegerField(default=0)
    fps = FloatField(default=0)
    target_size = FloatField(default=0)
    bitrate = FloatField(default=0)
    video_bitrate = FloatField(default=0)
    audio_bitrate = FloatField(default=128_000)
    is_transcoded = BooleanField(default=False)
    is_archived = BooleanField(default=False)
    is_deleted = BooleanField(default=False)
    
    file_id = CharField(unique=True)
    url = CharField(unique=True)
    download_url = CharField(unique=True)
    upload_count = IntegerField(default=0)
    platform = CharField(default='Invalid')

    file_name = CharField(default=None, null=True)
    full_path = CharField(default=None, null=True)
    thumb_path = CharField(default=None, null=True)

    requestors = ManyToManyField(User, backref='files')
    channel_id = CharField(default=None, null=True)
    server = ForeignKeyField(DiscordServer, backref='files')

UserFiles = File.requestors.get_through_model()

def initialize_db():
    db.connect()
    db.create_tables([
        Configuration,
        State,
        ErrLog,
        EventLog,
        File,
        User,
        DiscordServer,
        ServerUsers,
        UserFiles
    ],
    safe=True)
    if not Configuration.select().exists():
        Configuration.create()
    if not State.select().exists():
        State.create()

if __name__ == '__main__':
    initialize_db()