from pathlib import Path
from datetime import date
import uuid
import urllib
import re
import os
import json
import shutil

def get_project_root() -> Path:
    return Path(__file__).absolute().parent.parent.parent

def get_data_path() -> Path:
    root = get_project_root()
    return root.joinpath('data')

def get_db_file() -> Path:
    data_dir = get_data_path()
    return data_dir.joinpath('disco.db')

def get_err_logs_path() -> Path:
    root = get_project_root()
    dir_path = root.joinpath('log', 'bot')
    if not os.path.exists(dir_path):
        os.makedirs(dir_path)
    return dir_path

def get_bug_report_path(channel) -> Path:
    root = get_project_root()
    dir_path = root.joinpath('log', 'bug-reports')
    file_path = dir_path.joinpath(f'ERR_{uuid.uuid4()}.log')
    if not os.path.exists(dir_path):
        os.makedirs(dir_path)
    return file_path

def get_src_path() -> Path:
    root = get_project_root()
    return root.joinpath('src')

def get_cache_path(dir) -> Path:
    root = get_project_root()
    return root.joinpath('cache', dir)

def get_archive_path(plat) -> Path:
    root = get_project_root()
    return root.joinpath('archive', plat.lower())

def concat_path(*args):
    return Path(*args)

def gen_err_log_file_name():
    return f'BOT_{date.today().strftime("%d_%m_%Y")}.log'

def gen_err_log_file_path() -> Path:
    log_name = gen_err_log_file_name()
    log_path = get_err_logs_path().joinpath(log_name)
    set_values("statemachine", "logPath", str(log_path))
    return log_path

def get_current_err_log_file_path():
    log = get_data('statemachine').get('logPath', None)
    return log

DATA_PATH = get_data_path()

def list_files_in_dir(dir):
    return [i for i in os.listdir(dir)]

def get_data(file_name):
    with open(
        os.path.join(DATA_PATH, f"{file_name}.json"), "r"
    ) as fp:
        return json.load(fp)

def set_values(file_name, key, newval):
    values = get_data(file_name)
    values[key] = newval
    with open(
        os.path.join(DATA_PATH, f"{file_name}.json"), "w"
    ) as fpw:
        json.dump(values, fpw)


def set_file(file_name, json_data):
    with open(
        os.path.join(DATA_PATH, f"{file_name}.json"), "w"
    ) as fpw:
        json.dump(json_data, fpw)

def set_file_in_log(fileId, file, currently = None):
    data = get_data('activity')
    if currently != None:
        data['currently'] = currently
    for i, f in enumerate(data['log']):
        if f['Id'] == fileId:
            data['log'][i] = file
    set_file('activity', data)

def search_log_for_file(fileId):
    log = get_data('activity')['log']
    for file in log:
        if file['Id'] == fileId:
            return file
    return None

def update_file_in_log(fileId, key, val):
    file_in_log = search_log_for_file(fileId)
    if file_in_log != None:
        file_in_log[key] = val
        return file_in_log
    else:
        return

def add_file_to_log(file, currently = None):
    activity_file = get_data('activity')
    if currently == None:
        activity_file['currently'] = f'processing {file["Id"]}'
    else:
        activity_file['currently'] = f'{currently}'
    log = activity_file['log'][:19]
    existing = search_log_for_file(file['Id'])
    if existing == None:
        log.insert(0, file)
        activity_file['log'] = log
    else:
        set_file_in_log(file['Id'], file)
    set_file('activity', activity_file)

def update_archive_number():
    qty = 0
    
    ig = get_archive_path('instagram')
    qty += len(list_files_in_dir(ig))

    tt = get_archive_path('tiktok')
    qty += len(list_files_in_dir(tt))

    set_values('activity', 'filesInArchive', qty)

def get_current_status():
    return get_data('activity')['currently']

def get_file_size_mb(path):
    return Path(path).stat().st_size / 1_000_000

def move_file(src, dest):
    move = shutil.move(src, dest)
    return move

def delete_file(target):
    os.remove(target)

def check_file_exists(full_path):
    return os.path.isfile(full_path)

class ClipUrl:
    def __init__(self, url) -> None:
        self.url = url
        self.platform = self.__validate_link(url)
        self.__generate_file_for_log()

    @staticmethod
    def eval_msg(msg):
        return urllib.parse.urlparse(msg.content.strip().lower()).netloc in [
            'www.tiktok.com',
            'm.tiktok.com',
            'tiktok.com',
            'www.instagram.com',
            'instagram.com',
            'm.instagram.com'
        ]

    def __is_valid_tiktok_link(self, url):
        parsed_url = urllib.parse.urlparse(url)
        if parsed_url.netloc.lower() not in ['www.tiktok.com',
                                            'm.tiktok.com',
                                            'tiktok.com'] :
            return False
        # Extract the video ID from the path
        m = re.search(r'/t/([^/]+)/', parsed_url.path)
        if m:
            video_id = m.group(1)
            if video_id:
                self.Id = video_id
            return bool(video_id)
        return False

    def __is_valid_instagram_link(self, url):
        parsed_url = urllib.parse.urlparse(url)
        if parsed_url.netloc.lower() not in ['www.instagram.com',
                                            'm.instagram.com',
                                            'instagram.com']:
            return False
        # Extract the video ID from the path
        m = re.search(r'/reel/([^/]+)/', parsed_url.path)
        if m:
            video_id = m.group(1)
            if video_id:
                self.Id = video_id
            return bool(video_id)
        return False

    def __generate_file_for_log(self):
        file = {
            'Id': self.Id,
            'url': self.url,
            'platform': self.platform,
            'status': 'generated',
            'location': 'Does not yet exist',
            'size': 0
        }
        add_file_to_log(file)

    def __validate_link(self, url):
        if self.__is_valid_tiktok_link(url):
            return "TikTok"
        elif self.__is_valid_instagram_link(url):
            return "Instagram"
        else:
            self.Id = None
            return "Invalid"