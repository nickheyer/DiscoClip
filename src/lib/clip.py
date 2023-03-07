import requests
import subprocess
import urllib
import re
import os

from lib.utils import (
    get_cache_path,
    concat_path,
    delete_file,
    move_file,
    get_archive_path,
    check_file_exists
)

from models.models import (
    File,
    DiscordServer,
    User
)

import cv2
import instaloader as IL
from uuid import uuid4

class Clip:
    def __init__(self, message, url, config, callback, socket) -> None:
        self.message = message
        self.Id = uuid4().hex
        self.url = url
        self.config = config
        self.target_size = int(self.config['UPLOAD_SIZE_LIMIT']) * 1_000_000
        self.on_change = callback
        self.socket = socket

    @staticmethod
    def eval_msg(msg, config):
        avail_netlocs = []
        if config['IS_INSTAGRAM']:
            avail_netlocs.extend(['www.instagram.com', 'm.instagram.com', 'instagram.com'])
        if config['IS_TIKTOK']:
            avail_netlocs.extend(['www.tiktok.com', 'm.tiktok.com', 'tiktok.com'])
        urls = re.findall('http[s]?://(?:[a-zA-Z]|[0-9]|[$-_@.&+]|[!*\(\),]|(?:%[0-9a-fA-F][0-9a-fA-F]))+', msg.content.strip())
        for url in urls:
            parsed = urllib.parse.urlparse(url)
            if parsed.path and parsed.netloc.lower() in avail_netlocs:
                return url
        return None

    async def initialize(self):
        await self.__generate_log(f'Initializing clip-object #{self.Id}')
        self.platform = self.__validate_link(self.url)
        self.file_name = f'{self.__get_short_platform()}-{self.Id}.mp4'

    def __validate_link(self, url):
        parsed_url = urllib.parse.urlparse(url)
        netloc = parsed_url.netloc.lower()
        platforms = {
            'www.tiktok.com': 'TikTok',
            'm.tiktok.com': 'TikTok',
            'tiktok.com': 'TikTok',
            'www.instagram.com': 'Instagram',
            'm.instagram.com': 'Instagram',
            'instagram.com': 'Instagram',
        }
        pattern = {
            'TikTok': r'(/video/|/t/)([^/?]+)',
            'Instagram': r'/(reel|p)/([^/?]+)',
        }.get(platforms.get(netloc), None)
        if pattern is None:
            return 'Invalid'
        m = re.search(pattern, parsed_url.path)
        if m:
            self.short_code = m.group(2)
            return platforms.get(netloc, 'Invalid')
        return 'Invalid'


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

    def __generate_tt_info(self, creds) -> list:
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

    def __get_short_platform(self):
        shorts = {
            'TikTok': 'TT',
            'Instagram': 'IG',
        }
        return shorts.get(self.platform, '')

    async def __get_download_link(self):
        try:
            if self.platform == 'TikTok':
                creds = self.__get_tt_downloader_creds()
                cookies, headers, data = self.__generate_tt_info(creds)
                response = requests.post('https://ttdownloader.com/search/',
                        cookies=cookies,
                        headers=headers,
                        data=data)
                link = [i for i in str(response.text).split()
                    if i.startswith("href=")][0][6:-10]
                return link
            elif self.platform == 'Instagram':
                il = IL.Instaloader()
                ig_post = IL.Post.from_shortcode(il.context, self.short_code)
                return ig_post.video_url
        except Exception as e:
            e = f'Error occured while creating download link: {e}'
            print(e)
            await self.__generate_log(e)
    
    async def __generate_log(self, event_str = None):
        if self.socket:
            await self.socket.emit('bot_add_log', {
                'log': event_str
            })

    async def __generate_thumbnail(self, data):
        try:
            frame_num = data.get(cv2.CAP_PROP_FRAME_COUNT) // 2
            data.set(cv2.CAP_PROP_POS_FRAMES, frame_num)
            _, frame = data.read()
            thumb_dir = get_archive_path('thumbnails')
            thumb_file_name = f'THUMB_{self.Id}.jpg'
            await self.__generate_log(f'Generating {thumb_file_name}')
            thumb_path = concat_path(thumb_dir, thumb_file_name)
            cv2.imwrite(str(thumb_path), frame)
            data.release()
            self.thumb_path = thumb_path
        except Exception as e:
            e = f'Error occured while generating thumbnail: {e}'
            print(e)
            await self.__generate_log(e)

    async def __calculate_media_info(self):
        data = cv2.VideoCapture(str(self.full_path))
        self.frames = data.get(cv2.CAP_PROP_FRAME_COUNT)
        self.fps = data.get(cv2.CAP_PROP_FPS)
        self.duration = round(self.frames / self.fps) + 1
        self.bitrate = (self.size * 8) / self.duration
        self.target_size = self.size * 8
        self.audio_bitrate = 128_000
        self.video_bitrate = self.bitrate - self.audio_bitrate
        if self.is_archived:
            await self.__generate_thumbnail(data)
        else:
            self.thumb_path = None

    def __calculate_target_bitrate(self, target):
        data = cv2.VideoCapture(str(self.full_path))
        self.frames = data.get(cv2.CAP_PROP_FRAME_COUNT)
        self.fps = data.get(cv2.CAP_PROP_FPS)
        self.duration = round(self.frames / self.fps) + 1
        self.bitrate = ((self.target_size * 8) / self.duration) * .97
        self.audio_bitrate = 128_000
        self.video_bitrate = self.bitrate - self.audio_bitrate

    def __generate_server(self):
        server, _ = DiscordServer.get_or_create(
            server_name = self.message.guild.name,
            server_id = self.message.guild.id
            )
        return server

    def __generate_user(self):
        self.server = self.__generate_server()
        self.user, _ = User.get_or_create(
            username = f'{self.message.author.name}#{self.message.author.discriminator}'
        )
        if self.user not in self.server.users:
            self.server.users.add(self.user)

        return self.user

    async def generate_file(self):
        f = File.get_or_none(file_id=self.Id)
        if not f:
            file = File()
            file.file_id = self.Id
            file.short_code = self.short_code
            file.url = self.url
            file.platform = self.platform
            file.download_url = self.download_link
            file.file_name = self.file_name
            file.full_path = str(self.full_path)
            file.channel_id = self.message.channel.id
            self.user = self.__generate_user()
            file.server = self.server
            await self.__calculate_media_info()
            file.thumb_path = str(self.thumb_path)
            file.frames = self.frames
            file.fps = self.fps
            file.duration = self.duration
            file.target_size = self.target_size
            file.original_size = self.original_size
            file.size = self.size
            file.bitrate = self.bitrate
            file.audio_bitrate = self.audio_bitrate
            file.video_bitrate = self.video_bitrate
            file.is_archived = self.is_archived
            file.is_transcoded = self.is_transcoded
            file.is_deleted = self.is_deleted
            file.upload_count = 1
            file.save()
            if file not in self.user.files:
                self.user.files.add(file)
                self.user.save()
            self.file = file
            await self.__generate_log(f'File #{self.Id} generated in DB.')
        else:
            f.upload_count += 1
            f.save()
        await self.on_change(f'waiting for links')

    async def __generate_upload_embed(self, bot):
        emb = bot.Embed(
            type='rich',
            title=f'Shared a video from {self.platform}',
            url=f'{self.url}',
            color=0x966FD6
            )
        emb.set_author(
            name=str(self.message.author),
            icon_url=self.message.author.display_avatar
            )
        if self.config['IS_MENTION_USER']:
            emb.add_field(
                name='Requestor',
                value=f'{self.message.author.mention} :arrow_heading_down:',
                inline=False)
        if self.config['IS_MENTION_MENTIONED'] and len(self.message.mentions) > 0:
            users = [f'<@{user.id}>' for user in self.message.mentions]
            emb.add_field(
                name='Mentioned',
                value=f':speaking_head: {" ".join(users)}',
                inline=False)
        if self.config['IS_INCLUDE_MSG'] and (
        self.message.content.strip().lower() != self.url.strip().lower()):
            formatted = self.message.content
            for mention in self.message.mentions:
                formatted = formatted.replace(f'<@{mention.id}>', f'@{mention.name}')
            emb.add_field(
                name='Message',
                value=f"```{formatted}```",
                inline=False)
        return emb

    async def __prep_download(self) -> None:
        if self.platform == 'Invalid':
            raise Exception('Invalid Url, eval_msg failed.')
        self.download_loc = get_cache_path('waiting')
        self.full_path = concat_path(self.download_loc, self.file_name)
        self.download_link = await self.__get_download_link()
        # GETTING SIZE FROM REQUEST HEADERS
        resp = requests.get(self.download_link, stream=True)
        self.original_size = int(resp.headers.get('Content-Length', '0'))
        self.size = self.original_size
        if self.original_size == 0:
            e = 'Invalid link, cancelling download.'
            await self.__generate_log(e)
            raise Exception(e)
        resp.close()
        self.is_archived = False
        self.is_transcoded = False
        self.is_deleted = False
    
    async def check_archives(self):
        await self.__prep_download()
        archived_file = File.get_or_none(original_size=self.size, is_deleted=False)
        self.to_archives = True
        if archived_file:
                await self.__generate_log(f'File #{archived_file.file_name} found')
                self.file = archived_file
                self.Id = archived_file.file_id
                self.full_path = archived_file.full_path
                self.file_name = archived_file.file_name
                self.size = archived_file.size
                self.is_archived = True
                self.to_archives = False
                return True
        return False
    
    async def check_size(self):
        if self.size <= self.target_size:
                return True
        return False

    def __get_clip_size(self, clip_path):
        return os.path.getsize(str(clip_path))

    async def download(self):        
        try:
            res = requests.get(self.download_link)
            with open(self.full_path, 'wb') as f:
                f.write(res.content)
        except Exception as e:
            e = f'Error while downloading: {e}'
            print(e)
            await self.__generate_log(e)

    async def transcode(self):
        await self.move('incomplete')
        self.__calculate_target_bitrate(self.target_size)
        dest_dir = get_cache_path('complete')
        dest_path = concat_path(dest_dir, self.file_name)
        await self.on_change(f'transcoding {self.file_name}')
        try:
            async def run_transcode():
                proc = subprocess.Popen([
                        'ffmpeg',
                        '-y',
                        '-i',
                        str(self.full_path),
                        '-b:v',
                        f'{self.video_bitrate}',
                        '-b:a',
                        f'{self.audio_bitrate}',
                        str(dest_path)
                    ]).wait()
                if proc == 0:
                    await self.__generate_log(f'Transcoding {self.file_name} ({self.original_size // 1_000_000}mb)')
                    delete_file(self.full_path)
                    self.is_transcoded = True
                    self.full_path = dest_path
                    self.size = self.__get_clip_size(self.full_path)
                else:
                    raise Exception('Transcoding failed!')
            await run_transcode()
        except Exception as e:
            e = f'Error while transcoding: {e}'
            print(e)
            await self.__generate_log(e)

    async def upload(self, target, bot):
        e = f'Uploading {self.file_name}'
        await self.on_change(e)
        await self.__generate_log(e)
        clip = bot.File(self.full_path, filename=self.file_name)
        if self.config['IS_ONLY_CLIP']:
            await target.edit(content=None,
            embed=None,
            attachments=[clip]
            )
        else:
            await target.edit(content=None,
            embed=await self.__generate_upload_embed(bot=bot),
            attachments=[clip]
            )

    async def check_exists(self):
        path = self.full_path
        return check_file_exists(path)

    async def move(self, destination):
        dest_dir = get_cache_path(destination)
        dest_path = concat_path(dest_dir, self.file_name)
        move_file(self.full_path, dest_path)
        self.full_path = dest_path

    async def archive(self):
        if self.to_archives:
            arch_dir = get_archive_path('videos')
            arch_file_path = concat_path(arch_dir, self.file_name)
            move_file(self.full_path, arch_file_path)
            self.full_path = arch_file_path
            self.is_archived = True
            await self.__generate_log(f'Archiving {self.file_name}')
    
    async def delete(self, record=False):
        # IF FILE-TO-DELETE IS IN ARCHIVE, DONT DELETE
        if self.is_archived:
            return
        delete_file(self.full_path)
        if record:
            self.is_deleted = True
            self.size = 0
            self.full_path = None
            await self.__generate_log(f'Deleting {self.file_name}')