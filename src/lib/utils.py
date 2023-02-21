from pathlib import Path
import uuid
import os
import shutil

def get_project_root() -> Path:
    return Path(__file__).absolute().parent.parent.parent

def initialize_dirs():
    root = get_project_root()
    os.makedirs(os.path.join(root, 'data'), exist_ok=True)
    os.makedirs(os.path.join(root, 'archive', 'thumbnails'), exist_ok=True)
    os.makedirs(os.path.join(root, 'archive', 'videos'), exist_ok=True)
    os.makedirs(os.path.join(root, 'cache', 'waiting'), exist_ok=True)
    os.makedirs(os.path.join(root, 'cache', 'incomplete'), exist_ok=True)
    os.makedirs(os.path.join(root, 'cache', 'complete'), exist_ok=True)
    os.makedirs(os.path.join(root, 'log', 'bug-reports'), exist_ok=True)

def get_data_path() -> Path:
    root = get_project_root()
    return root.joinpath('data')

def get_db_file() -> Path:
    data_dir = get_data_path()
    return data_dir.joinpath('disco.db')

def get_bug_report_path(channel) -> Path:
    root = get_project_root()
    dir_path = root.joinpath('log', 'bug-reports')
    file_path = dir_path.joinpath(f'ERR_{channel}_{uuid.uuid4()}.log')
    return file_path

def get_venv_python_dict() -> dict:
    root = get_project_root()
    return root.joinpath('venv')
    
def get_cache_path(dir) -> Path:
    root = get_project_root()
    return root.joinpath('cache', dir)

def get_archive_path(dir) -> Path:
    root = get_project_root()
    return root.joinpath('archive', dir.lower())

def concat_path(*args):
    return Path(*args)

def move_file(src, dest):
    move = shutil.move(src, dest)
    return move

def delete_file(target):
    os.remove(target)

def check_file_exists(full_path):
    return os.path.isfile(full_path)
    