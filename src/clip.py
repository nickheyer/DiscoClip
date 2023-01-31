import requests
import instaloader as IL
import subprocess
import cv2


from utils import (
    get_cache_path,
    concat_path,
    search_log_for_file,
    set_file_in_log,
    delete_file,
    get_file_size_mb,
    move_file,
    get_archive_path,
    list_files_in_dir,
    update_archive_number,
    check_file_exists
)

class Clip:
    def __init__(self, ClipUrlObject) -> None:
        self.Id = ClipUrlObject.Id
        self.platform = ClipUrlObject.platform
        self.url = ClipUrlObject.url
        self.size = 0
        self.duration = 0
        self.valid = self.platform in ['TikTok', 'Instagram']
        if self.platform == 'TikTok':
            self.creds = self.__get_tt_downloader_creds()
            self.info = self.__generate_info(self.creds)
        else:
            self.creds, self.info = None, None
        
        self.download_loc = get_cache_path('waiting')
        self.file_name = f'{self.Id}-{self.platform}.mp4'
        self.full_path = concat_path(self.download_loc, self.file_name)
        self.download_link = self.__get_download_link()

    def __get_tt_downloader_creds(self) -> dict:
        response = requests.get('https://ttdownloader.com/')
        hidden_token_tag = '<input type="hidden" id="token" name="token" value="'
        point = response.text.find(hidden_token_tag) + len(hidden_token_tag)
        token = response.text[point:point+64]
        creds = {
            'token': token,
        }
        for i in response.cookies:
            creds[str(i).split()[1].split('=')[0].strip()] = str(
                i).split()[1].split('=')[1].strip()
        return creds

    def __change_file_in_log(self, status, currently = None):
        file = search_log_for_file(self.Id)
        file['status'] = status
        file['location'] = str(self.full_path)
        file['size'] = self.size
        set_file_in_log(self.Id, file, currently)
        return

    def __generate_info(self, creds) -> list:
        cookies = {
            'PHPSESSID': creds['PHPSESSID'],
        }
        headers = {
            'authority': 'ttdownloader.com',
            'accept': '*/*',
            'accept-language': 'en-US,en;q=0.9',
            'content-type': 'application/x-www-form-urlencoded; charset=UTF-8',
            'origin': 'https://ttdownloader.com',
            'referer': 'https://ttdownloader.com/',
            'sec-ch-ua': '"Not?A_Brand";v="8", "Chromium";v="108", "Google Chrome";v="108"',
            'sec-ch-ua-mobile': '?0',
            'sec-ch-ua-platform': '"Windows"',
            'sec-fetch-dest': 'empty',
            'sec-fetch-mode': 'cors',
            'sec-fetch-site': 'same-origin',
            'user-agent': 'Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/108.0.0.0 Safari/537.36',
            'x-requested-with': 'XMLHttpRequest',
        }
        data = {
            'url': self.url,
            'format': '',
            'token': creds['token'],
        }

        return cookies, headers, data

    def __get_download_link(self):
        try:
            if self.platform == 'TikTok':
                cookies, headers, data = self.info
                response = requests.post('https://ttdownloader.com/search/',
                        cookies=cookies,
                        headers=headers,
                        data=data)
                link = [i for i in str(response.text).split()
                    if i.startswith("href=")][0][6:-10]
                return link
            elif self.platform == 'Instagram':
                il = IL.Instaloader()
                ig_post = IL.Post.from_shortcode(il.context, self.Id)
                return ig_post.video_url
        except Exception as e:
            print(e)
            return None

    def __calculate_bitrate(self, target):
        data = cv2.VideoCapture(str(self.full_path))
        self.frames = data.get(cv2.CAP_PROP_FRAME_COUNT)
        self.fps = data.get(cv2.CAP_PROP_FPS)

        self.duration = round(self.frames / self.fps) + 1
        
        self.target_size = target * 1000 * 1000 * 7.5

        self.bitrate = self.target_size/self.duration
        self.audio_bitrate = 128 * 1000
        self.video_bitrate = self.bitrate - self.audio_bitrate

    def __check_archives(self):
        arch_dir = get_archive_path(self.platform)
        archived_files = list_files_in_dir(arch_dir)
        for file in archived_files:
            if file == self.file_name:
                self.full_path = concat_path(arch_dir, file)
                self.size = get_file_size_mb(self.full_path)
                return True
        return False

    def download(self):
        if not self.__check_archives():
            self.__change_file_in_log('downloading')
            try:
                res = requests.get(self.download_link)
                with open(self.full_path, 'wb') as f:
                    f.write(res.content)
                self.size = get_file_size_mb(self.full_path)
                self.__change_file_in_log('downloaded')
                return True       
            except Exception as e:
                print(e)
                self.__change_file_in_log('error while downloading')
                return False
        return False

    def check_fail(self):
        return not check_file_exists(self.full_path)

    def move(self, destination):
        dest_dir = get_cache_path(destination)
        dest_path = concat_path(dest_dir, self.file_name)
        move_file(self.full_path, dest_path)
        self.full_path = dest_path

    async def transcode(self, target_size_bytes):
        self.__change_file_in_log('transcoding')
        self.__calculate_bitrate(target_size_bytes)
        dest_dir = get_cache_path('completed')
        dest_path = concat_path(dest_dir, self.file_name)
        try:
            async def run_transcode():
                proc = subprocess.Popen([
                        'ffmpeg',
                        '-y',
                        '-i',
                        str(self.full_path),
                        '-b:v',
                        str(self.video_bitrate),
                        '-b:a',
                        str(self.audio_bitrate),
                        str(dest_path)

                    ]).wait()
                if proc == 0:
                    delete_file(self.full_path)
                    self.full_path = dest_path
                    self.size = get_file_size_mb(self.full_path)
            await run_transcode()

        except:
            self.__change_file_in_log('error while transcoding')
    
    def update_for_upload(self):
        self.__change_file_in_log('uploaded', currently='waiting for links')

    def archive(self):
        arch_dir = get_archive_path(self.platform)
        arch_file_path = concat_path(arch_dir, self.file_name)
        if self.full_path != arch_file_path:
            move_file(self.full_path, arch_file_path)
            self.full_path = arch_file_path
            update_archive_number()
            self.__change_file_in_log('archived')
    
    def delete(self):
        delete_file(self.full_path)
        self.full_path = None
        self.size = 0
        self.__change_file_in_log('deleted')